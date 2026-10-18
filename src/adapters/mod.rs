//! The edge: Apple's frameworks (macos/) and the speech helper process (speech.rs). The domain reaches them through
//! narrow calls; nothing here decides what's indexed or how it ranks. The embedding model is a crate of its own
//! (crates/llama).

pub mod macos;
pub mod speech;
