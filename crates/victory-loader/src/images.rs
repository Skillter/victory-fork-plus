//! Keeps the directory of patched image copies from growing for ever.
//!
//! [`crate::load`] writes one copy per image per build, named after the
//! original's BLAKE3 (see [`PreparedImage::file_name`]), so a title whose
//! build changes, or a second title, leaves the previous copies behind
//! untouched. A real machine had five of them, 121.6 MB together.
//!
//! The rule is the shader cache's (`victory_pipeline::cache::Cache::prune`):
//! drop what has gone a month untouched, then the oldest copies until the
//! directory fits in two gigabytes. It is written out here rather than
//! shared because that pruner walks only its own `<root>/<two hex digits>/
//! <file>` layout, and these copies are flat files directly in the
//! directory; its bounds are private to it as well.
//!
//! [`PreparedImage::file_name`]: crate::PreparedImage::file_name

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use crate::prepare::{IMAGE_NAME_PREFIX, IMAGE_NAME_SUFFIX};

/// How long a copy may go untouched before a pass drops it. A copy older than
/// this belongs to a build nothing has played for a month; keeping it costs
/// the size of an image, and re-writing it costs one patch of bytes already
/// read.
pub const MAX_IDLE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// What the directory may keep after the age pass, counted in bytes. A single
/// title measured 121.6 MB for five images, so two gigabytes holds many
/// builds before the oldest are dropped.
pub const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The rule a pass applies. Every field is a parameter so a test can run the
/// policy without waiting a month or filling 2 GiB.
#[derive(Debug, Clone, Copy)]
pub struct PrunePolicy {
    /// Copies last modified longer ago than this are dropped.
    pub max_idle: Duration,
    /// After the age pass, the oldest copies go until the directory holds at
    /// most this many bytes.
    pub max_bytes: u64,
}

impl Default for PrunePolicy {
    fn default() -> Self {
        Self {
            max_idle: MAX_IDLE,
            max_bytes: MAX_BYTES,
        }
    }
}

/// What one pass did to one directory.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Pruned {
    /// Copies removed, by either pass.
    pub entries: u64,
    /// Bytes they occupied.
    pub bytes: u64,
    /// Copies left in the directory.
    pub remaining_entries: u64,
    /// Bytes they occupy.
    pub remaining_bytes: u64,
}

/// Drops the copies in `dir` that no run will ask for again and logs one line
/// when it removed anything. `live` is the copy this run is about to map: its
/// name is never a candidate, whatever its timestamp says.
///
/// A copy that cannot be removed (another run has it mapped) is a warning: a
/// cache that cannot be pruned must not stop the run. The bytes counted are
/// what the directory holds after the pass; the copy about to be written is
/// not counted until the next pass sees it.
pub fn prune(dir: &Path, live: &Path) -> Pruned {
    // Every copy this process maps is spared for the rest of the run, not only
    // the one about to be written: `write_if_changed` leaves an unchanged copy
    // with its old timestamp, so a month-old main image is still the file the
    // loader has mapped, and the next title DLL's pass must not try to remove
    // it (Windows refuses, and the warning would repeat for every DLL).
    if let Ok(mut names) = mapped().lock()
        && !names.iter().any(|p| p == live)
    {
        names.push(live.to_path_buf());
    }
    let pruned = prune_with(dir, live, PrunePolicy::default(), SystemTime::now());
    if pruned.entries > 0 {
        tracing::info!(
            entries = pruned.entries,
            bytes = pruned.bytes,
            remaining_entries = pruned.remaining_entries,
            remaining_bytes = pruned.remaining_bytes,
            dir = %dir.display(),
            "pruned the image cache"
        );
    }
    pruned
}

/// Copies this process has written or mapped, by full path. Only [`prune`]
/// fills it; [`prune_with`] reads it.
fn mapped() -> &'static Mutex<Vec<PathBuf>> {
    static MAPPED: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
    MAPPED.get_or_init(|| Mutex::new(Vec::new()))
}

