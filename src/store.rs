//! The index: one SQLite file. A row per file, a row per segment (a passage, a PDF page, a picture, a video frame,
//! 30 s of sound, what's said in a recording) with its vector, and a full-text index over every segment's words and
//! its file's name and tags.
//!
//! The indexer is the only writer; searchers read alongside it (WAL), and notice its changes through SQLite's
//! data_version.

use crate::crawl::{Found, Kind};
use crate::embedder::DIM;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// An index of another version is dropped and made again.
const SCHEMA_VERSION: i64 = 5;

/// What a segment is, which also says which of the model's inputs made its vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Segment {
    /// A stretch of a text file or document.
    Passage,
    /// A PDF page's text (or, for a scanned page, the text read from it).
    Page,
    /// A picture, as a picture.
    Picture,
    /// The text in a picture or a scanned page (Apple's text recognition).
    Ocr,
    /// 30 s of sound.
    Sound,
    /// A video frame.
    Frame,
    /// What's said in a recording or video, written down (speech.rs): a sentence or a few, with their time.
    Speech,
}

impl Segment {
    pub fn as_str(self) -> &'static str {
        match self {
            Segment::Passage => "passage",
            Segment::Page => "page",
            Segment::Picture => "picture",
            Segment::Ocr => "ocr",
            Segment::Sound => "sound",
            Segment::Frame => "frame",
            Segment::Speech => "speech",
        }
    }

    pub fn parse(name: &str) -> Option<Segment> {
        [
            Segment::Passage,
            Segment::Page,
            Segment::Picture,
            Segment::Ocr,
            Segment::Sound,
            Segment::Frame,
            Segment::Speech,
        ]
        .into_iter()
        .find(|segment| segment.as_str() == name)
    }
}

/// A segment to write.
#[derive(Debug, Clone)]
pub struct NewSegment {
    pub segment: Segment,
    /// Page (from 1), line (from 1) or second, by segment.
    pub loc: Option<f64>,
    pub loc_end: Option<f64>,
    /// The words for the keyword index and excerpts.
    pub text: Option<String>,
    pub vector: Option<Vec<f32>>,
}

#[derive(Debug, Clone)]
pub struct FileRow {
    pub id: i64,
    pub path: PathBuf,
    pub kind: Kind,
    pub size: u64,
    pub modified: f64,
    /// How it's read (readers::fingerprint).
    pub reading: String,
    /// What's said in it is to be written down.
    pub speech: bool,
}

/// The files whose speech is to be written down, once they're read.
const SPEECH: &str = "speech AND state = 'done'";

/// The columns a FileRow is made of (file_row).
const FILE_COLUMNS: &str = "id, path, kind, size, modified, reading, speech";

fn file_row(row: &Row) -> rusqlite::Result<FileRow> {
    Ok(FileRow {
        id: row.get(0)?,
        path: PathBuf::from(row.get::<_, String>(1)?),
        kind: Kind::parse(&row.get::<_, String>(2)?).unwrap_or(Kind::Plaintext),
        size: row.get::<_, i64>(3)? as u64,
        modified: row.get(4)?,
        reading: row.get(5)?,
        speech: row.get(6)?,
    })
}

#[derive(Debug, Default, Serialize)]
pub struct Stats {
    pub files: i64,
    pub done: i64,
    pub pending: i64,
    pub failed: i64,
    pub segments: i64,
    pub vectors: i64,
    pub by_kind: HashMap<String, i64>,
    pub bytes: u64,
}

/// The work still queued (Store::queued).
#[derive(Debug, Serialize)]
pub struct Queued {
    pub files: usize,
    /// About how long reading them takes (estimate::cost).
    pub seconds: f64,
    /// The dearest of them, dearest first.
    pub heaviest: Vec<Heavy>,
}

#[derive(Debug, Serialize)]
pub struct Heavy {
    pub path: String,
    pub kind: Kind,
    pub seconds: f64,
}

