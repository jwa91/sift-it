//! Jupyter notebooks: their cells' source, each at its cell number (from 1). Their outputs (pictures, tables) are
//! passed over as they stream by.

use crate::ingest::Content;
use anyhow::{Context, Result};
use protocol::Segment;
use serde::Deserialize;
use std::path::Path;

/// What a notebook's read for: its cells' source.
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

pub fn read(path: &Path, content: &mut Content) -> Result<serde_json::Value> {
    let file = std::io::BufReader::new(std::fs::File::open(path)?);
    let notebook: Notebook = serde_json::from_reader(file).context("not a notebook")?;
    for (index, cell) in notebook.cells.iter().enumerate() {
        let source = match &cell.source {
            Source::Lines(lines) => lines.concat(),
            Source::Text(text) => text.clone(),
            Source::None => continue,
        };
        if source.trim().is_empty() {
            continue;
        }
        content.text(Segment::Passage, (index + 1) as f64, &source)?;
    }
    Ok(serde_json::json!({ "cells": notebook.cells.len(), "passages": content.passages }))
}
