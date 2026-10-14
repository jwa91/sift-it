//! Generated bindings to llama.cpp's C API (see build.rs).
#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code, clippy::all)]

include!(concat!(env!("OUT_DIR"), "/llama.rs"));

/// The llama.cpp release the engine was built with.
pub const BUILD_TAG: &str = env!("SIFT_LLAMA_TAG");
