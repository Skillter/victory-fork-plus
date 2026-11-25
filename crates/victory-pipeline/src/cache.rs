use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::Converter;

/// How long an entry may go untouched before opening the cache drops it.
/// Every key carries the converter's version and the build that wrote it, so
/// a rebuild orphans a whole generation at once and only age can retire those
/// entries. A month of never being read means the title or the executable
/// that wrote them is gone; re-converting costs one conversion per shader,
/// not a run.
const MAX_IDLE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// What one cache directory may keep after the age pass, counted in bytes. A
/// single 32-minute session was measured at 176 MB of shaders and 222 MiB of
/// pipelines on this machine, so two gigabytes holds several generations
/// before the oldest are dropped. The cap is per directory the code owns,
/// never across them.
const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Identifies one conversion result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey(blake3::Hash);

impl CacheKey {
    pub fn new(converter: &dyn Converter, input: &[u8]) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(converter.id().as_bytes());
        h.update(&[0]);
        h.update(&converter.version().to_le_bytes());
        h.update(input);
        CacheKey(h.finalize())
    }

    pub fn to_hex(&self) -> String {
        self.0.to_hex().to_string()
    }
}

/// Content-addressed store under the user's cache directory.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    /// Opens (creating) a cache at `root`. Refuses locations inside the repo.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        victory_core::ensure_outside_repo(&root)?;
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(Cache { root })
    }

    pub fn path_for(&self, key: &CacheKey) -> PathBuf {
        let hex = key.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    /// Returns the cached output for `input`, converting it first if needed.
    pub fn get_or_convert(&self, converter: &dyn Converter, input: &[u8]) -> Result<PathBuf> {
        let key = CacheKey::new(converter, input);
        let path = self.path_for(&key);
        if path.is_file() {
            tracing::trace!(converter = converter.id(), key = %key.to_hex(), "cache hit");
            return Ok(path);
        }
        let output = converter
            .convert(input)
            .with_context(|| format!("converter {} v{}", converter.id(), converter.version()))?;
        let dir = path.parent().expect("hashed path has a parent");
        std::fs::create_dir_all(dir)?;
        // Write-then-rename so readers never see partial output.
        let tmp = tempfile_in(dir);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        let written = file.write_all(&output);
        drop(file);
        let published = written.and_then(|()| std::fs::rename(&tmp, &path));
        if published.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        published?;
        Ok(path)
    }
}

/// What one pruning pass did to one cache directory.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Pruned {
    /// Files removed, by either pass.
    pub entries: u64,
    /// Bytes they occupied.
    pub bytes: u64,
    /// Files left in the shards.
    pub remaining_entries: u64,
    /// Bytes they occupy.
    pub remaining_bytes: u64,
}

/// The rule [`Cache::prune`] applies. Every field is a parameter so a test
/// can run the policy without waiting a month or filling 2 GiB.
#[derive(Debug, Clone, Copy)]
pub struct PrunePolicy {
    /// Entries last modified longer ago than this are dropped.
    pub max_idle: Duration,
    /// After the age pass, the oldest entries go until the total is at or
    /// below this.
    pub max_bytes: u64,
    /// Entries modified at or after this instant survive both passes: they
    /// were written by the process that is pruning. `None` protects nothing.
    pub protected_since: Option<SystemTime>,
}

impl Default for PrunePolicy {
    fn default() -> Self {
        Self {
            max_idle: MAX_IDLE,
            max_bytes: MAX_BYTES,
            // The production path sets this to the process start; a default
            // that protects nothing keeps the policy explicit in tests.
            protected_since: None,
        }
    }
}

/// When this process first considered pruning. Pruning happens while the
/// cache is opened, before anything is written, so an entry at or after this
/// instant is one this run published and must survive.
fn process_start() -> SystemTime {
    static START: std::sync::OnceLock<SystemTime> = std::sync::OnceLock::new();
    *START.get_or_init(SystemTime::now)
}

impl Cache {
    /// Drops entries that are too old or too many bytes, in one walk over the
    /// shards this cache created, and logs one line when it removed anything.
    ///
    /// Called when the cache is opened. A failure to remove a file (another
    /// run holds it, a permission) is a warning: a cache that cannot be
    /// pruned must not stop the run.
    pub fn prune(&self) -> Pruned {
        let now = SystemTime::now();
        let policy = PrunePolicy {
            protected_since: Some(process_start()),
            ..PrunePolicy::default()
        };
        let pruned = self.prune_with(policy, now);
        if pruned.entries > 0 {
            tracing::info!(
                entries = pruned.entries,
                bytes = pruned.bytes,
                remaining_bytes = pruned.remaining_bytes,
                remaining_entries = pruned.remaining_entries,
                dir = %self.root.display(),
                "pruned the cache"
            );
        }
        pruned
    }

