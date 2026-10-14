//! Source code and notebooks (only with "Read Source Code" on). Code is cut at blank lines, which in most code falls
//! between functions, into stretches of about CODE_PASSAGE characters (a function or two, so a result opens near the
//! code that matched), and each stretch is embedded with its path inside its project as the title: the model reads
//! "src/net/retry.rs" as much as the code. Notebooks are read cell by cell, without their outputs.

use super::text::{decode, mostly_numbers, passages, passages_sized};
use super::{Payload, Piece, Sink};
use crate::embedder::prompts;
use crate::store::Segment;
use anyhow::{Context, Result};
use std::path::Path;

/// Code stretches aim for this many characters.
pub const CODE_PASSAGE: usize = 600;

pub fn read(path: &Path, sink: Sink) -> Result<serde_json::Value> {
    let title = project_path(path);
    if path.extension().is_some_and(|e| e == "ipynb") {
        return read_notebook(path, &title, sink);
    }
    let text = decode(&std::fs::read(path)?);
    let stretches = passages_sized(&text, CODE_PASSAGE);
    let count = stretches.len();
    for stretch in stretches {
        let payload = if mostly_numbers(&stretch.text) {
            Payload::None
        } else {
            Payload::Text(prompts::document(Some(&title), &stretch.text))
        };
        sink(Piece {
            segment: Segment::Passage,
            loc: Some(stretch.first_line as f64),
            loc_end: Some(stretch.last_line as f64),
            text: Some(stretch.text),
            payload,
        })?;
    }
    Ok(serde_json::json!({ "passages": count, "lines": text.lines().count() }))
}

/// A notebook's code and Markdown cells, each a passage at its cell number (from 1).
fn read_notebook(path: &Path, title: &str, sink: Sink) -> Result<serde_json::Value> {
    let notebook: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?).context("not a notebook")?;
    let cells = notebook["cells"].as_array().cloned().unwrap_or_default();
    let mut count = 0;
    for (index, cell) in cells.iter().enumerate() {
        let source = match &cell["source"] {
            serde_json::Value::Array(lines) => lines.iter().filter_map(|l| l.as_str()).collect::<String>(),
            serde_json::Value::String(text) => text.clone(),
            _ => continue,
        };
        if source.trim().is_empty() {
            continue;
        }
        for stretch in passages(&source) {
            count += 1;
            sink(Piece {
                segment: Segment::Passage,
                loc: Some((index + 1) as f64),
                loc_end: None,
                text: Some(stretch.text.clone()),
                payload: Payload::Text(prompts::document(Some(title), &stretch.text)),
            })?;
        }
    }
    Ok(serde_json::json!({ "cells": cells.len(), "passages": count }))
}

/// The path from the project's root (the nearest folder up with .git or a project file), or the file name.
pub fn project_path(path: &Path) -> String {
    const MARKERS: &[&str] = &[".git", "package.json", "Cargo.toml", "go.mod", "pyproject.toml", "Package.swift"];
    let mut folder = path.parent();
    while let Some(candidate) = folder {
        if MARKERS.iter().any(|marker| candidate.join(marker).exists()) {
            let root_name = candidate.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let inside = path.strip_prefix(candidate).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
            return format!("{root_name}/{inside}");
        }
        folder = candidate.parent();
    }
    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}