/// Recordings and videos whose speech is written down (Store::transcripts).
#[derive(Debug, Serialize)]
pub struct Transcripts {
    pub done: usize,
    pub total: usize,
    /// Sound still to be written down.
    pub seconds: f64,
}

pub struct Store {
    pub conn: Connection,
    path: PathBuf,
}

pub fn vector_to_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
}

pub fn blob_to_vector(blob: &[u8], into: &mut Vec<f32>) {
    into.extend(blob.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
}

/// Words for the keyword index from a file: its name (name_words), and the tags read from it (a recording's title,
/// artist and album: see av::tags), which `info` holds.
pub fn file_words(path: &Path, info: &serde_json::Value) -> String {
    let mut words = name_words(path);
    if let Some(tags) = info.get("tags").and_then(|tags| tags.as_object()) {
        for value in tags.values().filter_map(|value| value.as_str()) {
            words.push(' ');
            words.push_str(value);
        }
    }
    words
}

/// Words for the keyword index from a path: the file's name split at punctuation and case changes, its extension,
/// and the folder it's in.
pub fn name_words(path: &Path) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mut words = String::new();
    let mut previous_lower = false;
    for c in stem.chars() {
        if c.is_uppercase() && previous_lower {
            words.push(' ');
        }
        previous_lower = c.is_lowercase();
        words.push(if c == '_' || c == '-' || c == '.' { ' ' } else { c });
    }
    if let Some(extension) = path.extension() {
        words.push(' ');
        words.push_str(&extension.to_string_lossy());
    }
    if let Some(folder) = path.parent().and_then(|p| p.file_name()) {
        words.push(' ');
        words.push_str(&folder.to_string_lossy());
    }
    words
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path).with_context(|| format!("can't open {}", path.display()))?;
        let store = Self { conn, path: path.to_path_buf() };
        store.setup()?;
        Ok(store)
    }

    /// A reader's connection: never writes, never creates.
    pub fn open_readonly(path: &Path) -> Result<Self> {
        if !path.exists() {
            // A searcher may start before the first index: make an empty one to read.
            drop(Self::open(path)?);
        }
        let conn =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
                .with_context(|| format!("can't open {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self { conn, path: path.to_path_buf() })
    }

    fn setup(&self) -> Result<()> {
        self.conn.busy_timeout(std::time::Duration::from_secs(10))?;
        self.conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )?;
        let version: Option<i64> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |row| row.get::<_, String>(0))
            .optional()?
            .and_then(|v| v.parse().ok());
        if version.is_some_and(|version| version != SCHEMA_VERSION) {
            eprintln!("{} was made by another version of sift-it: everything is indexed again", self.path.display());
            self.conn.execute_batch(
                "DROP TABLE IF EXISTS words; DROP TABLE IF EXISTS segments; DROP TABLE IF EXISTS files;
                 DELETE FROM meta; VACUUM;",
            )?;
        }
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                 id INTEGER PRIMARY KEY,
                 path TEXT NOT NULL UNIQUE,
                 kind TEXT NOT NULL,
                 size INTEGER NOT NULL,
                 modified REAL NOT NULL,
                 reading TEXT NOT NULL,                   -- how it's read (readers::fingerprint)
                 speech INTEGER NOT NULL,                 -- what's said in it is to be written down
                 cost REAL NOT NULL,                      -- about how long reading it takes, in seconds (estimate::cost)
                 state TEXT NOT NULL DEFAULT 'pending',   -- pending | done | failed
                 error TEXT,
                 info TEXT,                               -- JSON: pages, duration, width, height, tags
                 indexed_at REAL,
                 transcript TEXT                          -- NULL (to do) | at:<seconds> | done | failed: <why>
             );
             CREATE INDEX IF NOT EXISTS files_state ON files(state);
             CREATE TABLE IF NOT EXISTS segments (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                 segment TEXT NOT NULL,
                 loc REAL,
                 loc_end REAL,
                 text TEXT,
                 vector BLOB
             );
             CREATE INDEX IF NOT EXISTS segments_file ON segments(file_id);
             CREATE VIRTUAL TABLE IF NOT EXISTS words USING fts5(
                 name, text, content = '', contentless_delete = 1,
                 tokenize = 'unicode61 remove_diacritics 2', prefix = '2 3'
             );",
        )?;
        if version != Some(SCHEMA_VERSION) {
            self.set_meta("schema", &SCHEMA_VERSION.to_string())?;
        }
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| row.get(0)).optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO meta VALUES (?1, ?2)", [key, value])?;
        Ok(())
    }

    /// Changes when another connection commits (SQLite's data_version): a reader's cue to refresh.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self.conn.query_row("PRAGMA data_version", [], |row| row.get(0))?)
    }

    pub fn files(&self) -> Result<Vec<FileRow>> {
        let mut statement = self.conn.prepare(&format!("SELECT {FILE_COLUMNS} FROM files"))?;
        let rows = statement.query_map([], file_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The files at a path or under it.
    pub fn files_under(&self, path: &Path) -> Result<Vec<FileRow>> {
        let exact = path.to_string_lossy().to_string();
        let prefix = format!("{}/", exact.trim_end_matches('/'));
        let mut statement = self
            .conn
            .prepare(&format!("SELECT {FILE_COLUMNS} FROM files WHERE path = ?1 OR substr(path, 1, ?2) = ?3"))?;
        let rows = statement.query_map(params![exact, prefix.chars().count() as i64, prefix], file_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn pending_count(&self) -> Result<usize> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM files WHERE state = 'pending'", [], |row| row.get::<_, i64>(0))?
            as usize)
    }

    /// A new or changed file: (re)queued, with what was indexed from it before gone. `cost` is about how long reading
    /// it takes (estimate::cost): the queue goes cheapest first.
    pub fn queue(&self, found: &Found, existing: Option<i64>, cost: f64) -> Result<i64> {
        let path = found.path.to_string_lossy();
        let (kind, size, reading, speech) = (found.kind.as_str(), found.size as i64, found.reading(), found.speech());
        if let Some(id) = existing {
            self.delete_segments(id)?;
            self.conn.execute(
                "UPDATE files SET kind = ?2, size = ?3, modified = ?4, reading = ?5, speech = ?6, cost = ?7,
                 state = 'pending', error = NULL, info = NULL, indexed_at = NULL, transcript = NULL WHERE id = ?1",
                params![id, kind, size, found.modified, reading, speech, cost],
            )?;
            Ok(id)
        } else {
            self.conn.execute(
                "INSERT INTO files (path, kind, size, modified, reading, speech, cost)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![path, kind, size, found.modified, reading, speech, cost],
            )?;
            Ok(self.conn.last_insert_rowid())
        }
    }

    /// Whether what's said in a file is to be written down, when only that changed: nothing is read again. What was
    /// written down goes either way (it's to do, or not wanted).
    pub fn set_speech(&self, id: i64, speech: bool) -> Result<()> {
        self.conn.execute(
            "DELETE FROM words WHERE rowid IN (SELECT id FROM segments WHERE file_id = ?1 AND segment = 'speech')",
            [id],
        )?;
        self.conn.execute("DELETE FROM segments WHERE file_id = ?1 AND segment = 'speech'", [id])?;
        self.conn.execute(
            "UPDATE files SET speech = ?2, transcript = NULL, info = json_remove(info, '$.speech') WHERE id = ?1",
            params![id, speech],
        )?;
        Ok(())
    }

    pub fn remove_file(&self, id: i64) -> Result<()> {
        self.delete_segments(id)?;
        self.conn.execute("DELETE FROM files WHERE id = ?1", [id])?;
        Ok(())
    }

    fn delete_segments(&self, file_id: i64) -> Result<()> {
        delete_segments(&self.conn, file_id)
    }

    /// Files waiting to be indexed, cheapest first (newest first among equals).
    pub fn pending(&self, limit: usize) -> Result<Vec<FileRow>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {FILE_COLUMNS} FROM files WHERE state = 'pending' ORDER BY cost, modified DESC LIMIT ?1"
        ))?;
        let rows = statement.query_map([limit.min(i64::MAX as usize) as i64], file_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The work still queued: how many files, about how long reading them takes, and the `heaviest` dearest ones.
    pub fn queued(&self, heaviest: usize) -> Result<Queued> {
        let (files, seconds) =
            self.conn.query_row("SELECT COUNT(*), TOTAL(cost) FROM files WHERE state = 'pending'", [], |row| {
                Ok((row.get::<_, i64>(0)? as usize, row.get(1)?))
            })?;
        let mut statement = self.conn.prepare(
            "SELECT path, kind, cost FROM files WHERE state = 'pending' ORDER BY cost DESC, modified DESC LIMIT ?1",
        )?;
        let heaviest = statement
            .query_map([heaviest as i64], |row| {
                Ok(Heavy {
                    path: row.get(0)?,
                    kind: Kind::parse(&row.get::<_, String>(1)?).unwrap_or(Kind::Plaintext),
                    seconds: row.get(2)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(Queued { files, seconds, heaviest })
    }

    /// Writes a part of a big file's segments while the rest is still being read, so memory doesn't grow with the
    /// file, and a search finds that part meanwhile. The `first` part clears what an earlier read left. Nothing is
    /// written (false) when the file left the index or changed since it was handed out.
    pub fn write_part(&mut self, row: &FileRow, segments: &[NewSegment], first: bool) -> Result<bool> {
        let tx = self.conn.transaction()?;
        if !as_handed_out(&tx, row)? {
            return Ok(false);
        }
        if first {
            delete_segments(&tx, row.id)?;
        }
        insert_segments(&tx, row.id, &name_words(&row.path), segments)?;
        tx.commit()?;
        Ok(true)
    }

    /// Writes a file's (last) segments and marks it done, in one transaction. `parts`: some were written already
    /// (write_part), and stay. Nothing is written (false) when the file left the index or changed since it was handed
    /// out (`row`): it's been queued again.
    pub fn finish_file(
        &mut self,
        row: &FileRow,
        segments: &[NewSegment],
        info: &serde_json::Value,
        parts: bool,
    ) -> Result<bool> {
        self.write_file(row, segments, info, parts, None)
    }

    /// A file that couldn't be read: still findable by name, and not tried again until it changes. Parts written of
    /// it go.
    pub fn fail_file(&mut self, row: &FileRow, error: &str) -> Result<bool> {
        self.write_file(row, &[], &serde_json::json!({}), false, Some(error))
    }

    fn write_file(
        &mut self,
        row: &FileRow,
        segments: &[NewSegment],
        info: &serde_json::Value,
        parts: bool,
        error: Option<&str>,
    ) -> Result<bool> {
        let tx = self.conn.transaction()?;
        if !as_handed_out(&tx, row)? {
            return Ok(false);
        }
        let name = file_words(&row.path, info);
        if !parts {
            delete_segments(&tx, row.id)?;
        } else if name != name_words(&row.path) {
            // The parts went in by the file's name alone: now its tags are known, they're found by those too.
            rename_words(&tx, row.id, &name)?;
        }
        insert_segments(&tx, row.id, &name, segments)?;
        // A file with nothing in it is still findable by its name.
        if segments.is_empty() && !parts {
            let empty = NewSegment { segment: Segment::Passage, loc: None, loc_end: None, text: None, vector: None };
            insert_segments(&tx, row.id, &name, &[empty])?;
        }
        tx.execute(
            "UPDATE files SET state = ?2, error = ?3, info = ?4, indexed_at = ?5 WHERE id = ?1",
            params![row.id, if error.is_some() { "failed" } else { "done" }, error, info.to_string(), now()],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Recordings and videos whose speech is still to be written down (or was, partway: `at`), newest first. The
    /// second of each is where to go on from.
    pub fn transcripts_pending(&self, limit: usize) -> Result<Vec<(FileRow, f64)>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {FILE_COLUMNS}, transcript FROM files
             WHERE {SPEECH} AND (transcript IS NULL OR transcript LIKE 'at:%') ORDER BY modified DESC LIMIT ?1",
        ))?;
        let rows = statement.query_map([limit as i64], |row| {
            let progress: Option<String> = row.get(7)?;
            Ok((
                file_row(row)?,
                progress.and_then(|p| p.strip_prefix("at:").and_then(|at| at.parse().ok())).unwrap_or(0.0),
            ))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// How many recordings and videos have their speech written down, how many in all, and how much sound is left.
    pub fn transcripts(&self) -> Result<Transcripts> {
        Ok(self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FILTER (WHERE transcript IS NOT NULL AND transcript NOT LIKE 'at:%'), COUNT(*),
                     TOTAL(MAX(json_extract(info, '$.duration') - COALESCE(CAST(substr(transcript, 4) AS REAL), 0), 0))
                         FILTER (WHERE transcript IS NULL OR transcript LIKE 'at:%')
                 FROM files WHERE {SPEECH}"
            ),
            [],
            |row| {
                Ok(Transcripts {
                    done: row.get::<_, i64>(0)? as usize,
                    total: row.get::<_, i64>(1)? as usize,
                    seconds: row.get(2)?,
                })
            },
        )?)
    }

    /// Writes down a stretch of what's said in a file (speech segments with their vectors), and how far that goes:
    /// `upto` seconds, or `done`. The first stretch (from 0) replaces what was written before, and takes the file's
    /// tags, which go into the keyword index with its name. Nothing is written (false) when the file changed since
    /// it was handed out: it's being indexed again.
    pub fn add_transcript(
        &mut self,
        row: &FileRow,
        from: f64,
        passages: &[NewSegment],
        tags: Option<&serde_json::Value>,
        progress: &str,
    ) -> Result<bool> {
        let tx = self.conn.transaction()?;
        let current: Option<(String, i64, f64, String, Option<String>)> = tx
            .query_row("SELECT path, size, modified, state, info FROM files WHERE id = ?1", [row.id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()?;
        let Some((path, size, modified, state, info)) = current else { return Ok(false) };
        if Path::new(&path) != row.path
            || size as u64 != row.size
            || (modified - row.modified).abs() >= 0.001
            || state != "done"
        {
            return Ok(false);
        }
        let mut info: serde_json::Value =
            info.and_then(|info| serde_json::from_str(&info).ok()).unwrap_or_else(|| serde_json::json!({}));
        if from == 0.0 {
            tx.execute(
                "DELETE FROM words WHERE rowid IN (SELECT id FROM segments WHERE file_id = ?1 AND segment = 'speech')",
                [row.id],
            )?;
            tx.execute("DELETE FROM segments WHERE file_id = ?1 AND segment = 'speech'", [row.id])?;
            if let Some(tags) = tags.filter(|tags| tags.as_object().is_some_and(|tags| !tags.is_empty())) {
                info["tags"] = tags.clone();
                // The file's other segments are found by its tags too.
                rename_words(&tx, row.id, &file_words(&row.path, &info))?;
            }
        }
        insert_segments(&tx, row.id, &file_words(&row.path, &info), passages)?;
        let spoken: i64 =
            tx.query_row("SELECT COUNT(*) FROM segments WHERE file_id = ?1 AND segment = 'speech'", [row.id], |r| {
                r.get(0)
            })?;
        info["speech"] = serde_json::json!(spoken);
        tx.execute(
            "UPDATE files SET transcript = ?2, info = ?3 WHERE id = ?1",
            params![row.id, progress, info.to_string()],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Makes sure what's written down of recordings comes from the speech model that writes it now (`source`, see
    /// speech::source). When it doesn't, all of it is written down again. True then.
    pub fn adopt_speech_source(&self, source: &str) -> Result<bool> {
        let stored = self.meta("speech_source")?;
        let stale = stored.as_deref().is_some_and(|stored| stored != source);
        if stale {
            self.conn.execute_batch(
                "DELETE FROM words WHERE rowid IN (SELECT id FROM segments WHERE segment = 'speech');
                 DELETE FROM segments WHERE segment = 'speech';
                 UPDATE files SET transcript = NULL;",
            )?;
        }
        if stored.as_deref() != Some(source) {
            self.set_meta("speech_source", source)?;
        }
        Ok(stale)
    }

    /// A segment's whole text.
    pub fn segment_text(&self, segment_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT text FROM segments WHERE id = ?1", [segment_id], |row| row.get(0))
            .optional()?
            .flatten())
    }

    /// A file's text: its text segments in order, at most about 8,000 characters.
    pub fn file_text(&self, path: &str) -> Result<Option<String>> {
        let mut statement = self.conn.prepare(
            "SELECT s.text FROM segments s JOIN files f ON f.id = s.file_id
             WHERE f.path = ?1 AND s.text IS NOT NULL AND s.text != '' ORDER BY s.loc, s.id",
        )?;
        let mut text = String::new();
        for part in statement.query_map([path], |row| row.get::<_, String>(0))? {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&part?);
            if text.len() > 8000 {
                break;
            }
        }
        Ok((!text.is_empty()).then_some(text))
    }

    pub fn stats(&self) -> Result<Stats> {
        let mut stats = Stats::default();
        let mut statement =
            self.conn.prepare("SELECT kind, state, COUNT(*), SUM(size) FROM files GROUP BY kind, state")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let kind: String = row.get(0)?;
            let state: String = row.get(1)?;
            let count: i64 = row.get(2)?;
            stats.files += count;
            stats.bytes += row.get::<_, Option<i64>>(3)?.unwrap_or(0) as u64;
            match state.as_str() {
                "done" => stats.done += count,
                "pending" => stats.pending += count,
                _ => stats.failed += count,
            }
            *stats.by_kind.entry(kind).or_default() += count;
        }
        stats.segments = self.conn.query_row("SELECT COUNT(*) FROM segments", [], |row| row.get(0))?;
        stats.vectors =
            self.conn.query_row("SELECT COUNT(*) FROM segments WHERE vector IS NOT NULL", [], |row| row.get(0))?;
        Ok(stats)
    }

    /// Makes sure the index's vectors come from the pipeline that makes new ones (`source`, see
    /// embedder::vector_source). When they don't, every file is queued again; true then.
    pub fn adopt_vector_source(&self, source: &str) -> Result<bool> {
        let stored = self.meta("vector_source")?;
        let stale = stored.as_deref().is_some_and(|stored| stored != source);
        if stale {
            self.requeue_all()?;
        }
        if stored.as_deref() != Some(source) {
            self.set_meta("vector_source", source)?;
        }
        Ok(stale)
    }

    /// Queues again the files where some piece couldn't be embedded (a passing failure, perhaps), once they've had
    /// `hours` to settle. Returns how many.
    pub fn requeue_unembedded(&self, hours: f64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE files SET state = 'pending' WHERE state = 'done'
             AND json_extract(info, '$.unembedded') > 0 AND indexed_at < ?1",
            [now() - hours * 3600.0],
        )?)
    }

    /// Drops every vector and queues every file again (after a change of model or pipeline).
    pub fn requeue_all(&self) -> Result<()> {
        self.conn.execute_batch(
            "INSERT INTO words (words) VALUES ('delete-all'); DELETE FROM segments;
             UPDATE files SET state = 'pending', error = NULL, info = NULL, indexed_at = NULL, transcript = NULL;",
        )?;
        Ok(())
    }
}

/// Whether a file is still the one handed out, to be read the same way: its id, and its path too. A file renamed
/// meanwhile leaves its row, and the new path's row can get the same id (SQLite reuses the highest one) with the same
/// size and time.
fn as_handed_out(conn: &Connection, row: &FileRow) -> Result<bool> {
    let current: Option<(String, i64, f64, String)> = conn
        .query_row("SELECT path, size, modified, reading FROM files WHERE id = ?1", [row.id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .optional()?;
    Ok(current.is_some_and(|(path, size, modified, reading)| {
        Path::new(&path) == row.path
            && size as u64 == row.size
            && (modified - row.modified).abs() < 0.001
            && reading == row.reading
    }))
}

fn delete_segments(conn: &Connection, file_id: i64) -> Result<()> {
    conn.execute("DELETE FROM words WHERE rowid IN (SELECT id FROM segments WHERE file_id = ?1)", [file_id])?;
    conn.execute("DELETE FROM segments WHERE file_id = ?1", [file_id])?;
    Ok(())
}

/// Writes segments, each with its words (and the file's, `name`) in the keyword index.
fn insert_segments(conn: &Connection, file_id: i64, name: &str, segments: &[NewSegment]) -> Result<()> {
    let mut insert = conn.prepare_cached(
        "INSERT INTO segments (file_id, segment, loc, loc_end, text, vector) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    let mut words = conn.prepare_cached("INSERT INTO words (rowid, name, text) VALUES (?1, ?2, ?3)")?;
    for segment in segments {
        if let Some(vector) = &segment.vector
            && vector.len() != DIM
        {
            bail!("vector of {} values", vector.len());
        }
        insert.execute(params![
            file_id,
            segment.segment.as_str(),
            segment.loc,
            segment.loc_end,
            segment.text,
            segment.vector.as_deref().map(vector_to_blob),
        ])?;
        words.execute(params![conn.last_insert_rowid(), name, segment.text.as_deref().unwrap_or("")])?;
    }
    Ok(())
}

/// Puts a file's segments in the keyword index again under the file's words `name` (its name and tags).
fn rename_words(conn: &Connection, file_id: i64, name: &str) -> Result<()> {
    let segments: Vec<(i64, Option<String>)> = conn
        .prepare("SELECT id, text FROM segments WHERE file_id = ?1")?
        .query_map([file_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    for (id, text) in segments {
        conn.execute("DELETE FROM words WHERE rowid = ?1", [id])?;
        conn.execute(
            "INSERT INTO words (rowid, name, text) VALUES (?1, ?2, ?3)",
            params![id, name, text.unwrap_or_default()],
        )?;
    }
    Ok(())
}

pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;

    /// A store in a file of its own, removed when the test ends.
    struct TempStore(Store, PathBuf);
    impl TempStore {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("sift-it-test-{name}-{}.sqlite", std::process::id()));
            Self::remove(&path);
            Self(Store::open(&path).unwrap(), path)
        }
        fn remove(path: &Path) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
        }
    }
    impl Drop for TempStore {
        fn drop(&mut self) {
            Self::remove(&self.1);
        }
    }

    fn found(path: &str) -> Found {
        let settings = Settings::BASE;
        Found { path: PathBuf::from(path), kind: Kind::Plaintext, size: 10, modified: 100.0, settings }
    }

    fn passage(text: &str) -> NewSegment {
        NewSegment { segment: Segment::Passage, loc: None, loc_end: None, text: Some(text.into()), vector: None }
    }

    fn texts(store: &Store, id: i64) -> Vec<String> {
        let mut statement = store.conn.prepare("SELECT text FROM segments WHERE file_id = ?1 ORDER BY id").unwrap();
        statement
            .query_map([id], |row| row.get::<_, Option<String>>(0))
            .unwrap()
            .map(|t| t.unwrap().unwrap_or_default())
            .collect()
    }

    #[test]
    fn a_job_for_a_renamed_file_is_not_written_to_its_new_row() {
        let mut store = TempStore::new("rename");
        store.0.queue(&found("/folder/old.txt"), None, 0.0).unwrap();
        let job = store.0.pending(10).unwrap().remove(0);
        // Renamed while it was being read: its row goes, and the new path's row may get the same id.
        store.0.remove_file(job.id).unwrap();
        let renamed = store.0.queue(&found("/folder/new.txt"), None, 0.0).unwrap();
        assert_eq!(renamed, job.id);
        assert!(!store.0.fail_file(&job, "no such file").unwrap());
        assert!(!store.0.finish_file(&job, &[], &serde_json::json!({}), false).unwrap());
        assert_eq!(store.0.pending(10).unwrap()[0].path, PathBuf::from("/folder/new.txt"));
    }

    #[test]
    fn a_file_read_with_old_values_is_not_written() {
        let mut store = TempStore::new("reading");
        let id = store.0.queue(&found("/a/note.txt"), None, 0.0).unwrap();
        let job = store.0.pending(10).unwrap().remove(0);
        // The reading values changed while it was read: it's queued again, to be read the new way.
        let shorter = Found { settings: Settings { passage_chars: 600, ..Settings::BASE }, ..found("/a/note.txt") };
        store.0.queue(&shorter, Some(id), 0.0).unwrap();
        assert!(!store.0.finish_file(&job, &[], &serde_json::json!({}), false).unwrap());
        let again = store.0.pending(10).unwrap().remove(0);
        assert_eq!(again.reading, shorter.reading());
        assert!(store.0.finish_file(&again, &[], &serde_json::json!({}), false).unwrap());
    }

    #[test]
    fn the_queue_goes_cheapest_first_and_says_what_is_heaviest() {
        let store = TempStore::new("cost");
        store.0.queue(&found("/a/podcast.txt"), None, 3600.0).unwrap();
        store.0.queue(&found("/a/memo.txt"), None, 0.5).unwrap();
        store.0.queue(&found("/a/book.txt"), None, 60.0).unwrap();
        let order: Vec<_> = store.0.pending(10).unwrap().into_iter().map(|row| row.path).collect();
        assert_eq!(order, ["/a/memo.txt", "/a/book.txt", "/a/podcast.txt"].map(PathBuf::from));
        let queued = store.0.queued(2).unwrap();
        assert_eq!((queued.files, queued.seconds), (3, 3660.5));
        assert_eq!(
            queued.heaviest.iter().map(|heavy| heavy.path.as_str()).collect::<Vec<_>>(),
            ["/a/podcast.txt", "/a/book.txt"]
        );
    }

    #[test]
    fn a_big_file_is_written_in_parts() {
        let mut store = TempStore::new("parts");
        let id = store.0.queue(&found("/a/book.txt"), None, 0.0).unwrap();
        let job = store.0.pending(1).unwrap().remove(0);
        // What an earlier, unfinished read left goes with the first part.
        store.0.write_part(&job, &[passage("left over")], true).unwrap();
        assert!(store.0.write_part(&job, &[passage("one")], true).unwrap());
        assert!(store.0.write_part(&job, &[passage("two")], false).unwrap());
        assert_eq!(texts(&store.0, id), ["one", "two"]);
        let info = serde_json::json!({"tags": {"title": "Moby"}});
        assert!(store.0.finish_file(&job, &[passage("three")], &info, true).unwrap());
        assert_eq!(texts(&store.0, id), ["one", "two", "three"]);
        // The parts are found by the tags that came at the end.
        let named: i64 = store
            .0
            .conn
            .query_row("SELECT COUNT(*) FROM words WHERE words MATCH 'name:moby'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(named, 3);
        // A file that changed meanwhile isn't written to.
        store.0.queue(&Found { size: 20, ..found("/a/book.txt") }, Some(id), 0.0).unwrap();
        assert!(!store.0.write_part(&job, &[passage("four")], false).unwrap());
    }

    #[test]
    fn an_index_of_another_version_is_made_again() {
        let store = TempStore::new("version");
        store.0.queue(&found("/a/note.txt"), None, 0.0).unwrap();
        store.0.set_meta("schema", "3").unwrap();
        let reopened = Store::open(&store.1).unwrap();
        assert_eq!(reopened.files().unwrap().len(), 0);
        assert_eq!(reopened.meta("schema").unwrap().as_deref(), Some(SCHEMA_VERSION.to_string().as_str()));
    }
}
