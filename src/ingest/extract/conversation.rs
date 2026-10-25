//! Conversations: an agent's session log (Claude Code's or Codex's), a JSON record per line, read as what the user
//! said and what the agent answered, each message with its time. Not read: tool calls and what they gave back, the
//! agent's thinking, what an app wraps around a prompt (reminders, a command's output, the folder it ran in), and a
//! subagent's own transcript (its conversation is the one that started it). A log is known by its first lines, so
//! one is found in any folder.
//!
//! A running session's log grows every few seconds. A read ends with where its last exchange started, which may grow
//! still: when the log grows, the next read goes on from there (discover checks the log grew from what it was), and
//! what was read before stays, vectors and all.

use crate::adapters::macos::icloud;
use crate::ingest::read::{Content, Ended, From};
use crate::ingest::segment::exchanges::{Exchanges, Message};
use anyhow::{Result, bail};
use serde::de::{self, Deserializer, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// Whose log it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    /// The agent's name, as its messages are labelled.
    fn name(self) -> &'static str {
        match self {
            Agent::Claude => "Claude",
            Agent::Codex => "Codex",
        }
    }
}

/// What a log's first lines say: whose it is, and whether it's a subagent's transcript.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Log {
    pub agent: Agent,
    pub subagent: bool,
}

/// A log is known by its first few lines, each a JSON object, within HEAD_BYTES.
const HEAD_LINES: usize = 8;
const HEAD_BYTES: u64 = 256 << 10;

/// Whether the file is an agent's session log, by its first lines: Claude Code's records carry a `sessionId` (an
/// older log starts with a summary), and Codex's log starts with its session's `session_meta`. One only in iCloud
/// can't be told: its lines would be downloaded.
pub fn recognise(path: &Path) -> Option<Log> {
    if icloud::cloud_only_at(path) {
        return None;
    }
    let mut lines = BufReader::new(File::open(path).ok()?).take(HEAD_BYTES);
    let mut found = None;
    let mut line = String::new();
    for _ in 0..HEAD_LINES {
        line.clear();
        if lines.read_line(&mut line).ok()? == 0 || !line.ends_with('\n') {
            break;
        }
        let head: Head = serde_json::from_str(&line).ok()?;
        if head.kind == "session_meta" {
            let meta: Meta = serde_json::from_str(head.payload?.get()).ok()?;
            let subagent = meta.thread_source.as_deref() == Some("subagent")
                || meta.source.as_ref().is_some_and(|source| source.get("subagent").is_some());
            return Some(Log { agent: Agent::Codex, subagent });
        }
        if head.session_id.is_some() || (head.kind == "summary" && head.leaf_uuid.is_some()) {
            found = Some(Log { agent: Agent::Claude, subagent: false });
            // Whether it's a subagent's says its first message.
            if let Some(sidechain) = head.is_sidechain {
                return Some(Log { agent: Agent::Claude, subagent: sidechain });
            }
        }
    }
    found
}

/// The session a log is of: what resumes it, and where.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub agent: Agent,
    pub id: String,
    /// The folder it ran in, where it's resumed.
    pub cwd: Option<String>,
}

impl Session {
    /// The command that resumes it, in its folder.
    pub fn resume(&self) -> Vec<String> {
        match self.agent {
            Agent::Claude => vec!["claude".into(), "--resume".into(), self.id.clone()],
            Agent::Codex => vec!["codex".into(), "resume".into(), self.id.clone()],
        }
    }
}

/// The session of a log, from its first lines that say it.
pub fn session(path: &Path) -> Option<Session> {
    let log = recognise(path)?;
    let mut file = BufReader::new(File::open(path).ok()?);
    let mut line = String::new();
    let (mut id, mut cwd) = (None, None);
    for _ in 0..200 {
        line.clear();
        if file.read_line(&mut line).ok()? == 0 {
            break;
        }
        match log.agent {
            Agent::Codex => {
                if let Ok(head) = serde_json::from_str::<Head>(&line)
                    && head.kind == "session_meta"
                    && let Some(meta) = head.payload.and_then(|meta| serde_json::from_str::<Meta>(meta.get()).ok())
                {
                    return Some(Session { agent: log.agent, id: meta.id, cwd: meta.cwd });
                }
            }
            Agent::Claude => {
                if let Ok(record) = serde_json::from_str::<Record>(&line) {
                    id = id.or(record.session_id);
                    cwd = cwd.or(record.cwd);
                }
                if id.is_some() && cwd.is_some() {
                    break;
                }
            }
        }
    }
    Some(Session { agent: log.agent, id: id?, cwd })
}

