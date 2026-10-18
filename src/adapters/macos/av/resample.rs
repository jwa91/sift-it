//! Resampling: the decoded sound, at the file's own rate, through one converter to 16 kHz. AVAssetReader hands its
//! buffers over in sizes that vary from read to read (it reads ahead on a thread of its own), and AVFoundation's own
//! resampling then gives different samples; one converter fed blocks of its own asking gives the same every time.

use super::describe;
use anyhow::{Result, anyhow, bail};
use block2::RcBlock;
use embedder::AUDIO_RATE;
use objc2::AllocAnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2_av_foundation::AVAssetReaderOutput;
use objc2_avf_audio::{
    AVAudioBuffer, AVAudioCommonFormat, AVAudioConverter, AVAudioConverterInputStatus, AVAudioConverterOutputStatus,
    AVAudioFormat, AVAudioPCMBuffer,
};
use objc2_core_media::CMSampleBuffer;
use std::cell::RefCell;
use std::ptr::NonNull;

const FRAMES: u32 = 8192;

/// The decoded sound in the blocks the converter asks for, whatever the reader's buffers, up to the tracks' end.
struct Native<'a> {
    output: &'a AVAssetReaderOutput,
    sample: Option<Retained<CMSampleBuffer>>,
    offset: usize,
    remaining: usize,
}

impl Native<'_> {
    fn fill(&mut self, destination: &mut [f32]) -> Result<usize> {
        let wanted = destination.len().min(self.remaining);
        let mut filled = 0;
        while filled < wanted {
            if self.sample.is_none() {
                self.sample = autoreleasepool(|_| unsafe { self.output.copyNextSampleBuffer() });
            }
            let Some(sample) = &self.sample else { break };
            let Some(block) = (unsafe { sample.data_buffer() }) else {
                self.sample = None;
                continue;
            };
            let frames = unsafe { block.data_length() } / size_of::<f32>();
            let count = (frames - self.offset).min(wanted - filled);
            let pointer = NonNull::new(destination[filled..].as_mut_ptr()).unwrap().cast();
            let status = unsafe { block.copy_data_bytes(self.offset * 4, count * 4, pointer) };
            if status != 0 {
                bail!("couldn't copy decoded sound ({status})");
            }
            self.offset += count;
            filled += count;
            if self.offset == frames {
                self.sample = None;
                self.offset = 0;
            }
        }
        self.remaining -= filled;
        Ok(filled)
    }
}

/// Reads `seconds` of decoded sound from `output`, and hands it over at 16 kHz.
pub(super) fn read(
    output: &AVAssetReaderOutput,
    seconds: f64,
    on_samples: &mut dyn FnMut(&[f32]) -> Result<()>,
) -> Result<()> {
    let first = loop {
        let Some(sample) = autoreleasepool(|_| unsafe { output.copyNextSampleBuffer() }) else { return Ok(()) };
        if unsafe { sample.num_samples() } > 0 && unsafe { sample.data_buffer() }.is_some() {
            break sample;
        }
    };
    let description = unsafe { first.format_description() }.ok_or_else(|| anyhow!("decoded sound has no format"))?;
    let source = unsafe { AVAudioFormat::initWithCMAudioFormatDescription(AVAudioFormat::alloc(), &description) };
    let destination = unsafe {
        AVAudioFormat::initWithCommonFormat_sampleRate_channels_interleaved(
            AVAudioFormat::alloc(),
            AVAudioCommonFormat::PCMFormatFloat32,
            AUDIO_RATE as f64,
            1,
            true,
        )
    }
    .ok_or_else(|| anyhow!("can't create the sound format"))?;
    let converter =
        unsafe { AVAudioConverter::initFromFormat_toFormat(AVAudioConverter::alloc(), &source, &destination) }
            .ok_or_else(|| anyhow!("can't resample decoded sound"))?;
    let input = pcm_buffer(&source)?;
    let converted = pcm_buffer(&destination)?;
    // Not past the tracks' end: MP3's decoder sometimes adds its synthesis delay (529 samples) after it.
    let native = RefCell::new(Native {
        output,
        sample: Some(first),
        offset: 0,
        remaining: (seconds * unsafe { source.sampleRate() }).round().max(0.0) as usize,
    });
    let input_error = RefCell::new(None);
    let provide =
        RcBlock::new(|requested: u32, mut status: NonNull<AVAudioConverterInputStatus>| -> *mut AVAudioBuffer {
            let count = requested.min(FRAMES) as usize;
            // One float channel (pcm_settings), in a buffer that lives as long as the converter's calls.
            let samples = unsafe { std::slice::from_raw_parts_mut((*input.floatChannelData()).as_ptr(), count) };
            let filled = match native.borrow_mut().fill(samples) {
                Ok(filled) => filled,
                Err(error) => {
                    *input_error.borrow_mut() = Some(error);
                    0
                }
            };
            unsafe {
                input.setFrameLength(filled as u32);
                *status.as_mut() = if filled == 0 {
                    AVAudioConverterInputStatus::EndOfStream
                } else {
                    AVAudioConverterInputStatus::HaveData
                };
            }
            if filled == 0 { std::ptr::null_mut() } else { Retained::as_ptr(&input).cast_mut().cast() }
        });
    loop {
        let mut error = None;
        let status = autoreleasepool(|_| unsafe {
            converted.setFrameLength(0);
            converter.convertToBuffer_error_withInputFromBlock(
                &converted,
                Some(&mut error),
                &*provide as *const _ as *mut _,
            )
        });
        if let Some(error) = input_error.borrow_mut().take() {
            return Err(error);
        }
        if status == AVAudioConverterOutputStatus::Error {
            let reason = error.map(|e| describe(&e)).unwrap_or_default();
            bail!("couldn't resample decoded sound: {reason}");
        }
        let count = unsafe { converted.frameLength() } as usize;
        if count > 0 {
            let samples = unsafe { std::slice::from_raw_parts((*converted.floatChannelData()).as_ptr(), count) };
            on_samples(samples)?;
        }
        if status == AVAudioConverterOutputStatus::EndOfStream {
            break;
        }
    }
    // What the decoder puts out past the end is read and dropped: the reader then says how it ended.
    while autoreleasepool(|_| unsafe { output.copyNextSampleBuffer().is_some() }) {}
    Ok(())
}

fn pcm_buffer(format: &AVAudioFormat) -> Result<Retained<AVAudioPCMBuffer>> {
    let buffer =
        unsafe { AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(AVAudioPCMBuffer::alloc(), format, FRAMES) }
            .ok_or_else(|| anyhow!("can't allocate a sound buffer"))?;
    // Zeroed before it's seen as a slice.
    unsafe { std::ptr::write_bytes((*buffer.floatChannelData()).as_ptr(), 0, FRAMES as usize) };
    Ok(buffer)
}
