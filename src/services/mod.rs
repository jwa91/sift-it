//! The processes: the engine (search and the control plane, engine.rs) and the indexer (ingest, indexer.rs), each in a
//! LaunchAgent of its own (launchd.rs). They wire parts together and hold no logic of their own.

pub mod engine;
pub mod indexer;
pub mod launchd;
