//! Preview images for search results: a picture's thumbnail, a PDF page, a video frame, a recording's waveform with
//! the stretch that matched. Each is made once per file version, place and size, in the previews folder.

use crate::mac;
use anyhow::Result;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub fn render(
    path: &Path,
    kind: &str,
    loc: Option<f64>,
    loc_end: Option<f64>,
    size: u32,
    folder: &Path,
) -> Result<PathBuf> {
    let size = size.clamp(64, 2048);
    let modified = std::fs::metadata(path)?.modified()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (path, modified, kind, loc.map(f64::to_bits), loc_end.map(f64::to_bits), size).hash(&mut hasher);
    let extension = if kind == "waveform" { "png" } else { "jpg" };
    let target = folder.join(format!("{:016x}.{extension}", hasher.finish()));
    if target.exists() {
        return Ok(target);
    }
    std::fs::create_dir_all(folder)?;
    // Written under a temporary name, so a reader never sees half a JPEG.
    let partial = target.with_extension(format!("{}.{extension}", std::process::id()));
    objc2::rc::autoreleasepool(|_| -> Result<()> {
        match kind {
            "waveform" => {
                // `size` is the width; a column is 3 px.
                let columns = (size / 3).max(16) as usize;
                let levels = envelope(path, modified, columns)?;
                let seconds = crate::av::duration(path).max(0.1);
                let column = |t: f64| ((t / seconds * columns as f64) as usize).min(columns - 1);
                let highlight = loc.map(|start| (column(start), column(loc_end.unwrap_or(start + 30.0))));
                let image = mac::waveform(&levels, (size / 6).max(48) as usize, highlight)?;
                mac::save_png(&image, &partial)
            }
            "pdf" => {
                let page = mac::render_pdf_page(path, loc.unwrap_or(1.0).max(1.0) as usize, size)?;
                mac::save_jpeg(&page, &partial, 0.82)
            }
            "video" => {
                let frame = crate::av::frame_at(path, loc.unwrap_or(0.0), size)?;
                mac::save_jpeg(&frame, &partial, 0.82)
            }
            _ => mac::save_jpeg(&mac::open_picture(path, size)?.image, &partial, 0.82),
        }
    })?;
    std::fs::rename(&partial, &target)?;
    Ok(target)
}

/// A recording's loudness, decoded once per file version and kept for this process (one waveform per result, and
/// they differ only in what's highlighted).
fn envelope(path: &Path, modified: SystemTime, columns: usize) -> Result<Vec<f32>> {
    type Cache = std::sync::Mutex<std::collections::HashMap<(PathBuf, SystemTime, usize), Vec<f32>>>;
    static CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = (path.to_path_buf(), modified, columns);
    if let Some(levels) = cache.lock().unwrap().get(&key) {
        return Ok(levels.clone());
    }
    let levels = crate::av::envelope(path, columns)?;
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
