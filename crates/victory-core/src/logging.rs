//! One tracing setup for every binary.
//!
//! Filter with `VICTORY_LOG` (falls back to `RUST_LOG`), e.g.
//! `VICTORY_LOG=victory_loader=trace,info`. Debug and trace output exist
//! only in builds with the `diagnostics` feature; a plain release build
//! logs info and above whatever the filter asks for.
//!
//! Output is stderr plus a log file when `VICTORY_LOG_FILE` names one, or
//! for `victory run` by default. A run outlives the terminal it started
//! from (a title that relaunches itself, a crash, a black screen), and the
//! only copy of the diagnosis would otherwise go away with that terminal.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The most verbose level this build can log.
const CAP: LevelFilter = if cfg!(feature = "diagnostics") {
    LevelFilter::TRACE
} else {
    LevelFilter::INFO
};

/// Logging for every command but `run`: stderr, and a file only when
/// `VICTORY_LOG_FILE` asks for one.
pub fn init(default_level: &str) {
    install(default_level, log_target(override_path().as_deref(), None));
}

/// Logging for the audio recorder `run` starts: stderr only. It must not read
/// `VICTORY_LOG_FILE`, which the parent that started it is writing.
pub fn init_stderr(default_level: &str) {
    install(default_level, None);
}

/// Logging for `run`, which knows its data directory by now: stderr, plus a
/// `<log_dir>/run-<UTC date>-<UTC time>.log` of its own unless `VICTORY_LOG_FILE`
/// says otherwise.
pub fn init_run(default_level: &str, log_dir: &Path) {
    if override_path().is_none() {
        prune_run_logs(log_dir);
    }
    install(
        default_level,
        log_target(override_path().as_deref(), Some(log_dir)),
    );
}

/// Stamped run logs kept in the data directory. Each run writes its own file,
/// so without a bound they would accumulate at a few megabytes per session.
const RUN_LOGS_KEPT: usize = 20;

/// Removes the oldest stamped run logs, keeping the newest [`RUN_LOGS_KEPT`].
///
/// Only names [`log_target`] itself makes are candidates, so a log a developer
/// put beside them (`VICTORY_LOG_FILE`, a copied file) is never touched.
fn prune_run_logs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| is_run_log(name))
        .collect();
    if names.len() <= RUN_LOGS_KEPT {
        return;
    }
    // The name starts with the UTC date and time, so sorting by name is
    // sorting by age.
    names.sort();
    for name in &names[..names.len() - RUN_LOGS_KEPT] {
        if let Err(error) = std::fs::remove_file(dir.join(name)) {
            // The subscriber is not installed yet, so this goes to stderr.
            eprintln!("cannot remove the old run log {name}: {error}");
        }
    }
}

/// `run-YYYYMMDD-HHMMSS.log`, the name [`log_target`] gives a default log.
fn is_run_log(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("run-") else {
        return false;
    };
    let Some(stamp) = rest.strip_suffix(".log") else {
        return false;
    };
    let Some((date, time)) = stamp.split_once('-') else {
        return false;
    };
    date.len() == 8 && time.len() == 6 && date.bytes().chain(time.bytes()).all(|b| b.is_ascii_digit())
}

/// `VICTORY_LOG_FILE`, read once because the subscriber is built once.
fn override_path() -> Option<std::ffi::OsString> {
    std::env::var_os("VICTORY_LOG_FILE").filter(|v| !v.is_empty())
}

/// Where the file layer writes, or `None` for stderr alone.
///
/// Kept free of the filesystem so the rules stay testable: the override wins,
/// `0` and `off` mean "no file at all", and only a command that supplies
/// `log_dir` gets a default. The default carries the start time, so a second
/// run never overwrites the log of the one before it.
fn log_target(spec: Option<&OsStr>, log_dir: Option<&Path>) -> Option<PathBuf> {
    match spec {
        Some(s) if s.eq_ignore_ascii_case("off") || s == "0" => None,
        Some(s) => Some(PathBuf::from(s)),
        None => log_dir.map(|dir| {
            dir.join(format!(
                "run-{}.log",
                utc_stamp(std::time::SystemTime::now())
            ))
        }),
    }
}

/// `YYYYMMDD-HHMMSS` in UTC, matching the timestamps inside the file.
fn utc_stamp(now: std::time::SystemTime) -> String {
    let secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rest = secs % 86_400;
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3_600,
        rest / 60 % 60,
        rest % 60
    )
}

