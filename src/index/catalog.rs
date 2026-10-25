//! The catalog: one SQLite file. A row per file, a row per segment with where its vector is (vectors.rs) or which
//! round it waits for, a full-text index over every segment's words and its file's name and tags, and the queues: a
//! row per file and round of indexing it needs (`work`), with how much there is, how much is done, and where the next
//! item goes on.

use super::vectors;
use super::{Done, DueText, FileRow, Heavy, NewFile, NewSegment, Reader, RoundTotals, SegmentRow, Work, Writer};
use anyhow::{Context, Result, bail};
use embedder::DIM;
use protocol::{Kind, Round, Segment, Stats};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// An index of another version is dropped and made again.
const SCHEMA_VERSION: i64 = 7;

/// The segments whose vectors the text round makes.
const TEXT_SEGMENTS: &str = "('passage', 'page', 'ocr', 'speech', 'exchange')";

/// The columns a FileRow is made of (file_row), of `files f`.
/// A file's sound round waits while its speech round has work: the sound round listens closely only where little is
/// said (segment/sound.rs), so it needs to know where speech is.
const SOUND_WAITS: &str = "(w.round <> 'sound' OR NOT EXISTS (SELECT 1 FROM work s
    WHERE s.file_id = w.file_id AND s.round = 'speech' AND s.state = 'pending'))";

const FILE_COLUMNS: &str = "f.id, f.path, f.kind, f.size, f.modified, f.reading, f.speech";

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

fn round_of(name: &str) -> Round {
    Round::parse(name).unwrap_or(Round::Words)
}

