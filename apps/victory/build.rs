//! Bakes the commit the binary was built from: `VICTORY_COMMIT` (full),
//! `VICTORY_COMMIT_SHORT` (eight characters), `VICTORY_BRANCH` and
//! `VICTORY_DIRTY`.
//!
//! A log that cannot say which build wrote it cannot be compared with another
//! log, and `0.1.0` is the version of every build in this tree. The header
//! prints this, so a report of "it stalls here" carries the tree it came from.
//! A header that names a commit no build ever contained is worse than none:
//! every fact below is read from the repository at the moment the script runs,
//! and the script reruns on every path that can change an answer.
//!
//! Nothing here may fail the build: no git (an unpacked source archive), no
//! repository, a shallow clone all fall back to `unknown`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let root = tidy(&manifest.join("../.."));
    // A worktree keeps `.git` as a file that points at the real directory, and
    // a submodule does too, so ask git instead of guessing `<root>/.git`.
    let git_dir = git(&root, &["rev-parse", "--absolute-git-dir"]).map(PathBuf::from);
    // A linked worktree has two: `HEAD` and `index` of its own, and the refs of
    // the repository they are checked out from. A commit on this branch moves
    // the ref in the common directory, which is not the one above.
    let common = git(&root, &["rev-parse", "--git-common-dir"]).map(|path| {
        let path = PathBuf::from(path);
        if path.is_relative() { root.join(path) } else { path }
    });
    watch(&manifest, &root, git_dir.as_deref(), common.as_deref());
    let facts = facts(&root, git_dir.is_some());
    println!("cargo:rustc-env=VICTORY_COMMIT={}", facts.commit);
    println!("cargo:rustc-env=VICTORY_COMMIT_SHORT={}", facts.short);
    println!("cargo:rustc-env=VICTORY_BRANCH={}", facts.branch);
    println!("cargo:rustc-env=VICTORY_DIRTY={}", facts.dirty);
}

/// `path` with every `..` resolved, so the watcher list a build writes names a
/// path a human recognises when it reruns unexpectedly.
fn tidy(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            part => out.push(part),
        }
    }
    out
}

/// Everything that can change an answer above reruns this script.
fn watch(manifest: &Path, root: &Path, git_dir: Option<&Path>, common: Option<&Path>) {
    // A new commit moves `HEAD`; a staged change moves `index`. `HEAD` itself
    // is `ref: refs/heads/<name>` and stays put, so the file a commit writes is
    // the ref, and `packed-refs` holds it when git packed it.
    if let Some(dir) = git_dir {
        for path in ["HEAD", "index"] {
            println!("cargo:rerun-if-changed={}", dir.join(path).display());
        }
    }
    if let Some(dir) = common.or(git_dir) {
        for path in ["refs/heads", "packed-refs"] {
            println!("cargo:rerun-if-changed={}", dir.join(path).display());
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    for file in ["Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={}", root.join(file).display());
    }
    // The paths `git status` is asked about, one file at a time: a directory's
    // own mtime does not move when a file inside it is edited, and a stale hash
    // in the header is what this list exists for. The directories are named too,
    // so a file added or removed reruns the script as well.
    for dir in [manifest.join("src"), root.join("crates")] {
        println!("cargo:rerun-if-changed={}", dir.display());
        sources(&dir, &mut |file| {
            println!("cargo:rerun-if-changed={}", file.display());
        });
    }
}

/// Every `.rs` file under `dir`, at any depth, in a stable order.
fn sources(dir: &Path, each: &mut impl FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|entry| entry.ok()).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            sources(&path, each);
        } else if path.extension().is_some_and(|kind| kind == "rs") {
            each(&path);
        }
    }
}

/// What the build is of.
struct Facts {
    commit: String,
    short: String,
    branch: String,
    dirty: String,
}

impl Facts {
    fn unknown() -> Self {
        Self {
            commit: "unknown".to_owned(),
            short: "unknown".to_owned(),
            branch: "unknown".to_owned(),
            dirty: "unknown".to_owned(),
        }
    }
}

/// The commit under `HEAD` and the state of the tree over it.
fn facts(root: &Path, has_git: bool) -> Facts {
    if !has_git {
        return Facts::unknown();
    }
    let Some(commit) = git(root, &["rev-parse", "HEAD"]) else {
        return Facts::unknown();
    };
    // `--short=8` is at least eight characters and more when eight do not tell
    // two commits apart, which is what makes it safe to print in a caption.
    let short = git(root, &["rev-parse", "--short=8", "HEAD"])
        .unwrap_or_else(|| commit.chars().take(8).collect());
    let branch =
        git(root, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_else(|| "detached".to_owned());
    // Only the paths the binary is made of: an untracked log or a scratch file
    // elsewhere in the tree does not make the build differ from the commit.
    // Tracked changes only, and `unknown` when git cannot answer at all rather
    // than a `false` that was never read.
    let dirty = match git(
        root,
        &[
            "status",
            "--porcelain",
            "--untracked-files=no",
            "--",
            "crates",
            "apps",
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
        ],
    ) {
        Some(first) if first.is_empty() => "false".to_owned(),
        Some(_) => "true".to_owned(),
        None => "unknown".to_owned(),
    };
    Facts {
        commit,
        short,
        branch,
        dirty,
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
