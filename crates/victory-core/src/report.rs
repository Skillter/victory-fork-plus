//! The records a run leaves behind: `RUN` at the start, `STOP` at the end, and
//! one row of `runs.tsv` for every run that ever ended.
//!
//! The log is the detail and the history file is the series: a log is a
//! megabyte of tracing per run, so a directory of them cannot be read across
//! runs. `<log_dir>/runs.tsv` is one line per run - the commit, the frame
//! rate, the worst window - which is what answers "did this commit make it
//! slower" months later without opening a single log.
//!
//! Two runs of the same binary could not be compared, because the log never
//! said which switches were in effect, which build was running or where the
//! caches lived. The header answers that once; the footer answers what the run
//! came to. Both are plain text built from plain values — the caller reads the
//! environment, the clock and the config, so a test renders from a fake one
//! with no environment and no clock of its own.
//!
//! Every line starts with its marker, so `grep '^RUN '` is the whole header
//! even when the terminal wrapped it and `grep '^STOP '` is the whole footer.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::meter::Snapshot;

/// The line marker of the start of a run.
pub const RUN: &str = "RUN";
/// The line marker of the end of a run.
pub const STOP: &str = "STOP";

/// Everything the start of a run knows, as of the moment it knows all of it.
#[derive(Clone, Debug)]
pub struct RunHeader {
    /// Binary name and version, e.g. `victory 0.1.0`.
    pub program: String,
    /// `VICTORY_COMMIT_SHORT`: the eight characters of the commit, as
    /// `apps/victory/build.rs` read them.
    pub build: String,
    /// `VICTORY_COMMIT`: the whole hash, so a log and a `git log` agree
    /// without widening the eight.
    pub commit: String,
    /// `VICTORY_BRANCH`: the branch the build was made on, or `detached`.
    pub branch: String,
    /// `VICTORY_DIRTY`: whether the tree differed from the commit, or
    /// `unknown` when git could not be asked.
    pub dirty: String,
    /// `YYYY-MM-DDTHH:MM:SS.mmmZ`.
    pub started_utc: String,
    /// Local wall clock, same shape without the zone.
    pub started_local: String,
    /// The filter `VICTORY_LOG` (or `RUST_LOG`, or the default) asked for.
    pub filter: String,
    /// The most verbose level this build can log.
    pub cap: String,
    /// The log file, or `None` for stderr only.
    pub log_file: Option<PathBuf>,
    /// The config file that was read, if any.
    pub config_file: Option<PathBuf>,
    pub game_dir: Option<PathBuf>,
    pub exe: PathBuf,
    /// BLAKE3 of the image as the title shipped it.
    pub exe_hash: String,
    pub title_id: u32,
    /// Whether `MicrosoftGame.config` was there to be read.
    pub manifest: bool,
    pub storage_mb: u64,
    pub console: String,
    pub cache_dir: PathBuf,
    pub data_dir: PathBuf,
    /// Every `VICTORY_*` variable that is set, as the user set it.
    pub env: Vec<(String, String)>,
}

impl RunHeader {
    /// The block, one `RUN ` line per fact, ending with a newline.
    pub fn render(&self) -> String {
        let path = |p: &Path| p.display().to_string();
        let optional = |p: &Option<PathBuf>| {
            p.as_ref().map_or_else(|| "(none)".to_owned(), |p| p.display().to_string())
        };
        let mut out = String::new();
        let mut line = |text: String| {
            out.push_str(RUN);
            out.push(' ');
            out.push_str(&text);
            out.push('\n');
        };
        line(format!(
            "run start_utc={} start_local={}",
            self.started_utc, self.started_local
        ));
        line(format!(
            "bin name={} build={} commit={} branch={} dirty={}",
            self.program, self.build, self.commit, self.branch, self.dirty
        ));
        line(format!(
            "log filter={} cap={} file={}",
            self.filter,
            self.cap,
            self.log_file
                .as_ref()
                .map_or_else(|| "(stderr only)".to_owned(), |p| p.display().to_string())
        ));
        line(format!("config file={}", optional(&self.config_file)));
        line(format!("config game_dir={}", optional(&self.game_dir)));
        line(format!("title exe={} blake3={}", path(&self.exe), self.exe_hash));
        line(format!(
            "title id={:08X} manifest={} storage_mb={}",
            self.title_id,
            if self.manifest { "present" } else { "missing" },
            self.storage_mb
        ));
        line(format!("host console={}", self.console));
        line(format!(
            "path cache_dir={} data_dir={}",
            path(&self.cache_dir),
            path(&self.data_dir)
        ));
        // Sorted by name, one variable per line: `grep '^RUN env'` is the
        // answer to "what was different about the run that worked".
        let mut env = self.env.clone();
        env.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, value) in env {
            line(format!("env {name}={value}"));
        }
        out
    }
}

