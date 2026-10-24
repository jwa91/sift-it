//! The models, kept where other apps keep theirs: the Hugging Face hub cache ($HF_HUB_CACHE, else $HF_HOME/hub, else
//! ~/.cache/huggingface/hub), in its layout of blobs and snapshots. A model is a repository's files at a pinned
//! revision. The hub names each file's blob after its content: a file it stores with LFS after its SHA-256, a small one
//! after its git blob id (`hf` 2.x makes that name a link into a store all repositories share).
//!
//! A model of one file is there when any repository's folder in the cache holds a whole blob of its name, whichever app
//! or revision put it there. A model of several (the speech model's Core ML folders) is there when its revision's folder
//! holds each of them whole: that folder is what loads.
//!
//! What's missing is downloaded at its pinned revision, with resume, and checked against its hash before it gets its
//! blob's name, so a blob that's there is a whole one. The cache is shared: sift-it only adds what's missing, and never
//! removes anything. A file that goes (another app cleaning the cache) is missing from then on, until it's downloaded
//! again; a process that has it loaded keeps its pages.

use crate::config;
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

/// A model: a repository's files on the hub, at a pinned revision.
pub struct Model {
    /// What progress and messages call it.
    pub name: &'static str,
    pub repository: &'static str,
    pub revision: &'static str,
    pub files: &'static [File],
}

/// A file of a model: where it is in the repository, its size, and its blob's name: the SHA-256 of a file stored with
/// LFS (64 hex digits), else its git blob id (40: the SHA-1 of a "blob <size>\0" header and the file).
pub struct File {
    pub path: &'static str,
    pub bytes: u64,
    pub blob: &'static str,
}

/// EmbeddingGemma 2's text model: ggml-org's Q8_0 GGUF conversion, at the revision `sift-it selftest` checks against.
/// All that searching needs.
pub const TEXT: Model = Model {
    name: "embeddinggemma-2-Q8_0.gguf",
    repository: "ggml-org/embeddinggemma-2-GGUF",
    revision: "bfcd298762cc34d0357ece5ebdd31791a3a374d8",
    files: &[File {
        path: "embeddinggemma-2-Q8_0.gguf",
        bytes: 309_855_456,
        blob: "2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135",
    }],
};

/// Its vision and audio encoders (the mmproj), for pictures and sound. Each loads on its own.
pub const MEDIA: Model = Model {
    name: "mmproj-embeddinggemma-2-Q8_0.gguf",
    repository: "ggml-org/embeddinggemma-2-GGUF",
    revision: "bfcd298762cc34d0357ece5ebdd31791a3a374d8",
    files: &[File {
        path: "mmproj-embeddinggemma-2-Q8_0.gguf",
        bytes: 554_821_024,
        blob: "c4a8a52691ecef40618438928bdf9e68379b854e24166f292592353db0aab64f",
    }],
};

