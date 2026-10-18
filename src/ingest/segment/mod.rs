//! Segmenting: content cut into segments, each with its place in the file, by a strategy per content type. Pure:
//! nothing here reads a file or knows the model. Text is cut by its form (passages.rs): prose and Markdown, code, and
//! records.

pub mod code;
pub mod frames;
pub mod passages;
pub mod pictures;
pub mod prose;
pub mod records;
pub mod sound;
pub mod speech;