    /// [`Cache::prune`] with the clock and the policy given, for tests.
    ///
    /// Only `<root>/<two hex digits>/<file>` is considered: the layout
    /// [`Cache::path_for`] writes. A symlink is never followed and nothing
    /// outside the root is read or removed.
    pub fn prune_with(&self, policy: PrunePolicy, now: SystemTime) -> Pruned {
        let mut pruned = Pruned::default();
        if !self.root.is_dir() {
            // Nothing was ever written here.
            return pruned;
        }
        let mut kept: Vec<Entry> = Vec::new();
        let protected =
            |when: SystemTime| policy.protected_since.is_some_and(|since| when >= since);
        for entry in entries(&self.root) {
            // A time in the future (a clock that moved) counts as untouched.
            let idle = now.duration_since(entry.modified).unwrap_or_default();
            if idle > policy.max_idle && !protected(entry.modified) {
                match std::fs::remove_file(&entry.path) {
                    Ok(()) => {
                        pruned.entries += 1;
                        pruned.bytes += entry.len;
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(
                            path = %entry.path.display(),
                            %error,
                            "cannot prune a cache entry"
                        );
                    }
                }
            }
            kept.push(entry);
        }
        let mut remaining_bytes: u64 = kept.iter().map(|e| e.len).sum();
        let mut remaining_entries = kept.len() as u64;
        if remaining_bytes > policy.max_bytes {
            // Oldest first; the protected ones sort last, so the loop can
            // stop at the first of them instead of testing each.
            kept.sort_by_key(|e| e.modified);
            for entry in &kept {
                if remaining_bytes <= policy.max_bytes || protected(entry.modified) {
                    break;
                }
                match std::fs::remove_file(&entry.path) {
                    Ok(()) => {
                        remaining_bytes = remaining_bytes.saturating_sub(entry.len);
                        remaining_entries -= 1;
                        pruned.entries += 1;
                        pruned.bytes += entry.len;
                    }
                    Err(error) => {
                        tracing::warn!(
                            path = %entry.path.display(),
                            %error,
                            "cannot prune a cache entry"
                        );
                    }
                }
            }
        }
        pruned.remaining_entries = remaining_entries;
        pruned.remaining_bytes = remaining_bytes;
        pruned
    }
}

/// One file the cache published.
struct Entry {
    path: PathBuf,
    modified: SystemTime,
    len: u64,
}

/// Every regular file in the shards of `root`, from a single walk. Files that
/// vanish while the walk runs are another run's business and are skipped.
fn entries(root: &Path) -> Vec<Entry> {
    let mut found = Vec::new();
    let shards = match std::fs::read_dir(root) {
        Ok(shards) => shards,
        Err(error) => {
            tracing::warn!(dir = %root.display(), %error, "cannot read the cache to prune it");
            return found;
        }
    };
    for shard in shards.flatten() {
        if !is_shard(&shard.file_name()) || !shard.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let inner = match std::fs::read_dir(shard.path()) {
            Ok(inner) => inner,
            Err(error) => {
                tracing::warn!(dir = %shard.path().display(), %error, "cannot read a cache shard");
                continue;
            }
        };
        for file in inner.flatten() {
            // `file_type` and `metadata` describe the entry itself, so a
            // symlink is skipped rather than followed out of the cache.
            if !file.file_type().is_ok_and(|t| t.is_file()) {
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
                modified,
                len: meta.len(),
            });
        }
    }
    found
}

