// Links the llama.cpp build from scripts/build-llama.sh and generates Rust bindings for its C API (llama.h, mtmd.h).
use std::{env, fs, path::PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    // llama.cpp is built for the whole workspace, at its root.
    let build = manifest.join("../../vendor/llama-build");
    let lib = build.join("lib");
    let include = build.join("include");
    let tag = fs::read_to_string(build.join("TAG"))
        .expect("llama.cpp isn't built yet: run `make llama` (scripts/build-llama.sh)")
        .trim()
        .to_string();

    println!("cargo:rerun-if-changed={}", build.join("TAG").display());
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rustc-env=SIFT_LLAMA_TAG={tag}");

    println!("cargo:rustc-link-search=native={}", lib.display());
    for name in ["llama", "mtmd", "ggml", "ggml-base", "ggml-cpu", "ggml-metal", "ggml-blas", "vendor-hash"] {
        println!("cargo:rustc-link-lib=static={name}");
    }
    // Metal for llama.cpp, and Accelerate for ggml's BLAS backend.
    for framework in ["Accelerate", "Metal", "MetalKit", "Foundation"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    println!("cargo:rustc-link-lib=c++");

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", include.display()))
        .allowlist_function("(llama|mtmd|ggml_log)_.*")
        .allowlist_type("(llama|mtmd|ggml_log)_.*")
        .allowlist_var("(LLAMA|MTMD|GGML_LOG)_.*")
        .prepend_enum_name(false)
        .layout_tests(false)
        .generate()
        .expect("couldn't generate llama.cpp bindings");
    bindings.write_to_file(PathBuf::from(env::var("OUT_DIR").unwrap()).join("llama.rs")).unwrap();
}
