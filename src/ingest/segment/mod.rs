//! Segmenting: content cut into segments, each with its place in the file, by a strategy per content type. Pure:
//! nothing here reads a file or knows the model.

pub mod frames;
pub mod pictures;
pub mod prose;
pub mod sound;
pub mod speech;