/// Where the next read of a growing log goes on: where its last exchange started (the byte, and when it was said:
/// what was read from then on is read again), the bytes just before it (a log written anew, shorter or with other
/// bytes there, is read again from the top), how good its title is so far, and whether the folder the session started
/// in is known (Claude Code's records name the folder it's in now, which can change; it's resumed from the first).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Resume {
    agent: Agent,
    at: u64,
    from: f64,
    tail: String,
    titled: u8,
    placed: bool,
}

/// So many bytes before where a read goes on are checked to be the same.
const TAIL: usize = 32;

/// Whether a log that changed grew from what it was where its last read ended (`resume`), so a read can go on from
/// there. Logs are only added to: the bytes just before that point tell one written anew.
pub fn goes_on(path: &Path, resume: &serde_json::Value) -> bool {
    let Ok(resume) = Resume::deserialize(resume) else { return false };
    let Some(tail) = unhex(&resume.tail) else { return false };
    let Some(start) = resume.at.checked_sub(tail.len() as u64) else { return false };
    let mut bytes = vec![0; tail.len()];
    File::open(path)
        .and_then(|mut file| {
            file.seek(SeekFrom::Start(start))?;
            file.read_exact(&mut bytes)
        })
        .is_ok_and(|()| bytes == tail)
}

/// Reads a log's messages, or those from where its last read ended, into exchanges.
pub fn read(path: &Path, from: &From, content: &mut Content) -> Result<Ended> {
    let resume = from.resume.as_ref().and_then(|resume| Resume::deserialize(resume).ok());
    let agent = match &resume {
        Some(resume) => resume.agent,
        None => match recognise(path) {
            Some(log) => log.agent,
            None => bail!("not a conversation's log"),
        },
    };
    let mut file = BufReader::new(File::open(path)?);
    let mut at = resume.as_ref().map_or(0, |resume| resume.at);
    file.seek(SeekFrom::Start(at))?;
    let mut reading = Reading {
        agent,
        exchanges: Exchanges::new(agent.name()),
        info: serde_json::Map::new(),
        title: None,
        titled: resume.as_ref().map_or(0, |resume| resume.titled),
        placed: resume.as_ref().is_some_and(|resume| resume.placed),
        open: resume.as_ref().map(|resume| (resume.at, resume.from, resume.tail.clone())),
    };
    let (mut line, mut tail) = (Vec::new(), resume.as_ref().and_then(|resume| unhex(&resume.tail)).unwrap_or_default());
    loop {
        line.clear();
        let read = file.read_until(b'\n', &mut line)?;
        // A line still being written is read next time.
        if read == 0 || !line.ends_with(b"\n") {
            break;
        }
        let start = at;
        at += read as u64;
        if let Ok(text) = std::str::from_utf8(&line) {
            for message in reading.record(text) {
                if reading.exchanges.starts(&message) {
                    reading.open = Some((start, message.time, hex(&tail)));
                }
                if let Some(exchange) = reading.exchanges.push(message) {
                    content.exchange(&exchange)?;
                }
            }
        }
        if line.len() >= TAIL {
            tail = line[line.len() - TAIL..].to_vec();
        } else {
            tail.extend_from_slice(&line);
            tail.drain(..tail.len().saturating_sub(TAIL));
        }
    }
    if let Some(exchange) = reading.exchanges.finish() {
        content.exchange(&exchange)?;
    }
    // With no exchange yet, the next read goes on from the end.
    let (open_at, from, open_tail) = reading.open.take().unwrap_or((at, 0.0, hex(&tail)));
    let placed = reading.placed || reading.info.contains_key("cwd");
    let resume = Resume { agent, at: open_at, from, tail: open_tail, titled: reading.titled, placed };
    let mut info = reading.info;
    info.insert("agent".into(), json!(agent));
    if let Some(id) = info.get("session").and_then(|id| id.as_str()) {
        let session = Session { agent, id: id.to_string(), cwd: None };
        info.insert("resume".into(), json!(session.resume()));
    }
    if let Some(title) = reading.title {
        info.insert("tags".into(), json!({ "title": title }));
    }
    Ok(Ended {
        info: serde_json::Value::Object(info),
        done: 1.0,
        finished: true,
        resume: Some(serde_json::to_value(resume)?),
        ..Ended::default()
    })
}

