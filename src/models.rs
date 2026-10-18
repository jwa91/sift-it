//! The model files: ggml-org's Q8_0 GGUF conversion of EmbeddingGemma 2, at one pinned revision (the files `sift-it
//! selftest` checks against), and the speech model that writes down what's said in recordings (adapters/speech.rs).
//! Downloaded once, with resume, and checked against their SHA-256 before they get their final name, so a file that's
//! there is a whole one. The speech model isn't downloaded when Handy has the same file already: both use that one.

use crate::adapters::speech;
use anyhow::{Context, Result, bail};
use embedder::{MEDIA_FILE, TEXT_FILE};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;

const REPOSITORY: &str = "ggml-org/embeddinggemma-2-GGUF";
const REVISION: &str = "bfcd298762cc34d0357ece5ebdd31791a3a374d8";

pub struct RemoteFile {
    pub repository: &'static str,
    pub revision: &'static str,
    pub name: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

/// The text model first: with it alone, search works. The speech model last: the rest works without it.
pub const FILES: [RemoteFile; 3] = [
    RemoteFile {
        repository: REPOSITORY,
        revision: REVISION,
        name: TEXT_FILE,
        bytes: 309_855_456,
        sha256: "2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135",
    },
    RemoteFile {
        repository: REPOSITORY,
        revision: REVISION,
        name: MEDIA_FILE,
        bytes: 554_821_024,
        sha256: "c4a8a52691ecef40618438928bdf9e68379b854e24166f292592353db0aab64f",
    },
    RemoteFile {
        repository: speech::MODEL_REPOSITORY,
        revision: speech::MODEL_REVISION,
        name: speech::MODEL_FILE,
        bytes: speech::MODEL_BYTES,
        sha256: speech::MODEL_SHA256,
    },
];

#[derive(Serialize)]
pub struct Status {
    pub folder: String,
    pub text: bool,
    pub media: bool,
    /// The speech model is there: in the folder, or Handy's copy (`speech_model` says which).
    pub speech: bool,
    pub speech_model: Option<String>,
    pub bytes: u64,
}

pub fn status(folder: &Path) -> Status {
    let speech_model = speech::model_path();
    Status {
        folder: folder.display().to_string(),
        text: folder.join(TEXT_FILE).exists(),
        media: folder.join(MEDIA_FILE).exists(),
        speech: speech_model.is_some(),
        speech_model: speech_model.map(|path| path.display().to_string()),
        bytes: FILES.iter().map(|f| f.bytes).sum(),
    }
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Progress<'a> {
    Downloading { file: &'a str, done: u64, total: u64 },
    Verifying { file: &'a str },
    Ready { file: &'a str },
}

/// Downloads the files that aren't there yet.
pub fn download(folder: &Path, on_progress: &mut dyn FnMut(Progress)) -> Result<()> {
    std::fs::create_dir_all(folder)?;
    for file in &FILES {
        let target = folder.join(file.name);
        // Handy's copy of the speech model serves both apps.
        if target.exists() || (file.name == speech::MODEL_FILE && speech::model_path().is_some()) {
            on_progress(Progress::Ready { file: file.name });
            continue;
        }
        let partial = folder.join(format!("{}.part", file.name));
        fetch(file, &partial, on_progress).with_context(|| format!("downloading {}", file.name))?;
        on_progress(Progress::Verifying { file: file.name });
        let digest = sha256(&partial)?;
        if digest != file.sha256 {
            std::fs::remove_file(&partial)?;
            bail!("{} came down damaged (SHA-256 {digest}); run the download again", file.name);
        }
        std::fs::rename(&partial, &target)?;
        on_progress(Progress::Ready { file: file.name });
    }
    Ok(())
}

fn fetch(file: &RemoteFile, partial: &Path, on_progress: &mut dyn FnMut(Progress)) -> Result<()> {
    let url = format!("https://huggingface.co/{}/resolve/{}/{}", file.repository, file.revision, file.name);
    let mut done = std::fs::metadata(partial).map(|m| m.len()).unwrap_or(0);
    if done >= file.bytes {
        return Ok(());
    }
    let mut request = ureq::get(&url);
    if done > 0 {
        request = request.header("Range", &format!("bytes={done}-"));
    }
    let response = request.call()?;
    let resumed = response.status().as_u16() == 206;
    if !resumed {
        done = 0;
    }
    let mut output =
        std::fs::OpenOptions::new().create(true).write(true).append(resumed).truncate(!resumed).open(partial)?;
    let mut reader = response.into_body().into_reader();
    let mut buffer = vec![0u8; 1 << 20];
    let mut reported = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        done += read as u64;
        if done - reported >= 8 << 20 || done == file.bytes {
            on_progress(Progress::Downloading { file: file.name, done, total: file.bytes });
            reported = done;
        }
    }
    output.flush()?;
    if done != file.bytes {
        bail!("the download stopped at {done} of {} bytes; run it again to resume", file.bytes);
    }
    Ok(())
}

fn sha256(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 8 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}