/// Words for the keyword index from a file: its name (name_words), and the tags read from it (a recording's title,
/// artist and album: see av::tags), which `info` holds.
fn file_words(path: &Path, info: &serde_json::Value) -> String {
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
fn name_words(path: &Path) -> String {
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

impl Reader {
    /// A reader of the index at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if !path.exists() {
            // A searcher may start before the first index: make an empty one to read.
            drop(Writer::open(path)?);
        }
        let conn =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
                .with_context(|| format!("can't open {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self { conn, path: path.to_path_buf() })
    }

    /// The folder the vectors are in.
    pub(super) fn vectors_folder(&self) -> PathBuf {
        vectors::folder(&self.path)
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| row.get(0)).optional()?)
    }

    /// Changes when another connection commits (SQLite's data_version): a reader's cue to refresh.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self.conn.query_row("PRAGMA data_version", [], |row| row.get(0))?)
    }

    pub fn files(&self) -> Result<Vec<FileRow>> {
        let mut statement = self.conn.prepare(&format!("SELECT {FILE_COLUMNS} FROM files f"))?;
        let rows = statement.query_map([], file_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The files at a path or under it.
    pub fn files_under(&self, path: &Path) -> Result<Vec<FileRow>> {
        let exact = path.to_string_lossy().to_string();
        let prefix = format!("{}/", exact.trim_end_matches('/'));
        let mut statement = self
            .conn
            .prepare(&format!("SELECT {FILE_COLUMNS} FROM files f WHERE f.path = ?1 OR substr(f.path, 1, ?2) = ?3"))?;
        let rows = statement.query_map(params![exact, prefix.chars().count() as i64, prefix], file_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The files whose work waits for their download.
    pub fn waiting_for_download(&self) -> Result<HashSet<i64>> {
        let mut statement = self.conn.prepare_cached("SELECT DISTINCT file_id FROM work WHERE state = 'cloud'")?;
        let ids = statement.query_map([], |row| row.get(0))?;
        Ok(ids.collect::<std::result::Result<_, _>>()?)
    }

    /// The next of a round's work, at most `limit`: new files' first, then the cheapest, then the newest. With
    /// `new_only`, new files' alone. A file's sound waits for its speech (SOUND_WAITS).
    pub fn next(&self, round: Round, new_only: bool, limit: usize) -> Result<Vec<Work>> {
        let mut statement = self.conn.prepare_cached(&format!(
            "SELECT {FILE_COLUMNS}, w.amount, w.done, w.resume, w.fast FROM work w JOIN files f ON f.id = w.file_id
             WHERE w.round = ?1 AND w.state = 'pending' AND (?2 = 0 OR w.fast) AND {SOUND_WAITS}
             ORDER BY w.fast DESC, w.cost, f.modified DESC LIMIT ?3"
        ))?;
        let rows = statement.query_map(params![round.as_str(), new_only, limit as i64], |row| {
            Ok(Work {
                file: file_row(row)?,
                round,
                amount: row.get(7)?,
                done: row.get(8)?,
                resume: row.get::<_, Option<String>>(9)?.and_then(|resume| serde_json::from_str(&resume).ok()),
                new: row.get(10)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Which rounds have work that can go on, and whether some of it is new files' and some isn't: (round, new, old).
    pub fn queues(&self) -> Result<Vec<(Round, bool, bool)>> {
        let mut statement = self.conn.prepare_cached(&format!(
            "SELECT EXISTS (SELECT 1 FROM work w WHERE w.round = ?1 AND w.state = 'pending' AND w.fast = ?2
             AND {SOUND_WAITS})"
        ))?;
        let mut queues = Vec::new();
        for round in Round::ALL {
            let mut any = |fast: bool| statement.query_row(params![round.as_str(), fast], |row| row.get::<_, bool>(0));
            let (new, old) = (any(true)?, any(false)?);
            if new || old {
                queues.push((round, new, old));
            }
        }
        Ok(queues)
    }

    /// How far each round is, for those with any work in this index. Work that waits for a file's download isn't
    /// counted: a round is done with what it can do.
    pub fn rounds(&self) -> Result<Vec<RoundTotals>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT round, TOTAL(done), TOTAL(amount), COUNT(*) FILTER (WHERE state = 'pending'),
                 COUNT(*) FILTER (WHERE state = 'pending' AND fast), COUNT(*) FILTER (WHERE state = 'failed')
             FROM work WHERE state <> 'cloud' GROUP BY round",
        )?;
        let mut totals: Vec<RoundTotals> = statement
            .query_map([], |row| {
                Ok(RoundTotals {
                    round: round_of(&row.get::<_, String>(0)?),
                    done: row.get(1)?,
                    amount: row.get(2)?,
                    files: row.get::<_, i64>(3)? as usize,
                    new: row.get::<_, i64>(4)? as usize,
                    failed: row.get::<_, i64>(5)? as usize,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        totals.sort_by_key(|totals| totals.round);
        Ok(totals)
    }

    /// The passages waiting for their vectors, at most `limit`, a file's together: new files' first, then those of
    /// the files with the fewest. With `new_only`, new files' alone.
    pub fn due_text(&self, new_only: bool, limit: usize) -> Result<Vec<DueText>> {
        let mut statement = self.conn.prepare_cached(&format!(
            "SELECT {FILE_COLUMNS}, s.id, s.segment, s.text, s.context, json_extract(f.info, '$.tags.title')
             FROM work w JOIN files f ON f.id = w.file_id JOIN segments s ON s.file_id = w.file_id
             WHERE w.round = 'text' AND w.state = 'pending' AND (?1 = 0 OR w.fast) AND s.due = 'text'
             ORDER BY w.fast DESC, w.cost, w.file_id, s.id LIMIT ?2"
        ))?;
        let rows = statement.query_map(params![new_only, limit as i64], |row| {
            Ok(DueText {
                file: file_row(row)?,
                id: row.get(7)?,
                segment: Segment::parse(&row.get::<_, String>(8)?).unwrap_or(Segment::Passage),
                text: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
                context: row.get(10)?,
                title: row.get(11)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The pages of a document whose pictures wait for the pictures round: scanned pages with little text on them.
    /// What was written down of what's said in a file: each passage's stretch, (start, end) in seconds, and its
    /// words, in order.
    pub fn spoken(&self, file_id: i64) -> Result<Vec<(f64, f64, usize)>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT loc, loc_end, text FROM segments WHERE file_id = ?1 AND segment = 'speech' AND loc_end IS NOT NULL
             ORDER BY loc",
        )?;
        let passages = statement.query_map([file_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, String>(2)?.split_whitespace().count()))
        })?;
        Ok(passages.collect::<std::result::Result<_, _>>()?)
    }

    pub fn due_pictures(&self, file_id: i64) -> Result<Vec<usize>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT loc FROM segments WHERE file_id = ?1 AND segment = 'picture' AND due = 'pictures' ORDER BY loc",
        )?;
        let pages = statement.query_map([file_id], |row| row.get::<_, Option<f64>>(0))?;
        Ok(pages.filter_map(|page| page.ok().flatten()).map(|page| page as usize).collect())
    }

    /// The files with the most sound left in the rounds that listen to it, most first: what `ignore` or a lower depth
    /// could leave out.
    pub fn heaviest(&self, limit: usize) -> Result<Vec<Heavy>> {
        let mut statement = self.conn.prepare(
            "SELECT f.path, w.round, w.amount - w.done FROM work w JOIN files f ON f.id = w.file_id
             WHERE w.state = 'pending' AND w.round IN ('sound', 'speech') ORDER BY w.amount - w.done DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit as i64], |row| {
            Ok(Heavy { path: row.get(0)?, round: round_of(&row.get::<_, String>(1)?), left: row.get(2)? })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// A segment's whole text.
    pub fn segment_text(&self, segment_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .prepare_cached("SELECT text FROM segments WHERE id = ?1")?
            .query_row([segment_id], |row| row.get(0))
            .optional()?
            .flatten())
    }

    /// A segment's text, with `around` of its file's text segments before it and after it, in order (by their place,
    /// as file_text), each a paragraph of its own.
    pub fn segment_text_around(&self, segment_id: i64, around: usize) -> Result<Option<String>> {
        let Some((file, loc, text)) = self
            .conn
            .query_row("SELECT file_id, COALESCE(loc, -1), text FROM segments WHERE id = ?1", [segment_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?, row.get::<_, Option<String>>(2)?))
            })
            .optional()?
        else {
            return Ok(None);
        };
        let neighbours = |side: &str, order: &str| -> Result<Vec<String>> {
            let mut statement = self.conn.prepare_cached(&format!(
                "SELECT text FROM segments WHERE file_id = ?1 AND text IS NOT NULL AND text != '' AND id != ?3
                     AND (COALESCE(loc, -1), id) {side} (?2, ?3)
                 ORDER BY COALESCE(loc, -1) {order}, id {order} LIMIT ?4"
            ))?;
            let rows = statement.query_map(params![file, loc, segment_id, around as i64], |row| row.get(0))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        };
        let mut parts = neighbours("<", "DESC")?;
        parts.reverse();
        parts.extend(text.filter(|text| !text.is_empty()));
        parts.extend(neighbours(">", "ASC")?);
        Ok((!parts.is_empty()).then(|| parts.join("\n\n")))
    }

    /// A file's text, for a preview: its text segments in order, until there are some 8,000 bytes (more than a
    /// preview shows).
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

    /// What's in the index. A file is pending while a round has work for it, failed when one couldn't do it, and
    /// cloud while its work waits for its download.
    pub fn stats(&self) -> Result<Stats> {
        let mut stats = Stats::default();
        let mut statement = self.conn.prepare(
            "SELECT f.kind,
                 CASE WHEN EXISTS (SELECT 1 FROM work WHERE file_id = f.id AND state = 'failed') THEN 'failed'
                      WHEN EXISTS (SELECT 1 FROM work WHERE file_id = f.id AND state = 'cloud') THEN 'cloud'
                      WHEN EXISTS (SELECT 1 FROM work WHERE file_id = f.id AND state = 'pending') THEN 'pending'
                      ELSE 'done' END,
                 COUNT(*), SUM(f.size)
             FROM files f GROUP BY 1, 2",
        )?;
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
                "cloud" => stats.cloud += count,
                _ => stats.failed += count,
            }
            *stats.by_kind.entry(kind).or_default() += count;
        }
        stats.segments = self.conn.query_row("SELECT COUNT(*) FROM segments", [], |row| row.get(0))?;
        let mut statement = self.conn.prepare(
            "SELECT f.kind, COUNT(*) FROM segments s JOIN files f ON f.id = s.file_id WHERE s.row IS NOT NULL
             GROUP BY f.kind",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let count: i64 = row.get(1)?;
            stats.vectors += count;
            stats.vectors_by_kind.insert(row.get(0)?, count);
        }
        Ok(stats)
    }

    /// Each file's kind, by its id.
    pub fn file_kinds(&self) -> Result<HashMap<i64, Kind>> {
        let mut statement = self.conn.prepare("SELECT id, kind FROM files")?;
        Ok(statement
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?
            .filter_map(|row| row.ok())
            .filter_map(|(id, kind)| Kind::parse(&kind).map(|kind| (id, kind)))
            .collect())
    }

    /// A segment, with its file.
    pub fn segment(&self, id: i64) -> Result<Option<SegmentRow>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT f.path, f.kind, f.modified, f.size, f.info, s.segment, s.loc, s.loc_end, s.text
             FROM segments s JOIN files f ON f.id = s.file_id WHERE s.id = ?1",
        )?;
        Ok(statement
            .query_row([id], |row| {
                Ok(SegmentRow {
                    path: row.get(0)?,
                    kind: Kind::parse(&row.get::<_, String>(1)?).unwrap_or(Kind::Plaintext),
                    modified: row.get(2)?,
                    size: row.get::<_, i64>(3)? as u64,
                    info: row
                        .get::<_, Option<String>>(4)?
                        .and_then(|info| serde_json::from_str(&info).ok())
                        .unwrap_or(serde_json::Value::Null),
                    segment: Segment::parse(&row.get::<_, String>(5)?).unwrap_or(Segment::Passage),
                    loc: row.get(6)?,
                    loc_end: row.get(7)?,
                    text: row.get(8)?,
                })
            })
            .optional()?)
    }

    /// The segments with all of `words`, best first (FTS5's bm25), each with its file, for as long as `take` takes
    /// more. With `prefix_last`, the last word matches as a prefix.
    pub fn word_matches(&self, words: &[String], prefix_last: bool, take: impl FnMut(i64, i64) -> bool) -> Result<()> {
        let query = words
            .iter()
            .enumerate()
            .map(|(i, word)| {
                let quoted = format!("\"{}\"", word.replace('"', "\"\""));
                if prefix_last && i == words.len() - 1 { format!("{quoted}*") } else { quoted }
            })
            .collect::<Vec<_>>()
            .join(" ");
        self.text_matches(&query, take)
    }

    /// The segments with every phrase, its words together and in order (FTS5 phrase queries), as word_matches.
    pub fn phrase_matches(&self, phrases: &[Vec<String>], take: impl FnMut(i64, i64) -> bool) -> Result<()> {
        let query = phrases
            .iter()
            .map(|words| format!("\"{}\"", words.join(" ").replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" ");
        self.text_matches(&query, take)
    }

    fn text_matches(&self, query: &str, mut take: impl FnMut(i64, i64) -> bool) -> Result<()> {
        let mut statement = self.conn.prepare_cached(
            "SELECT words.rowid, segments.file_id FROM words JOIN segments ON segments.id = words.rowid
             WHERE words MATCH ?1 ORDER BY words.rank",
        )?;
        let mut rows = statement.query([query])?;
        while let Some(row) = rows.next()? {
            if !take(row.get(0)?, row.get(1)?) {
                break;
            }
        }
        Ok(())
    }

    /// The first segment of each of the files of these kinds changed last, newest first.
    pub fn recent(&self, kinds: &[Kind], limit: usize) -> Result<Vec<i64>> {
        // The kinds go into the query itself (kind names are fixed words, safe to inline): filtering after the limit
        // could leave nothing.
        let kinds: Vec<String> = kinds.iter().map(|kind| format!("'{}'", kind.as_str())).collect();
        let mut statement = self.conn.prepare(&format!(
            "SELECT (SELECT MIN(id) FROM segments WHERE file_id = f.id) FROM files f
             WHERE f.kind IN ({}) AND EXISTS (SELECT 1 FROM work WHERE file_id = f.id AND round = 'words' AND state = 'done')
             ORDER BY f.modified DESC LIMIT ?1",
            kinds.join(", ")
        ))?;
        let segments: Vec<Option<i64>> =
            statement.query_map([limit as i64], |row| row.get(0))?.collect::<std::result::Result<_, _>>()?;
        Ok(segments.into_iter().flatten().collect())
    }

    /// A round's state for a file: pending, done or failed (or none, when the file doesn't pass through it).
    #[cfg(test)]
    pub fn work_state(&self, path: &Path, round: Round) -> String {
        let path = path.to_string_lossy();
        self.conn
            .query_row(
                "SELECT w.state FROM work w JOIN files f ON f.id = w.file_id WHERE f.path = ?1 AND w.round = ?2",
                params![path, round.as_str()],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
            .unwrap_or_else(|| "none".into())
    }
}

impl Writer {
    /// The writer of the index at `path`, made when it isn't there. An index of another version is dropped and made
    /// again.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path).with_context(|| format!("can't open {}", path.display()))?;
        let writer = Self { reader: Reader { conn, path: path.to_path_buf() } };
        writer.setup()?;
        Ok(writer)
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
                "DROP TABLE IF EXISTS words; DROP TABLE IF EXISTS segments; DROP TABLE IF EXISTS work;
                 DROP TABLE IF EXISTS files; DROP TABLE IF EXISTS partitions; DELETE FROM meta; VACUUM;",
            )?;
            let folder = self.vectors_folder();
            if folder.exists() {
                std::fs::remove_dir_all(&folder).with_context(|| format!("can't remove {}", folder.display()))?;
            }
        }
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                 id INTEGER PRIMARY KEY,
                 path TEXT NOT NULL UNIQUE,
                 kind TEXT NOT NULL,
                 size INTEGER NOT NULL,
                 modified REAL NOT NULL,
                 reading TEXT NOT NULL,                   -- how it's read (ingest::fingerprint)
                 speech INTEGER NOT NULL,                 -- what's said in it is to be written down
                 info TEXT                                -- JSON: what its rounds found: pages, duration, size, tags
             );
             CREATE TABLE IF NOT EXISTS work (
                 file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                 round TEXT NOT NULL,                     -- words | text | ocr | pictures | sound | speech
                 state TEXT NOT NULL DEFAULT 'pending',   -- pending | done | failed | cloud (waits for a download)
                 cost REAL NOT NULL,                      -- what orders the round's queue: cheapest first
                 amount REAL NOT NULL,                    -- how much there is, in the round's unit
                 done REAL NOT NULL DEFAULT 0,            -- how much of it is done
                 fast INTEGER NOT NULL DEFAULT 0,         -- a new file's: it goes first
                 resume TEXT,                             -- JSON: where its next item goes on
                 error TEXT,
                 failed_at REAL,                          -- when it failed in a way that may pass: it's tried again
                 PRIMARY KEY (file_id, round)
             ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS work_queue ON work(round, fast DESC, cost) WHERE state = 'pending';
             CREATE INDEX IF NOT EXISTS work_cloud ON work(file_id) WHERE state = 'cloud';
             CREATE TABLE IF NOT EXISTS segments (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                 segment TEXT NOT NULL,
                 loc REAL,
                 loc_end REAL,
                 text TEXT,
                 context TEXT,                            -- what a passage is embedded with besides its text
                 due TEXT,                                -- the round its vector comes from, while it waits for one
                 partition INTEGER,                       -- where its vector is (vectors.rs): the partition,
                 row INTEGER                              -- and the row in its files
             );
             CREATE INDEX IF NOT EXISTS segments_file ON segments(file_id);
             CREATE INDEX IF NOT EXISTS segments_row ON segments(partition, row) WHERE row IS NOT NULL;
             CREATE INDEX IF NOT EXISTS segments_due ON segments(file_id) WHERE due IS NOT NULL;
             CREATE VIRTUAL TABLE IF NOT EXISTS words USING fts5(
                 name, text, content = '', contentless_delete = 1,
                 tokenize = 'unicode61 remove_diacritics 2', prefix = '2 3'
             );",
        )?;
        self.conn.execute_batch(vectors::SCHEMA)?;
        if version != Some(SCHEMA_VERSION) {
            self.set_meta("schema", &SCHEMA_VERSION.to_string())?;
        }
        Ok(())
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO meta VALUES (?1, ?2)", [key, value])?;
        Ok(())
    }

    /// A new or changed file: (re)queued for its words, with what was indexed from it before gone. `cost` is about how
    /// long reading its words takes (discover::cost): the words round goes cheapest first.
    pub fn queue(&self, file: &NewFile, existing: Option<i64>, cost: f64) -> Result<i64> {
        let path = file.path.to_string_lossy();
        let (kind, size, reading, speech) = (file.kind.as_str(), file.size as i64, &file.reading, file.speech);
        let id = if let Some(id) = existing {
            delete_segments(&self.conn, id)?;
            self.conn.execute("DELETE FROM work WHERE file_id = ?1", [id])?;
            self.conn.execute(
                "UPDATE files SET kind = ?2, size = ?3, modified = ?4, reading = ?5, speech = ?6, info = NULL
                 WHERE id = ?1",
                params![id, kind, size, file.modified, reading, speech],
            )?;
            id
        } else {
            self.conn.execute(
                "INSERT INTO files (path, kind, size, modified, reading, speech) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![path, kind, size, file.modified, reading, speech],
            )?;
            self.conn.last_insert_rowid()
        };
        self.conn.execute(
            "INSERT INTO work (file_id, round, cost, amount, fast) VALUES (?1, 'words', ?2, 1, ?3)",
            params![id, cost, file.new],
        )?;
        Ok(id)
    }

    /// A file that grew, whose words can be read on from where its last read ended (a conversation's log, as its
    /// session goes on): they're queued again from there, and what was read before stays.
    pub fn queue_grown(&self, id: i64, file: &NewFile, cost: f64) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET size = ?2, modified = ?3 WHERE id = ?1",
            params![id, file.size as i64, file.modified],
        )?;
        self.conn.execute(
            "UPDATE work SET state = 'pending', done = 0, cost = ?2, fast = ?3, error = NULL, failed_at = NULL
             WHERE file_id = ?1 AND round = 'words'",
            params![id, cost, file.new],
        )?;
        Ok(())
    }

    /// Where a file's words go on from, when its last read said so (a conversation's log).
    pub fn words_resume(&self, id: i64) -> Result<Option<serde_json::Value>> {
        let resume: Option<Option<String>> = self
            .conn
            .query_row("SELECT resume FROM work WHERE file_id = ?1 AND round = 'words'", [id], |row| row.get(0))
            .optional()?;
        Ok(resume.flatten().and_then(|resume| serde_json::from_str(&resume).ok()))
    }

    /// Whether what's said in a file is to be written down, when only that changed: nothing is read again. What was
    /// written down goes either way (it's to do, or not wanted). Before its words are read, its words round decides.
    pub fn set_speech(&self, id: i64, speech: bool) -> Result<()> {
        delete_kind(&self.conn, id, Segment::Speech)?;
        self.conn.execute("DELETE FROM work WHERE file_id = ?1 AND round = 'speech'", [id])?;
        self.conn.execute(
            "UPDATE files SET speech = ?2, info = json_remove(info, '$.speech') WHERE id = ?1",
            params![id, speech],
        )?;
        if speech {
            self.conn.execute(
                "INSERT INTO work (file_id, round, cost, amount)
                 SELECT f.id, 'speech', json_extract(f.info, '$.duration'), json_extract(f.info, '$.duration')
                 FROM files f JOIN work w ON w.file_id = f.id AND w.round = 'words' AND w.state = 'done'
                 WHERE f.id = ?1 AND json_extract(f.info, '$.duration') > 0",
                [id],
            )?;
        }
        recount_text(&self.conn, id, false)
    }

    /// A file only in iCloud: the work that would read it waits for its download (the text round's has what it needs,
    /// and goes on). With nothing read of it, it's found by its name meanwhile.
    pub fn wait_for_download(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE work SET state = 'cloud' WHERE file_id = ?1 AND state = 'pending' AND round <> 'text'",
            [id],
        )?;
        let unread: Option<String> = self
            .conn
            .query_row(
                "SELECT path FROM files WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM segments WHERE file_id = ?1)",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(path) = unread {
            placeholder(&self.conn, id, &file_words(Path::new(&path), &stored_info(&self.conn, id)?))?;
        }
        Ok(())
    }

    /// A file that's downloaded again: the work that waited for it goes on (first in every round, when `new`: FSEvents
    /// said so).
    pub fn downloaded(&self, id: i64, new: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE work SET state = 'pending', fast = max(fast, ?2) WHERE file_id = ?1 AND state = 'cloud'",
            params![id, new],
        )?;
        Ok(())
    }

    pub fn remove_file(&self, id: i64) -> Result<()> {
        delete_segments(&self.conn, id)?;
        self.conn.execute("DELETE FROM files WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Writes a part of a big file's words while the rest is still being read, so memory doesn't grow with the file,
    /// and a search finds that part meanwhile. The `first` part clears what an earlier read left (delete_read).
    /// Nothing is written (false) when the file left the index or changed since it was handed out.
    pub fn write_words_part(&mut self, work: &Work, segments: &[NewSegment], first: bool) -> Result<bool> {
        let folder = self.vectors_folder();
        let tx = self.reader.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !as_handed_out(&tx, &work.file)? {
            return Ok(false);
        }
        let fresh = if first { clear_words(&tx, work, segments)? } else { Cow::Borrowed(segments) };
        insert_segments(&tx, &folder, &work.file, &name_words(&work.file.path), &fresh)?;
        tx.commit()?;
        Ok(true)
    }

    /// Writes what the words round read of a file (its last segments, or all of them), what it learned (`info`), and
    /// the rounds it passes through next, each with how much it has for it, in one transaction, with where its next
    /// read goes on (`resume`, for a file that grows). `parts`: some of its segments were written already
    /// (write_words_part), and stay. A file with nothing in it is still findable by its name. Nothing is written
    /// (false) when the file left the index or changed since it was handed out: it's been queued again.
    pub fn write_words(
        &mut self,
        work: &Work,
        segments: &[NewSegment],
        info: &serde_json::Value,
        rounds: &[(Round, f64)],
        parts: bool,
        resume: Option<&serde_json::Value>,
    ) -> Result<bool> {
        let folder = self.vectors_folder();
        let tx = self.reader.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let file = &work.file;
        if !as_handed_out(&tx, file)? {
            return Ok(false);
        }
        let fresh = if parts { Cow::Borrowed(segments) } else { clear_words(&tx, work, segments)? };
        let named = file_words(&file.path, &stored_info(&tx, file.id)?);
        merge_info(&tx, file.id, info)?;
        let name = file_words(&file.path, &stored_info(&tx, file.id)?);
        if name != named && read_again_from(work).is_some() {
            // What a read that goes on kept is found by the file's name and tags as they are now (a new title).
            tx.execute("DELETE FROM words WHERE rowid IN (SELECT id FROM segments WHERE file_id = ?1)", [file.id])?;
            tx.execute(
                "INSERT INTO words (rowid, name, text) SELECT id, ?2, COALESCE(text, '') FROM segments WHERE file_id = ?1",
                params![file.id, name],
            )?;
        }
        insert_segments(&tx, &folder, file, &name, &fresh)?;
        if segments.is_empty() && !parts && read_again_from(work).is_none() {
            placeholder(&tx, file.id, &name)?;
        }
        for &(round, amount) in rounds {
            // What's said in it may be written down already (it's read again for another model): that stays.
            tx.execute(
                "INSERT INTO work (file_id, round, cost, amount, fast) VALUES (?1, ?2, ?3, ?3, ?4)
                 ON CONFLICT DO NOTHING",
                params![file.id, round.as_str(), amount, work.new],
            )?;
        }
        finish(&tx, work, &Done { done: 1.0, finished: true, resume: resume.cloned(), ..Default::default() })?;
        recount_text(&tx, file.id, work.new)?;
        tx.commit()?;
        Ok(true)
    }

    /// Writes an item of a file's OCR, pictures, sound or speech: the segments it found, what it learned, and how far
    /// the file's work in that round is now (`done`), in one transaction. The first item clears what an earlier try
    /// left of the round. Nothing is written (false) when the file left the index or changed since it was handed out.
    pub fn write_item(&mut self, work: &Work, segments: &[NewSegment], done: &Done) -> Result<bool> {
        let folder = self.vectors_folder();
        let tx = self.reader.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let file = &work.file;
        if !as_handed_out(&tx, file)? {
            return Ok(false);
        }
        if work.done == 0.0 {
            clear_round(&tx, file, work.round)?;
        }
        merge_info(&tx, file.id, &done.info)?;
        let name = file_words(&file.path, &stored_info(&tx, file.id)?);
        let pictures = insert_segments(&tx, &folder, file, &name, segments)?;
        if pictures > 0 {
            // Scanned pages with little text on them: what they show, then.
            add_work(&tx, file.id, Round::Pictures, pictures as f64, work.new)?;
        }
        if work.round == Round::Speech {
            let spoken = count_kind(&tx, file.id, Segment::Speech)?;
            merge_info(&tx, file.id, &serde_json::json!({ "speech": spoken }))?;
        }
        finish(&tx, work, done)?;
        if work.round == Round::Pictures {
            // A document's scanned pages OCR found little text on while this item was out wait still.
            tx.execute(
                "UPDATE work SET state = 'pending',
                     amount = done + (SELECT COUNT(*) FROM segments WHERE file_id = ?1 AND due = 'pictures')
                 WHERE file_id = ?1 AND round = 'pictures'
                     AND EXISTS (SELECT 1 FROM segments WHERE file_id = ?1 AND due = 'pictures')",
                [file.id],
            )?;
        }
        recount_text(&tx, file.id, work.new)?;
        tx.commit()?;
        Ok(true)
    }

    /// Writes the vectors the text round made for passages that waited for one, and how far their files' text is, in
    /// one transaction. Passages of files that changed or left since they were handed out are passed over.
    pub fn write_text(&mut self, embedded: &[(&DueText, Vec<f32>)]) -> Result<()> {
        let folder = self.vectors_folder();
        let tx = self.reader.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut files: HashMap<i64, bool> = HashMap::new();
        let mut rows: HashMap<&'static str, Vec<(i64, &[f32])>> = HashMap::new();
        for (due, vector) in embedded {
            if vector.len() != DIM {
                bail!("vector of {} values", vector.len());
            }
            let current = match files.get(&due.file.id) {
                Some(&current) => current,
                None => *files.entry(due.file.id).or_insert(as_handed_out(&tx, &due.file)?),
            };
            let waits = tx
                .query_row("SELECT due = 'text' FROM segments WHERE id = ?1", [due.id], |row| row.get::<_, bool>(0))
                .optional()?
                .unwrap_or(false);
            if current && waits {
                rows.entry(vectors::partition_of(due.file.kind)).or_default().push((due.id, vector));
            }
        }
        for (partition, rows) in rows {
            let vectors: Vec<&[f32]> = rows.iter().map(|(_, vector)| *vector).collect();
            let (partition, first) = vectors::append(&tx, &folder, partition, &vectors)?;
            for (at, (id, _)) in rows.iter().enumerate() {
                tx.execute(
                    "UPDATE segments SET partition = ?2, row = ?3, due = NULL, context = NULL WHERE id = ?1",
                    params![id, partition, first + at as i64],
                )?;
            }
        }
        for (file, current) in files {
            if current {
                recount_text(&tx, file, false)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// A round that couldn't do its work for a file. When its words can't be read, the file is still findable by its
    /// name. `may_pass`: the failure may well pass (reading got stuck, a vector couldn't be made), so it's tried again
    /// later (requeue_passing); else not until the file changes. What its items wrote before stays. Nothing is
    /// written (false) when the file left the index or changed since it was handed out.
    pub fn fail(&mut self, work: &Work, error: &str, may_pass: bool) -> Result<bool> {
        let tx = self.reader.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let file = &work.file;
        if !as_handed_out(&tx, file)? {
            return Ok(false);
        }
        if work.round == Round::Words {
            delete_read(&tx, file.id)?;
            placeholder(&tx, file.id, &file_words(&file.path, &stored_info(&tx, file.id)?))?;
            recount_text(&tx, file.id, false)?;
            // What it read before is gone: its next read starts from the top.
            tx.execute("UPDATE work SET resume = NULL WHERE file_id = ?1 AND round = 'words'", [file.id])?;
        }
        tx.execute(
            "UPDATE work SET state = 'failed', error = ?3, failed_at = ?4 WHERE file_id = ?1 AND round = ?2",
            params![file.id, work.round.as_str(), error, may_pass.then(now)],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Makes sure what's written down of recordings comes from the speech model that writes it now (`source`, see
    /// speech::SOURCE). When it doesn't, all of it is written down again. True then.
    pub fn adopt_speech_source(&self, source: &str) -> Result<bool> {
        self.adopt("speech_source", source, || {
            self.conn.execute_batch(
                "DELETE FROM words WHERE rowid IN (SELECT id FROM segments WHERE segment = 'speech');
                 DELETE FROM segments WHERE segment = 'speech';
                 UPDATE work SET state = 'pending', done = 0, resume = NULL, error = NULL, failed_at = NULL
                 WHERE round = 'speech';
                 UPDATE files SET info = json_remove(info, '$.speech');",
            )?;
            let texts: Vec<i64> = self
                .conn
                .prepare("SELECT file_id FROM work WHERE round = 'text'")?
                .query_map([], |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            texts.into_iter().try_for_each(|file| recount_text(&self.conn, file, false))
        })
    }

    /// Makes sure the index's vectors come from the pipeline that makes new ones (`source`, see
    /// embedder::VECTOR_SOURCE). When they don't, every file is read again (requeue_vectors); true then.
    pub fn adopt_vector_source(&self, source: &str) -> Result<bool> {
        self.adopt("vector_source", source, || self.requeue_vectors())
    }

    /// Records the `source` under `key`, after `reset` when another one was recorded. True then.
    fn adopt(&self, key: &str, source: &str, reset: impl FnOnce() -> Result<()>) -> Result<bool> {
        let stored = self.meta(key)?;
        let stale = stored.as_deref().is_some_and(|stored| stored != source);
        if stale {
            reset()?;
        }
        if stored.as_deref() != Some(source) {
            self.set_meta(key, source)?;
        }
        Ok(stale)
    }

    /// Tries again the rounds that failed in a way that may pass (reading got stuck, a vector couldn't be made), once
    /// they've had `hours` to settle. Returns how many.
    pub fn requeue_passing(&self, hours: f64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE work SET state = 'pending', error = NULL, failed_at = NULL
             WHERE state = 'failed' AND failed_at <= ?1",
            [now() - hours * 3600.0],
        )?)
    }

    /// Drops every vector and reads every file again (after a change of model or pipeline). What's written down of
    /// recordings stays, as it was cut into passages: they lose their vectors, and wait for the text round's. The
    /// vector files go with the next tidy_vectors.
    pub fn requeue_vectors(&self) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute_batch(
            "DELETE FROM words WHERE rowid IN (SELECT id FROM segments WHERE segment != 'speech');
             DELETE FROM segments WHERE segment != 'speech';
             UPDATE segments SET partition = NULL, row = NULL, due = 'text';
             DELETE FROM work WHERE round NOT IN ('words', 'speech');
             UPDATE work SET state = 'pending', done = 0, resume = NULL, error = NULL, failed_at = NULL
             WHERE round = 'words';",
        )?;
        vectors::clear(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Writes the next generation of the partitions that are due one (vectors::compact), for when the indexer is idle.
    /// Returns their names.
    pub fn compact_vectors(&mut self) -> Result<Vec<String>> {
        let folder = self.vectors_folder();
        vectors::compact(&mut self.reader.conn, &folder)
    }

    /// Removes the vector files no partition uses any more.
    pub fn tidy_vectors(&mut self) -> Result<()> {
        let folder = self.vectors_folder();
        vectors::tidy(&mut self.reader.conn, &folder)
    }

    /// Runs `work` in one transaction: its writes commit together, which is faster than one by one. An error rolls
    /// them back.
    pub fn batch<T>(&self, work: impl FnOnce() -> Result<T>) -> Result<T> {
        let transaction = self.conn.unchecked_transaction()?;
        let value = work()?;
        transaction.commit()?;
        Ok(value)
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

/// How far a file's work in a round is now: done once it's `finished`, with its amount what was done.
fn finish(tx: &Transaction, work: &Work, done: &Done) -> Result<()> {
    tx.execute(
        "UPDATE work SET done = ?3, resume = ?4, state = CASE WHEN ?5 THEN 'done' ELSE 'pending' END,
             amount = CASE WHEN ?5 THEN ?3 ELSE MAX(amount, ?3) END, error = NULL, failed_at = NULL
         WHERE file_id = ?1 AND round = ?2",
        params![
            work.file.id,
            work.round.as_str(),
            done.done,
            done.resume.as_ref().map(|resume| resume.to_string()),
            done.finished
        ],
    )?;
    Ok(())
}

/// More work for a file in a round (`amount` of it), which it waits for.
fn add_work(conn: &Connection, file_id: i64, round: Round, amount: f64, new: bool) -> Result<()> {
    conn.execute(
        "INSERT INTO work (file_id, round, cost, amount, fast) VALUES (?1, ?2, ?3, ?3, ?4)
         ON CONFLICT (file_id, round) DO UPDATE SET amount = amount + excluded.amount,
             cost = amount + excluded.amount, state = 'pending', fast = max(fast, excluded.fast)",
        params![file_id, round.as_str(), amount, new],
    )?;
    Ok(())
}

/// Brings a file's text round in step with its passages: as many as have or wait for a vector, done as far as they
/// have one, and done when none waits. A file with no passage to embed has none.
fn recount_text(conn: &Connection, file_id: i64, new: bool) -> Result<()> {
    let (due, embedded): (i64, i64) = conn.query_row(
        &format!(
            "SELECT COUNT(*) FILTER (WHERE due = 'text'), COUNT(*) FILTER (WHERE due IS NULL AND row IS NOT NULL)
             FROM segments WHERE file_id = ?1 AND segment IN {TEXT_SEGMENTS}"
        ),
        [file_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if due == 0 && embedded == 0 {
        conn.execute("DELETE FROM work WHERE file_id = ?1 AND round = 'text'", [file_id])?;
        return Ok(());
    }
    conn.execute(
        "INSERT INTO work (file_id, round, state, cost, amount, done, fast) VALUES (?1, 'text', ?2, ?3, ?3, ?4, ?5)
         ON CONFLICT (file_id, round) DO UPDATE SET state = excluded.state, cost = excluded.cost,
             amount = excluded.amount, done = excluded.done, fast = max(fast, excluded.fast), error = NULL,
             failed_at = NULL",
        params![file_id, if due > 0 { "pending" } else { "done" }, (due + embedded) as f64, embedded as f64, new],
    )?;
    Ok(())
}

/// What a file's rounds found out about it so far.
fn stored_info(conn: &Connection, file_id: i64) -> Result<serde_json::Value> {
    let info: Option<String> = conn.query_row("SELECT info FROM files WHERE id = ?1", [file_id], |row| row.get(0))?;
    Ok(info.and_then(|info| serde_json::from_str(&info).ok()).unwrap_or_else(|| serde_json::json!({})))
}

/// Adds what a round found out about a file to what's known (a JSON merge patch).
fn merge_info(conn: &Connection, file_id: i64, info: &serde_json::Value) -> Result<()> {
    if info.as_object().is_some_and(|info| !info.is_empty()) {
        conn.execute(
            "UPDATE files SET info = json_patch(COALESCE(info, '{}'), ?2) WHERE id = ?1",
            params![file_id, info.to_string()],
        )?;
    }
    Ok(())
}

fn delete_segments(conn: &Connection, file_id: i64) -> Result<()> {
    conn.execute("DELETE FROM words WHERE rowid IN (SELECT id FROM segments WHERE file_id = ?1)", [file_id])?;
    conn.execute("DELETE FROM segments WHERE file_id = ?1", [file_id])?;
    Ok(())
}

/// Drops what reading a file wrote, and the rounds that came of it: all but what's written down of what's said in it,
/// which doesn't depend on how it's read (a file that changed is queued again, and loses that too).
fn delete_read(conn: &Connection, file_id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM words WHERE rowid IN (SELECT id FROM segments WHERE file_id = ?1 AND segment != 'speech')",
        [file_id],
    )?;
    conn.execute("DELETE FROM segments WHERE file_id = ?1 AND segment != 'speech'", [file_id])?;
    conn.execute("DELETE FROM work WHERE file_id = ?1 AND round NOT IN ('words', 'speech')", [file_id])?;
    Ok(())
}

/// Where a words round that goes on from where the last read ended reads again from (`resume.from`, a place in the
/// file): what was read from there on may have changed since (a conversation's last exchange, which grew).
fn read_again_from(work: &Work) -> Option<f64> {
    (work.round == Round::Words).then(|| work.resume.as_ref()?.get("from")?.as_f64()).flatten()
}

/// Clears what an earlier read of a file's words left, before the first of its segments are written: all of it, or
/// for a read that goes on, what it read again that changed. Of that, the segments that are as they were stay, their
/// vectors with them, and aren't written again. Returns the segments to write.
fn clear_words<'a>(tx: &Transaction, work: &Work, segments: &'a [NewSegment]) -> Result<Cow<'a, [NewSegment]>> {
    let Some(from) = read_again_from(work) else {
        delete_read(tx, work.file.id)?;
        return Ok(Cow::Borrowed(segments));
    };
    let mut statement = tx.prepare_cached(
        "SELECT id, segment, loc, loc_end, text FROM segments WHERE file_id = ?1 AND loc >= ?2 AND segment != 'speech'",
    )?;
    let mut before: Vec<(i64, NewSegment)> = statement
        .query_map(params![work.file.id, from], |row| {
            let segment = Segment::parse(&row.get::<_, String>(1)?).unwrap_or_default();
            let (loc, loc_end, text) = (row.get(2)?, row.get(3)?, row.get(4)?);
            Ok((row.get(0)?, NewSegment { segment, loc, loc_end, text, ..Default::default() }))
        })?
        .collect::<std::result::Result<_, _>>()?;
    let mut fresh = Vec::new();
    for segment in segments {
        let same = before.iter().position(|(_, was)| {
            (was.segment, was.loc, was.loc_end, &was.text)
                == (segment.segment, segment.loc, segment.loc_end, &segment.text)
        });
        match same {
            Some(at) => _ = before.swap_remove(at),
            None => fresh.push(segment.clone()),
        }
    }
    for (id, ..) in before {
        tx.execute("DELETE FROM words WHERE rowid = ?1", [id])?;
        tx.execute("DELETE FROM segments WHERE id = ?1", [id])?;
    }
    Ok(Cow::Owned(fresh))
}

/// Drops a file's segments of one sort.
fn delete_kind(conn: &Connection, file_id: i64, segment: Segment) -> Result<()> {
    conn.execute(
        "DELETE FROM words WHERE rowid IN (SELECT id FROM segments WHERE file_id = ?1 AND segment = ?2)",
        params![file_id, segment.as_str()],
    )?;
    conn.execute("DELETE FROM segments WHERE file_id = ?1 AND segment = ?2", params![file_id, segment.as_str()])?;
    Ok(())
}

/// How many segments of one sort a file has.
fn count_kind(conn: &Connection, file_id: i64, segment: Segment) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM segments WHERE file_id = ?1 AND segment = ?2",
        params![file_id, segment.as_str()],
        |r| r.get(0),
    )?)
}

/// Drops what an earlier try of a round left of a file, before its first item is written again. OCR's are its
/// passages, and the pictures of a document's scanned pages it found little text on (with their round); a
/// screenshot's picture keeps its place, and loses the words OCR gave it.
fn clear_round(tx: &Transaction, file: &FileRow, round: Round) -> Result<()> {
    match round {
        Round::Words | Round::Text => {}
        Round::Ocr => {
            delete_kind(tx, file.id, Segment::Ocr)?;
            if file.kind == Kind::Document {
                delete_kind(tx, file.id, Segment::Picture)?;
                tx.execute("DELETE FROM work WHERE file_id = ?1 AND round = 'pictures'", [file.id])?;
            } else {
                tx.execute("UPDATE segments SET text = NULL WHERE file_id = ?1 AND segment = 'picture'", [file.id])?;
            }
        }
        Round::Pictures => delete_kind(tx, file.id, Segment::Frame)?,
        Round::Sound => delete_kind(tx, file.id, Segment::Sound)?,
        Round::Speech => delete_kind(tx, file.id, Segment::Speech)?,
    }
    Ok(())
}

/// What makes a file with nothing (else) in it findable by its name: a segment with no text, which goes once the file
/// has segments of its own.
fn placeholder(conn: &Connection, file_id: i64, name: &str) -> Result<()> {
    conn.execute("INSERT INTO segments (file_id, segment) VALUES (?1, 'passage')", [file_id])?;
    conn.execute("INSERT INTO words (rowid, name, text) VALUES (?1, ?2, '')", params![conn.last_insert_rowid(), name])?;
    Ok(())
}

/// Writes a file's segments, each with its words (and the file's, `name`) in the keyword index. Their vectors are
/// appended to the file's partition first (`folder`), and the segments point at their rows: `conn`'s transaction holds
/// the write lock (BEGIN IMMEDIATE), and commits after the rows are on disk. A picture at a place where the file has
/// one already gives that one what it brings: its words, the round its vector comes from, or its vector. The file's
/// placeholder goes. Returns how many pictures it added that wait for their vector.
fn insert_segments(
    conn: &Connection,
    folder: &Path,
    file: &FileRow,
    name: &str,
    segments: &[NewSegment],
) -> Result<usize> {
    if segments.is_empty() {
        return Ok(0);
    }
    let vectors: Vec<&[f32]> = segments.iter().filter_map(|segment| segment.vector.as_deref()).collect();
    if let Some(vector) = vectors.iter().find(|vector| vector.len() != DIM) {
        bail!("vector of {} values", vector.len());
    }
    let (partition, mut next) = if vectors.is_empty() {
        (None, 0)
    } else {
        let (partition, first) = vectors::append(conn, folder, vectors::partition_of(file.kind), &vectors)?;
        (Some(partition), first)
    };
    let placeholders = "SELECT id FROM segments WHERE file_id = ?1 AND segment = 'passage' AND text IS NULL
                        AND loc IS NULL AND due IS NULL AND row IS NULL";
    conn.execute(&format!("DELETE FROM words WHERE rowid IN ({placeholders})"), [file.id])?;
    conn.execute(&format!("DELETE FROM segments WHERE id IN ({placeholders})"), [file.id])?;
    let mut insert = conn.prepare_cached(
        "INSERT INTO segments (file_id, segment, loc, loc_end, text, context, due, partition, row)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    let mut words = conn.prepare_cached("INSERT INTO words (rowid, name, text) VALUES (?1, ?2, ?3)")?;
    let mut waiting = 0;
    for segment in segments {
        let row = segment.vector.as_ref().map(|_| {
            next += 1;
            next - 1
        });
        let due = segment.due.map(Round::as_str);
        let picture: Option<(i64, Option<String>, Option<i64>)> = if segment.segment == Segment::Picture {
            conn.query_row(
                "SELECT id, text, partition FROM segments WHERE file_id = ?1 AND segment = 'picture' AND loc IS ?2",
                params![file.id, segment.loc],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
        } else {
            None
        };
        if let Some((id, text, had)) = picture {
            if let Some(row) = row {
                if let Some(had) = had {
                    // The vector it had is a dead row now.
                    conn.execute("UPDATE partitions SET dead = dead + 1 WHERE id = ?1", [had])?;
                }
                conn.execute(
                    "UPDATE segments SET partition = ?2, row = ?3, due = NULL WHERE id = ?1",
                    params![id, partition, row],
                )?;
            } else if due.is_some() && had.is_none() {
                conn.execute("UPDATE segments SET due = ?2 WHERE id = ?1", params![id, due])?;
            }
            if segment.text.is_some() && segment.text != text {
                conn.execute("UPDATE segments SET text = ?2 WHERE id = ?1", params![id, segment.text])?;
                conn.execute("DELETE FROM words WHERE rowid = ?1", [id])?;
                words.execute(params![id, name, segment.text.as_deref().unwrap_or("")])?;
            }
            continue;
        }
        insert.execute(params![
            file.id,
            segment.segment.as_str(),
            segment.loc,
            segment.loc_end,
            segment.text,
            segment.context,
            due,
            row.and(partition),
            row,
        ])?;
        words.execute(params![conn.last_insert_rowid(), name, segment.text.as_deref().unwrap_or("")])?;
        waiting += (segment.segment == Segment::Picture && segment.due == Some(Round::Pictures)) as usize;
    }
    Ok(waiting)
}

pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::testing::{TempIndex, add, file, unit};

    fn found(path: &str) -> NewFile {
        file(path, Kind::Plaintext)
    }

    fn passage(text: &str) -> NewSegment {
        NewSegment { segment: Segment::Passage, text: Some(text.into()), ..Default::default() }
    }

    /// A passage that waits for the text round.
    fn due(text: &str) -> NewSegment {
        NewSegment { due: Some(Round::Text), ..passage(text) }
    }

    fn texts(store: &Reader, id: i64) -> Vec<String> {
        let mut statement = store.conn.prepare("SELECT text FROM segments WHERE file_id = ?1 ORDER BY id").unwrap();
        statement
            .query_map([id], |row| row.get::<_, Option<String>>(0))
            .unwrap()
            .map(|t| t.unwrap().unwrap_or_default())
            .collect()
    }

    /// The next of a round's work, for the file at `path`.
    fn next(store: &Reader, round: Round, path: &str) -> Work {
        store.next(round, false, 100).unwrap().into_iter().find(|work| work.file.path == Path::new(path)).unwrap()
    }

    fn totals(store: &Reader, round: Round) -> (f64, f64, usize) {
        let totals = store.rounds().unwrap().into_iter().find(|totals| totals.round == round).unwrap();
        (totals.done, totals.amount, totals.files)
    }

    #[test]
    fn stats_count_the_vectors_of_each_kind() {
        let index = TempIndex::new("vectors-by-kind");
        let mut store = Writer::open(&index.path).unwrap();
        add(&mut store, "/folder/a.txt", Kind::Plaintext, vec![("one".into(), Some(unit(1))), ("two".into(), None)]);
        add(&mut store, "/folder/b.md", Kind::Plaintext, vec![("three".into(), Some(unit(2)))]);
        add(&mut store, "/folder/c.rs", Kind::Code, vec![("four".into(), Some(unit(3)))]);
        let stats = store.stats().unwrap();
        assert_eq!(stats.vectors, 3);
        assert_eq!(stats.vectors_by_kind, HashMap::from([("plaintext".into(), 2), ("code".into(), 1)]));
        assert_eq!(stats.by_kind, HashMap::from([("plaintext".into(), 2), ("code".into(), 1)]));
    }

    #[test]
    fn work_for_a_renamed_file_is_not_written_to_its_new_row() {
        let index = TempIndex::new("rename");
        let mut store = Writer::open(&index.path).unwrap();
        store.queue(&found("/folder/old.txt"), None, 0.0).unwrap();
        let work = next(&store, Round::Words, "/folder/old.txt");
        // Renamed while it was being read: its row goes, and the new path's row may get the same id.
        store.remove_file(work.file.id).unwrap();
        let renamed = store.queue(&found("/folder/new.txt"), None, 0.0).unwrap();
        assert_eq!(renamed, work.file.id);
        assert!(!store.fail(&work, "no such file", false).unwrap());
        assert!(!store.write_words(&work, &[], &serde_json::json!({}), &[], false, None).unwrap());
        assert_eq!(store.work_state(Path::new("/folder/new.txt"), Round::Words), "pending");
    }

    #[test]
    fn a_file_read_with_old_values_is_not_written() {
        let index = TempIndex::new("reading");
        let mut store = Writer::open(&index.path).unwrap();
        let id = store.queue(&found("/a/note.txt"), None, 0.0).unwrap();
        let work = next(&store, Round::Words, "/a/note.txt");
        // The reading values changed while it was read: it's queued again, to be read the new way.
        let shorter = NewFile { reading: "shorter passages".into(), ..found("/a/note.txt") };
        store.queue(&shorter, Some(id), 0.0).unwrap();
        assert!(!store.write_words(&work, &[], &serde_json::json!({}), &[], false, None).unwrap());
        let again = next(&store, Round::Words, "/a/note.txt");
        assert_eq!(again.file.reading, shorter.reading);
        assert!(store.write_words(&again, &[], &serde_json::json!({}), &[], false, None).unwrap());
    }

    #[test]
    fn a_round_goes_new_files_first_then_cheapest() {
        let index = TempIndex::new("cost");
        let store = Writer::open(&index.path).unwrap();
        store.queue(&found("/a/podcast.txt"), None, 3600.0).unwrap();
        store.queue(&found("/a/memo.txt"), None, 0.5).unwrap();
        store.queue(&NewFile { new: true, ..found("/a/screenshot.txt") }, None, 9.0).unwrap();
        store.queue(&found("/a/book.txt"), None, 60.0).unwrap();
        let order: Vec<_> = store.next(Round::Words, false, 10).unwrap().into_iter().map(|w| w.file.path).collect();
        assert_eq!(order, ["/a/screenshot.txt", "/a/memo.txt", "/a/book.txt", "/a/podcast.txt"].map(PathBuf::from));
        let new: Vec<_> = store.next(Round::Words, true, 10).unwrap().into_iter().map(|w| w.file.path).collect();
        assert_eq!(new, [PathBuf::from("/a/screenshot.txt")]);
        assert_eq!(store.queues().unwrap(), [(Round::Words, true, true)]);
        assert_eq!(totals(&store, Round::Words), (0.0, 4.0, 4));
    }

    #[test]
    fn a_read_that_goes_on_keeps_what_it_read_again_as_it_was() {
        let index = TempIndex::new("goes-on");
        let mut store = Writer::open(&index.path).unwrap();
        let log = file("/a/session.jsonl", Kind::Conversation);
        let id = store.queue(&log, None, 0.0).unwrap();
        let said = |at: f64, text: &str| NewSegment {
            segment: Segment::Exchange,
            loc: Some(at),
            loc_end: Some(at),
            due: Some(Round::Text),
            ..passage(text)
        };
        let ids = |store: &Reader| -> Vec<(i64, String, bool)> {
            let mut statement = store
                .conn
                .prepare("SELECT id, text, row IS NOT NULL FROM segments WHERE file_id = ?1 ORDER BY id")
                .unwrap();
            statement.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect()
        };
        let work = next(&store, Round::Words, "/a/session.jsonl");
        let resume = serde_json::json!({ "from": 20.0 });
        let first = [said(10.0, "asked"), said(20.0, "asked again"), said(20.0, "answered so far")];
        assert!(store.write_words(&work, &first, &serde_json::json!({}), &[], false, Some(&resume)).unwrap());
        let due = store.due_text(false, 10).unwrap();
        store.write_text(&due.iter().map(|due| (due, unit(1))).collect::<Vec<_>>()).unwrap();
        let before = ids(&store);
        assert!(before.iter().all(|(_, _, embedded)| *embedded));

        // It grew: its words go on from where its last exchange started, which is read again.
        assert_eq!(store.words_resume(id).unwrap(), Some(resume.clone()));
        store.queue_grown(id, &NewFile { size: 2, modified: 2.0, ..log }, 0.0).unwrap();
        let work = next(&store, Round::Words, "/a/session.jsonl");
        assert_eq!(work.resume, Some(resume));
        let grown = [said(20.0, "asked again"), said(20.0, "answered in full"), said(30.0, "asked a third time")];
        let resume = serde_json::json!({ "from": 30.0 });
        assert!(store.write_words(&work, &grown, &serde_json::json!({}), &[], false, Some(&resume)).unwrap());
        let after = ids(&store);
        // What's as it was keeps its row and its vector; what changed is written again, and waits for one.
        assert_eq!(after[..2], before[..2]);
        let rest: Vec<(&str, bool)> = after[2..].iter().map(|(_, text, embedded)| (text.as_str(), *embedded)).collect();
        assert_eq!(rest, [("answered in full", false), ("asked a third time", false)]);
        assert_eq!(totals(&store, Round::Text), (2.0, 4.0, 1));
        // A read that fails forgets where it went on: the next starts from the top.
        store
            .queue_grown(id, &NewFile { size: 3, modified: 3.0, ..file("/a/session.jsonl", Kind::Conversation) }, 0.0)
            .unwrap();
        let work = next(&store, Round::Words, "/a/session.jsonl");
        assert!(store.fail(&work, "can't read it", false).unwrap());
        assert_eq!(store.words_resume(id).unwrap(), None);
    }

    #[test]
    fn a_segment_is_read_with_the_ones_around_it() {
        let index = TempIndex::new("around");
        let mut store = Writer::open(&index.path).unwrap();
        let texts = |names: &[&str]| names.iter().map(|name| (name.to_string(), None)).collect::<Vec<_>>();
        add(&mut store, "/a/other.txt", Kind::Plaintext, texts(&["elsewhere"]));
        add(&mut store, "/a/story.txt", Kind::Plaintext, texts(&["one", "two", "three", "four", "five"]));
        let id = |text: &str| -> i64 {
            store.conn.query_row("SELECT id FROM segments WHERE text = ?1", [text], |row| row.get(0)).unwrap()
        };
        let around = |text: &str, n| store.segment_text_around(id(text), n).unwrap().unwrap();
        assert_eq!(around("three", 1), "two\n\nthree\n\nfour");
        assert_eq!(around("one", 2), "one\n\ntwo\n\nthree");
        assert_eq!(around("five", 0), "five");
        assert_eq!(store.segment_text_around(-1, 1).unwrap(), None);
    }

    #[test]
    fn a_big_files_words_are_written_in_parts() {
        let index = TempIndex::new("parts");
        let mut store = Writer::open(&index.path).unwrap();
        let id = store.queue(&found("/a/book.txt"), None, 0.0).unwrap();
        let work = next(&store, Round::Words, "/a/book.txt");
        // What an earlier, unfinished read left goes with the first part.
        store.write_words_part(&work, &[passage("left over")], true).unwrap();
        assert!(store.write_words_part(&work, &[due("one")], true).unwrap());
        assert!(store.write_words_part(&work, &[due("two")], false).unwrap());
        assert_eq!(texts(&store, id), ["one", "two"]);
        assert!(store.write_words(&work, &[passage("three")], &serde_json::json!({}), &[], true, None).unwrap());
        assert_eq!(texts(&store, id), ["one", "two", "three"]);
        // Two of them wait for their vectors.
        assert_eq!(totals(&store, Round::Text), (0.0, 2.0, 1));
        // A file that changed meanwhile isn't written to.
        store.queue(&NewFile { size: 20, ..found("/a/book.txt") }, Some(id), 0.0).unwrap();
        assert!(!store.write_words_part(&work, &[passage("four")], false).unwrap());
    }

    #[test]
    fn words_bring_a_files_other_rounds_and_passages_wait_for_the_text_round() {
        let index = TempIndex::new("rounds");
        let mut store = Writer::open(&index.path).unwrap();
        let id = store.queue(&NewFile { new: true, ..file("/a/scan.pdf", Kind::Document) }, None, 1.0).unwrap();
        let words = next(&store, Round::Words, "/a/scan.pdf");
        let info = serde_json::json!({"pages": 3});
        assert!(
            store
                .write_words(&words, &[due("page one"), due("page two")], &info, &[(Round::Ocr, 1.0)], false, None)
                .unwrap()
        );
        assert_eq!(store.queues().unwrap(), [(Round::Text, true, false), (Round::Ocr, true, false)]);

        // The text round: its passages, embedded.
        let waiting = store.due_text(false, 10).unwrap();
        assert_eq!(waiting.iter().map(|d| d.text.as_str()).collect::<Vec<_>>(), ["page one", "page two"]);
        store.write_text(&[(&waiting[0], unit(1))]).unwrap();
        assert_eq!(totals(&store, Round::Text), (1.0, 2.0, 1));
        store.write_text(&[(&waiting[1], unit(2))]).unwrap();
        assert_eq!(totals(&store, Round::Text), (2.0, 2.0, 0));

        // OCR: its page with enough text is passages, which wait for the text round in turn; its page with little
        // is a picture, which waits for the pictures round.
        let ocr = next(&store, Round::Ocr, "/a/scan.pdf");
        let read = NewSegment { segment: Segment::Ocr, loc: Some(3.0), ..due("a receipt, total due within 30 days") };
        let little = NewSegment {
            segment: Segment::Picture,
            loc: Some(2.0),
            text: Some("OK".into()),
            due: Some(Round::Pictures),
            ..Default::default()
        };
        let end = Done { done: 2.0, finished: true, ..Default::default() };
        assert!(store.write_item(&ocr, &[read, little], &end).unwrap());
        assert_eq!(totals(&store, Round::Text), (2.0, 3.0, 1));
        assert_eq!(totals(&store, Round::Pictures), (0.0, 1.0, 1));

        // The pictures round gives the page's picture its vector: one segment, with the words OCR found on it.
        let pictures = next(&store, Round::Pictures, "/a/scan.pdf");
        let shown =
            NewSegment { segment: Segment::Picture, loc: Some(2.0), vector: Some(unit(3)), ..Default::default() };
        assert!(
            store.write_item(&pictures, &[shown], &Done { done: 1.0, finished: true, ..Default::default() }).unwrap()
        );
        assert_eq!(texts(&store, id), ["page one", "page two", "a receipt, total due within 30 days", "OK"]);
        assert_eq!(stored(&store, id), [unit(1), unit(2), unit(3)]);
        let info: String = store.conn.query_row("SELECT info FROM files WHERE id = ?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&info).unwrap(), info_of(3));
    }

    #[test]
    fn a_documents_pictures_wait_for_the_pages_ocr_found_meanwhile() {
        let index = TempIndex::new("late-pages");
        let mut store = Writer::open(&index.path).unwrap();
        store.queue(&file("/a/scan.pdf", Kind::Document), None, 1.0).unwrap();
        let words = next(&store, Round::Words, "/a/scan.pdf");
        assert!(store.write_words(&words, &[], &serde_json::json!({}), &[(Round::Ocr, 2.0)], false, None).unwrap());
        let page = |loc: f64| NewSegment {
            segment: Segment::Picture,
            loc: Some(loc),
            due: Some(Round::Pictures),
            ..Default::default()
        };
        // OCR's first item finds a page with little text; the pictures round takes it.
        let ocr = next(&store, Round::Ocr, "/a/scan.pdf");
        assert!(store.write_item(&ocr, &[page(1.0)], &Done { done: 1.0, ..Default::default() }).unwrap());
        let pictures = next(&store, Round::Pictures, "/a/scan.pdf");
        // Meanwhile OCR's next item finds another.
        let ocr = next(&store, Round::Ocr, "/a/scan.pdf");
        assert!(
            store.write_item(&ocr, &[page(2.0)], &Done { done: 2.0, finished: true, ..Default::default() }).unwrap()
        );
        let shown = NewSegment { vector: Some(unit(1)), due: None, ..page(1.0) };
        assert!(
            store.write_item(&pictures, &[shown], &Done { done: 1.0, finished: true, ..Default::default() }).unwrap()
        );
        assert_eq!(totals(&store, Round::Pictures), (1.0, 2.0, 1));
        assert_eq!(store.due_pictures(pictures.file.id).unwrap(), [2]);
    }

    fn info_of(pages: usize) -> serde_json::Value {
        serde_json::json!({ "pages": pages })
    }

    #[test]
    fn a_files_sound_waits_for_its_speech() {
        let index = TempIndex::new("sound-after-speech");
        let mut store = Writer::open(&index.path).unwrap();
        store.queue(&NewFile { speech: true, ..file("/a/talk.mp3", Kind::Audio) }, None, 1.0).unwrap();
        store.queue(&file("/a/song.mp3", Kind::Audio), None, 1.0).unwrap();
        for path in ["/a/talk.mp3", "/a/song.mp3"] {
            let words = next(&store, Round::Words, path);
            let rounds: &[(Round, f64)] = if path.contains("talk") {
                &[(Round::Sound, 60.0), (Round::Speech, 60.0)]
            } else {
                &[(Round::Sound, 60.0)]
            };
            assert!(store.write_words(&words, &[], &serde_json::json!({}), rounds, false, None).unwrap());
        }
        let sound = |store: &Writer| -> Vec<String> {
            let work = store.next(Round::Sound, false, 10).unwrap();
            work.iter().map(|work| work.file.path.display().to_string()).collect()
        };
        assert_eq!(sound(&store), ["/a/song.mp3"]);
        // Its speech written down, the sound round takes it, with where speech was heard.
        let speech = next(&store, Round::Speech, "/a/talk.mp3");
        let said = NewSegment {
            segment: Segment::Speech,
            loc: Some(2.0),
            loc_end: Some(30.0),
            text: Some("hello".into()),
            ..Default::default()
        };
        assert!(
            store.write_item(&speech, &[said], &Done { done: 60.0, finished: true, ..Default::default() }).unwrap()
        );
        assert_eq!(sound(&store).len(), 2);
        assert_eq!(store.spoken(speech.file.id).unwrap(), [(2.0, 30.0, 1)]);
        assert!(store.queues().unwrap().iter().any(|(round, ..)| *round == Round::Sound));
    }

    #[test]
    fn a_long_files_round_goes_on_from_the_item_it_was_on() {
        let index = TempIndex::new("items");
        let mut store = Writer::open(&index.path).unwrap();
        let id = store.queue(&file("/a/podcast.mp3", Kind::Audio), None, 1.0).unwrap();
        let words = next(&store, Round::Words, "/a/podcast.mp3");
        let info = serde_json::json!({"duration": 500.0});
        assert!(store.write_words(&words, &[], &info, &[(Round::Sound, 500.0)], false, None).unwrap());
        // Findable by its name meanwhile.
        assert_eq!(texts(&store, id), [""]);
        let window = |at: f64| NewSegment {
            segment: Segment::Sound,
            loc: Some(at),
            vector: Some(unit(1)),
            ..Default::default()
        };
        let sound = next(&store, Round::Sound, "/a/podcast.mp3");
        assert!(store.write_item(&sound, &[window(0.0)], &Done { done: 300.0, ..Default::default() }).unwrap());
        let sound = next(&store, Round::Sound, "/a/podcast.mp3");
        assert_eq!((sound.done, sound.amount), (300.0, 500.0));
        let end = Done { done: 498.5, finished: true, ..Default::default() };
        assert!(store.write_item(&sound, &[window(300.0)], &end).unwrap());
        assert_eq!(totals(&store, Round::Sound), (498.5, 498.5, 0));
        // The placeholder went with the first window.
        assert_eq!(texts(&store, id), ["", ""]);
        assert_eq!(stored(&store, id).len(), 2);
        // A first item again (the round tried anew) starts from nothing.
        let mut again = sound.clone();
        again.done = 0.0;
        assert!(store.write_item(&again, &[window(0.0)], &end).unwrap());
        assert_eq!(stored(&store, id).len(), 1);
    }

    #[test]
    fn a_failure_that_may_pass_is_tried_again() {
        let index = TempIndex::new("failures");
        let mut store = Writer::open(&index.path).unwrap();
        store.queue(&found("/a/stuck.txt"), None, 0.0).unwrap();
        store.queue(&found("/a/broken.txt"), None, 0.0).unwrap();
        assert!(store.fail(&next(&store, Round::Words, "/a/stuck.txt"), "it got stuck", true).unwrap());
        assert!(store.fail(&next(&store, Round::Words, "/a/broken.txt"), "not text", false).unwrap());
        assert_eq!(store.stats().unwrap().failed, 2);
        assert_eq!(store.requeue_passing(0.0).unwrap(), 1);
        assert_eq!(store.work_state(Path::new("/a/stuck.txt"), Round::Words), "pending");
        assert_eq!(store.work_state(Path::new("/a/broken.txt"), Round::Words), "failed");
    }

    #[test]
    fn another_model_embeds_what_is_written_down_again_without_writing_it_down_again() {
        let index = TempIndex::new("speech");
        let mut store = Writer::open(&index.path).unwrap();
        let recording = NewFile { speech: true, ..file("/a/memo.m4a", Kind::Audio) };
        let id = store.queue(&recording, None, 0.0).unwrap();
        let words = next(&store, Round::Words, "/a/memo.m4a");
        let rounds = [(Round::Sound, 4.0), (Round::Speech, 4.0)];
        assert!(store.write_words(&words, &[], &serde_json::json!({"duration": 4.0}), &rounds, false, None).unwrap());
        let end = Done { done: 4.0, finished: true, ..Default::default() };
        let said = NewSegment {
            segment: Segment::Speech,
            loc: Some(1.0),
            loc_end: Some(4.0),
            text: Some("buy milk".into()),
            due: Some(Round::Text),
            ..Default::default()
        };
        // What's said first: the sound round waits for it.
        assert!(store.write_item(&next(&store, Round::Speech, "/a/memo.m4a"), &[said], &end).unwrap());
        let sound = NewSegment { segment: Segment::Sound, loc: Some(0.0), vector: Some(unit(1)), ..Default::default() };
        assert!(store.write_item(&next(&store, Round::Sound, "/a/memo.m4a"), &[sound], &end).unwrap());
        let waiting = store.due_text(false, 10).unwrap();
        store.write_text(&[(&waiting[0], unit(2))]).unwrap();
        assert_eq!(stored(&store, id), [unit(2), unit(1)]);

        // Another model: every file is read again, and what's said keeps its words, waiting for a vector.
        store.requeue_vectors().unwrap();
        assert_eq!(texts(&store, id), ["buy milk"]);
        assert!(stored(&store, id).is_empty());
        let words = next(&store, Round::Words, "/a/memo.m4a");
        assert!(store.write_words(&words, &[], &serde_json::json!({"duration": 4.0}), &rounds, false, None).unwrap());
        assert_eq!(store.work_state(Path::new("/a/memo.m4a"), Round::Speech), "done");
        assert_eq!(totals(&store, Round::Text), (0.0, 1.0, 1));
        let waiting = store.due_text(false, 10).unwrap();
        assert_eq!(waiting[0].text, "buy milk");
        store.write_text(&[(&waiting[0], unit(3))]).unwrap();
        assert_eq!(stored(&store, id), [unit(3)]);
        let info: String = store.conn.query_row("SELECT info FROM files WHERE id = ?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&info).unwrap()["speech"], 1);
    }

    #[test]
    fn an_index_of_another_version_is_made_again() {
        let index = TempIndex::new("version");
        let store = Writer::open(&index.path).unwrap();
        store.queue(&found("/a/note.txt"), None, 0.0).unwrap();
        store.set_meta("schema", "3").unwrap();
        let reopened = Writer::open(&index.path).unwrap();
        assert_eq!(reopened.files().unwrap().len(), 0);
        assert_eq!(reopened.meta("schema").unwrap().as_deref(), Some(SCHEMA_VERSION.to_string().as_str()));
    }

    /// A file of `kind` with a passage for each of `vectors`, written.
    fn add_vectors(store: &mut Writer, path: &str, kind: Kind, vectors: &[Vec<f32>]) -> i64 {
        add(store, path, kind, vectors.iter().map(|vector| ("words".into(), Some(vector.clone()))).collect())
    }

    /// A file's vectors, read from where its segments say they are.
    fn stored(store: &Reader, file: i64) -> Vec<Vec<f32>> {
        let mut statement = store
            .conn
            .prepare(
                "SELECT p.name, s.row FROM segments s JOIN partitions p ON p.id = s.partition
                 WHERE s.file_id = ?1 AND s.row IS NOT NULL ORDER BY s.id",
            )
            .unwrap();
        let places: Vec<(String, i64)> =
            statement.query_map([file], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
        let bytes = DIM * 4;
        places
            .into_iter()
            .map(|(name, row)| {
                let row = row as usize;
                let rows = vectors::read_rows(&store.conn, &store.vectors_folder(), &name, vectors::Form::F32).unwrap();
                rows[row * bytes..(row + 1) * bytes].as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
            })
            .collect()
    }

    fn partition(store: &Reader, name: &str) -> vectors::Partition {
        vectors::partitions(&store.conn).unwrap().into_iter().find(|p| p.name == name).unwrap()
    }

    /// The files in the vectors folder.
    fn files(store: &Reader) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(store.vectors_folder())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn vectors_get_a_next_generation_when_a_quarter_is_dead_or_the_mean_moved() {
        let index = TempIndex::new("vectors");
        let mut store = Writer::open(&index.path).unwrap();
        let unit = vectors::unit;
        let gone = add_vectors(&mut store, "/a/gone.txt", Kind::Plaintext, &[unit(1), unit(2), unit(3)]);
        let kept = add_vectors(&mut store, "/a/kept.txt", Kind::Plaintext, &[unit(4), unit(5)]);
        add_vectors(&mut store, "/a/main.rs", Kind::Code, &[unit(6)]);
        assert_eq!((partition(&store, "main").rows, partition(&store, "code").rows), (5, 1));
        assert_eq!(stored(&store, kept), [unit(4), unit(5)]);

        // The sketches were made against the first file's mean, and five rows have another: they're made again, and
        // the rows stay where they are.
        assert_eq!(store.compact_vectors().unwrap(), ["main"]);
        assert_eq!((partition(&store, "main").generation, partition(&store, "main").rows), (1, 5));
        assert_eq!(stored(&store, kept), [unit(4), unit(5)]);
        assert!(store.compact_vectors().unwrap().is_empty());

        // Three of five rows dead: the live ones move to a generation of their own.
        store.remove_file(gone).unwrap();
        assert_eq!(partition(&store, "main").dead, 3);
        assert_eq!(store.compact_vectors().unwrap(), ["main"]);
        let main = partition(&store, "main");
        assert_eq!((main.generation, main.rows, main.dead), (2, 2, 0));
        assert_eq!(stored(&store, kept), [unit(4), unit(5)]);
        let names = ["code.0.f16", "code.0.f32", "code.0.sketch", "main.2.f16", "main.2.f32", "main.2.sketch"];
        assert_eq!(files(&store), names);

        // Everything again: the partitions start over, and their files go.
        store.requeue_vectors().unwrap();
        store.tidy_vectors().unwrap();
        assert_eq!((partition(&store, "main").generation, partition(&store, "main").rows), (3, 0));
        assert!(files(&store).is_empty());
    }

    #[test]
    fn rows_a_crash_left_are_written_over() {
        use std::io::Write;
        let index = TempIndex::new("leftovers");
        let mut store = Writer::open(&index.path).unwrap();
        add_vectors(&mut store, "/a/one.txt", Kind::Plaintext, &[vectors::unit(1)]);
        // A crash between appending and committing leaves rows nothing points at.
        let path = partition(&store, "main").path(&store.vectors_folder(), vectors::Form::F32);
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(&[7; 1000]).unwrap();
        let two = add_vectors(&mut store, "/a/two.txt", Kind::Plaintext, &[vectors::unit(2)]);
        assert_eq!(stored(&store, two), [vectors::unit(2)]);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 2 * DIM as u64 * 4);
    }
}
