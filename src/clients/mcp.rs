//! `sift-it mcp`: an MCP server over stdio, for agents. Two read-only tools: `search` the files on this Mac by what's in
//! them, and past Claude Code and Codex conversations; `read` a result's text with what's around it. It asks the engine
//! over its socket, as every client does (client.rs), so the model stays in the engine and nothing loads here.
//!
//! Both eras of MCP: a request that names its protocol version in `_meta` is answered on its own, as the current
//! revision (2026-07-28) says; an `initialize` handshake (2025-11-25 and before) makes this process answer the revision
//! it agreed. JSON-RPC, a message a line on stdin and stdout; nothing else goes to stdout.

use super::Client;
use super::open::shell_quoted;
use super::present::place;
use anyhow::Result;
use protocol::{Hit, Kind, Mode, Op, Results, Search, Text};
use serde_json::{Map, Value, json};
use std::io::{BufRead, Write};

/// The revision answered request by request.
const MODERN: &str = "2026-07-28";
/// The revisions with a handshake, newest first: an `initialize` for another gets the newest.
const LEGACY: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const CAPABILITIES_KEY: &str = "io.modelcontextprotocol/clientCapabilities";

/// A search returns this many results unless asked, at most MAX_RESULTS.
const RESULTS: usize = 10;
const MAX_RESULTS: usize = 50;
/// `read` returns this many passages before and after the one asked for unless asked, at most MAX_AROUND.
const AROUND: usize = 1;
const MAX_AROUND: usize = 5;

const INSTRUCTIONS: &str = "Sift It searches the files on this Mac by what's in them (documents, notes, PDFs, \
pictures, audio, video) and past Claude Code and Codex conversations. `search` finds them; `read` gives a result's \
text with what's around it.";

/// Answers MCP messages from stdin until it closes, asking the engine (started when it isn't running).
pub fn run() -> Result<()> {
    let mut server = Server::default();
    let mut engine: Option<Client> = None;
    let mut ask = |op: Op| -> Result<Value> {
        if let Some(client) = engine.as_mut()
            && let Ok(reply) = client.request(op.clone())
        {
            return Ok(reply);
        }
        // Not connected yet, or the engine went meanwhile (stopped, or replaced by a new build): a new connection.
        engine.insert(Client::connect()?).request(op)
    };
    let mut stdout = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        if let Some(reply) = server.handle(&line?, &mut ask) {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// What a request asks the engine: an op, and its reply as JSON.
type Ask<'a> = &'a mut dyn FnMut(Op) -> Result<Value>;

/// A JSON-RPC error: its code and message, and what it says besides.
struct Failure(i64, String, Option<Value>);

impl Failure {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self(code, message.into(), None)
    }
}

const INVALID_REQUEST: i64 = -32600;
const NO_SUCH_METHOD: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const UNSUPPORTED_VERSION: i64 = -32022;

/// The server's state: the revision an `initialize` agreed, if one came. Requests that name their own aren't bound
/// to it.
#[derive(Default)]
pub struct Server {
    legacy: Option<&'static str>,
}

impl Server {
    /// The reply to a line, or None for a notification (or a response, which it never asked for).
    pub fn handle(&mut self, line: &str, ask: Ask) -> Option<String> {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(error) => return Some(failure(&Value::Null, Failure::new(-32700, format!("not JSON: {error}")))),
        };
        if !message.is_object() {
            return Some(failure(&Value::Null, Failure::new(INVALID_REQUEST, "one message a line, not a batch")));
        }
        let (id, method) = (message.get("id")?, message["method"].as_str()?);
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        let named = params["_meta"][VERSION_KEY].as_str();
        let modern = named == Some(MODERN);
        let result = match named {
            Some(MODERN) => modern_request(method, &params, ask),
            // An earlier revision, named on the request rather than agreed by `initialize`: answered by its rules.
            Some(version) if LEGACY.contains(&version) => answer(method, &params, ask),
            Some(version) => Err(Failure(
                UNSUPPORTED_VERSION,
                "Unsupported protocol version".into(),
                Some(json!({ "supported": supported(), "requested": version })),
            )),
            None if method == "initialize" => Ok(self.initialize(&params)),
            None if self.legacy.is_some() => answer(method, &params, ask),
            None => Err(Failure(
                INVALID_PARAMS,
                format!("name the protocol version in _meta.{VERSION_KEY}, or start with initialize"),
                Some(json!({ "supported": supported() })),
            )),
        };
        Some(match result {
            Ok(mut result) => {
                if modern {
                    result["resultType"] = json!("complete");
                    result["_meta"] = json!({ "io.modelcontextprotocol/serverInfo": server_info() });
                }
                json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
            }
            Err(error) => failure(id, error),
        })
    }

    /// The handshake of the revisions before 2026-07-28: the one asked for when it's known, else the newest.
    fn initialize(&mut self, params: &Value) -> Value {
        let asked = params["protocolVersion"].as_str().unwrap_or_default();
        let agreed = LEGACY.into_iter().find(|version| *version == asked).unwrap_or(LEGACY[0]);
        self.legacy = Some(agreed);
        json!({
            "protocolVersion": agreed,
            "capabilities": { "tools": {} },
            "serverInfo": server_info(),
            "instructions": INSTRUCTIONS,
        })
    }
}