/// What a run came to, whether it ended by itself or was cut short.
#[derive(Clone, Debug)]
pub struct RunFooter {
    /// Free text: `title exited with 0`, `error`, a suspend reason.
    pub reason: String,
    pub duration: Duration,
    pub frames: u64,
    /// The worst frame of the run, when something measured frames.
    pub worst_frame_ms: Option<u64>,
    /// [`crate::meter::Meter::totals`]: counters and timers since the start.
    pub meter: Snapshot,
    /// Named regions of the cache on disk, in bytes.
    pub cache: Vec<(String, u64)>,
    /// Key and count of every [`crate::logging::warn_once`] condition.
    pub warns: Vec<(String, u64)>,
}

impl RunFooter {
    /// The block, one `STOP ` line per fact, ending with a newline.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut line = |text: String| {
            out.push_str(STOP);
            out.push(' ');
            out.push_str(&text);
            out.push('\n');
        };
        let secs = self.duration.as_secs_f64();
        let mut end = format!(
            "end reason={} duration_s={} frames={}",
            quote(&self.reason),
            self.duration.as_secs(),
            self.frames
        );
        // One decimal, always present: a float printed with `{}` changes width
        // between runs and defeats a textual diff of two logs.
        if self.frames > 0 && secs > 0.0 {
            end.push_str(&format!(" fps_mean={:.1}", self.frames as f64 / secs));
        }
        if let Some(ms) = self.worst_frame_ms.filter(|ms| *ms > 0) {
            end.push_str(&format!(" fps_worst={:.1}", 1_000.0 / ms as f64));
        }
        line(end);
        line(format!("meter {}", self.meter.fields()));
        let mut cache = self.cache.clone();
        cache.sort_by(|a, b| a.0.cmp(&b.0));
        let sizes: Vec<String> = cache
            .iter()
            .map(|(region, bytes)| format!("{region}_mb={}", bytes / (1 << 20)))
            .collect();
        if !sizes.is_empty() {
            line(format!("cache {}", sizes.join(" ")));
        }
        // Sorted, so the footer of two runs with the same storms compares.
        let mut warns = self.warns.clone();
        warns.sort_by(|a, b| a.0.cmp(&b.0));
        for (key, count) in warns {
            line(format!("warn key={key} count={count}"));
        }
        out
    }
}

impl RunFooter {
    /// The footer of a run that ends where only this crate's statics are left:
    /// the meter ([`crate::meter::METER`]) and the [`crate::logging`] watches.
    ///
    /// No directory is walked, so nothing is answered by a layer that may
    /// already have let go of its arenas. A path that leaves the process
    /// without returning through the binary's own shutdown - a title that
    /// exits, the suspend routines, the deadline that stops waiting for them -
    /// still reaches both, because they are statics of this crate rather than
    /// state of the run.
    pub fn from_counters(reason: &str, duration: Duration) -> Self {
        let meter = crate::meter::meter().totals();
        let metric = |name: &str| meter.metrics.iter().find(|m| m.name == name);
        let footer = RunFooter {
            reason: reason.to_owned(),
            duration,
            frames: metric("frames_n").map_or(0, |m| m.count),
            worst_frame_ms: metric("frame_ms").map(|m| m.max).filter(|ms| *ms > 0),
            meter,
            cache: Vec::new(),
            warns: crate::logging::warn_once_totals(),
        };
        // What this block is about to state about the run, for a summary line
        // written beside it: a reader of the two must not find two runs.
        *STATED.lock().unwrap_or_else(|e| e.into_inner()) = Some((duration, footer.frames));
        footer
    }
}

