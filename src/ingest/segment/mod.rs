//! Segmenting: content cut into segments, each with its place in the file, by a strategy per content type. Pure:
//! nothing here reads a file or knows the model. Text is cut by its form (passages.rs): prose and Markdown, code, and
//! records. A conversation's messages are joined into exchanges (exchanges.rs), which are cut as text.

pub mod code;
pub mod exchanges;
pub mod frames;
pub mod passages;
pub mod pictures;
pub mod prose;
pub mod records;
pub mod sound;
pub mod speech;
