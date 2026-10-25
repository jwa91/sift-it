//! Documents other than PDFs: rtf, doc, docx, odt, html, webarchive. macOS's textutil reads them all, into lines of
//! text.

use super::text::{Lines, feed};
use crate::ingest::read::{Content, Ended};
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::{Command, Stdio};

pub fn read(path: &Path, content: &mut Content) -> Result<Ended> {
    let mut child = Command::new("/usr/bin/textutil")
        .args(["-convert", "txt", "-stdout", "-encoding", "UTF-8"])
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("can't run textutil")?;
    let stdout = child.stdout.take().expect("piped");
    // The pipe closes when this returns, so textutil ends even when reading stopped early.
    let read = Lines::new(stdout).map_err(anyhow::Error::from).and_then(|lines| feed(lines, content));
    let output = child.wait_with_output()?;
    let info = read?;
    if !output.status.success() {
        bail!("textutil couldn't read it: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(info)
}
