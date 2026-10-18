//! The Mac's own frameworks, for reading what's in files: ImageIO decodes every picture format macOS knows (HEIC, RAW,
//! WebP, AVIF…) and writes JPEG previews; CoreGraphics turns pictures and PDF pages into pixels; Vision reads the text
//! in them. AVFoundation plays recordings and videos (av.rs), and IOKit says whether the Mac is on battery (power.rs).

pub mod av;
pub mod power;

use anyhow::{Result, anyhow, bail};
use embedder::imaging::Image;
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CFURL, CGFloat, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGColorSpace, CGContext, CGImage, CGInterpolationQuality, CGPDFBox, CGPDFDocument, kCGColorSpaceSRGB,
};
use objc2_foundation::{NSArray, NSDictionary};
use objc2_image_io::{
    CGImageDestination, CGImageSource, kCGImageDestinationLossyCompressionQuality,
    kCGImagePropertyExifDateTimeOriginal, kCGImagePropertyExifDictionary, kCGImagePropertyPixelHeight,
    kCGImagePropertyPixelWidth, kCGImageSourceCreateThumbnailFromImageAlways,
    kCGImageSourceCreateThumbnailWithTransform, kCGImageSourceShouldCacheImmediately,
    kCGImageSourceThumbnailMaxPixelSize,
};
use objc2_vision::{
    VNImageOption, VNImageRequestHandler, VNRecognizeTextRequest, VNRecognizedTextObservation, VNRequest,
    VNRequestTextRecognitionLevel,
};
use std::ffi::c_void;
use std::path::Path;
use std::ptr::NonNull;

unsafe extern "C-unwind" {
    // objc2-core-graphics leaves this one out (its buffer is the caller's to keep alive).
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: Option<&CGColorSpace>,
        bitmap_info: u32,
    ) -> Option<NonNull<CGContext>>;
}

/// kCGImageAlphaNoneSkipLast with the default byte order: R, G, B, unused.
const RGBX: u32 = 5;

fn dictionary(pairs: &[(&CFString, &CFType)]) -> CFRetained<CFDictionary<CFString, CFType>> {
    let keys: Vec<&CFString> = pairs.iter().map(|(key, _)| *key).collect();
    let values: Vec<&CFType> = pairs.iter().map(|(_, value)| *value).collect();
    CFDictionary::from_slices(&keys, &values)
}

fn url(path: &Path) -> Result<CFRetained<CFURL>> {
    CFURL::from_file_path(path).ok_or_else(|| anyhow!("not a usable path: {}", path.display()))
}

/// A picture file: its pixels (at most `max_side` on the long side, upright per its EXIF orientation) and what's known
/// about it.
pub struct Picture {
    pub image: CFRetained<CGImage>,
    pub width: u32,
    pub height: u32,
    /// When it was taken (EXIF), as written there: "2024:05:01 10:30:12".
    pub taken: Option<String>,
}

pub fn open_picture(path: &Path, max_side: u32) -> Result<Picture> {
    let source =
        unsafe { CGImageSource::with_url(&*url(path)?, None) }.ok_or_else(|| anyhow!("ImageIO can't open it"))?;
    let (mut width, mut height, mut taken) = (0u32, 0u32, None);
    if let Some(properties) = unsafe { source.properties_at_index(0, None) } {
        let properties: &CFDictionary<CFString, CFType> = unsafe { properties.cast_unchecked() };
        let number = |key: &CFString| {
            properties.get(key).and_then(|v| v.downcast::<CFNumber>().ok()).and_then(|n| n.as_i64()).unwrap_or(0)
        };
        width = number(unsafe { kCGImagePropertyPixelWidth }) as u32;
        height = number(unsafe { kCGImagePropertyPixelHeight }) as u32;
        taken = properties
            .get(unsafe { kCGImagePropertyExifDictionary })
            .and_then(|exif| exif.downcast::<CFDictionary>().ok())
            .and_then(|exif| {
                let exif: &CFDictionary<CFString, CFType> = unsafe { exif.cast_unchecked() };
                exif.get(unsafe { kCGImagePropertyExifDateTimeOriginal })
            })
            .and_then(|date| date.downcast::<CFString>().ok())
            .map(|date| date.to_string());
    }
    let max = CFNumber::new_i64(max_side as i64);
    let options = dictionary(&[
        (unsafe { kCGImageSourceCreateThumbnailFromImageAlways }, CFBoolean::new(true).as_ref()),
        (unsafe { kCGImageSourceCreateThumbnailWithTransform }, CFBoolean::new(true).as_ref()),
        (unsafe { kCGImageSourceShouldCacheImmediately }, CFBoolean::new(true).as_ref()),
        (unsafe { kCGImageSourceThumbnailMaxPixelSize }, max.as_ref()),
    ]);
    let image = unsafe { source.thumbnail_at_index(0, Some(options.as_opaque())) }
        .ok_or_else(|| anyhow!("ImageIO can't decode it"))?;
    if width == 0 || height == 0 {
        width = CGImage::width(Some(&image)) as u32;
        height = CGImage::height(Some(&image)) as u32;
    }
    Ok(Picture { image, width, height, taken })
}