/// A read of a log as it goes: the exchanges so far, what's known about the session, the title and how good it is
/// (custom 3, the agent's 2, the first prompt 1), and where the last exchange started.
struct Reading {
    agent: Agent,
    exchanges: Exchanges,
    info: serde_json::Map<String, serde_json::Value>,
    title: Option<String>,
    titled: u8,
    /// The folder the session started in was found by an earlier read.
    placed: bool,
    open: Option<(u64, f64, String)>,
}

impl Reading {
    /// The messages a line holds, and what it says about the session.
    fn record(&mut self, line: &str) -> Vec<Message> {
        match self.agent {
            Agent::Claude => serde_json::from_str::<Record>(line).ok().map(|r| self.claude(r)).unwrap_or_default(),
            Agent::Codex => serde_json::from_str::<Head>(line).ok().map(|r| self.codex(r)).unwrap_or_default(),
        }
    }

    fn claude(&mut self, record: Record) -> Vec<Message> {
        let time = record.timestamp.as_deref().and_then(seconds).unwrap_or(0.0);
        match record.kind.as_ref() {
            "custom-title" => self.title(3, record.custom_title.as_deref()),
            "ai-title" => self.title(2, record.ai_title.as_deref()),
            "summary" => self.title(2, record.summary.as_deref()),
            _ => {}
        }
        if let (Some(id), None) = (&record.session_id, self.info.get("session")) {
            self.info.insert("session".into(), json!(id));
        }
        let (user, said) = match record.kind.as_ref() {
            _ if record.is_sidechain || record.is_meta || record.is_compact_summary => return Vec::new(),
            _ if record.is_visible_in_transcript_only || record.is_api_error_message => return Vec::new(),
            // A message from the app, another session or a subagent's end, rather than the user.
            _ if record.origin.as_ref().is_some_and(|origin| !origin.is_human()) => return Vec::new(),
            "user" | "assistant" => match record.message.and_then(|m| serde_json::from_str::<Said>(m.get()).ok()) {
                Some(said) => (record.kind == "user", said.0),
                None => return Vec::new(),
            },
            // What the user typed while the agent worked: it's handed over as an attachment.
            "attachment" => match record.attachment {
                Some(Attachment { kind, command_mode, prompt: Some(prompt), origin })
                    if kind == "queued_command"
                        && command_mode.as_deref() == Some("prompt")
                        && origin.as_ref().is_none_or(Origin::is_human) =>
                {
                    (true, vec![prompt])
                }
                _ => return Vec::new(),
            },
            _ => return Vec::new(),
        };
        if let (Some(cwd), None, false) = (&record.cwd, self.info.get("cwd"), self.placed) {
            self.info.insert("cwd".into(), json!(cwd));
        }
        self.messages(user, time, said)
    }

