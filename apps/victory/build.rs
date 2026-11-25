//! Bakes `VICTORY_BUILD`: the commit the binary was built from.
//!
//! A log that cannot say which build wrote it cannot be compared with another
//! log, and `0.1.0` is the version of every build in this tree. The header
//! prints this, so a report of "it stalls here" carries the tree it came from.
//!
//! Nothing here may fail the build: no git (an unpacked source archive), no
//! repository, a shallow clone all fall back to `unknown`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let root = manifest.join("../..");
    // A worktree keeps `.git` as a file that points at the real directory, and
    // a submodule does too, so ask git instead of guessing `<root>/.git`.
    let git_dir = git(&root, &["rev-parse", "--absolute-git-dir"]).map(PathBuf::from);
    if let Some(dir) = &git_dir {
        // A new commit moves `HEAD`; a staged change moves `index`. Both make
        // this script run again, so the hash in the next build is the next
        // commit rather than the one before it.
        for watch in ["HEAD", "index"] {
            println!("cargo:rerun-if-changed={}", dir.join(watch).display());
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rustc-env=VICTORY_BUILD={}", build(&root, git_dir.is_some()));
}

/// `504b1c6`, or `504b1c6+dirty` when tracked files differ from that commit.
fn build(root: &Path, has_git: bool) -> String {
    if !has_git {
        return "unknown".to_owned();
    }
    let rev = git(root, &["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    // Tracked changes only: an untracked log or a scratch file does not make
    // the code differ from the commit the line names.
    let changes = git(
        root,
        &["status", "--porcelain", "--untracked-files=no"],
    )
    .unwrap_or_default();
    if changes.is_empty() {
        rev
    } else {
        format!("{rev}+dirty")
    }
}

/// The first line of git's output, or `None` when git is absent or says no.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        // A build script that waits for a credential prompt hangs the build;
        // and `status` would otherwise take the index lock of a parallel run.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(text.lines().next().unwrap_or_default().trim().to_owned())
}