/// Days since the Unix epoch to a proleptic Gregorian date (Hinnant's
/// `civil_from_days`), so the filename needs no calendar crate.
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Installs the process-wide subscriber: the stderr layer both callers
/// share, plus the file layer when `target` names a file.
fn install(default_level: &str, target: Option<PathBuf>) {
    let asked = std::env::var("VICTORY_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok();
    // The header states the filter in effect; a filter that does not parse is
    // replaced by the default, so the header states that instead.
    if let Some(text) = asked.clone().filter(|v| EnvFilter::try_new(v).is_ok()) {
        let _ = FILTER_TEXT.set(text);
    } else {
        let _ = FILTER_TEXT.set(default_level.to_owned());
    }
    let filter = asked
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new(default_level));
    let subscriber = tracing_subscriber::registry().with(CAP).with(filter).with(
        tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_writer(std::io::stderr),
    );
    // A file that cannot be opened is not worth failing the command over:
    // the command's own work would stop before it could report anything,
    // and its paths are usually the missing parent directory itself.
    let opened = target.map(|path| (open(&path), path));
    let _ = match &opened {
        // The header names the file it is being written to; a path that failed
        // to open is not a sink and must not be named as one.
        Some((Some(_), path)) => LOG_PATH.set(Some(path.clone())),
        _ => LOG_PATH.set(None),
    };
    match opened.and_then(|(file, _)| file) {
        Some(file) => {
            // `emit` writes the header, the footer and the `warn_once` lines
            // here, outside the subscriber: they must survive a filter that
            // silences the level they would have been logged at.
            let _ = SINK.set(Some(file.clone()));
            let _ = subscriber
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_target(true)
                        .with_ansi(false)
                        .with_writer(file),
                )
                .try_init();
        }
        None => {
            let _ = subscriber.try_init();
        }
    }
}

/// Opens `path` for this process only. Each run writes its own timestamped
/// file, so the truncation here only guards two runs that start in the same
/// second; there is no separate retention policy.
fn open(path: &Path) -> Option<Shared> {
    // The default target is built from the data directory, which
    // `crates/victory-core/src/config.rs` already checks; an override names
    // its own path, and a log inside the repository would fail
    // `cargo xtask policy` on the next commit.
    if let Err(e) = crate::policy::ensure_outside_repo(path) {
        eprintln!("warning: no log file at {}: {e}", path.display());
        return None;
    }
    let result = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(path)
        });
    match result {
        Ok(file) => Some(Shared(Arc::new(Mutex::new(file)))),
        Err(e) => {
            eprintln!("warning: no log file at {}: {e}", path.display());
            None
        }
    }
}

/// The open file, shared by every thread that logs.
///
/// The subscriber asks for a writer per event; reopening the path each time
/// is what the `OpenOptions`-in-a-closure idiom would do, and it would lose
/// the truncation guarantee and pay a syscall per line.
#[derive(Clone)]
struct Shared(Arc<Mutex<File>>);

/// One event's turn at the file.
struct Locked<'a>(MutexGuard<'a, File>);

impl Write for Locked<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<'a> MakeWriter<'a> for Shared {
    type Writer = Locked<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        // A thread that panicked while logging must not silence the rest of
        // the run: the log is most needed right after something went wrong.
        Locked(self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

// ---- Records a filter cannot hide ------------------------------------------
//
// The run header, the run footer and a repeating warning are the three
// records a reader needs even when `VICTORY_LOG` silences everything else:
// one says what the run was, one says what it came to, and one says the thing
// that went wrong 4 359 times. They bypass the level filter on purpose — a
// filter is for volume, not for the two lines that explain the run.

/// The file [`install`] opened, so [`emit`] can reach the same sink the
/// subscriber writes. `None` when logging is on stderr alone.
static SINK: OnceLock<Option<Shared>> = OnceLock::new();

/// The log file in effect, for the run header.
static LOG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The filter text in effect, for the run header.
static FILTER_TEXT: OnceLock<String> = OnceLock::new();

/// The log file every record also reaches, or `None` for stderr only.
pub fn current_log_file() -> Option<&'static Path> {
    LOG_PATH.get().and_then(Option::as_deref)
}

/// What `VICTORY_LOG` (or `RUST_LOG`, or the caller's default) asked for.
pub fn filter_text() -> &'static str {
    FILTER_TEXT.get().map_or("info", String::as_str)
}