/// [`prune`] with the clock and the policy given, for tests.
///
/// Only copies named like [`PreparedImage::file_name`] writes them are
/// considered, and only directly in `dir`: a staging `.tmp`, a crash dump or a
/// subdirectory is somebody else's and is neither read nor removed. A symlink
/// is never followed.
///
/// [`PreparedImage::file_name`]: crate::PreparedImage::file_name
pub fn prune_with(dir: &Path, live: &Path, policy: PrunePolicy, now: SystemTime) -> Pruned {
    let mut pruned = Pruned::default();
    if !dir.is_dir() {
        // Nothing was ever written here.
        return pruned;
    }
    // `write_if_changed` leaves an unchanged copy in place, so the copy this
    // run maps can carry a month-old timestamp: it is protected by name, and
    // every copy mapped earlier in the run by path.
    let live = live.file_name();
    let protected = mapped().lock().map(|names| names.clone()).unwrap_or_default();
    let is_live = |entry: &Entry| {
        Some(entry.name.as_os_str()) == live || protected.iter().any(|p| p == &entry.path)
    };
    let mut kept: Vec<Entry> = Vec::new();
    for entry in entries(dir) {
        // A time in the future (a clock that moved) counts as untouched.
        let idle = now.duration_since(entry.modified).unwrap_or_default();
        if !is_live(&entry) && idle > policy.max_idle && remove(&entry, &mut pruned) {
            continue;
        }
        kept.push(entry);
    }
    let mut remaining_bytes: u64 = kept.iter().map(|e| e.len).sum();
    let mut remaining_entries = kept.len() as u64;
    if remaining_bytes > policy.max_bytes {
        // Oldest first, so the builds this player stopped using go before
        // the one just started.
        kept.sort_by_key(|e| e.modified);
        for entry in &kept {
            if remaining_bytes <= policy.max_bytes {
                break;
            }
            if is_live(entry) || !remove(entry, &mut pruned) {
                continue;
            }
            remaining_bytes = remaining_bytes.saturating_sub(entry.len);
            remaining_entries -= 1;
        }
    }
    pruned.remaining_entries = remaining_entries;
    pruned.remaining_bytes = remaining_bytes;
    pruned
}

/// One copy the loader wrote.
struct Entry {
    path: PathBuf,
    name: OsString,
    modified: SystemTime,
    len: u64,
}

/// Removes one copy, counting it in `pruned`. `false` when it stayed.
fn remove(entry: &Entry, pruned: &mut Pruned) -> bool {
    match std::fs::remove_file(&entry.path) {
        Ok(()) => {
            pruned.entries += 1;
            pruned.bytes += entry.len;
            true
        }
        Err(error) => {
            tracing::warn!(path = %entry.path.display(), %error, "cannot prune an image copy");
            false
        }
    }
}

/// Every copy directly in `dir`, from a single walk. Files that vanish while
/// the walk runs are another run's business and are skipped.
fn entries(dir: &Path) -> Vec<Entry> {
    let mut found = Vec::new();
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) => {
            tracing::warn!(dir = %dir.display(), %error, "cannot read the image cache to prune it");
            return found;
        }
    };
    for file in read.flatten() {
        let name = file.file_name();
        // `file_type` and `metadata` describe the entry itself, so a symlink
        // is skipped rather than followed out of the cache.
        if !is_image_copy(&name) || !file.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let Ok(meta) = file.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        found.push(Entry {
            path: file.path(),
            name,
            modified,
            len: meta.len(),
        });
    }
    found
}

