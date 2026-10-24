//! The model files, kept where other apps keep theirs: the Hugging Face hub cache ($HF_HUB_CACHE, else $HF_HOME/hub,
//! else ~/.cache/huggingface/hub), in its layout of blobs and snapshots. The hub names a model file's blob after its
//! SHA-256 (`hf` 2.x makes that name a link into a store all repositories share), so a file is there when any
//! repository's folder in the cache holds a whole blob of that name, whichever app or revision put it there: Handy's
//! copy of the speech model serves sift-it too.
//!
//! What's missing is downloaded at its pinned revision, with resume, and checked against its SHA-256 before it gets
//! its blob's name, so a blob that's there is a whole one. The cache is shared: sift-it only adds what's missing, and
//! never removes anything. A file that goes (another app cleaning the cache) is missing from then on, until it's
//! downloaded again; a process that has it loaded keeps its pages.

use crate::config;
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

/// A model file, at its pinned revision on the hub.
pub struct ModelFile {
    pub repository: &'static str,
    pub revision: &'static str,
    pub name: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

/// EmbeddingGemma 2's text model: ggml-org's Q8_0 GGUF conversion, at the revision `sift-it selftest` checks against.
/// All that searching needs.
pub const TEXT: ModelFile = ModelFile {
    repository: "ggml-org/embeddinggemma-2-GGUF",
    revision: "bfcd298762cc34d0357ece5ebdd31791a3a374d8",
    name: "embeddinggemma-2-Q8_0.gguf",
    bytes: 309_855_456,
    sha256: "2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135",
};

/// Its image and audio encoders, for pictures and sound.
pub const MEDIA: ModelFile = ModelFile {
    repository: "ggml-org/embeddinggemma-2-GGUF",
    revision: "bfcd298762cc34d0357ece5ebdd31791a3a374d8",
    name: "mmproj-embeddinggemma-2-Q8_0.gguf",
    bytes: 554_821_024,
    sha256: "c4a8a52691ecef40618438928bdf9e68379b854e24166f292592353db0aab64f",
};

/// The speech model (adapters/speech.rs): Moondream's Parakeet Ultra (a post-trained Parakeet TDT 0.6B v3), Handy's
/// GGUF conversion at Q8_0.
pub const SPEECH: ModelFile = ModelFile {
    repository: "handy-computer/parakeet-ultra-gguf",
    revision: "a1539be1b849ea7463b88343e5277b373d0c4ea9",
    name: "parakeet-ultra-Q8_0.gguf",
    bytes: 740_363_168,
    sha256: "007a59761e9258779f189df396b50d12b83bb5b79e66df4b955c230d2f2a0a59",
};

/// The text model first: with it alone, search works. The speech model last: the rest works without it.
const FILES: [&ModelFile; 3] = [&TEXT, &MEDIA, &SPEECH];

/// The hub cache, as the Hugging Face libraries find it.
pub fn hub() -> PathBuf {
    if let Some(cache) = std::env::var_os("HF_HUB_CACHE") {
        return PathBuf::from(cache);
    }
    if let Some(home) = std::env::var_os("HF_HOME") {
        return PathBuf::from(home).join("hub");
    }
    config::dirs_home().join(".cache/huggingface/hub")
}

impl ModelFile {
    /// The file, when the cache holds it whole: in its repository's folder, or in another's.
    pub fn path(&self) -> Option<PathBuf> {
        self.path_in(&hub())
    }

    fn path_in(&self, hub: &Path) -> Option<PathBuf> {
        let whole = |path: PathBuf| std::fs::metadata(&path).is_ok_and(|m| m.len() == self.bytes).then_some(path);
        whole(self.folder(hub).join("blobs").join(self.sha256)).or_else(|| {
            std::fs::read_dir(hub)
                .ok()?
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("models--"))
                .find_map(|entry| whole(entry.path().join("blobs").join(self.sha256)))
        })
    }

    /// The file, or why it can't be had.
    pub fn require(&self) -> Result<PathBuf> {
        self.path().ok_or_else(|| anyhow!("{} isn't downloaded (run `sift-it model download`)", self.name))
    }

    /// The repository's folder in the cache.
    fn folder(&self, hub: &Path) -> PathBuf {
        hub.join(format!("models--{}", self.repository.replace('/', "--")))
    }
}

#[derive(Serialize)]
pub struct Status {
    /// The hub cache the files are looked for in, and downloaded to.
    pub cache: String,
    pub text: bool,
    pub media: bool,
    pub speech: bool,
    pub bytes: u64,
}