/// The run length and frame count the last [`RunFooter::from_counters`] stated.
///
/// For the summary line `victory run` writes after the [`STOP`] block: the
/// block's cache sizes are a walk of the run's cache (1.7 s on the 2026-11-25
/// 19:24 run), and a summary that read the counters again after that walk
/// disagreed with the block it follows - 8.8 fps over 8,003 frames beside the
/// block's 8.9 over 7,997, off one frame count and 1.7 s. `None` before any
/// footer was rendered, which a caller answers from what it still has.
pub fn stated_run() -> Option<(Duration, u64)> {
    *STATED.lock().unwrap_or_else(|e| e.into_inner())
}

/// See [`stated_run`].
static STATED: Mutex<Option<(Duration, u64)>> = Mutex::new(None);

/// Whether this process has already written its [`STOP`] block.
static STOPPED: AtomicBool = AtomicBool::new(false);

/// Renders a whole [`STOP`] block from a reason and a duration.
pub type FooterHook = Box<dyn Fn(&str, Duration) -> String + Send>;

/// The binary's footer writer: it knows the directories the cache sizes are
/// measured in, which this crate does not.
static HOOK: Mutex<Option<FooterHook>> = Mutex::new(None);

/// Installs the renderer [`stop_block`] uses, replacing any earlier one.
///
/// `victory run` installs one that adds the cache left on disk, so the block
/// [`crate::report::stop_block`] renders is the same one whether the run
/// returns from `run` or is terminated from inside the HLE. The writer is
/// taken by the one render, so a second path cannot make a second footer.
pub fn set_footer_hook(hook: FooterHook) {
    *HOOK.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
}

/// The block, drawing on the installed [`FooterHook`] when there is one and on
/// the counters alone when there is not.
fn render_stop(reason: &str, duration: Duration) -> String {
    let hook = HOOK.lock().unwrap_or_else(|e| e.into_inner()).take();
    render_stop_with(hook, reason, duration)
}

fn render_stop_with(hook: Option<FooterHook>, reason: &str, duration: Duration) -> String {
    match hook {
        Some(hook) => hook(reason, duration),
        None => RunFooter::from_counters(reason, duration).render(),
    }
}

/// The [`STOP`] block of a run that ends by leaving the process, and `None`
/// once one has been written.
///
/// Several paths reach the end of a run - the title asking to exit, the
/// suspend routines returning, the deadline that stops waiting for them - and
/// the process is terminated rather than returned from, so no caller can be
/// asked which came first. The first caller renders the block ([`RunFooter`],
/// or the [`FooterHook`] `victory run` installed) and the paths that only
/// return from `run` write it through this same guard, so a log holds one
/// `STOP` block and it names the path that ended the run.
pub fn stop_block(reason: &str, duration: Duration) -> Option<String> {
    let wrote = STOPPED.swap(true, Ordering::SeqCst);
    if wrote {
        return None;
    }
    Some(render_stop(reason, duration))
}

/// One line of free text inside `key=value`: quoted, with the characters that
/// would end the line or the field escaped, so a parse cannot swallow the rest
/// of the block.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' | '\r' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The name of the history file, in the directory the logs are written to.
pub const RUNS_FILE: &str = "runs.tsv";

/// The first line of [`RUNS_FILE`], written once when the file is made.
///
/// Tab separated, one row per run, nothing quoted: the only free text is the
/// end reason, and [`field`] flattens it, so a reader always gets sixteen
/// columns. An empty column is a fact nothing measured - a run that never drew
/// has no frame rate, and a `0` there would average into the next one.
pub const RUNS_HEADER: &str = "start_utc\tend_utc\tcommit\tbranch\tdirty\tconsole\tframes\t\
secs\tfps_mean\tms_mean\tfps_worst\tms_worst\tdraws_mean\tgpu_wait_ms_mean\tend_reason\tlog";