    fn codex(&mut self, record: Head) -> Vec<Message> {
        let time = record.timestamp.as_deref().and_then(seconds).unwrap_or(0.0);
        let Some(payload) = record.payload else { return Vec::new() };
        match record.kind.as_ref() {
            "session_meta" if !self.info.contains_key("session") => {
                if let Ok(meta) = serde_json::from_str::<Meta>(payload.get()) {
                    self.info.insert("session".into(), json!(meta.id));
                    if let Some(cwd) = meta.cwd {
                        self.info.insert("cwd".into(), json!(cwd));
                    }
                }
                Vec::new()
            }
            "response_item" => match serde_json::from_str::<Item>(payload.get()) {
                Ok(item) if item.kind == "message" && matches!(item.role.as_ref(), "user" | "assistant") => {
                    // Its instructions, which Codex hands over as the user's.
                    let said = item.content.0.into_iter().filter(|text| !text.starts_with("# AGENTS.md")).collect();
                    self.messages(item.role == "user", time, said)
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// The messages said, without what an app wrapped around the user's.
    fn messages(&mut self, user: bool, time: f64, said: Vec<String>) -> Vec<Message> {
        let mut messages = Vec::new();
        for text in said {
            let text = if user { users_words(&text) } else { text.trim().to_string() };
            if text.is_empty() {
                continue;
            }
            if user && self.titled < 1 {
                self.title(1, Some(&text));
            }
            messages.push(Message { user, time, text });
        }
        messages
    }

    /// A title, kept when it's at least as good as the one so far: a later one of the same kind replaces it.
    fn title(&mut self, rank: u8, title: Option<&str>) {
        let Some(title) = title.map(one_line).filter(|title| !title.is_empty()) else { return };
        if rank >= self.titled {
            self.title = Some(title);
            self.titled = rank;
        }
    }
}

/// A title is at most this many characters.
const TITLE_CHARS: usize = 100;

/// A text on one line, cut at about TITLE_CHARS.
fn one_line(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(TITLE_CHARS) {
        Some((end, _)) => format!("{}…", line[..end].trim_end()),
        None => line,
    }
}

/// Elements an app wraps the user's own words in (a command they typed, text they pasted, an answer to the agent's
/// question): their text stays, without the tags.
const USERS_OWN: &[&str] =
    &["command-name", "command-args", "bash-input", "pasted_content", "send_user_message_question_reply"];

/// What the user said in a message, without what an app wrapped around it: an element that starts a line
/// (`<system-reminder>…</system-reminder>`, a command's output, the folder it ran in) goes, unless it holds the
/// user's own words; so does a note that a request was interrupted. Code blocks stay as they are.
fn users_words(text: &str) -> String {
    let mut kept = String::new();
    let mut rest = text;
    let mut fenced = false;
    while !rest.is_empty() {
        let line_end = rest.find('\n').map_or(rest.len(), |end| end + 1);
        let indent = line_end - rest[..line_end].trim_start_matches([' ', '\t']).len();
        let trimmed = &rest[indent..line_end];
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
        } else if !fenced && let Some((name, inner, end)) = element(&rest[indent..]) {
            rest = &rest[indent + end..];
            if USERS_OWN.contains(&name) {
                kept.push_str(inner.trim());
                if !rest.starts_with('\n') {
                    kept.push('\n');
                }
            } else {
                // The line it was on goes with it.
                rest = rest.strip_prefix('\n').unwrap_or(rest);
            }
            continue;
        } else if !fenced && trimmed.starts_with("[Request interrupted by user") {
            rest = &rest[line_end..];
            continue;
        }
        kept.push_str(&rest[..line_end]);
        rest = &rest[line_end..];
    }
    kept.trim().to_string()
}

/// The element a text starts with: its name, what's inside it, and where it ends. None when it doesn't start with a
/// tag, or the tag isn't closed.
fn element(text: &str) -> Option<(&str, &str, usize)> {
    let after = text.strip_prefix('<')?;
    let name_end = after.find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':')))?;
    let name = &after[..name_end];
    if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let open_end = after[name_end..].starts_with(['>', ' ']).then(|| after[name_end..].find('>'))??;
    let inner_start = 1 + name_end + open_end + 1;
    let closing = format!("</{name}>");
    let inner_end = inner_start + text[inner_start..].find(&closing)?;
    Some((name, &text[inner_start..inner_end], inner_end + closing.len()))
}

/// An RFC 3339 time ("2026-10-25T09:41:07.123Z", or with an offset) in seconds since 1970.
fn seconds(time: &str) -> Option<f64> {
    let number = |range: std::ops::Range<usize>| time.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let mut rest = &time[19..];
    let mut fraction = 0.0;
    if let Some(digits) = rest.strip_prefix('.') {
        let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
        fraction = format!("0.{}", &digits[..end]).parse().ok()?;
        rest = &digits[end..];
    }
    let offset = match rest {
        "Z" | "z" | "" => 0,
        _ => {
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            sign * (rest.get(1..3)?.parse::<i64>().ok()? * 3600 + rest.get(4..6)?.parse::<i64>().ok()? * 60)
        }
    };
    // Days since 1970 of a date in the proleptic Gregorian calendar (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some((days * 86_400 + hour * 3600 + minute * 60 + second - offset) as f64 + fraction)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    (0..text.len()).step_by(2).map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok()).collect()
}