/// The most verbose level this build can log: `trace` with `diagnostics`,
/// `info` without. A filter asking for more than this gets this.
pub fn cap_text() -> &'static str {
    if cfg!(feature = "diagnostics") {
        "trace"
    } else {
        "info"
    }
}

/// Writes `text` to the log sinks outside the level filter, one timestamped
/// line each.
///
/// For the run header, the run footer and [`warn_once`]. A line that cannot be
/// written is dropped: logging never fails a run.
pub fn emit(level: &str, text: &str) {
    let stamp = utc_text(SystemTime::now());
    let mut rendered = String::with_capacity(text.len() + 32 * (text.lines().count() + 1));
    for line in text.lines() {
        rendered.push_str(&format!("{stamp} {level:>5} {line}\n"));
    }
    if let Some(Some(sink)) = SINK.get() {
        // A poisoned mutex means a thread panicked mid-line; the rest of the
        // run still gets its log.
        let mut file = sink.0.lock().unwrap_or_else(|e| e.into_inner());
        let _ = file.write_all(rendered.as_bytes());
    }
    let _ = io::stderr().write_all(rendered.as_bytes());
}

/// The same instant as [`utc_text`], in the wall clock this machine shows,
/// `YYYY-MM-DDTHH:MM:SS`.
///
/// `victory-core` stays dependency-free, so the one Win32 call that reports
/// local time is declared here rather than pulling `windows-sys` into a leaf
/// crate for a header field. Elsewhere the two are the same and this returns
/// the UTC text.
pub fn local_text() -> String {
    #[cfg(windows)]
    {
        // `SYSTEMTIME`: eight `WORD`s, the layout kernel32 has always had.
        #[repr(C)]
        struct SystemTime {
            year: u16,
            month: u16,
            day_of_week: u16,
            day: u16,
            hour: u16,
            minute: u16,
            second: u16,
            milliseconds: u16,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetLocalTime(out: *mut SystemTime);
        }
        let mut now = SystemTime {
            year: 0,
            month: 0,
            day_of_week: 0,
            day: 0,
            hour: 0,
            minute: 0,
            second: 0,
            milliseconds: 0,
        };
        // SAFETY: `GetLocalTime` fills one `SYSTEMTIME` it is given and reads
        // nothing else; the argument points at a live local.
        unsafe { GetLocalTime(&mut now) };
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            now.year, now.month, now.day, now.hour, now.minute, now.second
        )
    }
    #[cfg(not(windows))]
    {
        utc_text(SystemTime::now())
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ`, the same shape and zone as the timestamps
/// `tracing-subscriber` writes, so one `grep` finds both.
pub fn utc_text(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs());
    let millis = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.subsec_millis());
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rest = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rest / 3_600,
        rest / 60 % 60,
        rest % 60
    )
}

/// A condition that repeats: the first occurrence is logged, then the 10th,
/// the 100th, the 1 000th …, and the first one after a minute of quiet.
const QUIET: Duration = Duration::from_secs(60);

/// The first count that is logged after the first occurrence.
const ESCALATION: u64 = 10;

/// How often one keyed condition has been seen, and how many of those
/// occurrences produced no line.
struct Watch {
    count: u64,
    suppressed: u64,
    last_seen: Instant,
}

/// What [`warn_once`] decided, when it decided to log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Occurrence {
    /// Occurrences so far, first one included.
    pub count: u64,
    /// Occurrences the log does not show, first one included in the count.
    pub suppressed: u64,
}

impl Watch {
    fn new(now: Instant) -> Self {
        Self {
            count: 0,
            suppressed: 0,
            last_seen: now,
        }
    }

    /// Counts one occurrence and says whether this one is worth a line.
    ///
    /// Ten, a hundred, a thousand: a storm that stays silent hides the number
    /// that says how bad it is, and one that prints every occurrence hides
    /// everything else. A minute of quiet starts the escalation again, so a
    /// condition that comes back much later is visible at once.
    fn observe(&mut self, now: Instant) -> Option<Occurrence> {
        let quiet = now.saturating_duration_since(self.last_seen) >= QUIET;
        self.count += 1;
        self.last_seen = now;
        if self.count == 1 || quiet || milestone(self.count) {
            return Some(Occurrence {
                count: self.count,
                suppressed: self.suppressed,
            });
        }
        self.suppressed += 1;
        None
    }
}

/// Whether `n` is 10, 100, 1 000 …
fn milestone(n: u64) -> bool {
    if n < ESCALATION {
        return false;
    }
    let mut step = ESCALATION;
    while step < n {
        step = step.saturating_mul(ESCALATION);
    }
    step == n
}

fn watches() -> &'static Mutex<HashMap<&'static str, Watch>> {
    static WATCHES: OnceLock<Mutex<HashMap<&'static str, Watch>>> = OnceLock::new();
    WATCHES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Logs the first occurrence of a keyed condition and escalates from there.
///
/// `key` is a stable dotted name (`draw.refused`), the word an agent greps.
/// Every line carries `count=` and `suppressed=`, so a message that prints
/// once still says how many it stood for — the draw refusal that hid 4 359
/// lost draws per frame for minutes did not.
///
/// `message` runs only when a line is written: a per-draw call costs one lock
/// and one integer compare in the storm. Returns whether a line was written.
pub fn warn_once(key: &'static str, message: impl FnOnce() -> String) -> bool {
    let occurrence = {
        let mut watches = watches().lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        watches
            .entry(key)
            .or_insert_with(|| Watch::new(now))
            .observe(now)
    };
    let Some(occurrence) = occurrence else {
        return false;
    };
    emit(
        "WARN",
        &format!(
            "{key} count={} suppressed={} {}",
            occurrence.count,
            occurrence.suppressed,
            message()
        ),
    );
    true
}

/// Every keyed condition and how many times it was seen, sorted by key, for
/// the run footer, which states the totals the escalation left out.
pub fn warn_once_totals() -> Vec<(String, u64)> {
    let watches = watches().lock().unwrap_or_else(|e| e.into_inner());
    let mut totals: Vec<(String, u64)> = watches
        .iter()
        .map(|(key, watch)| ((*key).to_owned(), watch.count))
        .collect();
    totals.sort();
    totals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_names_the_file() {
        let dir = PathBuf::from("data").join("logs");
        let named = Some(OsStr::new("D:/logs/title.log"));
        assert_eq!(
            log_target(named, Some(&dir)),
            Some(PathBuf::from("D:/logs/title.log"))
        );
        // `inspect` and friends have no data dir, and do not need one.
        assert_eq!(
            log_target(named, None),
            Some(PathBuf::from("D:/logs/title.log"))
        );
    }

    #[test]
    fn off_means_no_file() {
        let dir = PathBuf::from("data").join("logs");
        for spec in ["0", "off", "OFF"] {
            assert_eq!(
                log_target(Some(OsStr::new(spec)), Some(&dir)),
                None,
                "{spec}"
            );
        }
    }

    #[test]
    fn run_defaults_to_a_stamped_file() {
        let dir = PathBuf::from("data").join("logs");
        let name = log_target(None, Some(&dir))
            .and_then(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()))
            .expect("run logs to a file");
        let (date, time) = name
            .strip_prefix("run-")
            .and_then(|n| n.strip_suffix(".log"))
            .and_then(|s| s.split_once('-'))
            .unwrap_or_else(|| panic!("run-YYYYMMDD-HHMMSS.log, got {name}"));
        assert_eq!(date.len(), 8, "{name}");
        assert_eq!(time.len(), 6, "{name}");
        assert!(
            date.bytes().chain(time.bytes()).all(|b| b.is_ascii_digit()),
            "{name}"
        );
        // Everything else stays stderr only.
        assert_eq!(log_target(None, None), None);
    }

    #[test]
    fn the_stamp_is_a_utc_date_and_time() {
        use std::time::{Duration, UNIX_EPOCH};
        assert_eq!(utc_stamp(UNIX_EPOCH), "19700101-000000");
        // 2024-02-29T00:00:00Z, the leap day a naive year-of-365 gets wrong.
        assert_eq!(
            utc_stamp(UNIX_EPOCH + Duration::from_secs(1_709_164_800)),
            "20240229-000000"
        );
        assert_eq!(
            utc_stamp(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            "20231114-221320"
        );
    }

    #[test]
    fn opening_the_same_path_twice_keeps_the_newest() {
        let dir = std::env::temp_dir().join(format!("victory-logging-{}", std::process::id()));
        let path = dir.join("logs").join("run.log");
        for line in ["first run\n", "second run\n"] {
            let shared = open(&path).expect("log file");
            shared
                .make_writer()
                .write_all(line.as_bytes())
                .expect("write");
        }
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "second run\n"
        );
    }

    #[test]
    fn a_log_file_inside_the_repository_is_refused() {
        // `VICTORY_LOG_FILE` names its own path; the default comes from a
        // data directory the config already checks. A log inside the tree
        // fails `cargo xtask policy` on the next commit.
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("target-refused.log");
        assert!(open(&path).is_none(), "a log inside the repo must not open");
        assert!(!path.exists(), "and must not be created either");
    }

    #[test]
    fn only_the_newest_run_logs_are_kept() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let made: Vec<String> = (0..RUN_LOGS_KEPT + 5)
            .map(|i| format!("run-20260101-{:06}.log", i))
            .collect();
        for name in &made {
            std::fs::write(dir.path().join(name), "log\n").expect("write");
        }
        // Names this function must never remove, and one log that is not ours.
        for name in ["notes.log", "run-latest.log", "run-20260101-00000.log"] {
            std::fs::write(dir.path().join(name), "keep\n").expect("write");
        }
        prune_run_logs(dir.path());
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read")
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| is_run_log(name))
            .collect();
        // The five oldest stamped logs went; the kept ones are the newest.
        assert_eq!(left.len(), RUN_LOGS_KEPT);
        assert!(!left.contains(&made[0]));
        assert!(!left.contains(&made[4]));
        assert!(left.contains(&made[5]));
        assert!(dir.path().join("notes.log").exists());
        assert!(dir.path().join("run-latest.log").exists());
        assert!(dir.path().join("run-20260101-00000.log").exists());
    }

    #[test]
    fn a_condition_escalates_at_ten_and_a_hundred() {
        let start = Instant::now();
        let mut watch = Watch::new(start);
        assert_eq!(
            watch.observe(start),
            Some(Occurrence {
                count: 1,
                suppressed: 0
            })
        );
        let mut logged = Vec::new();
        for i in 1..100 {
            let at = start + Duration::from_millis(i);
            if let Some(occurrence) = watch.observe(at) {
                logged.push(occurrence);
            }
        }
        // Occurrences 2..=9 said nothing, 10 spoke, 11..=99 said nothing,
        // 100 spoke. Nine silent between the first line and the tenth.
        assert_eq!(
            logged,
            [
                Occurrence {
                    count: 10,
                    suppressed: 8
                },
                Occurrence {
                    count: 100,
                    suppressed: 97
                }
            ]
        );
        assert_eq!(watch.count, 100);
    }

    #[test]
    fn a_quiet_minute_starts_the_escalation_again() {
        let start = Instant::now();
        let mut watch = Watch::new(start);
        assert!(watch.observe(start).is_some());
        for i in 1..=5 {
            assert!(watch.observe(start + Duration::from_millis(i)).is_none());
        }
        // 59 s after the last occurrence is not quiet, and 7 is no milestone.
        assert!(watch.observe(start + Duration::from_secs(59)).is_none());
        // A minute of silence is: the condition is back, and so is its line.
        assert_eq!(
            watch.observe(start + Duration::from_secs(120)),
            Some(Occurrence {
                count: 8,
                suppressed: 6
            })
        );
    }

    #[test]
    fn a_milestone_is_a_power_of_ten_from_ten() {
        for n in [0, 1, 9, 11, 99, 101, 1_001] {
            assert!(!milestone(n), "{n}");
        }
        for n in [10, 100, 1_000, 10_000, 1_000_000] {
            assert!(milestone(n), "{n}");
        }
    }

    #[test]
    fn the_timestamp_is_a_zulu_date_and_time() {
        use std::time::UNIX_EPOCH;
        assert_eq!(utc_text(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            utc_text(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            "2023-11-14T22:13:20.000Z"
        );
        // Milliseconds, not microseconds: the same width on every line.
        assert_eq!(
            utc_text(UNIX_EPOCH + Duration::from_millis(1_700_000_000_123)),
            "2023-11-14T22:13:20.123Z"
        );
    }

    #[test]
    fn a_condition_is_logged_only_as_often_as_documented() {
        // The real entry point, on a real clock: one line, then nothing.
        let key: &'static str = Box::leak(format!("test.{}", std::process::id()).into_boxed_str());
        assert!(warn_once(key, || "first".to_owned()));
        for _ in 0..8 {
            assert!(!warn_once(key, || "silent".to_owned()));
        }
        assert!(warn_once(key, || "tenth".to_owned()));
        let totals = warn_once_totals();
        assert!(
            totals.iter().any(|(name, count)| name == key && *count == 10),
            "{totals:?}"
        );
    }
}
