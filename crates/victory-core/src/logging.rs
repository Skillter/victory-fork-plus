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

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

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
    install(
        default_level,
        log_target(override_path().as_deref(), Some(log_dir)),
    );
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
    let filter = std::env::var("VICTORY_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
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
    match target.and_then(|path| open(&path)) {
        Some(file) => {
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
}
