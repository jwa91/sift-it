//! The index: one SQLite file. A row per file, a row per segment (a passage, a PDF page, a picture, a video frame,
//! 30 s of sound) with its vector, and a full-text index over every segment's words and its file's name.
//!
//! The indexer is the only writer; searchers read alongside it (WAL), and notice its changes through SQLite's
//! data_version.

use crate::crawl::{Found, Kind};
use crate::embedder::DIM;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 2: segment ids are never reused (AUTOINCREMENT), so a searcher can tell new segments from replaced ones.
const SCHEMA_VERSION: i64 = 2;

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
        }
    }

    pub fn parse(name: &str) -> Option<Segment> {
        [Segment::Passage, Segment::Page, Segment::Picture, Segment::Ocr, Segment::Sound, Segment::Frame]
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
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS files (
                 id INTEGER PRIMARY KEY,
                 path TEXT NOT NULL UNIQUE,
                 kind TEXT NOT NULL,
                 size INTEGER NOT NULL,
                 modified REAL NOT NULL,
                 state TEXT NOT NULL DEFAULT 'pending',   -- pending | done | failed
                 error TEXT,
                 info TEXT,                               -- JSON: pages, duration, width, height
                 indexed_at REAL
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
        let version: Option<i64> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |row| row.get::<_, String>(0))
            .optional()?
            .and_then(|v| v.parse().ok());
        match version {
            None => {
                self.conn.execute("INSERT INTO meta VALUES ('schema', ?1)", [SCHEMA_VERSION.to_string()])?;
            }
            Some(SCHEMA_VERSION) => {}
            Some(1) => self.conn.execute_batch(
                "BEGIN;
                 ALTER TABLE segments RENAME TO segments_v1;
                 CREATE TABLE segments (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                     segment TEXT NOT NULL,
                     loc REAL,
                     loc_end REAL,
                     text TEXT,
                     vector BLOB
                 );
                 INSERT INTO segments SELECT id, file_id, segment, loc, loc_end, text, vector FROM segments_v1;
                 DROP TABLE segments_v1;
                 CREATE INDEX segments_file ON segments(file_id);
                 UPDATE meta SET value = '2' WHERE key = 'schema';
                 COMMIT;",
            )?,
            Some(other) => bail!("{} is index version {other}; this build reads {SCHEMA_VERSION}", self.path.display()),
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
        let mut statement = self.conn.prepare("SELECT id, path, kind, size, modified FROM files")?;
        let rows = statement.query_map([], |row| {
            Ok(FileRow {
                id: row.get(0)?,
                path: PathBuf::from(row.get::<_, String>(1)?),
                kind: Kind::parse(&row.get::<_, String>(2)?).unwrap_or(Kind::Text),
                size: row.get::<_, i64>(3)? as u64,
                modified: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The files at a path or under it.
    pub fn files_under(&self, path: &Path) -> Result<Vec<FileRow>> {
        let exact = path.to_string_lossy().to_string();
        let prefix = format!("{}/", exact.trim_end_matches('/'));
        let mut statement = self
            .conn
            .prepare("SELECT id, path, kind, size, modified FROM files WHERE path = ?1 OR substr(path, 1, ?2) = ?3")?;
        let rows = statement.query_map(params![exact, prefix.chars().count() as i64, prefix], |row| {
            Ok(FileRow {
                id: row.get(0)?,
                path: PathBuf::from(row.get::<_, String>(1)?),
                kind: Kind::parse(&row.get::<_, String>(2)?).unwrap_or(Kind::Text),
                size: row.get::<_, i64>(3)? as u64,
                modified: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn pending_count(&self) -> Result<usize> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM files WHERE state = 'pending'", [], |row| row.get::<_, i64>(0))?
            as usize)
    }

    /// A new or changed file: (re)queued, with what was indexed from it before gone.
    pub fn queue(&self, found: &Found, existing: Option<i64>) -> Result<i64> {
        let path = found.path.to_string_lossy();
        if let Some(id) = existing {
            self.delete_segments(id)?;
            self.conn.execute(
                "UPDATE files SET kind = ?2, size = ?3, modified = ?4, state = 'pending', error = NULL, info = NULL,
                 indexed_at = NULL WHERE id = ?1",
                params![id, found.kind.as_str(), found.size as i64, found.modified],
            )?;
            Ok(id)
        } else {
            self.conn.execute(
                "INSERT INTO files (path, kind, size, modified) VALUES (?1, ?2, ?3, ?4)",
                params![path, found.kind.as_str(), found.size as i64, found.modified],
            )?;
            Ok(self.conn.last_insert_rowid())
        }
    }

    pub fn remove_file(&self, id: i64) -> Result<()> {
        self.delete_segments(id)?;
        self.conn.execute("DELETE FROM files WHERE id = ?1", [id])?;
        Ok(())
    }

    fn delete_segments(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM words WHERE rowid IN (SELECT id FROM segments WHERE file_id = ?1)", [file_id])?;
        self.conn.execute("DELETE FROM segments WHERE file_id = ?1", [file_id])?;
        Ok(())
    }

    /// Files waiting to be indexed, cheapest kinds and newest files first.
    pub fn pending(&self, limit: usize) -> Result<Vec<FileRow>> {
        let mut statement = self.conn.prepare(
            "SELECT id, path, kind, size, modified FROM files WHERE state = 'pending' ORDER BY modified DESC",
        )?;
        let mut rows: Vec<FileRow> = statement
            .query_map([], |row| {
                Ok(FileRow {
                    id: row.get(0)?,
                    path: PathBuf::from(row.get::<_, String>(1)?),
                    kind: Kind::parse(&row.get::<_, String>(2)?).unwrap_or(Kind::Text),
                    size: row.get::<_, i64>(3)? as u64,
                    modified: row.get(4)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        rows.sort_by_key(|row| row.kind.cost_rank());
        rows.truncate(limit);
        Ok(rows)
    }

    /// Writes a file's segments and marks it done, in one transaction. Nothing is written (false) when the file left
    /// the index or changed since it was handed out (`row`): it's been queued again.
    pub fn finish_file(&mut self, row: &FileRow, segments: &[NewSegment], info: &serde_json::Value) -> Result<bool> {
        self.write_file(row, segments, info, None)
    }

    /// A file that couldn't be read: still findable by name, and not tried again until it changes.
    pub fn fail_file(&mut self, row: &FileRow, error: &str) -> Result<bool> {
        self.write_file(row, &[], &serde_json::json!({}), Some(error))
    }

    fn write_file(
        &mut self,
        row: &FileRow,
        segments: &[NewSegment],
        info: &serde_json::Value,
        error: Option<&str>,
    ) -> Result<bool> {
        let tx = self.conn.transaction()?;
        // The same file as handed out: its id, and its path too. A file renamed meanwhile leaves its row, and the new
        // path's row can get the same id (SQLite reuses the highest one) with the same size and time.
        let current: Option<(String, i64, f64)> = tx
            .query_row("SELECT path, size, modified FROM files WHERE id = ?1", [row.id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?;
        match current {
            Some((path, size, modified))
                if Path::new(&path) == row.path
                    && size as u64 == row.size
                    && (modified - row.modified).abs() < 0.001 => {}
            _ => return Ok(false),
        }
        let name = name_words(&row.path);
        {
            tx.execute("DELETE FROM words WHERE rowid IN (SELECT id FROM segments WHERE file_id = ?1)", [row.id])?;
            tx.execute("DELETE FROM segments WHERE file_id = ?1", [row.id])?;
            let mut insert = tx.prepare(
                "INSERT INTO segments (file_id, segment, loc, loc_end, text, vector) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            let mut words = tx.prepare("INSERT INTO words (rowid, name, text) VALUES (?1, ?2, ?3)")?;
            for segment in segments {
                if let Some(vector) = &segment.vector
                    && vector.len() != DIM
                {
                    bail!("vector of {} values", vector.len());
                }
                insert.execute(params![
                    row.id,
                    segment.segment.as_str(),
                    segment.loc,
                    segment.loc_end,
                    segment.text,
                    segment.vector.as_deref().map(vector_to_blob),
                ])?;
                words.execute(params![tx.last_insert_rowid(), name, segment.text.as_deref().unwrap_or("")])?;
            }
            // A file with nothing in it is still findable by its name.
            if segments.is_empty() {
                insert.execute(params![
                    row.id,
                    "passage",
                    None::<f64>,
                    None::<f64>,
                    None::<String>,
                    None::<Vec<u8>>
                ])?;
                words.execute(params![tx.last_insert_rowid(), name, ""])?;
            }
        }
        tx.execute(
            "UPDATE files SET state = ?2, error = ?3, info = ?4, indexed_at = ?5 WHERE id = ?1",
            params![row.id, if error.is_some() { "failed" } else { "done" }, error, info.to_string(), now()],
        )?;
        tx.commit()?;
        Ok(true)
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

    /// Queues the unreadable files again when the readers changed since they failed (see readers::VERSION). Returns
    /// how many.
    pub fn adopt_readers_version(&self, version: u32) -> Result<usize> {
        let stored: Option<u32> = self.meta("readers")?.and_then(|v| v.parse().ok());
        let mut requeued = 0;
        if stored.is_some_and(|stored| stored < version) {
            requeued =
                self.conn.execute("UPDATE files SET state = 'pending', error = NULL WHERE state = 'failed'", [])?;
        }
        if stored != Some(version) {
            self.set_meta("readers", &version.to_string())?;
        }
        Ok(requeued)
    }

    /// Queues text and code files again when they're cut into passages differently since (see readers::PASSAGES).
    /// They stay searchable as they were until they're read again. Returns how many.
    pub fn adopt_passages_version(&self, version: u32) -> Result<usize> {
        let stored: Option<u32> = self.meta("passages")?.and_then(|v| v.parse().ok());
        let mut requeued = 0;
        if stored.is_none_or(|stored| stored < version) {
            requeued = self
                .conn
                .execute("UPDATE files SET state = 'pending' WHERE state = 'done' AND kind IN ('text', 'code')", [])?;
            self.set_meta("passages", &version.to_string())?;
        }
        Ok(requeued)
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
             UPDATE files SET state = 'pending', error = NULL, info = NULL, indexed_at = NULL;",
        )?;
        Ok(())
    }
}

pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        Found { path: PathBuf::from(path), kind: Kind::Text, size: 10, modified: 100.0 }
    }

    #[test]
    fn a_job_for_a_renamed_file_is_not_written_to_its_new_row() {
        let mut store = TempStore::new("rename");
        store.0.queue(&found("/folder/old.txt"), None).unwrap();
        let job = store.0.pending(10).unwrap().remove(0);
        // Renamed while it was being read: its row goes, and the new path's row may get the same id.
        store.0.remove_file(job.id).unwrap();
        let renamed = store.0.queue(&found("/folder/new.txt"), None).unwrap();
        assert_eq!(renamed, job.id);
        assert!(!store.0.fail_file(&job, "no such file").unwrap());
        assert!(!store.0.finish_file(&job, &[], &serde_json::json!({})).unwrap());
        assert_eq!(store.0.pending(10).unwrap()[0].path, PathBuf::from("/folder/new.txt"));
    }

    #[test]
    fn text_and_code_are_read_again_once_when_passages_change() {
        let mut store = TempStore::new("passages");
        for (path, kind) in [("/a/note.txt", Kind::Text), ("/a/main.rs", Kind::Code), ("/a/photo.jpg", Kind::Image)] {
            store.0.queue(&Found { kind, ..found(path) }, None).unwrap();
        }
        for job in store.0.pending(10).unwrap() {
            assert!(store.0.finish_file(&job, &[], &serde_json::json!({})).unwrap());
        }
        assert_eq!(store.0.adopt_passages_version(2).unwrap(), 2);
        let mut again: Vec<PathBuf> = store.0.pending(10).unwrap().into_iter().map(|row| row.path).collect();
        again.sort();
        assert_eq!(again, vec![PathBuf::from("/a/main.rs"), PathBuf::from("/a/note.txt")]);
        assert_eq!(store.0.adopt_passages_version(2).unwrap(), 0);
    }
}