/// A record's envelope: what it is, when, and (Codex) its payload, which is read only when it's a message.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Head<'a> {
    #[serde(rename = "type", default, borrow)]
    kind: Cow<'a, str>,
    timestamp: Option<String>,
    #[serde(borrow)]
    payload: Option<&'a RawValue>,
    session_id: Option<IgnoredAny>,
    leaf_uuid: Option<IgnoredAny>,
    is_sidechain: Option<bool>,
}

/// Codex's session: its id, the folder it ran in, and whether a subagent's.
#[derive(Deserialize)]
struct Meta {
    id: String,
    cwd: Option<String>,
    source: Option<serde_json::Value>,
    thread_source: Option<String>,
}

/// An item of a Codex session: a message (the user's, the agent's, or the app's own as `developer`), a tool call, a
/// tool's output, the agent's reasoning.
#[derive(Deserialize)]
struct Item<'a> {
    #[serde(rename = "type", default, borrow)]
    kind: Cow<'a, str>,
    #[serde(default, borrow)]
    role: Cow<'a, str>,
    #[serde(default)]
    content: Said,
}

/// A Claude Code record: a message (the user's, or the agent's), something the app noted (a title, an attachment),
/// with the session it's in. A message's content is read only when it's one to keep.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record<'a> {
    #[serde(rename = "type", default, borrow)]
    kind: Cow<'a, str>,
    timestamp: Option<String>,
    session_id: Option<String>,
    cwd: Option<String>,
    #[serde(default)]
    is_sidechain: bool,
    #[serde(default)]
    is_meta: bool,
    #[serde(default)]
    is_compact_summary: bool,
    #[serde(default)]
    is_visible_in_transcript_only: bool,
    #[serde(default)]
    is_api_error_message: bool,
    origin: Option<Origin>,
    #[serde(borrow)]
    message: Option<&'a RawValue>,
    attachment: Option<Attachment>,
    custom_title: Option<String>,
    ai_title: Option<String>,
    summary: Option<String>,
}

/// Who a message is from: the user (`human`), or the app, another session, a subagent's end.
#[derive(Deserialize)]
struct Origin {
    #[serde(default)]
    kind: String,
}

impl Origin {
    fn is_human(&self) -> bool {
        self.kind == "human"
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Attachment {
    #[serde(rename = "type", default)]
    kind: String,
    command_mode: Option<String>,
    prompt: Option<String>,
    origin: Option<Origin>,
}

/// What a message says: its text, or the text of its text blocks (other blocks, a tool's call or what it gave back,
/// thinking, a picture, are passed over without being kept).
#[derive(Default)]
struct Said(Vec<String>);

impl<'de> Deserialize<'de> for Said {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Texts;
        impl<'de> Visitor<'de> for Texts {
            type Value = Said;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a message's text or its blocks")
            }

            fn visit_str<E: de::Error>(self, text: &str) -> std::result::Result<Said, E> {
                Ok(Said(vec![text.to_string()]))
            }

            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> std::result::Result<Said, A::Error> {
                // A message: its content.
                let mut said = Said::default();
                while let Some(key) = map.next_key::<Cow<str>>()? {
                    if key == "content" {
                        said = map.next_value()?;
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(said)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut blocks: A) -> std::result::Result<Said, A::Error> {
                let mut texts = Vec::new();
                while let Some(block) = blocks.next_element::<Block>()? {
                    if let (true, Some(text)) =
                        (matches!(block.kind.as_ref(), "text" | "input_text" | "output_text"), block.text)
                    {
                        texts.push(text);
                    }
                }
                Ok(Said(texts))
            }

            fn visit_unit<E: de::Error>(self) -> std::result::Result<Said, E> {
                Ok(Said::default())
            }
        }
        deserializer.deserialize_any(Texts)
    }
}

#[derive(Deserialize)]
struct Block<'a> {
    #[serde(rename = "type", default, borrow)]
    kind: Cow<'a, str>,
    text: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use crate::ingest::read::{self, Piece};
    use protocol::{Kind, Round, Segment};
    use std::path::PathBuf;

    /// A made-up log in tests/conversations.
    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/conversations").join(name)
    }