pub fn status() -> Status {
    Status {
        cache: hub().display().to_string(),
        text: TEXT.path().is_some(),
        media: MEDIA.path().is_some(),
        speech: SPEECH.path().is_some(),
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
pub fn download(on_progress: &mut dyn FnMut(Progress)) -> Result<()> {
    let hub = hub();
    for file in FILES {
        if file.path().is_none() {
            file.download(&hub, on_progress).with_context(|| format!("downloading {}", file.name))?;
        }
        on_progress(Progress::Ready { file: file.name });
    }
    Ok(())
}

impl ModelFile {
    /// Downloads the file into the cache as the Hugging Face libraries do: `blobs/<sha256>.incomplete` while it comes
    /// down, under their lock for it (`.locks/`), so two downloads of it don't write at once; then `blobs/<sha256>`,
    /// and the revision's name for it in `snapshots/`.
    fn download(&self, hub: &Path, on_progress: &mut dyn FnMut(Progress)) -> Result<()> {
        let folder = self.folder(hub);
        let blobs = folder.join("blobs");
        let locks = hub.join(".locks").join(folder.file_name().expect("a folder name"));
        std::fs::create_dir_all(&blobs)?;
        std::fs::create_dir_all(&locks)?;
        let lock = std::fs::File::create(locks.join(format!("{}.lock", self.sha256)))?;
        // Held until `lock` is dropped (closed).
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            bail!("can't lock {}: {}", self.name, std::io::Error::last_os_error());
        }
        // Another download may have finished it meanwhile.
        if self.path_in(hub).is_some() {
            return Ok(());
        }
        let blob = blobs.join(self.sha256);
        let partial = blobs.join(format!("{}.incomplete", self.sha256));
        self.fetch(&partial, on_progress)?;
        on_progress(Progress::Verifying { file: self.name });
        let digest = sha256(&partial)?;
        if digest != self.sha256 {
            std::fs::remove_file(&partial)?;
            bail!("{} came down damaged (SHA-256 {digest}); run the download again", self.name);
        }
        std::fs::rename(&partial, &blob)?;
        let snapshot = folder.join("snapshots").join(self.revision);
        std::fs::create_dir_all(&snapshot)?;
        match std::os::unix::fs::symlink(Path::new("../../blobs").join(self.sha256), snapshot.join(self.name)) {
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => Err(error.into()),
            _ => Ok(()),
        }
    }

    fn fetch(&self, partial: &Path, on_progress: &mut dyn FnMut(Progress)) -> Result<()> {
        let url = format!("https://huggingface.co/{}/resolve/{}/{}", self.repository, self.revision, self.name);
        let mut done = std::fs::metadata(partial).map(|m| m.len()).unwrap_or(0);
        if done >= self.bytes {
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
            if done - reported >= 8 << 20 || done == self.bytes {
                on_progress(Progress::Downloading { file: self.name, done, total: self.bytes });
                reported = done;
            }
        }
        output.flush()?;
        if done != self.bytes {
            bail!("the download stopped at {done} of {} bytes; run it again to resume", self.bytes);
        }
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(hub: &Path, repository: &str, name: &str, bytes: usize) -> PathBuf {
        let blobs = hub.join(repository).join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blobs.join(name), vec![0u8; bytes]).unwrap();
        blobs.join(name)
    }

    #[test]
    fn a_file_is_there_as_a_whole_blob_of_its_hash_in_any_repository() {
        let hub = std::env::temp_dir().join(format!("sift-it-hub-{}", std::process::id()));
        let file = ModelFile { repository: "org/model", revision: "r", name: "m.gguf", bytes: 4, sha256: "abc" };
        assert_eq!(file.path_in(&hub), None);
        // Another app's download of the same file, under another repository.
        let theirs = blob(&hub, "models--someone--copy", "abc", 4);
        assert_eq!(file.path_in(&hub), Some(theirs));
        // Its own repository's comes first, and a blob that isn't whole doesn't count.
        let ours = blob(&hub, "models--org--model", "abc", 4);
        assert_eq!(file.path_in(&hub), Some(ours.clone()));
        std::fs::write(&ours, [0u8; 2]).unwrap();
        assert_eq!(file.path_in(&hub), Some(hub.join("models--someone--copy/blobs/abc")));
        blob(&hub, "datasets--someone--data", "abc", 4);
        std::fs::remove_dir_all(hub.join("models--someone--copy")).unwrap();
        assert_eq!(file.path_in(&hub), None);
        std::fs::remove_dir_all(&hub).unwrap();
    }
}