/// A request in the current revision, which says what the client can do.
fn modern_request(method: &str, params: &Value, ask: Ask) -> Result<Value, Failure> {
    if !params["_meta"][CAPABILITIES_KEY].is_object() {
        return Err(Failure::new(INVALID_PARAMS, format!("_meta.{CAPABILITIES_KEY} is missing")));
    }
    match method {
        "server/discover" => Ok(json!({
            "supportedVersions": supported(),
            "capabilities": { "tools": {} },
            "instructions": INSTRUCTIONS,
        })),
        _ => answer(method, params, ask),
    }
}

/// What both revisions ask the same way.
fn answer(method: &str, params: &Value, ask: Ask) -> Result<Value, Failure> {
    match method {
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let outcome = match params["name"].as_str() {
                Some("search") => search(&arguments, ask),
                Some("read") => read(&arguments, ask),
                name => return Err(Failure::new(INVALID_PARAMS, format!("no tool {}", name.unwrap_or("named")))),
            };
            // What went wrong is the tool's answer, so the model can do it differently.
            Ok(match outcome {
                Ok(result) => result,
                Err(error) => json!({ "content": [{ "type": "text", "text": format!("{error:#}") }], "isError": true }),
            })
        }
        _ => Err(Failure::new(NO_SUCH_METHOD, format!("no method {method}"))),
    }
}

fn supported() -> Vec<&'static str> {
    std::iter::once(MODERN).chain(LEGACY).collect()
}

fn server_info() -> Value {
    json!({ "name": "sift-it", "title": "Sift It", "version": env!("CARGO_PKG_VERSION") })
}

fn failure(id: &Value, Failure(code, message, data): Failure) -> String {
    let mut error = json!({ "code": code, "message": message });
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error }).to_string()
}