/// One row of [`RUNS_FILE`]: what one run came to, in the shape a script or a
/// spreadsheet reads back months later without the log it came from.
#[derive(Clone, Debug)]
pub struct RunRecord {
    /// `YYYY-MM-DDTHH:MM:SS.mmmZ`, as the header states it.
    pub started_utc: String,
    /// The same shape, at the moment the run ended.
    pub ended_utc: String,
    /// The whole commit hash; the row does not shorten it.
    pub commit: String,
    pub branch: String,
    /// `true`, `false`, or `unknown`.
    pub dirty: String,
    /// `series-x` or `series-s`.
    pub console: String,
    pub duration: Duration,
    /// Frames presented, as the layer counted them.
    pub frames: u64,
    /// The worst frame of the run, as the meter recorded it.
    pub worst_frame_ms: Option<u64>,
    /// Draws a presented frame made, averaged over the run's windows.
    pub draws_mean: Option<f64>,
    /// Time a presented frame waited for the card, in milliseconds.
    pub gpu_wait_ms: Option<f64>,
    /// Free text, the same one the `STOP` block names.
    pub reason: String,
    pub log: Option<PathBuf>,
}

impl RunRecord {
    /// The row, ending with a newline.
    pub fn line(&self) -> String {
        let secs = self.duration.as_secs_f64();
        // The same arithmetic the `STOP` line prints, so a row and the log it
        // points at cannot disagree: nothing measured prints nothing.
        let fps_mean = (self.frames > 0 && secs > 0.0).then(|| self.frames as f64 / secs);
        let ms_mean = fps_mean.map(|fps| 1_000.0 / fps);
        let ms_worst = self.worst_frame_ms.filter(|ms| *ms > 0);
        let fps_worst = ms_worst.map(|ms| 1_000.0 / ms as f64);
        let fields = [
            field(&self.started_utc),
            field(&self.ended_utc),
            field(&self.commit),
            field(&self.branch),
            field(&self.dirty),
            field(&self.console),
            self.frames.to_string(),
            tenth(Some(secs)),
            tenth(fps_mean),
            tenth(ms_mean),
            tenth(fps_worst),
            ms_worst.map_or_else(String::new, |ms| ms.to_string()),
            tenth(self.draws_mean),
            tenth(self.gpu_wait_ms),
            field(&self.reason),
            self.log
                .as_ref()
                .map_or_else(String::new, |path| field(&path.display().to_string())),
        ];
        let mut out = fields.join("\t");
        out.push('\n');
        out
    }

    /// The row split back into the columns [`RUNS_HEADER`] names.
    pub fn columns(&self) -> Vec<String> {
        let line = self.line();
        line.trim_end_matches('\n').split('\t').map(str::to_owned).collect()
    }
}

/// One decimal, or nothing when nothing measured it.
fn tenth(value: Option<f64>) -> String {
    value.map_or_else(String::new, |value| format!("{value:.1}"))
}

/// A field of a tab-separated row: the characters that would end the field or
/// the row become spaces, so a row is always one line of [`RUNS_HEADER`]'s
/// columns.
fn field(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\t' | '\n' | '\r' => ' ',
            c => c,
        })
        .collect()
}

/// Appends `record` to the history file at `path`, making the file and its
/// header when they are not there.
///
/// One open, one [`Write::write_all`], one close, in append mode: a run killed
/// in the middle of this call can leave half of its own row, and cannot lose a
/// row an earlier run wrote, because append never seeks over what is there and
/// never truncates. The caller logs a warning instead of failing the run - a
/// history file is worth less than the run it describes.
///
/// The path is checked by [`crate::policy::ensure_outside_repo`] first: this is
/// written next to the logs, which the same rule already keeps out of the tree.
pub fn append_run(path: &Path, record: &RunRecord) -> std::io::Result<()> {
    crate::policy::ensure_outside_repo(path).map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, error.to_string())
    })?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // The header once: a file that holds bytes already carries it, and one a
    // crash left empty gets it with this run's row.
    let mut text = String::new();
    if std::fs::metadata(path).map_or(true, |meta| meta.len() == 0) {
        text.push_str(RUNS_HEADER);
        text.push('\n');
    }
    text.push_str(&record.line());
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(text.as_bytes())
}

/// The commit the binary was built from, for the crates whose build script
/// does not read git: `victory-gpu` draws the window caption, and `victory run`
/// hands it these answers once, through [`set_build`].
static COMMIT: OnceLock<String> = OnceLock::new();
static COMMIT_SHORT: OnceLock<String> = OnceLock::new();

