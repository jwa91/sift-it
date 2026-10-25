//! Preview images for search results: a picture's thumbnail, a PDF page, a video frame, a recording's waveform with
//! the stretch that matched. Each is made once per file version, place and size, in the previews folder: JPEG for
//! front ends, PNG for terminals (the kitty graphics protocol takes PNG). A file only in iCloud has none: drawing it
//! would download it.

use crate::adapters::macos::{self, icloud};
use anyhow::{Result, bail};
use protocol::Drawn;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A preview for a front end (JPEG; the waveform is PNG for its transparency).
pub fn render(
    path: &Path,
    kind: Drawn,
    loc: Option<f64>,
    loc_end: Option<f64>,
    size: u32,
    folder: &Path,
) -> Result<PathBuf> {
    render_as(path, kind, loc, loc_end, size, folder, kind == Drawn::Waveform)
}

/// A thumbnail for a terminal: PNG.
pub fn thumbnail(path: &Path, kind: Drawn, loc: Option<f64>, size: u32, folder: &Path) -> Result<PathBuf> {
    render_as(path, kind, loc, None, size, folder, true)
}

fn render_as(
    path: &Path,
    kind: Drawn,
    loc: Option<f64>,
    loc_end: Option<f64>,
    size: u32,
    folder: &Path,
    png: bool,
) -> Result<PathBuf> {
    let size = size.clamp(64, 2048);
    let metadata = std::fs::metadata(path)?;
    if icloud::cloud_only(&metadata) {
        bail!("{} is only in iCloud: it has no preview until it's downloaded", path.display());
    }
    let modified = metadata.modified()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (path, modified, kind, loc.map(f64::to_bits), loc_end.map(f64::to_bits), size, png).hash(&mut hasher);
    let extension = if png { "png" } else { "jpg" };
    let target = folder.join(format!("{:016x}.{extension}", hasher.finish()));
    if target.exists() {
        return Ok(target);
    }
    std::fs::create_dir_all(folder)?;
    // Written under a temporary name, so a reader never sees half a picture.
    let partial = target.with_extension(format!("{}.{extension}", std::process::id()));
    let save = |image: &objc2_core_graphics::CGImage| macos::save(image, &partial, png);
    objc2::rc::autoreleasepool(|_| -> Result<()> {
        match kind {
            Drawn::Waveform => {
                // `size` is the width; a column is 3 px.
                let columns = (size / 3).max(16) as usize;
                let levels = levels(path, columns, folder)?;
                let highlight = loc.map(|start| span(path, columns, start, loc_end));
                let image = macos::waveform(&levels, (size / 6).max(48) as usize, highlight)?;
                macos::save(&image, &partial, true)
            }
            Drawn::Pdf => {
                let page = macos::render_pdf_page(path, loc.unwrap_or(1.0).max(1.0) as usize, size)?;
                save(&page)
            }
            Drawn::Video => {
                let frame = crate::adapters::macos::av::frame_at(path, loc.unwrap_or(0.0), size)?;
                save(&frame)
            }
            Drawn::Image => save(&macos::open_picture(path, size)?.image),
        }
    })?;
    std::fs::rename(&partial, &target)?;
    Ok(target)
}

/// The columns (of `columns`) that the stretch from `start` to `end` seconds covers; without an end, the 30 seconds
/// a sound segment lasts.
pub fn span(path: &Path, columns: usize, start: f64, end: Option<f64>) -> (usize, usize) {
    let seconds = crate::adapters::macos::av::duration(path).max(0.1);
    let column = |t: f64| ((t / seconds * columns as f64) as usize).min(columns - 1);
    (column(start), column(end.unwrap_or(start + 30.0)))
}

/// A recording's loudness in `columns` slices: decoded once per file version and kept in the previews folder (a
/// waveform per result, and a search shows one per recording).
pub fn levels(path: &Path, columns: usize, folder: &Path) -> Result<Vec<f32>> {
    let modified = std::fs::metadata(path)?.modified()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (path, modified, columns).hash(&mut hasher);
    let file = folder.join(format!("{:016x}.levels", hasher.finish()));
    if let Ok(bytes) = std::fs::read(&file)
        && bytes.len() == columns * 4
    {
        return Ok(bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect());
    }
    let levels = crate::adapters::macos::av::envelope(path, columns)?;
    std::fs::create_dir_all(folder)?;
    let bytes: Vec<u8> = levels.iter().flat_map(|l| l.to_le_bytes()).collect();
    let _ = crate::config::write_atomically(&file, &bytes);
    Ok(levels)
}

/// Removes previews not used for a month.
pub fn prune(folder: &Path) {
    let Ok(entries) = std::fs::read_dir(folder) else { return };
    let month = Duration::from_secs(30 * 24 * 3600);
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.accessed().or_else(|_| m.modified()))
            .map(|time| SystemTime::now().duration_since(time).unwrap_or_default() > month)
            .unwrap_or(false);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
