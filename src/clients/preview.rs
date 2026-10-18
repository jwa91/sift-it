//! Preview images for search results: a picture's thumbnail, a PDF page, a video frame, a recording's waveform with
//! the stretch that matched. Each is made once per file version, place and size, in the previews folder: JPEG for
//! front ends, PNG for terminals (the kitty graphics protocol takes PNG).

use crate::adapters::macos;
use anyhow::Result;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A preview for a front end (JPEG; the waveform is PNG for its transparency).
pub fn render(
    path: &Path,
    kind: &str,
    loc: Option<f64>,
    loc_end: Option<f64>,
    size: u32,
    folder: &Path,
) -> Result<PathBuf> {
    render_as(path, kind, loc, loc_end, size, folder, kind == "waveform")
}

/// A thumbnail for a terminal: PNG.
pub fn thumbnail(path: &Path, kind: &str, loc: Option<f64>, size: u32, folder: &Path) -> Result<PathBuf> {
    render_as(path, kind, loc, None, size, folder, true)
}

fn render_as(
    path: &Path,
    kind: &str,
    loc: Option<f64>,
    loc_end: Option<f64>,
    size: u32,
    folder: &Path,
    png: bool,
) -> Result<PathBuf> {
    let size = size.clamp(64, 2048);
    let modified = std::fs::metadata(path)?.modified()?;
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
    let save = |image: &objc2_core_graphics::CGImage| {
        if png { macos::save_png(image, &partial) } else { macos::save_jpeg(image, &partial, 0.82) }
    };
    objc2::rc::autoreleasepool(|_| -> Result<()> {
        match kind {
            "waveform" => {
                // `size` is the width; a column is 3 px.
                let columns = (size / 3).max(16) as usize;
                let levels = levels(path, columns, folder)?;
                let highlight = loc.map(|start| span(path, columns, start, loc_end.unwrap_or(start + 30.0)));
                let image = macos::waveform(&levels, (size / 6).max(48) as usize, highlight)?;
                macos::save_png(&image, &partial)
            }
            "pdf" => {
                let page = macos::render_pdf_page(path, loc.unwrap_or(1.0).max(1.0) as usize, size)?;
                save(&page)
            }
            "video" => {
                let frame = crate::adapters::macos::av::frame_at(path, loc.unwrap_or(0.0), size)?;
                save(&frame)
            }
            _ => save(&macos::open_picture(path, size)?.image),
        }
    })?;
    std::fs::rename(&partial, &target)?;
    Ok(target)
}

/// The columns (of `columns`) that the stretch from `start` to `end` seconds covers.
pub fn span(path: &Path, columns: usize, start: f64, end: f64) -> (usize, usize) {
    let seconds = crate::adapters::macos::av::duration(path).max(0.1);
    let column = |t: f64| ((t / seconds * columns as f64) as usize).min(columns - 1);
    (column(start), column(end))
}

/// A recording's loudness in `columns` slices: decoded once per file version, then kept in memory and on disk (a
/// waveform per result, and a search shows one per recording).
pub fn levels(path: &Path, columns: usize, folder: &Path) -> Result<Vec<f32>> {
    type Cache = std::sync::Mutex<std::collections::HashMap<(PathBuf, SystemTime, usize), Vec<f32>>>;
    static CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let modified = std::fs::metadata(path)?.modified()?;
    let key = (path.to_path_buf(), modified, columns);
    if let Some(levels) = cache.lock().unwrap().get(&key) {
        return Ok(levels.clone());
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let file = folder.join(format!("{:016x}.levels", hasher.finish()));
    let levels = match std::fs::read(&file) {
        Ok(bytes) if bytes.len() == columns * 4 => {
            bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
        }
        _ => {
            let levels = crate::adapters::macos::av::envelope(path, columns)?;
            std::fs::create_dir_all(folder)?;
            let bytes: Vec<u8> = levels.iter().flat_map(|l| l.to_le_bytes()).collect();
            let _ = crate::config::write_atomically(&file, &bytes);
            levels
        }
    };
    cache.lock().unwrap().insert(key, levels.clone());
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
