//! Preview images for search results: a picture's thumbnail, a PDF page, a video frame. Each is made once per file
//! version and size, as a JPEG in the previews folder.

use crate::mac;
use anyhow::Result;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub fn render(path: &Path, kind: &str, loc: Option<f64>, size: u32, folder: &Path) -> Result<PathBuf> {
    let size = size.clamp(64, 2048);
    let modified = std::fs::metadata(path)?.modified()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (path, modified, kind, loc.map(f64::to_bits), size).hash(&mut hasher);
    let target = folder.join(format!("{:016x}.jpg", hasher.finish()));
    if target.exists() {
        return Ok(target);
    }
    std::fs::create_dir_all(folder)?;
    // Written under a temporary name, so a reader never sees half a JPEG.
    let partial = target.with_extension(format!("{}.jpg", std::process::id()));
    objc2::rc::autoreleasepool(|_| -> Result<()> {
        match kind {
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