/// The picture's pixels as packed RGB, on white where it's transparent.
pub fn pixels(image: &CGImage) -> Result<Image> {
    let width = CGImage::width(Some(image));
    let height = CGImage::height(Some(image));
    if width == 0 || height == 0 {
        bail!("empty picture");
    }
    let mut rgbx = vec![0u8; width * height * 4];
    let context = bitmap(&mut rgbx, width, height)?;
    let rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(width as CGFloat, height as CGFloat));
    CGContext::set_rgb_fill_color(Some(&context), 1.0, 1.0, 1.0, 1.0);
    CGContext::fill_rect(Some(&context), rect);
    CGContext::draw_image(Some(&context), rect, Some(image));
    drop(context);
    Ok(Image { width: width as u32, height: height as u32, rgb: rgbx_to_rgb(&rgbx) })
}

fn bitmap(buffer: &mut [u8], width: usize, height: usize) -> Result<CFRetained<CGContext>> {
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).ok_or_else(|| anyhow!("no sRGB"))?;
    let context =
        unsafe { CGBitmapContextCreate(buffer.as_mut_ptr().cast(), width, height, 8, width * 4, Some(&space), RGBX) }
            .ok_or_else(|| anyhow!("can't make a {width}×{height} bitmap"))?;
    let context = unsafe { CFRetained::from_raw(context) };
    CGContext::set_interpolation_quality(Some(&context), CGInterpolationQuality::High);
    Ok(context)
}

fn rgbx_to_rgb(rgbx: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(rgbx.len() / 4 * 3);
    for pixel in rgbx.as_chunks::<4>().0 {
        rgb.extend_from_slice(&pixel[..3]);
    }
    rgb
}

fn image_from_rgbx(rgbx: &mut [u8], width: usize, height: usize) -> Result<CFRetained<CGImage>> {
    let context = bitmap(rgbx, width, height)?;
    objc2_core_graphics::CGBitmapContextCreateImage(Some(&context)).ok_or_else(|| anyhow!("can't make the picture"))
}

/// A PDF page (from 1) drawn on white, at most `max_side` pixels on the long side.
pub fn render_pdf_page(path: &Path, page_number: usize, max_side: u32) -> Result<CFRetained<CGImage>> {
    let document = CGPDFDocument::with_url(Some(&*url(path)?)).ok_or_else(|| anyhow!("CoreGraphics can't open it"))?;
    let page = CGPDFDocument::page(Some(&document), page_number).ok_or_else(|| anyhow!("no page {page_number}"))?;
    let bounds = objc2_core_graphics::CGPDFPage::box_rect(Some(&page), CGPDFBox::CropBox);
    let rotated = objc2_core_graphics::CGPDFPage::rotation_angle(Some(&page)) % 180 != 0;
    let (page_width, page_height) =
        if rotated { (bounds.size.height, bounds.size.width) } else { (bounds.size.width, bounds.size.height) };
    if page_width <= 0.0 || page_height <= 0.0 {
        bail!("page {page_number} has no size");
    }
    let scale = max_side as CGFloat / page_width.max(page_height);
    let (width, height) =
        ((page_width * scale).round().max(1.0) as usize, (page_height * scale).round().max(1.0) as usize);
    let mut rgbx = vec![0u8; width * height * 4];
    let context = bitmap(&mut rgbx, width, height)?;
    let rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(width as CGFloat, height as CGFloat));
    CGContext::set_rgb_fill_color(Some(&context), 1.0, 1.0, 1.0, 1.0);
    CGContext::fill_rect(Some(&context), rect);
    // The page's own transform fits it into the rect, rotation included, but never scales up: scale first.
    CGContext::scale_ctm(Some(&context), scale, scale);
    let unscaled = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(page_width, page_height));
    let transform =
        objc2_core_graphics::CGPDFPage::drawing_transform(Some(&page), CGPDFBox::CropBox, unscaled, 0, true);
    CGContext::concat_ctm(Some(&context), transform);
    CGContext::draw_pdf_page(Some(&context), Some(&page));
    drop(context);
    image_from_rgbx(&mut rgbx, width, height)
}