/// The speech model (adapters/speech.rs): Moondream's Parakeet Ultra (a post-trained Parakeet TDT 0.6B v3), compiled
/// for Core ML by FluidInference with an int8 encoder: the four models FluidAudio runs, and the vocabulary.
#[rustfmt::skip]
pub const SPEECH: Model = Model {
    name: "parakeet-ultra-coreml",
    repository: "FluidInference/parakeet-ultra-coreml",
    revision: "95eaa59a39d4394f047a4dc5cce480388a60d1b6",
    files: &[
        File { path: "Decoder.mlmodelc/analytics/coremldata.bin", bytes: 243, blob: "fe92b6cfaa012abd5248c0bc877832f19807015abffc60d87b8ccc8ccb48b3b5" },
        File { path: "Decoder.mlmodelc/coremldata.bin", bytes: 560, blob: "3b06e66768f0df7e21795f50e2b29300e33eeb1a2579dc42c695279c2d308497" },
        File { path: "Decoder.mlmodelc/model.mil", bytes: 13_110, blob: "36dcd5cbb71d13f42a177427ddbab7539e0d167e" },
        File { path: "Decoder.mlmodelc/weights/weight.bin", bytes: 23_604_992, blob: "02a0d219f281b9665bc10c8768649403b2eebbbf4b44c627041d948e0de11bb4" },
        File { path: "Encoder.mlmodelc/analytics/coremldata.bin", bytes: 243, blob: "d87101d824d6723cf95304da33755c3c60e762663b9ef2b0c4bd0aa166a09a0d" },
        File { path: "Encoder.mlmodelc/coremldata.bin", bytes: 514, blob: "397a84a4062f563cbc5f56077c674f09a61d85be5090f61d2f1932afb92ac0fe" },
        File { path: "Encoder.mlmodelc/model.mil", bytes: 1_002_653, blob: "8c5c9915fc71bb7e9b0be1e7b55e38ec6a007e7f" },
        File { path: "Encoder.mlmodelc/weights/weight.bin", bytes: 594_211_328, blob: "315ba01f33cadbf601d43ac7f5c86208b7aa75fdaa34705c9869d5abe3521c9b" },
        File { path: "JointDecisionv3.mlmodelc/analytics/coremldata.bin", bytes: 243, blob: "68d38ca646aebafa7a9329e2efda50f5767c49e89fdb5f77f212072bb66f97c4" },
        File { path: "JointDecisionv3.mlmodelc/coremldata.bin", bytes: 592, blob: "5e3af5a4ce686f6c237cadbd9284e10d333bc0e1546633cd0e430e6194044bc4" },
        File { path: "JointDecisionv3.mlmodelc/model.mil", bytes: 11_777, blob: "be552d46f98f9dc9b242b0d1659833db60aa3f83" },
        File { path: "JointDecisionv3.mlmodelc/weights/weight.bin", bytes: 12_642_764, blob: "3f310b85b82341c53ec383025ab094a4e462ee1c592e1ad7c6bfe39cff66ca25" },
        File { path: "Preprocessor.mlmodelc/analytics/coremldata.bin", bytes: 243, blob: "c9beeb989c8d66f8be11df59bc6df277ec76cee404f6865b46243835ef562f6d" },
        File { path: "Preprocessor.mlmodelc/coremldata.bin", bytes: 486, blob: "dbde3f2300842c1fd51ef3ff948a0bcffe65ffd2dca10707f2509f32c1d65b1d" },
        File { path: "Preprocessor.mlmodelc/metadata.json", bytes: 2_841, blob: "06162c58cf2f5343a0e60f36f843bcdf3cc58527" },
        File { path: "Preprocessor.mlmodelc/model.mil", bytes: 28_181, blob: "f987e2479228436c49189aa47edb376db74dcff5" },
        File { path: "Preprocessor.mlmodelc/weights/weight.bin", bytes: 491_072, blob: "129b76e3aeafa8afa3ea76d995b964b145fe83700d579f6ff42c4c38fa0968ea" },
        File { path: "parakeet_vocab.json", bytes: 151_122, blob: "c684822ec3b671ebc449c433a7012398d39b8bb0" },
    ],
};

/// The text model first: with it alone, search works. The speech model last: the rest works without it.
const MODELS: [&Model; 3] = [&TEXT, &MEDIA, &SPEECH];

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

impl Model {
    /// Where the model is, when the cache holds all of it: the file itself for a model of one, else its revision's
    /// folder.
    pub fn path(&self) -> Option<PathBuf> {
        self.path_in(&hub())
    }

    fn path_in(&self, hub: &Path) -> Option<PathBuf> {
        match self.files {
            [file] => file.blob_in(&self.folder(hub)).or_else(|| {
                std::fs::read_dir(hub)
                    .ok()?
                    .flatten()
                    .filter(|entry| entry.file_name().to_string_lossy().starts_with("models--"))
                    .find_map(|entry| file.blob_in(&entry.path()))
            }),
            files => {
                let snapshot = self.snapshot(hub);
                files.iter().all(|file| file.whole(&snapshot.join(file.path))).then_some(snapshot)
            }
        }
    }

    /// The model, or why it can't be had.
    pub fn require(&self) -> Result<PathBuf> {
        self.path().ok_or_else(|| anyhow!("{} isn't downloaded (run `sift-it model download`)", self.name))
    }

    /// The repository's folder in the cache.
    fn folder(&self, hub: &Path) -> PathBuf {
        hub.join(format!("models--{}", self.repository.replace('/', "--")))
    }

    /// The revision's folder: its files by their names in the repository, each a link to its blob.
    fn snapshot(&self, hub: &Path) -> PathBuf {
        self.folder(hub).join("snapshots").join(self.revision)
    }

    fn bytes(&self) -> u64 {
        self.files.iter().map(|file| file.bytes).sum()
    }
}

impl File {
    /// Whether `path` holds the file whole.
    fn whole(&self, path: &Path) -> bool {
        std::fs::metadata(path).is_ok_and(|m| m.len() == self.bytes)
    }