/// Whether `name` is what [`PreparedImage::file_name`] writes. Anything else
/// in the directory came from someone else.
///
/// [`PreparedImage::file_name`]: crate::PreparedImage::file_name
fn is_image_copy(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|s| s.starts_with(IMAGE_NAME_PREFIX) && s.ends_with(IMAGE_NAME_SUFFIX))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A copy where the loader would put one, with a chosen modification time.
    fn planted(dir: &Path, name: &str, len: usize, when: SystemTime) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, vec![b'x'; len]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(when)
            .unwrap();
        path
    }

    /// A copy's name as `PreparedImage::file_name` spells it: 16 hex digits.
    fn copy_name(n: u64) -> String {
        format!("victory-image-{n:016x}.dll")
    }

    fn ago(now: SystemTime, days: u64) -> SystemTime {
        now - Duration::from_secs(days * 24 * 60 * 60)
    }

    fn a_fixed_now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 * 24 * 60 * 60)
    }

    #[test]
    fn prunes_copies_idle_past_the_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("images");
        let now = a_fixed_now();
        let stale = planted(&dir, &copy_name(0xaa), 100, ago(now, 40));
        let recent = planted(&dir, &copy_name(0xbb), 40, ago(now, 1));
        let live = dir.join(copy_name(0xcc));

        let pruned = prune_with(&dir, &live, PrunePolicy::default(), now);
        assert_eq!(pruned.entries, 1);
        assert_eq!(pruned.bytes, 100);
        assert_eq!(pruned.remaining_entries, 1);
        assert_eq!(pruned.remaining_bytes, 40);
        assert!(!stale.exists(), "forty days untouched is gone");
        assert!(recent.exists(), "yesterday's copy stays");
    }

    #[test]
    fn keeps_the_copy_this_run_maps() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("images");
        let now = a_fixed_now();
        // The copy is unchanged since it was written, so `write_if_changed`
        // left it old; it is still the one about to be mapped.
        let live = planted(&dir, &copy_name(0xaa), 100, ago(now, 40));
        let other = planted(&dir, &copy_name(0xbb), 100, ago(now, 40));

        let pruned = prune_with(&dir, &live, PrunePolicy::default(), now);
        assert_eq!(pruned.entries, 1);
        assert!(live.exists(), "the copy this run maps survives any age");
        assert!(!other.exists(), "the other old build went");
    }

    #[test]
    fn drops_the_oldest_copies_until_under_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("images");
        let now = a_fixed_now();
        let mut paths = Vec::new();
        for i in 0..5u64 {
            paths.push(planted(&dir, &copy_name(i), 100, ago(now, 10 - i)));
        }
        let live = dir.join(copy_name(0xff));

        let policy = PrunePolicy {
            max_bytes: 250,
            ..PrunePolicy::default()
        };
        let pruned = prune_with(&dir, &live, policy, now);
        assert_eq!((pruned.entries, pruned.bytes), (3, 300));
        assert_eq!((pruned.remaining_entries, pruned.remaining_bytes), (2, 200));
        let left: Vec<bool> = paths.iter().map(|p| p.exists()).collect();
        assert_eq!(left, [false, false, false, true, true], "the oldest went");
    }

    #[test]
    fn never_drops_the_live_copy_to_fit_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("images");
        let now = a_fixed_now();
        let oldest = 0u64;
        let mut paths = Vec::new();
        for i in 0..5u64 {
            paths.push(planted(&dir, &copy_name(i), 100, ago(now, 10 - i)));
        }
        let live = dir.join(copy_name(oldest));

        let policy = PrunePolicy {
            max_bytes: 250,
            ..PrunePolicy::default()
        };
        let pruned = prune_with(&dir, &live, policy, now);
        // The oldest copy is the live one, so the three next oldest go.
        assert_eq!((pruned.entries, pruned.remaining_entries), (3, 2));
        assert_eq!(pruned.remaining_bytes, 200);
        let left: Vec<bool> = paths.iter().map(|p| p.exists()).collect();
        assert_eq!(left, [true, false, false, false, true]);
    }

    #[test]
    fn touches_nothing_outside_its_own_names() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("images");
        let now = a_fixed_now();
        // A crash dump, a staging file from a half-finished write and a
        // subdirectory, all old enough for the age pass to want them.
        let dump = planted(&dir, "crash-1.bin", 100, ago(now, 40));
        let staging = planted(
            &dir,
            "victory-image-00000000000000aa.tmp",
            100,
            ago(now, 40),
        );
        let inner = dir.join("nested");
        std::fs::create_dir_all(&inner).unwrap();
        let nested = planted(&inner, &copy_name(0xaa), 100, ago(now, 40));
        // A copy of ours, but in the neighbouring directory.
        let neighbour = planted(
            &tmp.path().join("other"),
            &copy_name(0xaa),
            100,
            ago(now, 40),
        );
        let live = dir.join(copy_name(0xbb));

        let pruned = prune_with(&dir, &live, PrunePolicy::default(), now);
        assert_eq!(pruned.entries, 0);
        assert_eq!(pruned.remaining_entries, 0);
        assert!(dump.exists(), "a file the loader would not have written");
        assert!(staging.exists(), "a copy mid-write");
        assert!(nested.exists(), "a directory inside the cache");
        assert!(neighbour.exists(), "a directory beside the cache");

        // The same file, where the loader does put one, does go.
        let ours = planted(&dir, &copy_name(0xcc), 100, ago(now, 40));
        assert_eq!(
            prune_with(&dir, &live, PrunePolicy::default(), now).entries,
            1
        );
        assert!(!ours.exists());
    }

    #[test]
    fn a_directory_that_was_never_written_prunes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("images");
        let live = dir.join(copy_name(0xaa));
        assert_eq!(
            prune_with(&dir, &live, PrunePolicy::default(), a_fixed_now()),
            Pruned::default()
        );
    }

    #[test]
    fn the_name_matches_what_prepare_writes() {
        let original = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let prepared = crate::prepare(&original).unwrap();
        assert!(is_image_copy(OsStr::new(&prepared.file_name())));
    }
}
