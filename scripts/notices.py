#!/usr/bin/env python3
"""Writes THIRD_PARTY_NOTICES to stdout: the licenses of what's built into sift-it and sift-it-transcribe.

    python3 scripts/notices.py > THIRD_PARTY_NOTICES

Run after `make build`, from the repository: it reads llama.cpp's license from vendor/llama.cpp, FluidAudio's from the
speech helper's SwiftPM checkout (target/swift), and every Rust crate built for macOS from cargo's registry, each crate's
license files as it ships them. Identical license texts are printed once, with the crates that carry them. The models
aren't in the archive (sift-it downloads them), but they're credited at the end.
"""
import json
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
LICENSE_NAMES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE")


def run(*args):
    return subprocess.run(args, cwd=ROOT, check=True, capture_output=True, text=True).stdout


def read(path):
    text = path.read_text(errors="replace").strip()
    if not text:
        sys.exit(f"notices: {path} is empty")
    return text


def license_files(folder):
    return sorted(
        p for p in folder.iterdir()
        if p.is_file() and p.name.upper().startswith(LICENSE_NAMES)
    )


def crates():
    """The third-party crates in the macOS build, by name and version, with their declared license and folder."""
    tree = run("cargo", "tree", "--locked", "--workspace", "-e", "normal", "--target", "aarch64-apple-darwin",
               "--prefix", "none", "--format", "{p}")
    built = set()
    for line in filter(None, tree.splitlines()):
        name, version = line.split(" ")[0:2]
        built.add((name, version.lstrip("v")))
    metadata = json.loads(run("cargo", "metadata", "--locked", "--format-version", "1"))
    members = set(metadata["workspace_members"])
    found = []
    for package in metadata["packages"]:
        if package["id"] in members or (package["name"], package["version"]) not in built:
            continue
        found.append((package["name"], package["version"], package.get("license") or "see its license file",
                      pathlib.Path(package["manifest_path"]).parent))
    return sorted(found)


def section(title, text):
    return f"{'=' * 100}\n{title}\n{'=' * 100}\n\n{text}\n"


def main():
    out = [
        "Third-party notices for sift-it and sift-it-transcribe\n\n"
        "sift-it is MIT licensed (LICENSE). It's built with the software below, under the licenses that follow.\n"
    ]

    llama = ROOT / "vendor/llama.cpp"
    out.append(section("llama.cpp and ggml (MIT), built into sift-it", read(llama / "LICENSE")))
    out.append(section(
        "stb_image and miniaudio, part of llama.cpp's mtmd",
        "stb_image (vendor/stb) is available as public domain or under the MIT license, and miniaudio "
        "(vendor/miniaudio) as public domain or under MIT No Attribution. Their licenses are in their headers in "
        "llama.cpp's source.",
    ))

    fluid = ROOT / "target/swift/checkouts/FluidAudio"
    texts = "\n\n".join(read(p) for p in license_files(fluid))
    out.append(section("FluidAudio (Apache-2.0), built into sift-it-transcribe", texts))

    by_text = {}
    listing = []
    for name, version, license, folder in crates():
        listing.append(f"{name} {version} ({license})")
        files = license_files(folder)
        if not files:
            by_text.setdefault(f"Licensed under {license}; the crate ships no license file.", []).append(
                f"{name} {version}")
        for path in files:
            by_text.setdefault(read(path), []).append(f"{name} {version}")
    out.append(section(f"Rust crates built into sift-it ({len(listing)})", "\n".join(listing)))
    for text, users in sorted(by_text.items(), key=lambda item: item[1][0]):
        out.append(section("Used by: " + ", ".join(users), text))

    out.append(section(
        "Models (downloaded by sift-it, not in this archive)",
        "EmbeddingGemma 2 by Google DeepMind, Apache-2.0: ggml-org/embeddinggemma-2-GGUF on Hugging Face.\n"
        "Parakeet Ultra by Moondream (a post-trained NVIDIA Parakeet TDT 0.6B v3), CC BY 4.0, in FluidInference's "
        "Core ML conversion: FluidInference/parakeet-ultra-coreml on Hugging Face.",
    ))
    sys.stdout.write("\n".join(out))


if __name__ == "__main__":
    main()
