//! PDFs, page by page, through PDFKit: each page's text layer, at its page number. A page without one (a scan) is
//! drawn, and Vision reads the text on it.

use crate::ingest::Content;
use crate::mac;
use anyhow::{Result, bail};
use objc2::AllocAnyThread;
use objc2::rc::autoreleasepool;
use objc2_foundation::{NSString, NSURL};
use objc2_pdf_kit::PDFDocument;
use protocol::Segment;
use std::path::Path;

/// A page with fewer characters than this has no real text layer: it's a scan.
pub const SCANNED_BELOW: usize = 40;
/// Scanned pages are drawn this large for OCR.
const SCAN_SIDE: u32 = 2200;

pub fn open(path: &Path) -> Result<objc2::rc::Retained<PDFDocument>> {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    let Some(document) = (unsafe { PDFDocument::initWithURL(PDFDocument::alloc(), &url) }) else {
        bail!("PDFKit can't open it");
    };
    if unsafe { document.isLocked() } {
        bail!("it's password protected");
    }
    Ok(document)
}

pub fn read(path: &Path, content: &mut Content) -> Result<serde_json::Value> {
    let document = open(path)?;
    let pages = unsafe { document.pageCount() };
    let mut scanned = 0usize;
    for index in 0..pages {
        let text = autoreleasepool(|_| unsafe {
            document.pageAtIndex(index).and_then(|page| page.string()).map(|s| s.to_string()).unwrap_or_default()
        });
        let number = index + 1;
        if text.trim().chars().count() < SCANNED_BELOW {
            scanned += 1;
            let page = autoreleasepool(|_| mac::render_pdf_page(path, number, SCAN_SIDE))?;
            let text = autoreleasepool(|_| mac::recognize_text(&page))?;
            content.scan(number, &page, &text)?;
            continue;
        }
        content.text(Segment::Page, number as f64, &text)?;
    }
    Ok(serde_json::json!({ "pages": pages, "scanned_pages": scanned }))
}