    /// What a read keeps of each piece: its segment, its place, its text.
    type Kept = (Segment, Option<f64>, Option<f64>, String);

    fn read_from(path: &Path, resume: Option<serde_json::Value>) -> (Vec<Kept>, Ended) {
        let mut pieces = Vec::new();
        let from = From { resume, ..From::default() };
        let mut sink = |piece: Piece| {
            pieces.push((piece.segment, piece.loc, piece.loc_end, piece.text.unwrap_or_default()));
            Ok(())
        };
        let ended =
            read::read(Round::Words, path, Kind::Conversation, &Settings::BASE, &from, &mut sink, &mut |_| Ok(true))
                .unwrap();
        (pieces, ended)
    }

    fn texts(pieces: &[Kept]) -> Vec<&str> {
        pieces.iter().map(|(_, _, _, text)| text.as_str()).collect()
    }

    #[test]
    fn a_log_is_known_by_its_first_lines() {
        let log = |name| recognise(&fixture(name));
        assert_eq!(log("claude.jsonl"), Some(Log { agent: Agent::Claude, subagent: false }));
        assert_eq!(log("claude-subagent.jsonl"), Some(Log { agent: Agent::Claude, subagent: true }));
        assert_eq!(log("codex.jsonl"), Some(Log { agent: Agent::Codex, subagent: false }));
        assert_eq!(log("codex-subagent.jsonl"), Some(Log { agent: Agent::Codex, subagent: true }));
        assert_eq!(log("records.jsonl"), None);
    }

    #[test]
    fn claude_codes_log_reads_as_what_was_asked_and_answered() {
        let (pieces, ended) = read_from(&fixture("claude.jsonl"), None);
        // No thinking, tool calls or their output, reminders, a command's output, notes from the app, or an error.
        assert_eq!(
            texts(&pieces),
            [
                "User: Which tomatoes grow well on a balcony?\n\nClaude: Let me check your seed list.\n\nCherry tomatoes \
                 and Tumbling Tom both stay small.\n\nGive them a pot of at least 10 litres.",
                "User: /model\nsmall\n\nAnd peppers?\n\nAlso chillies, please.\n\nClaude: Peppers and chillies like the \
                 sunniest corner.\n\n```\nwater: every two days\n```",
                "User: ls pots\n\nClaude: Two pots: big and small.",
            ]
        );
        assert!(pieces.iter().all(|(segment, ..)| *segment == Segment::Exchange));
        // Each is at when it was asked, to when it was last answered.
        let may_1 = seconds("2026-05-01T09:00:00Z").unwrap();
        assert_eq!((pieces[0].1, pieces[0].2), (Some(may_1), Some(may_1 + 12.0)));
        assert_eq!((pieces[1].1, pieces[1].2), (Some(may_1 + 301.0), Some(may_1 + 420.0)));
        assert_eq!(ended.info["tags"]["title"], "Balcony vegetables");
        assert_eq!(ended.info["agent"], "claude");
        assert_eq!(ended.info["cwd"], "/Users/someone/projects/garden");
        let id = "6f1c2a9e-0b7d-4c3e-9a51-2d8e4f6a7b90";
        assert_eq!(ended.info["resume"], json!(["claude", "--resume", id]));
        let session = session(&fixture("claude.jsonl")).unwrap();
        assert_eq!((session.id.as_str(), session.cwd.as_deref()), (id, Some("/Users/someone/projects/garden")));
    }

