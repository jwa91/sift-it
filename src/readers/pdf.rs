//! PDFs, page by page, through PDFKit. A page's text is one passage (or more, for a dense page), embedded with the
//! document's title; the page number is where a result opens. A page without a text layer (a scan) is drawn and read
//! by Vision; a scanned page with no text on it (a photo, a drawing) is embedded as a picture.

use super::image::{MIN_TEXT, emit_text};
use super::text::{mostly_numbers, passages};
use super::{Payload, Piece, Sink, title_of};
use crate::embedder::prompts;
use crate::imaging;
use crate::mac;
use crate::store::Segment;
use anyhow::{Result, bail};
use objc2::AllocAnyThread;
use objc2::rc::autoreleasepool;
use objc2_foundation::{NSString, NSURL};
use objc2_pdf_kit::PDFDocument;
use std::path::Path;

/// Pages read from one PDF, at most.
pub const MAX_PAGES: usize = 5000;
/// Scanned pages read by OCR in one PDF, at most (each takes about half a second).
pub const MAX_SCANNED: usize = 300;
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

pub fn read(path: &Path, sink: Sink) -> Result<serde_json::Value> {
    let document = open(path)?;
    let title = title_of(path);
    let pages = unsafe { document.pageCount() };
    let mut scanned = 0usize;
    for index in 0..pages.min(MAX_PAGES) {
        let text = autoreleasepool(|_| unsafe {
            document.pageAtIndex(index).and_then(|page| page.string()).map(|s| s.to_string()).unwrap_or_default()
        });
        let number = index + 1;
        if text.trim().chars().count() < SCANNED_BELOW {
            if scanned < MAX_SCANNED {
                scanned += 1;
                read_scanned_page(path, number, sink)?;
            }
            continue;
        }
        for passage in passages(&text) {
            let payload = if mostly_numbers(&passage.text) {
                Payload::None
            } else {
                Payload::Text(prompts::document(Some(&title), &passage.text))
            };
            sink(Piece {
                segment: Segment::Page,
                loc: Some(number as f64),
                loc_end: None,
                text: Some(passage.text),
                payload,
            })?;
        }
    }
    Ok(serde_json::json!({ "pages": pages, "scanned_pages": scanned }))
}

fn read_scanned_page(path: &Path, number: usize, sink: Sink) -> Result<()> {
    let page = autoreleasepool(|_| mac::render_pdf_page(path, number, SCAN_SIDE))?;
    let text = autoreleasepool(|_| mac::recognize_text(&page))?;
    if text.chars().count() >= MIN_TEXT {
        return emit_text(path, &text, Some(number as f64), sink);
    }
    // Nothing to read: what the page shows, then.
    let pixels = imaging::fit(&mac::pixels(&page)?, super::image::SCREENSHOT_BUDGET)?;
    sink(Piece {
        segment: Segment::Picture,
        loc: Some(number as f64),
        loc_end: None,
        text: (!text.trim().is_empty()).then_some(text),
        payload: Payload::Image(pixels),
    })
}