/// The tools, read-only.
fn tools() -> Value {
    let kinds: Vec<&str> = Kind::ALL.iter().map(|kind| kind.as_str()).collect();
    let read_only =
        json!({ "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false });
    json!([
        {
            "name": "search",
            "title": "Search files and conversations",
            "description": "Search this Mac's files by what's in them (documents, notes, PDFs, pictures, audio, video) \
                and past Claude Code and Codex conversations (\"the conversation about X\"). Describe what you're \
                looking for in your own words; put exact words, names or numbers in \"quotes\". Each result says \
                where it matched; `read` gives its text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to look for. \"Quoted\" words must all be in it." },
                    "kinds": {
                        "type": "array",
                        "items": { "type": "string", "enum": kinds },
                        "description": "Only these kinds of file (all but code when left out)."
                    },
                    "by": {
                        "type": "string",
                        "enum": ["meaning", "words"],
                        "description": "By meaning (the default), or by the words alone."
                    },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_RESULTS, "default": RESULTS }
                },
                "required": ["query"],
                "additionalProperties": false
            },
            "annotations": read_only,
        },
        {
            "name": "read",
            "title": "Read a result",
            "description": "Read a search result's text with the passages around it (in a conversation, the \
                exchanges before and after), to use what's in a file without opening it. Or a file's text, by its \
                path.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "A search result's id." },
                    "around": {
                        "type": "integer", "minimum": 0, "maximum": MAX_AROUND, "default": AROUND,
                        "description": "Passages before and after it."
                    },
                    "path": { "type": "string", "description": "A file's path instead: the start of its text." }
                },
                "additionalProperties": false
            },
            "annotations": read_only,
        }
    ])
}

/// A search, its results as the model reads them.
fn search(arguments: &Value, ask: Ask) -> Result<Value> {
    let q = arguments["query"].as_str().ok_or_else(|| anyhow::anyhow!("what to search for? (query)"))?;
    let mut kinds = Vec::new();
    for name in arguments["kinds"].as_array().into_iter().flatten() {
        let kind = name.as_str().and_then(Kind::parse);
        let known = || Kind::ALL.map(Kind::as_str).join(", ");
        kinds.push(kind.ok_or_else(|| anyhow::anyhow!("no kind {name}: the kinds are {}", known()))?);
    }
    let mode = match arguments["by"].as_str() {
        None => None,
        Some("meaning") => Some(Mode::Meaning),
        Some("words") => Some(Mode::Words),
        Some(by) => anyhow::bail!("search by meaning or words, not {by}"),
    };
    let limit = arguments["limit"].as_u64().map_or(RESULTS, |limit| (limit as usize).clamp(1, MAX_RESULTS));
    // By meaning, it waits for the engine to load the model if it hasn't yet.
    let request = Search { q: q.to_string(), mode, kinds, limit, prefix_last: false, wait: true };
    let found: Results = serde_json::from_value(ask(Op::Search(request))?)?;
    let results: Vec<Value> = found.hits.iter().map(result).collect();
    Ok(structured(json!({ "results": results, "by_meaning": found.meaning })))
}

/// A result: what `read` takes (its id), the file, what it's called, where it matched, the text there, how well it
/// matched, and for a conversation the command that resumes it.
fn result(hit: &Hit) -> Value {
    let mut result = Map::new();
    result.insert("id".into(), json!(hit.segment_id));
    result.insert("title".into(), json!(hit.name));
    result.insert("path".into(), json!(hit.path));
    result.insert("kind".into(), json!(hit.kind));
    if let Some(place) = place(hit) {
        result.insert("place".into(), json!(place));
    }
    if let Some(excerpt) = &hit.excerpt {
        result.insert("excerpt".into(), json!(excerpt));
    }
    result.insert("score".into(), json!((f64::from(hit.score) * 1000.0).round() / 1000.0));
    result.insert("strong".into(), json!(hit.strong));
    if let Some(command) = hit.info["resume"].as_array() {
        let words: Vec<String> = command.iter().filter_map(|word| word.as_str()).map(shell_quoted).collect();
        let line = match hit.info["cwd"].as_str() {
            Some(cwd) => format!("cd {} && {}", shell_quoted(cwd), words.join(" ")),
            None => words.join(" "),
        };
        result.insert("resume".into(), json!(line));
    }
    Value::Object(result)
}