/// Records the commit `apps/victory/build.rs` baked in. The first call wins.
pub fn set_build(commit: &str, short: &str) {
    let _ = COMMIT.set(commit.to_owned());
    let _ = COMMIT_SHORT.set(short.to_owned());
}

/// The eight characters of the commit, or `unknown` before [`set_build`].
pub fn commit_short() -> &'static str {
    COMMIT_SHORT.get().map_or("unknown", String::as_str)
}

/// The whole commit hash, or `unknown` before [`set_build`].
pub fn commit() -> &'static str {
    COMMIT.get().map_or("unknown", String::as_str)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::meter::{Kind, Metric};

    fn header() -> RunHeader {
        RunHeader {
            program: "victory 0.1.0".to_owned(),
            build: "504b1c6d".to_owned(),
            commit: "504b1c6d9e2f4a7c1b3d5e6f7a8b9c0d1e2f3a4b".to_owned(),
            branch: "main".to_owned(),
            dirty: "true".to_owned(),
            started_utc: "2026-11-25T06:12:50.123Z".to_owned(),
            started_local: "2026-11-25T07:12:50".to_owned(),
            filter: "victory_gpu=debug,info".to_owned(),
            cap: "trace".to_owned(),
            log_file: Some(PathBuf::from(r"C:\v\data\logs\run-20261125-061250.log")),
            config_file: None,
            game_dir: Some(PathBuf::from(r"D:\titles\game")),
            exe: PathBuf::from(r"D:\titles\game\Title.exe"),
            exe_hash: "3f2a".to_owned(),
            title_id: 0x6B4E_1F2A,
            manifest: true,
            storage_mb: 8192,
            console: "series-x".to_owned(),
            cache_dir: PathBuf::from(r"C:\v\cache"),
            data_dir: PathBuf::from(r"C:\v\data"),
            env: vec![
                ("VICTORY_LOG_FILE".to_owned(), "0".to_owned()),
                ("VICTORY_LOG".to_owned(), "victory_gpu=debug,info".to_owned()),
            ],
        }
    }

    fn footer() -> RunFooter {
        RunFooter {
            reason: "title exited with 0".to_owned(),
            duration: Duration::from_secs(412),
            frames: 24_310,
            worst_frame_ms: Some(81),
            meter: Snapshot {
                window: Duration::from_secs(412),
                metrics: vec![
                    Metric {
                        name: "draws_n",
                        kind: Kind::Counter,
                        count: 1_706_731,
                        sum: 0,
                        max: 0,
                        last: 0,
                    },
                    Metric {
                        name: "preparing_ms",
                        kind: Kind::Observed,
                        count: 12,
                        sum: 18_344,
                        max: 4_201,
                        last: 312,
                    },
                ],
            },
            cache: vec![
                ("shaders".to_owned(), 11 << 20),
                ("images".to_owned(), 229 << 20),
            ],
            warns: vec![("draw.refused".to_owned(), 4_359)],
        }
    }

    #[test]
    fn the_header_renders_from_a_fixed_config() {
        assert_eq!(
            header().render(),
            concat!(
                "RUN run start_utc=2026-11-25T06:12:50.123Z start_local=2026-11-25T07:12:50\n",
                "RUN bin name=victory 0.1.0 build=504b1c6d commit=504b1c6d9e2f4a7c1b3d5e6f7a8b9c0d1e2f3a4b ",
                "branch=main dirty=true\n",
                "RUN log filter=victory_gpu=debug,info cap=trace ",
                "file=C:\\v\\data\\logs\\run-20261125-061250.log\n",
                "RUN config file=(none)\n",
                "RUN config game_dir=D:\\titles\\game\n",
                "RUN title exe=D:\\titles\\game\\Title.exe blake3=3f2a\n",
                "RUN title id=6B4E1F2A manifest=present storage_mb=8192\n",
                "RUN host console=series-x\n",
                "RUN path cache_dir=C:\\v\\cache data_dir=C:\\v\\data\n",
                // Sorted, so the order never depends on the environment's.
                "RUN env VICTORY_LOG=victory_gpu=debug,info\n",
                "RUN env VICTORY_LOG_FILE=0\n",
            )
        );
    }

    #[test]
    fn a_run_without_a_config_says_so() {
        let mut header = header();
        header.config_file = Some(PathBuf::from(r"D:\titles\victory.toml"));
        header.game_dir = None;
        header.log_file = None;
        let text = header.render();
        assert!(text.contains("RUN config file=D:\\titles\\victory.toml\n"), "{text}");
        assert!(text.contains("RUN config game_dir=(none)\n"), "{text}");
        assert!(text.contains("file=(stderr only)"), "{text}");
        assert!(!text.contains("(none)")
            || text.lines().filter(|line| line.contains("(none)")).count() == 1);
    }

    #[test]
    fn the_footer_renders_from_a_fixed_total() {
        assert_eq!(
            footer().render(),
            concat!(
                "STOP end reason=\"title exited with 0\" duration_s=412 frames=24310 ",
                "fps_mean=59.0 fps_worst=12.3\n",
                // The window of the total is the run itself, hence 412000 ms.
                "STOP meter window_ms=412000 draws_n=1706731 preparing_ms.sum=18344 ",
                "preparing_ms.max=4201 preparing_ms.last=312 preparing_ms.n=12\n",
                "STOP cache images_mb=229 shaders_mb=11\n",
                "STOP warn key=draw.refused count=4359\n",
            )
        );
    }

    #[test]
    fn a_run_that_never_drew_has_no_fps() {
        let mut footer = footer();
        footer.frames = 0;
        footer.worst_frame_ms = None;
        footer.meter = Snapshot::default();
        footer.cache.clear();
        footer.warns.clear();
        footer.reason = "error: \"no\" \n here".to_owned();
        assert_eq!(
            footer.render(),
            "STOP end reason=\"error: \\\"no\\\" \\n here\" duration_s=412 frames=0\n\
             STOP meter window_ms=0\n"
        );
    }

    #[test]
    fn both_blocks_are_single_marked_multiline() {
        // The shape, not the words: a reader greps one marker and gets one
        // block, and nothing else in the log carries it.
        let mut out: Vec<u8> = Vec::new();
        out.write_all(header().render().as_bytes()).expect("header");
        out.write_all(footer().render().as_bytes()).expect("footer");
        let text = String::from_utf8(out).expect("utf-8");
        let lines: Vec<&str> = text.lines().collect();
        let (run, stop): (Vec<&&str>, Vec<&&str>) = lines
            .iter()
            .partition(|line| line.starts_with("RUN "));
        assert_eq!(run.len() + stop.len(), lines.len(), "every line has a marker");
        assert_eq!(run.len(), 11, "{}", run.len());
        assert_eq!(stop.len(), 4, "{}", stop.len());
        assert!(stop.iter().all(|line| line.starts_with("STOP ")));
        assert!(lines.iter().all(|line| !line.ends_with(' ')), "{text}");
        assert!(text.ends_with('\n'), "the block ends the line it writes");
        // Nothing else may prefix a line: a second marker would break both.
        assert!(lines.iter().all(|line| line.starts_with("RUN ") || line.starts_with("STOP ")));
    }

    /// The end of a run is written once, however many paths reach it: the
    /// suspend routines and the deadline that stops waiting for them both call
    /// it, and the second must not write a second block over the first.
    #[test]
    fn the_end_of_a_run_is_written_once() {
        let first = stop_block("title suspended", Duration::from_secs(3))
            .expect("the first path writes the block");
        assert!(first.lines().all(|line| line.starts_with("STOP ")), "{first}");
        assert!(first.contains("\"title suspended\""), "{first}");
        assert!(
            stop_block("title did not finish suspending in time", Duration::ZERO).is_none(),
            "the second path is told the run already has its footer"
        );
    }

    /// The binary's writer renders the whole block when one is installed: it is
    /// the only renderer, so the run summary and the `STOP cache` lines are in
    /// the block whether the process returns from `run` or is terminated.
    #[test]
    fn a_footer_hook_renders_the_block_when_installed() {
        let hook: FooterHook = Box::new(|reason, duration| {
            format!(
                "STOP end reason={reason} duration_s={}\nSTOP cache images_mb=229\n",
                duration.as_secs()
            )
        });
        assert_eq!(
            render_stop_with(Some(hook), "title suspended", Duration::from_secs(3)),
            "STOP end reason=title suspended duration_s=3\nSTOP cache images_mb=229\n"
        );
        // No hook (a binary that installed none) still renders from counters.
        let bare = render_stop_with(None, "title suspended", Duration::ZERO);
        assert!(bare.starts_with("STOP end reason=\"title suspended\""), "{bare}");
        assert!(!bare.contains("STOP cache "), "{bare}");
    }

    /// The counters a path that leaves the process still has: the meter and
    /// the warning watches, never a directory.
    #[test]
    fn a_footer_from_counters_states_the_frames_it_was_given() {
        // `frames_n` is a process static and counts accumulate, so a run that
        // another test has already counted into is part of the figure: it is
        // read before the seven are added, and the two tests that render a
        // footer take turns.
        let _turn = STATED_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let meter = crate::meter::meter();
        let had = meter
            .totals()
            .metrics
            .iter()
            .find(|m| m.name == "frames_n")
            .map_or(0, |m| m.count);
        meter.count("frames_n", 7);
        let frames = had + 7;
        let text = RunFooter::from_counters("title exited with 0", Duration::from_secs(2)).render();
        let fps = frames as f64 / 2.0;
        assert!(
            text.contains(&format!("duration_s=2 frames={frames} fps_mean={fps:.1}")),
            "{text}"
        );
        assert!(text.lines().all(|line| line.starts_with("STOP ")), "{text}");
        assert!(!text.contains("STOP cache "), "a directory is not walked: {text}");
    }

    /// The summary `victory run` writes after the block quotes the run this
    /// states ([`stated_run`]), so it must be the block's figures as rendered,
    /// fixed when the block was built. The 2026-11-25 19:24 run read them back
    /// after the 1.7 s cache walk and stated 8,003 frames beside the block's
    /// 7,997.
    #[test]
    fn the_stated_run_is_what_the_block_says() {
        // `frames_n` is a process static, and so is what this states, so the
        // two tests that render a footer from counters take turns: no count of
        // its own, and no footer built between the render and the read.
        let _turn = STATED_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let text = RunFooter::from_counters("title exited with 0", Duration::from_secs(2)).render();
        let stated = stated_run().expect("a block was just rendered");
        assert!(
            text.starts_with(&format!(
                "STOP end reason=\"title exited with 0\" duration_s={} frames={}",
                stated.0.as_secs(),
                stated.1
            )),
            "the line states what the block states: {text}"
        );
        // A frame that lands after the block was built does not move it.
        crate::meter::meter().count("frames_n", 90);
        assert_eq!(stated_run(), Some(stated), "the line under the block followed the meter");
    }

    /// See [`the_stated_run_is_what_the_block_says`].
    static STATED_TESTS: Mutex<()> = Mutex::new(());

    fn record() -> RunRecord {        RunRecord {
            started_utc: "2026-11-25T06:12:50.123Z".to_owned(),
            ended_utc: "2026-11-25T06:13:02.456Z".to_owned(),
            commit: "504b1c6d9e2f4a7c1b3d5e6f7a8b9c0d1e2f3a4b".to_owned(),
            branch: "main".to_owned(),
            dirty: "false".to_owned(),
            console: "series-x".to_owned(),
            duration: Duration::from_millis(12_333),
            frames: 740,
            worst_frame_ms: Some(81),
            draws_mean: Some(1_706.44),
            gpu_wait_ms: Some(4.5),
            reason: "title exited with 0".to_owned(),
            log: Some(PathBuf::from(r"C:\v\data\logs\run-20261125-061250.log")),
        }
    }

    /// A directory to write a history file in: outside the tree, and never the
    /// same one twice, because the tests share a process.
    fn scratch(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        std::env::temp_dir().join(format!(
            "victory-report-{}-{name}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// The row is a wire format a script reads back, so its shape is the
    /// contract: one line, one tab per column, nothing quoted.
    #[test]
    fn a_run_is_one_row_of_tab_separated_figures() {
        assert_eq!(
            record().line(),
            "2026-11-25T06:12:50.123Z\t2026-11-25T06:13:02.456Z\t\
             504b1c6d9e2f4a7c1b3d5e6f7a8b9c0d1e2f3a4b\tmain\tfalse\tseries-x\t740\t12.3\t\
             60.0\t16.7\t12.3\t81\t1706.4\t4.5\ttitle exited with 0\t\
             C:\\v\\data\\logs\\run-20261125-061250.log\n"
        );
    }

    /// A run that never drew leaves those columns empty rather than zero: an
    /// average over nothing is not a slow run, and `0` would read as one.
    #[test]
    fn nothing_measured_leaves_its_column_empty() {
        let mut unmeasured = record();
        unmeasured.frames = 0;
        unmeasured.worst_frame_ms = None;
        unmeasured.draws_mean = None;
        unmeasured.gpu_wait_ms = None;
        unmeasured.log = None;
        let columns = unmeasured.columns();
        assert_eq!(columns.len(), RUNS_HEADER.split('\t').count(), "{columns:?}");
        assert_eq!(&columns[8..12], ["", "", "", ""], "{columns:?}");
        assert_eq!(&columns[12..14], ["", ""], "{columns:?}");
        assert_eq!(columns[15], "", "{columns:?}");
    }

    /// The free text cannot widen a row: a reason with a tab or a newline in it
    /// would otherwise become columns of its own.
    #[test]
    fn a_reason_cannot_add_columns() {
        let mut noisy = record();
        noisy.reason = "error: \"no\"\there\nnow".to_owned();
        let columns = noisy.columns();
        assert_eq!(columns.len(), RUNS_HEADER.split('\t').count(), "{columns:?}");
        assert_eq!(columns[14], "error: \"no\" here now");
    }

    /// One header, one row per run, and a run appending never loses a row an
    /// earlier one wrote: the file is read back months later, after a crash.
    #[test]
    fn the_history_file_keeps_a_header_and_a_row_per_run() {
        let path = scratch("history").join(RUNS_FILE);
        append_run(&path, &record()).expect("the first run makes the file");
        let first = std::fs::read_to_string(&path).expect("read the first row");
        let mut second = record();
        second.commit = "7668994b0c1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f".to_owned();
        second.frames = 5_344;
        append_run(&path, &second).expect("the second run appends");
        let text = std::fs::read_to_string(&path).expect("read both rows");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "one header and two rows:\n{text}");
        assert_eq!(lines[0], RUNS_HEADER, "the header is written once:\n{text}");
        assert!(text.starts_with(&first), "the first row is untouched:\n{text}");
        assert!(lines[2].contains("7668994b0c1d"), "{}", lines[2]);
        assert!(lines[2].contains("\t5344\t"), "{}", lines[2]);
        // Both rows are the same width, so a column read cannot shift.
        for row in [lines[1], lines[2]] {
            assert_eq!(row.split('\t').count(), lines[0].split('\t').count(), "{row}");
        }
    }

    /// Rule #1: a file this crate writes is checked against the tree it was
    /// built from, the same guard the logs go through.
    #[test]
    fn a_history_file_in_the_repository_is_refused() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join(RUNS_FILE);
        let refused = append_run(&inside, &record()).expect_err("the tree is not a log directory");
        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied, "{refused}");
        assert!(!Path::new(env!("CARGO_MANIFEST_DIR")).join(RUNS_FILE).exists());
    }

    /// The caption is drawn by another crate, which has no build script of its
    /// own that reads git: it asks here, and gets `unknown` before anyone has
    /// said.
    #[test]
    fn the_build_is_readable_from_another_crate() {
        set_build("504b1c6d9e2f", "504b1c6d");
        assert_eq!(commit_short(), "504b1c6d");
        assert_eq!(commit(), "504b1c6d9e2f");
    }

    #[test]
    fn a_marker_is_a_prefix_not_a_word_inside_a_line() {
        let text = header().render();
        for line in text.lines() {
            assert!(
                line[RUN.len()..].starts_with(' '),
                "{line} must start with `RUN `"
            );
        }
        let footer = footer().render();
        for line in footer.lines() {
            assert!(
                line[STOP.len()..].starts_with(' '),
                "{line} must start with `STOP `"
            );
        }
    }
}
