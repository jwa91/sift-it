# Decoding sound the same every time

An audio file must give the same samples every time it's read, or its vectors change with each read. AVFoundation alone
doesn't promise that. This says where it varies, and how `adapters/macos/av.rs` reads sound so it doesn't. Measured on
macOS 27.0.1 (26A434), Apple silicon.

## Where AVFoundation varies

Read with AVFoundation's own resampling to 16 kHz, one audio file can come out at two lengths, and with two sets of
samples:

| File | Samples at 16 kHz | Seconds |
| --- | --- | --- |
| `nota-voz.mp3` | 123704 or 124088 | 7.7315 or 7.7555 |
| `episode-12.m4a` | 1260033, 1260468 and others near them | 78.7520625 to 78.77925 |

It's inside AVFoundation's decoding and resampling: a small Swift program reads the same files with the same
variation, and the reader says `Completed`, without an error, either way.

- **MP3**: the lengths differ by exactly 384 samples. Decoding at the file's own rate (22050 Hz), the decoder sometimes
  puts out 529 samples past the track's end, its synthesis delay, and 529 samples at 22050 Hz are 384 at 16 kHz. The
  encoder delay in the file's metadata stays the same; the last buffers carry no trim attachments, so their timestamps
  can't say which output is right.
- **AAC**: the asset says it's 78.7307 seconds long, its track 78.78. Read without a time range, decoding sometimes
  stops at 1736704 samples instead of the track's 1737099. Apart from that, AVFoundation's own resampling gives
  different sample values when the reader's buffers come in different sizes.

These don't help: precise timing (`AVURLAssetPreferPreciseDurationAndTimingKey`), `alwaysCopiesSampleData`, choosing
the sample-rate converter's algorithm or its processing (spectral, time-domain, varispeed), integer or non-interleaved
PCM, an audio mix with explicit input parameters, an explicit time range with AVFoundation still resampling, random
access, and a composition. Decoding at the file's own rate alone keeps the varying ends. Trimming or padding the end
alone leaves different samples inside the sound windows. `copyNextSampleBuffer` is synchronous and returns nothing at
the end or after an error, so waiting or polling would hide the fault, not fix it.

## How sift-it reads sound

- **To the tracks' end.** `read_pass` sets the reader's time range to end where the audio tracks do, which gives AAC its
  last partial buffer.
- **At the file's own rate.** AVFoundation decodes and mixes the tracks down to one channel of float PCM, without
  resampling.
- **Through one converter.** `av/resample.rs` hands the decoded sound to one `AVAudioConverter` per pass, in the blocks
  it asks for, whatever the sizes of the reader's buffers. It stops at the tracks' end (dropping MP3's extra 529
  samples), says end of stream, and drains the converter; then it reads the reader to its end, so the reader can say
  whether it completed.
- **From partway, as from the start.** An audio file stopped partway goes on from where it stopped. Such a read starts
  decoding up to three seconds earlier, at a whole second (a whole number of samples at every rate, so the converter's
  output falls on the same instants), and drops what comes before where it starts: the decoder and the converter warm
  up on it. The samples are then the same to the bit as a read from the start. Without the warm-up, the first second
  comes out differently, and a sound window there loses up to 1.2% of its cosine (0.988).
- **In one go where it can.** In a VBR MP3 (44.1 kHz stereo music), AVFoundation's seek itself lands a fraction of a
  second off: read in five-minute pieces that each start partway, its windows after the first seek shift by about
  0.2 s (cosine 0.977). So the sound round reads an audio file in one go, saying how far it got every five minutes, and
  reads from partway only to go on after it was stopped.

Memory stays bounded: two buffers of 8192 frames and the reader's current buffer. A file AVFoundation gives up on
partway is read on from a second past where it stopped, with that second as silence.

`nota-voz.mp3` reads as 123716 samples (7.73225 s) and `episode-12.m4a` as 1260480 (78.78 s), every time.

## How it's tested

`adapters::macos::av::tests` decodes two small files in `tests/audio` 20 times each, alternately consuming the samples
at once and with a delay, and compares every sample's bits, the lengths, a read resumed at one second, and that an error
from the consumer stops decoding; and that reads resumed partway give the same bits as a read from the start. No model
runs.

Beyond the tests: the same PCM hash on 200 runs of a Swift prototype and 30 of `sift-it read` per file, and one
length per format on every run, for WAV (8 kHz, and 44.1 kHz stereo), AIFF (48 kHz, 24-bit), ALAC, FLAC, ADTS AAC, VBR
MP3, two tracks of different lengths, a video with shorter sound, and ten minutes of audio.
