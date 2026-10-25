//! Clients of the engine, in this binary: the engine's client (client.rs), the terminal finder (finder.rs, term.rs),
//! search results printed (present.rs), previews (preview.rs), opening results (open.rs), `sift-it serve`, the relay
//! front ends talk to (serve.rs), and `sift-it mcp`, the MCP server agents talk to (mcp.rs). They speak the protocol.

pub mod client;
pub mod finder;
pub mod mcp;
pub mod open;
pub mod present;
pub mod preview;
pub mod serve;
pub mod term;

pub use client::Client;