/// A result's text with what's around it, or a file's.
fn read(arguments: &Value, ask: Ask) -> Result<Value> {
    let around = arguments["around"].as_u64().map_or(AROUND, |around| (around as usize).min(MAX_AROUND));
    let op = match (arguments["id"].as_i64(), arguments["path"].as_str()) {
        (Some(id), _) => Op::Text { segment_id: Some(id), path: None, around: Some(around) },
        (None, Some(path)) => Op::Text { segment_id: None, path: Some(path.to_string()), around: None },
        (None, None) => anyhow::bail!("which result (id) or file (path)?"),
    };
    let text: Text = serde_json::from_value(ask(op)?)?;
    let text =
        text.text.ok_or_else(|| anyhow::anyhow!("nothing to read there: search again, the file may have changed"))?;
    Ok(json!({ "content": [{ "type": "text", "text": text }] }))
}

/// A tool's structured result, with the same as JSON text for clients that read only text.
fn structured(value: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": value.to_string() }], "structuredContent": value })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A conversation and a document the engine finds, with made-up content, and the text around a result.
    fn engine(asked: &mut Vec<Op>) -> impl FnMut(Op) -> Result<Value> + '_ {
        move |op| {
            asked.push(op.clone());
            Ok(match op {
                Op::Search(_) => json!({
                    "meaning": true, "ms": 4.2,
                    "hits": [
                        {
                            "path": "/Users/someone/.claude/projects/-Users-someone-garden/6f1c2a9e.jsonl",
                            "name": "Balcony vegetables", "kind": "conversation", "segment": "exchange",
                            "segment_id": 41, "loc": 1777626000.0, "loc_end": 1777626012.0, "score": 0.18349,
                            "lift": 0.2, "cosine": 0.61, "meaning": true, "words": true, "strong": true,
                            "excerpt": "User: Which tomatoes grow well on a balcony? Claude: Cherry tomatoes…",
                            "modified": 1777629730.0, "size": 8200,
                            "info": {"agent": "claude", "session": "6f1c2a9e", "cwd": "/Users/someone/my garden",
                                     "resume": ["claude", "--resume", "6f1c2a9e"], "tags": {"title": "Balcony vegetables"}}
                        },
                        {
                            "path": "/Users/someone/Documents/Seeds.pdf", "name": "Seeds.pdf", "kind": "document",
                            "segment": "page", "segment_id": 7, "loc": 3.0, "loc_end": null, "score": 0.12,
                            "lift": 0.1, "cosine": 0.5, "meaning": true, "words": false, "strong": false,
                            "excerpt": "Sow tomatoes indoors in March.", "modified": 1.0, "size": 1, "info": {}
                        }
                    ]
                }),
                Op::Text { .. } => json!({ "text": "Before.\n\nUser: Which tomatoes…\n\nAfter." }),
                _ => json!({}),
            })
        }
    }

    fn reply(server: &mut Server, message: Value, asked: &mut Vec<Op>) -> Value {
        serde_json::from_str(&server.handle(&message.to_string(), &mut engine(asked)).unwrap()).unwrap()
    }

    fn modern(id: i64, method: &str, mut params: Value) -> Value {
        params["_meta"] = json!({ VERSION_KEY: MODERN, CAPABILITIES_KEY: {}, "io.modelcontextprotocol/clientInfo": {"name": "test"} });
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    #[test]
    fn a_session_with_a_handshake_lists_and_calls_the_tools() {
        let (mut server, mut asked) = (Server::default(), Vec::new());
        let before = reply(&mut server, json!({"jsonrpc": "2.0", "id": 0, "method": "tools/list"}), &mut asked);
        assert_eq!(before["error"]["code"], INVALID_PARAMS);
        let initialize = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}});
        let initialized = reply(&mut server, initialize, &mut asked);
        assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(initialized["result"]["serverInfo"]["name"], "sift-it");
        let notification = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert_eq!(server.handle(&notification.to_string(), &mut engine(&mut asked)), None);

        let listed = reply(&mut server, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}), &mut asked);
        let names: Vec<&str> =
            listed["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["search", "read"]);
        assert!(listed["result"].get("resultType").is_none());

        let call = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "search", "arguments": {"query": "tomatoes on a balcony", "kinds": ["conversation"], "limit": 5}}});
        let found = reply(&mut server, call, &mut asked);
        let Op::Search(search) = &asked[0] else { panic!() };
        assert_eq!(
            (search.q.as_str(), search.kinds.as_slice(), search.limit, search.wait),
            ("tomatoes on a balcony", [Kind::Conversation].as_slice(), 5, true)
        );
        let results = &found["result"]["structuredContent"]["results"];
        assert_eq!(results[0]["id"], 41);
        assert_eq!(results[0]["title"], "Balcony vegetables");
        assert_eq!(results[0]["score"], 0.183);
        assert_eq!(results[0]["resume"], "cd '/Users/someone/my garden' && claude --resume 6f1c2a9e");
        assert_eq!(results[1]["place"], "page 3");
        assert!(results[1].get("resume").is_none());
        let text: Value = serde_json::from_str(found["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, found["result"]["structuredContent"]);

        let other = json!({"jsonrpc": "2.0", "id": 4, "method": "resources/list"});
        assert_eq!(reply(&mut server, other, &mut asked)["error"]["code"], NO_SUCH_METHOD);
    }

    #[test]
    fn a_request_that_names_its_version_is_answered_on_its_own() {
        let (mut server, mut asked) = (Server::default(), Vec::new());
        let discovered = reply(&mut server, modern(1, "server/discover", json!({})), &mut asked);
        assert_eq!(discovered["result"]["supportedVersions"][0], MODERN);
        assert_eq!(discovered["result"]["resultType"], "complete");
        assert_eq!(discovered["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "sift-it");

        let call = modern(2, "tools/call", json!({"name": "read", "arguments": {"id": 41}}));
        let read = reply(&mut server, call, &mut asked);
        assert_eq!(asked[0], Op::Text { segment_id: Some(41), path: None, around: Some(AROUND) });
        assert_eq!(read["result"]["content"][0]["text"], "Before.\n\nUser: Which tomatoes…\n\nAfter.");
        assert_eq!(read["result"]["resultType"], "complete");

        let mut old = modern(3, "tools/list", json!({}));
        old["params"]["_meta"][VERSION_KEY] = json!("1900-01-01");
        let refused = reply(&mut server, old, &mut asked);
        assert_eq!(refused["error"]["code"], UNSUPPORTED_VERSION);
        assert_eq!(refused["error"]["data"]["requested"], "1900-01-01");
        let mut earlier = modern(5, "tools/list", json!({}));
        earlier["params"]["_meta"][VERSION_KEY] = json!("2025-11-25");
        let listed = reply(&mut server, earlier, &mut asked);
        assert!(listed["result"]["tools"].is_array() && listed["result"].get("resultType").is_none());
        let mut bare = modern(4, "tools/list", json!({}));
        bare["params"]["_meta"].as_object_mut().unwrap().remove(CAPABILITIES_KEY);
        assert_eq!(reply(&mut server, bare, &mut asked)["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn a_call_the_model_can_fix_is_the_tools_answer() {
        let (mut server, mut asked) = (Server::default(), Vec::new());
        let wrong = modern(1, "tools/call", json!({"name": "search", "arguments": {"query": "x", "kinds": ["poems"]}}));
        let answer = reply(&mut server, wrong, &mut asked);
        assert_eq!(answer["result"]["isError"], true);
        assert!(answer["result"]["content"][0]["text"].as_str().unwrap().contains("conversation"));
        assert!(asked.is_empty());
        let unknown = modern(2, "tools/call", json!({"name": "delete", "arguments": {}}));
        assert_eq!(reply(&mut server, unknown, &mut asked)["error"]["code"], INVALID_PARAMS);
        let garbled = server.handle("{not json", &mut engine(&mut asked)).unwrap();
        assert!(garbled.contains("-32700"));
    }
}