    /// Its blob in a repository's folder, when that's whole.
    fn blob_in(&self, folder: &Path) -> Option<PathBuf> {
        let blob = folder.join("blobs").join(self.blob);
        self.whole(&blob).then_some(blob)
    }
}

#[derive(Serialize)]
pub struct Status {
    /// The hub cache the models are looked for in, and downloaded to.
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
        bytes: MODELS.iter().map(|model| model.bytes()).sum(),
    }
}

/// How a download goes, a model at a time (`file` is its name).
#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Progress<'a> {
    Downloading { file: &'a str, done: u64, total: u64 },
    Verifying { file: &'a str },
    Ready { file: &'a str },
}

/// Downloads the models that aren't there yet, or what's missing of them.
pub fn download(on_progress: &mut dyn FnMut(Progress)) -> Result<()> {
    let hub = hub();
    for model in MODELS {
        if model.path_in(&hub).is_none() {
            model.download(&hub, on_progress).with_context(|| format!("downloading {}", model.name))?;
        }
        on_progress(Progress::Ready { file: model.name });
    }
    Ok(())
}

impl Model {
    /// Downloads the files its revision's folder doesn't hold whole, as the Hugging Face libraries do: each to
    /// `blobs/<blob>.incomplete` while it comes down, under their lock for it (`.locks/`), so two downloads of it don't
    /// write at once; then `blobs/<blob>`, and the revision's name for it in `snapshots/`.
    fn download(&self, hub: &Path, on_progress: &mut dyn FnMut(Progress)) -> Result<()> {
        let (folder, snapshot) = (self.folder(hub), self.snapshot(hub));
        let missing: Vec<&File> = self.files.iter().filter(|file| !file.whole(&snapshot.join(file.path))).collect();
        let total = missing.iter().map(|file| file.bytes).sum();
        let mut before = 0;
        for file in missing {
            if file.blob_in(&folder).is_none() {
                self.fetch(file, hub, before, total, on_progress)?;
            }
            // A link from where the file is in the revision to its blob: up from its folders and the revision's.
            let link = snapshot.join(file.path);
            std::fs::create_dir_all(link.parent().expect("in the revision's folder"))?;
            let up = "../".repeat(file.path.matches('/').count() + 2);
            match std::os::unix::fs::symlink(format!("{up}blobs/{}", file.blob), &link) {
                Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => return Err(error.into()),
                _ => {}
            }
            before += file.bytes;
        }
        Ok(())
    }

    /// Downloads a file to its blob, under the lock for it, and checks it before it gets the blob's name. Progress
    /// counts the `before` bytes of the model's files that came down already, of `total`.
    fn fetch(
        &self,
        file: &File,
        hub: &Path,
        before: u64,
        total: u64,
        on_progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        let folder = self.folder(hub);
        let blobs = folder.join("blobs");
        let locks = hub.join(".locks").join(folder.file_name().expect("a folder name"));
        std::fs::create_dir_all(&blobs)?;
        std::fs::create_dir_all(&locks)?;
        let lock = std::fs::File::create(locks.join(format!("{}.lock", file.blob)))?;
        // Held until `lock` is dropped (closed).
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            bail!("can't lock {}: {}", file.path, std::io::Error::last_os_error());
        }
        // Another download may have finished it meanwhile.
        if file.blob_in(&folder).is_some() {
            return Ok(());
        }
        let partial = blobs.join(format!("{}.incomplete", file.blob));
        let url = format!("https://huggingface.co/{}/resolve/{}/{}", self.repository, self.revision, file.path);
        file.fetch(&url, &partial, &mut |done| {
            on_progress(Progress::Downloading { file: self.name, done: before + done, total })
        })?;
        on_progress(Progress::Verifying { file: self.name });
        let digest = file.digest(&partial)?;
        if digest != file.blob {
            std::fs::remove_file(&partial)?;
            bail!("{} came down damaged (its hash is {digest}); run the download again", file.path);
        }
        Ok(std::fs::rename(&partial, blobs.join(file.blob))?)
    }
}

impl File {
    fn fetch(&self, url: &str, partial: &Path, progress: &mut dyn FnMut(u64)) -> Result<()> {
        let mut done = std::fs::metadata(partial).map(|m| m.len()).unwrap_or(0);
        if done >= self.bytes {
            return Ok(());
        }
        let mut request = ureq::get(url);
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
                progress(done);
                reported = done;
            }
        }
        output.flush()?;
        if done != self.bytes {
            bail!("the download stopped at {done} of {} bytes; run it again to resume", self.bytes);
        }
        Ok(())
    }

    /// The hash of what's at `path` that names the file's blob: its git blob id when that's the name, else its
    /// SHA-256.
    fn digest(&self, path: &Path) -> Result<String> {
        if self.blob.len() == 40 {
            hash(Sha1::new_with_prefix(format!("blob {}\0", std::fs::metadata(path)?.len())), path)
        } else {
            hash(Sha256::new(), path)
        }
    }
}

