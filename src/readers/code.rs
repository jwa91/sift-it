//! Source code and notebooks (only with `sources.code` on). Code is cut at blank lines, which in most code falls
//! between functions, into stretches of about `passage_chars` characters (600 by default: a function or two, so a
//! result opens near the code that matched), and each stretch is embedded with its path inside its project as the
//! title: the model reads "src/net/retry.rs" as much as the code. Notebooks are read cell by cell, without their
//! outputs.

use super::text::{Lines, emit_passages, passages};
use super::{Piece, Sink, text_payload};
use crate::config::Settings;
use anyhow::{Context, Result};
use protocol::Segment;
use serde::Deserialize;
use std::path::Path;

pub fn read(path: &Path, settings: &Settings, sink: Sink) -> Result<serde_json::Value> {
    let title = project_path(path);
    if path.extension().is_some_and(|e| e == "ipynb") {
        return read_notebook(path, &title, settings, sink);
    }
    emit_passages(Lines::new(std::fs::File::open(path)?)?, &title, settings, sink)
}

/// What a notebook's read for: its cells' source. Their outputs (pictures, tables) are passed over as they stream by.
#[derive(Deserialize)]
struct Notebook {
    cells: Vec<Cell>,
}

#[derive(Deserialize)]
struct Cell {
    #[serde(default)]
    source: Source,
}

#[derive(Deserialize, Default)]
#[serde(untagged)]
enum Source {
    Lines(Vec<String>),
    Text(String),
    #[default]
    None,
}

/// A notebook's code and Markdown cells, each a passage at its cell number (from 1).
fn read_notebook(path: &Path, title: &str, settings: &Settings, sink: Sink) -> Result<serde_json::Value> {
    let file = std::io::BufReader::new(std::fs::File::open(path)?);
    let notebook: Notebook = serde_json::from_reader(file).context("not a notebook")?;
    let mut count = 0;
    for (index, cell) in notebook.cells.iter().enumerate() {
        let source = match &cell.source {
            Source::Lines(lines) => lines.concat(),
            Source::Text(text) => text.clone(),
            Source::None => continue,
        };
        if source.trim().is_empty() {
            continue;
        }
        for stretch in passages(&source, settings) {
            count += 1;
            sink(Piece {
                segment: Segment::Passage,
                loc: Some((index + 1) as f64),
                loc_end: None,
                payload: text_payload(title, &stretch.text, settings),
                text: Some(stretch.text),
            })?;
        }
    }
    Ok(serde_json::json!({ "cells": notebook.cells.len(), "passages": count }))
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