/// Writes a JPEG.
pub fn save_jpeg(image: &CGImage, path: &Path, quality: f64) -> Result<()> {
    let jpeg = CFString::from_str("public.jpeg");
    let destination = unsafe { CGImageDestination::with_url(&*url(path)?, &jpeg, 1, None) }
        .ok_or_else(|| anyhow!("can't write {}", path.display()))?;
    let quality = CFNumber::new_f64(quality);
    let properties = dictionary(&[(unsafe { kCGImageDestinationLossyCompressionQuality }, quality.as_ref())]);
    unsafe {
        destination.add_image(image, Some(properties.as_opaque()));
        if !destination.finalize() {
            bail!("couldn't write {}", path.display());
        }
    }
    Ok(())
}

/// The text Vision reads in a picture, line by line from the top.
///
/// Vision reads text in one picture at a time per process, whichever thread asks. The first time a binary reads text,
/// it compiles its model for the Neural Engine, which can take a few minutes, and now and then it never returns from
/// that request, nor from any after it (ingest::schedule::STUCK).
pub fn recognize_text(image: &CGImage) -> Result<String> {
    let request = unsafe { VNRecognizeTextRequest::init(VNRecognizeTextRequest::alloc()) };
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);
    request.setAutomaticallyDetectsLanguage(true);
    let options: Retained<NSDictionary<VNImageOption, AnyObject>> = NSDictionary::new();
    let handler =
        unsafe { VNImageRequestHandler::initWithCGImage_options(VNImageRequestHandler::alloc(), image, &options) };
    let base: &VNRequest = &request;
    let requests = NSArray::from_slice(&[base]);
    handler.performRequests_error(&requests).map_err(|error| anyhow!("Vision: {}", error.localizedDescription()))?;
    let Some(results) = request.results() else { return Ok(String::new()) };
    let mut lines: Vec<(f64, f64, String)> = Vec::new();
    for observation in results.iter() {
        let Ok(observation) = observation.downcast::<VNRecognizedTextObservation>() else { continue };
        let candidates = observation.topCandidates(1);
        let Some(best) = candidates.firstObject() else { continue };
        let bounds = unsafe { observation.boundingBox() };
        // Vision's origin is bottom left: higher y comes first.
        lines.push((-(bounds.origin.y + bounds.size.height), bounds.origin.x, best.string().to_string()));
    }
    // Rows top to bottom (lines within half a line's height count as one row), each left to right.
    lines.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut rows: Vec<Vec<(f64, String)>> = Vec::new();
    let mut last_top = f64::NEG_INFINITY;
    for (top, left, text) in lines {
        if rows.is_empty() || top - last_top > 0.01 {
            rows.push(Vec::new());
            last_top = top;
        }
        rows.last_mut().unwrap().push((left, text));
    }
    let text = rows
        .into_iter()
        .map(|mut row| {
            row.sort_by(|a, b| a.0.total_cmp(&b.0));
            row.into_iter().map(|(_, text)| text).collect::<Vec<_>>().join("  ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(text)
}

/// A 16 × 9 grey thumbnail: enough to tell one shot of a video from the next.
pub fn signature(image: &CGImage) -> Result<Vec<u8>> {
    let (width, height) = (16usize, 9usize);
    let mut rgbx = vec![0u8; width * height * 4];
    let context = bitmap(&mut rgbx, width, height)?;
    let rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(width as CGFloat, height as CGFloat));
    CGContext::set_interpolation_quality(Some(&context), CGInterpolationQuality::Low);
    CGContext::draw_image(Some(&context), rect, Some(image));
    drop(context);
    Ok(rgbx
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| ((p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000) as u8)
        .collect())
}

/// How many pages a PDF has (without reading them).
pub fn pdf_pages(path: &Path) -> Option<usize> {
    let document = CGPDFDocument::with_url(Some(&*url(path).ok()?))?;
    Some(CGPDFDocument::number_of_pages(Some(&document)))
}

/// Writes a PNG (for pictures with transparency, like the waveform).
pub fn save_png(image: &CGImage, path: &Path) -> Result<()> {
    let png = CFString::from_str("public.png");
    let destination = unsafe { CGImageDestination::with_url(&*url(path)?, &png, 1, None) }
        .ok_or_else(|| anyhow!("can't write {}", path.display()))?;
    unsafe {
        destination.add_image(image, None);
        if !destination.finalize() {
            bail!("couldn't write {}", path.display());
        }
    }
    Ok(())
}

/// A recording's waveform: one bar per column (`levels`, 0–1), on a transparent background so it reads in light and
/// dark mode, with the columns in `highlight` (the stretch that matched) in blue on a light blue band.
pub fn waveform(levels: &[f32], height: usize, highlight: Option<(usize, usize)>) -> Result<CFRetained<CGImage>> {
    const BAR: usize = 3; // pixels per column: a 2 px bar and a 1 px gap
    let width = levels.len().max(1) * BAR;
    let mut rgba = vec![0u8; width * height * 4];
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).ok_or_else(|| anyhow!("no sRGB"))?;
    // kCGImageAlphaPremultipliedLast: R, G, B, A.
    let context =
        unsafe { CGBitmapContextCreate(rgba.as_mut_ptr().cast(), width, height, 8, width * 4, Some(&space), 1) }
            .ok_or_else(|| anyhow!("can't make a {width}×{height} bitmap"))?;
    let context = unsafe { CFRetained::from_raw(context) };
    let middle = height as CGFloat / 2.0;
    if let Some((first, last)) = highlight {
        CGContext::set_rgb_fill_color(Some(&context), 0.04, 0.52, 1.0, 0.16);
        let x = (first * BAR) as CGFloat;
        let band = ((last.saturating_sub(first) + 1) * BAR) as CGFloat;
        CGContext::fill_rect(Some(&context), CGRect::new(CGPoint::new(x, 0.0), CGSize::new(band, height as CGFloat)));
    }
    for (column, level) in levels.iter().enumerate() {
        let lit = highlight.is_some_and(|(first, last)| column >= first && column <= last);
        if lit {
            CGContext::set_rgb_fill_color(Some(&context), 0.04, 0.52, 1.0, 1.0);
        } else {
            CGContext::set_rgb_fill_color(Some(&context), 0.56, 0.56, 0.58, 1.0);
        }
        let bar = (level.clamp(0.0, 1.0) as CGFloat * (height as CGFloat - 4.0)).max(2.0);
        let rect = CGRect::new(CGPoint::new((column * BAR) as CGFloat, middle - bar / 2.0), CGSize::new(2.0, bar));
        CGContext::fill_rect(Some(&context), rect);
    }
    objc2_core_graphics::CGBitmapContextCreateImage(Some(&context)).ok_or_else(|| anyhow!("can't make the waveform"))
}

/// How much memory the Mac has, in bytes (read once; 8 GB, the least sift-it runs on, if it can't be read).
pub fn memory() -> u64 {
    static MEMORY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MEMORY.get_or_init(|| {
        let mut bytes = 0u64;
        let mut size = std::mem::size_of::<u64>();
        let read = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                &mut bytes as *mut u64 as *mut c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if read == 0 { bytes } else { 8 << 30 }
    })
}
