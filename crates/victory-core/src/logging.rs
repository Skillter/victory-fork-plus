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

/// Logging for `run`, which knows its data directory by now: stderr, plus
/// `<log_dir>/run.log` unless `VICTORY_LOG_FILE` says otherwise.
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
/// Kept free of the environment and the filesystem so the rules stay
/// testable: the override wins, `0` and `off` mean "no file at all", and
/// only a command that supplies `log_dir` gets a default.
fn log_target(spec: Option<&OsStr>, log_dir: Option<&Path>) -> Option<PathBuf> {
    match spec {
        Some(s) if s.eq_ignore_ascii_case("off") || s == "0" => None,
        Some(s) => Some(PathBuf::from(s)),
        None => log_dir.map(|dir| dir.join("run.log")),
    }
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

/// Opens `path` for this process only, truncating whatever a previous run
/// left there: one bounded file per run is the whole retention policy, so
/// the user never has to curate a log directory.
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
    fn run_defaults_under_the_data_dir() {
        let dir = PathBuf::from("data").join("logs");
        assert_eq!(
            log_target(None, Some(&dir)),
            Some(PathBuf::from("data").join("logs").join("run.log"))
        );
        // Everything else stays stderr only.
        assert_eq!(log_target(None, None), None);
    }

    #[test]
    fn a_run_truncates_the_previous_log() {
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