    #[test]
    fn codexs_log_reads_as_what_was_asked_and_answered() {
        let (pieces, ended) = read_from(&fixture("codex.jsonl"), None);
        // Not its instructions, the folder it runs in, its reasoning, its tools, nor what its compaction repeats.
        assert_eq!(
            texts(&pieces),
            [
                "User: How long should sourdough proof?\n\nCodex: Checking the recipe notes.\n\nAbout four hours at \
                 room temperature, or overnight in the fridge.",
                "User: Thanks, and for rye?\n\nCodex: Rye proofs faster: about two hours.",
            ]
        );
        assert_eq!(ended.info["tags"]["title"], "How long should sourdough proof?");
        assert_eq!(ended.info["resume"], json!(["codex", "resume", "019a3f2e-7c41-7d20-b8e5-5f0c9d1a2b3c"]));
        assert_eq!(ended.info["cwd"], "/Users/someone/projects/bakery");
    }

    /// A log in a folder of its own, removed when the test ends.
    struct Temp(PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    fn temp(name: &str) -> Temp {
        let folder = std::env::temp_dir().join(format!("sift-it-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        Temp(folder.join("log.jsonl"))
    }

    #[test]
    fn a_growing_log_read_on_from_where_it_ended_is_read_as_a_whole_one() {
        for name in ["claude.jsonl", "codex.jsonl"] {
            let whole = std::fs::read(fixture(name)).unwrap();
            let (all, ended) = read_from(&fixture(name), None);
            let log = temp("growing-log");
            // Grown at each line, and partway through one.
            let mut ends: Vec<usize> =
                whole.iter().enumerate().filter(|(_, b)| **b == b'\n').map(|(at, _)| at + 1).collect();
            ends.push(ends[ends.len() / 2] - 10);
            for end in ends {
                std::fs::write(&log.0, &whole[..end]).unwrap();
                let (before, first) = read_from(&log.0, None);
                std::fs::write(&log.0, &whole).unwrap();
                let resume = first.resume.clone().unwrap();
                assert!(goes_on(&log.0, &resume));
                let (after, second) = read_from(&log.0, Some(resume.clone()));
                // What was read from where it goes on is read again; the rest stays.
                let from = resume["from"].as_f64().unwrap();
                let mut joined: Vec<Kept> = before.into_iter().filter(|piece| piece.1 < Some(from)).collect();
                joined.extend(after);
                assert_eq!(joined, all, "{name} grown at byte {end}");
                let title = second.info.get("tags").or(first.info.get("tags"));
                assert_eq!(title, ended.info.get("tags"), "{name} grown at byte {end}");
                let cwd = first.info.get("cwd").or(second.info.get("cwd"));
                assert_eq!(cwd, ended.info.get("cwd"), "{name} grown at byte {end}");
                assert!(first.info.get("cwd").is_none() || second.info.get("cwd").is_none());
            }
            // Changed before where it goes on: it's read again from the top.
            let resume = ended.resume.unwrap();
            let changed =
                String::from_utf8(whole).unwrap().replacen("balcony", "roof", 1).replacen("sourdough", "rye", 1);
            std::fs::write(&log.0, changed).unwrap();
            assert!(!goes_on(&log.0, &resume));
            std::fs::write(&log.0, b"").unwrap();
            assert!(!goes_on(&log.0, &resume));
        }
    }

    #[test]
    fn what_an_app_wraps_around_the_users_words_goes() {
        assert_eq!(
            users_words("Fix the tap.\n<system-reminder>\nToday is Monday.\n</system-reminder>\n"),
            "Fix the tap."
        );
        assert_eq!(users_words("  <ide_selection>line 4</ide_selection>Why does this fail?"), "Why does this fail?");
        assert_eq!(
            users_words("<pasted_content id=\"1\">\nerror: no such file\n</pasted_content>\nWhat now?"),
            "error: no such file\nWhat now?"
        );
        // In code, and where a tag isn't closed, it's what the user wrote.
        let code = "Is this right?\n```html\n<div>hello</div>\n```";
        assert_eq!(users_words(code), code);
        assert_eq!(users_words("<b is how bold starts"), "<b is how bold starts");
        assert_eq!(users_words("[Request interrupted by user for tool use]"), "");
    }

    #[test]
    fn times_are_read_as_seconds_since_1970() {
        assert_eq!(seconds("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(seconds("2000-03-01T00:00:00.250Z"), Some(951_868_800.25));
        assert_eq!(seconds("2026-10-25T11:41:07+02:00"), seconds("2026-10-25T09:41:07Z"));
        assert_eq!(seconds("yesterday"), None);
    }
}
