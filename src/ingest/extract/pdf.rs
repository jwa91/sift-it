//! PDFs, through PDFKit. Their words are each page's text layer, at its page number. A page without one (a scan) is
//! drawn for the OCR round, and Vision reads the text on it; one with little text on it is drawn again for the
//! pictures round, for what it shows.

use crate::adapters::macos;
use crate::ingest::read::{Content, Ended, From};
use crate::ingest::rounds::{Found, SLICE_PAGES};
use crate::ingest::segment::passages::Form;
use anyhow::{Result, bail};
use objc2::AllocAnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2_foundation::{NSString, NSURL};
use objc2_pdf_kit::PDFDocument;
use protocol::Segment;
use serde_json::json;
use std::path::Path;

/// A page with fewer characters than this has no real text layer: it's a scan.
const SCANNED_BELOW: usize = 40;
/// Scanned pages are drawn this large for OCR, and for what they show.
const SCAN_SIDE: u32 = 2200;

pub fn open(path: &Path) -> Result<Retained<PDFDocument>> {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    let Some(document) = (unsafe { PDFDocument::initWithURL(PDFDocument::alloc(), &url) }) else {
        bail!("PDFKit can't open it");
    };
    if unsafe { document.isLocked() } {
        bail!("it's password protected");
    }
    Ok(document)
}

/// A page's text layer (from 0).
fn page_text(document: &PDFDocument, index: usize) -> String {
    autoreleasepool(|_| unsafe {
        document.pageAtIndex(index).and_then(|page| page.string()).map(|s| s.to_string()).unwrap_or_default()
    })
}

fn scanned(text: &str) -> bool {
    text.trim().chars().count() < SCANNED_BELOW
}

/// How many pages a PDF has, and about what share of them are scans, from `sample` pages spread over it: for an
/// estimate, before its words are read.
pub fn scans(path: &Path, sample: usize) -> Result<(usize, f64)> {
    autoreleasepool(|_| {
        let document = open(path)?;
        let pages = unsafe { document.pageCount() };
        let looked = sample.min(pages);
        if looked == 0 {
            return Ok((pages, 0.0));
        }
        let scans = (0..looked).filter(|n| scanned(&page_text(&document, n * pages / looked))).count();
        Ok((pages, scans as f64 / looked as f64))
    })
}

/// The words round: each page's text layer. The scans are counted, for the OCR round.
pub fn words(path: &Path, content: &mut Content) -> Result<Ended> {
    let document = open(path)?;
    let pages = unsafe { document.pageCount() };
    let mut scans = 0usize;
    for index in 0..pages {
        let text = page_text(&document, index);
        if scanned(&text) {
            scans += 1;
            continue;
        }
        content.text(Segment::Page, Some((index + 1) as f64), &text, Form::Prose)?;
    }
    let mut ended = Ended::whole(json!({ "pages": pages, "scanned_pages": scans }), 1.0);
    ended.found = Found { scanned: scans, ..Found::default() };
    Ok(ended)
}

/// The OCR round: the next SLICE_PAGES scans after the `from.done` read before, drawn, and the text Vision reads on
/// them.
pub fn ocr(path: &Path, from: &From, content: &mut Content) -> Result<Ended> {
    let document = open(path)?;
    let pages = unsafe { document.pageCount() };
    let (mut seen, mut read) = (0usize, 0usize);
    for index in 0..pages {
        if !scanned(&page_text(&document, index)) {
            continue;
        }
        seen += 1;
        if seen <= from.done as usize {
            continue;
        }
        if read == SLICE_PAGES {
            return Ok(Ended { done: from.done + read as f64, ..Ended::default() });
        }
        let number = index + 1;
        let page = autoreleasepool(|_| macos::render_pdf_page(path, number, SCAN_SIDE))?;
        let text = autoreleasepool(|_| macos::recognize_text(&page))?;
        content.read_off_page(number, &text)?;
        read += 1;
    }
    Ok(Ended::whole(json!({}), from.done + read as f64))
}

/// The pictures round: the scanned pages with little text on them (`from.pages`), for what they show.
pub fn pictures(path: &Path, settings: &crate::config::Settings, from: &From, content: &mut Content) -> Result<Ended> {
    for &number in &from.pages {
        let page = autoreleasepool(|_| macos::render_pdf_page(path, number, SCAN_SIDE))?;
        content.shown(Some(number as f64), &page, settings.text_tokens)?;
    }
    Ok(Ended::whole(json!({}), from.done + from.pages.len() as f64))
}
