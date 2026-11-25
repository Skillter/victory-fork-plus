//! The fingerprint of the shader translator (`VICTORY_TRANSLATOR_HASH`),
//! which keys the on-disk shader cache (`src/shader_cache.rs`).
//!
//! A translation depends on the translator crate's source (`victory-shader`,
//! which uses no other crate of the workspace), on the cache's own encoding
//! of its result (`src/shader_cache.rs`), on the external crates' versions
//! (`Cargo.lock`) and on the compiler (`rust-toolchain.toml`). The cache was
//! keyed by the whole executable before: every rebuild, whatever it changed,
//! threw away every translation (2,465 shaders translated again at the next
//! start, 2026-11-25).

use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let root = manifest.join("../..");
    let inputs = [
        root.join("crates/victory-shader/src"),
        root.join("crates/victory-shader/Cargo.toml"),
        manifest.join("src/shader_cache.rs"),
        root.join("Cargo.lock"),
        root.join("rust-toolchain.toml"),
    ];
    let mut files = Vec::new();
    for input in &inputs {
        // A directory is watched whole (any file in it changing reruns this).
        println!("cargo:rerun-if-changed={}", input.display());
        collect(input, &mut files);
    }
    files.sort();
    let mut hasher = blake3::Hasher::new();
    for file in &files {
        // The path relative to the root, so a checkout elsewhere hashes the same.
        let name = file.strip_prefix(&root).unwrap_or(file).to_string_lossy().replace('\\', "/");
        let bytes = std::fs::read(file).unwrap_or_default();
        // Line endings as git may check them out either way.
        let text: Vec<u8> = bytes.into_iter().filter(|&b| b != b'\r').collect();
        hasher.update(name.as_bytes());
        hasher.update(&[0]);
        hasher.update(&(text.len() as u64).to_le_bytes());
        hasher.update(&text);
    }
    println!("cargo:rustc-env=VICTORY_TRANSLATOR_HASH={}", hasher.finalize().to_hex());
}

fn collect(path: &Path, files: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            collect(&entry.path(), files);
        }
    } else if path.is_file() {
        files.push(path.to_owned());
    }
}