/// The shard names [`Cache::path_for`] creates: two hex digits of the key.
/// Anything else at the root belongs to someone else and is left alone.
fn is_shard(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|s| s.len() == 2 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn tempfile_in(dir: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".tmp-{}-{n}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Upper {
        version: u32,
        runs: AtomicUsize,
    }

    impl Converter for Upper {
        fn id(&self) -> &'static str {
            "test.upper"
        }
        fn version(&self) -> u32 {
            self.version
        }
        fn convert(&self, input: &[u8]) -> Result<Vec<u8>> {
            self.runs.fetch_add(1, Ordering::Relaxed);
            Ok(input.to_ascii_uppercase())
        }
    }

    #[test]
    fn converts_once_then_hits() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let a = cache.get_or_convert(&conv, b"abc").unwrap();
        let b = cache.get_or_convert(&conv, b"abc").unwrap();
        assert_eq!(a, b);
        assert_eq!(std::fs::read(&a).unwrap(), b"ABC");
        assert_eq!(conv.runs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn failed_publication_removes_the_staging_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let path = cache.path_for(&CacheKey::new(&conv, b"abc"));
        std::fs::create_dir_all(&path).unwrap();

        assert!(cache.get_or_convert(&conv, b"abc").is_err());
        assert!(path.is_dir());
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn version_bump_invalidates() {
        let v1 = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let v2 = Upper {
            version: 2,
            runs: AtomicUsize::new(0),
        };
        assert_ne!(CacheKey::new(&v1, b"x"), CacheKey::new(&v2, b"x"));
    }

    #[test]
    fn refuses_cache_inside_repo() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache");
        assert!(Cache::open(inside).is_err());
    }

    /// A file where the cache would put one, with a chosen modification time.
    fn planted(root: &Path, name: &str, len: usize, when: SystemTime) -> PathBuf {
        assert!(name.len() >= 3, "a name needs a shard and the rest of a key");
        let path = root.join(&name[..2]).join(&name[2..]);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![b'x'; len]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(when)
            .unwrap();
        path
    }

    fn ago(now: SystemTime, days: u64) -> SystemTime {
        now - Duration::from_secs(days * 24 * 60 * 60)
    }

    fn a_fixed_now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 * 24 * 60 * 60)
    }

    #[test]
    fn prunes_entries_idle_past_the_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let cache = Cache::open(&root).unwrap();
        let now = a_fixed_now();
        let stale = planted(&root, "aa00", 100, ago(now, 40));
        let recent = planted(&root, "bb11", 40, ago(now, 1));

        let pruned = cache.prune_with(PrunePolicy::default(), now);
        assert_eq!(pruned.entries, 1);
        assert_eq!(pruned.bytes, 100);
        assert_eq!(pruned.remaining_entries, 1);
        assert_eq!(pruned.remaining_bytes, 40);
        assert!(!stale.exists(), "forty days untouched is gone");
        assert!(recent.exists(), "yesterday's entry stays");
    }

    #[test]
    fn keeps_entries_written_by_this_process() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let cache = Cache::open(&root).unwrap();
        let now = a_fixed_now();
        let entry = planted(&root, "cc22", 100, ago(now, 40));

        // The process started sixty days ago, so this entry is its own work
        // even though it looks older than the age limit.
        let policy = PrunePolicy {
            protected_since: Some(ago(now, 60)),
            ..PrunePolicy::default()
        };
        let pruned = cache.prune_with(policy, now);
        assert_eq!(pruned.entries, 0);
        assert_eq!(pruned.remaining_entries, 1);
        assert!(entry.exists());
    }

    #[test]
    fn drops_the_oldest_entries_until_under_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let cache = Cache::open(&root).unwrap();
        let now = a_fixed_now();
        let mut paths = Vec::new();
        for i in 0..5u64 {
            paths.push(planted(&root, &format!("0{i}0{i}"), 100, ago(now, 10 - i)));
        }

        let policy = PrunePolicy {
            max_bytes: 250,
            ..PrunePolicy::default()
        };
        let pruned = cache.prune_with(policy, now);
        assert_eq!((pruned.entries, pruned.bytes), (3, 300));
        assert_eq!((pruned.remaining_entries, pruned.remaining_bytes), (2, 200));
        let left: Vec<bool> = paths.iter().map(|p| p.exists()).collect();
        assert_eq!(left, [false, false, false, true, true], "the oldest went");
    }

    #[test]
    fn touches_nothing_outside_its_own_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let cache = Cache::open(&root).unwrap();
        let now = a_fixed_now();
        // A neighbour directory, a loose file at the root and a directory
        // that is not one of the shard names `path_for` writes.
        let neighbour = planted(&tmp.path().join("other"), "aa00", 100, ago(now, 40));
        let loose = root.join("loose.bin");
        std::fs::write(&loose, vec![b'x'; 100]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&loose)
            .unwrap()
            .set_modified(ago(now, 40))
            .unwrap();
        let not_a_shard = planted(&root.join("zz"), "aa00", 100, ago(now, 40));

        let pruned = cache.prune_with(PrunePolicy::default(), now);
        assert_eq!(pruned.entries, 0);
        assert_eq!(pruned.remaining_entries, 0);
        assert!(neighbour.exists(), "a directory beside the cache");
        assert!(loose.exists(), "a file the cache would not have written");
        assert!(not_a_shard.exists(), "a directory that is not a shard");

        // The same file, where the cache does put one, does go.
        let ours = planted(&root, "ef33", 100, ago(now, 40));
        assert_eq!(cache.prune_with(PrunePolicy::default(), now).entries, 1);
        assert!(!ours.exists());
    }

    #[test]
    fn an_empty_cache_prunes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        assert_eq!(cache.prune(), Pruned::default());
    }
}