fn hash(mut hasher: impl Digest, path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
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

    fn temp_hub(name: &str) -> PathBuf {
        let hub = std::env::temp_dir().join(format!("sift-it-hub-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&hub);
        hub
    }

    fn write(path: &Path, bytes: usize) -> PathBuf {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![0u8; bytes]).unwrap();
        path.to_path_buf()
    }

    #[test]
    fn a_model_of_one_file_is_there_as_a_whole_blob_of_its_name_in_any_repository() {
        let hub = temp_hub("one");
        let files = &[File { path: "m.gguf", bytes: 4, blob: "abc" }];
        let model = Model { name: "m.gguf", repository: "org/model", revision: "r", files };
        assert_eq!(model.path_in(&hub), None);
        // Another app's download of the same file, under another repository.
        let theirs = write(&hub.join("models--someone--copy/blobs/abc"), 4);
        assert_eq!(model.path_in(&hub), Some(theirs.clone()));
        // Its own repository's comes first, and a blob that isn't whole doesn't count.
        let ours = write(&hub.join("models--org--model/blobs/abc"), 4);
        assert_eq!(model.path_in(&hub), Some(ours.clone()));
        std::fs::write(&ours, [0u8; 2]).unwrap();
        assert_eq!(model.path_in(&hub), Some(theirs));
        write(&hub.join("datasets--someone--data/blobs/abc"), 4);
        std::fs::remove_dir_all(hub.join("models--someone--copy")).unwrap();
        assert_eq!(model.path_in(&hub), None);
        std::fs::remove_dir_all(&hub).unwrap();
    }

    #[test]
    fn a_model_of_several_files_is_there_when_its_revision_holds_each_whole() {
        let hub = temp_hub("several");
        let files = &[
            File { path: "a.mlmodelc/weights/weight.bin", bytes: 4, blob: "abc" },
            File { path: "vocab.json", bytes: 2, blob: "def" },
        ];
        let model = Model { name: "m", repository: "org/model", revision: "r", files };
        let snapshot = hub.join("models--org--model/snapshots/r");
        write(&hub.join("models--org--model/blobs/abc"), 4);
        write(&hub.join("models--org--model/blobs/def"), 2);
        // The blobs alone aren't the folder that loads: the revision's links are.
        assert_eq!(model.path_in(&hub), None);
        std::fs::create_dir_all(snapshot.join("a.mlmodelc/weights")).unwrap();
        std::os::unix::fs::symlink("../../../../blobs/abc", snapshot.join("a.mlmodelc/weights/weight.bin")).unwrap();
        assert_eq!(model.path_in(&hub), None);
        std::os::unix::fs::symlink("../../blobs/def", snapshot.join("vocab.json")).unwrap();
        assert_eq!(model.path_in(&hub), Some(snapshot));
        // A blob that goes takes its file with it.
        std::fs::remove_file(hub.join("models--org--model/blobs/abc")).unwrap();
        assert_eq!(model.path_in(&hub), None);
        std::fs::remove_dir_all(&hub).unwrap();
    }

    #[test]
    fn a_file_is_checked_by_the_hash_its_blob_is_named_after() {
        let hub = temp_hub("hash");
        let path = hub.join("hello");
        std::fs::create_dir_all(&hub).unwrap();
        std::fs::write(&path, "hello\n").unwrap();
        // As `git hash-object` and `shasum -a 256` have it.
        let git = File { path: "hello", bytes: 6, blob: "ce013625030ba8dba906f756967f9e9ca394464a" };
        let lfs =
            File { path: "hello", bytes: 6, blob: "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03" };
        assert_eq!(git.digest(&path).unwrap(), git.blob);
        assert_eq!(lfs.digest(&path).unwrap(), lfs.blob);
        std::fs::remove_dir_all(&hub).unwrap();
    }
}
