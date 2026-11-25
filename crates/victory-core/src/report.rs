//! The two records a run leaves behind: `RUN` at the start and `STOP` at the
//! end.
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

use std::path::{Path, PathBuf};
use std::sync::Mutex;
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
    /// `VICTORY_BUILD` from `apps/victory/build.rs`: commit, plus `+dirty`.
    pub build: String,
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
        line(format!("bin name={} build={}", self.program, self.build));
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
        RunFooter {
            reason: reason.to_owned(),
            duration,
            frames: metric("frames_n").map_or(0, |m| m.count),
            worst_frame_ms: metric("frame_ms").map(|m| m.max).filter(|ms| *ms > 0),
            meter,
            cache: Vec::new(),
            warns: crate::logging::warn_once_totals(),
        }
    }
}

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

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::meter::{Kind, Metric};

    fn header() -> RunHeader {
        RunHeader {
            program: "victory 0.1.0".to_owned(),
            build: "504b1c6+dirty".to_owned(),
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
                "RUN bin name=victory 0.1.0 build=504b1c6+dirty\n",
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
        crate::meter::meter().count("frames_n", 7);
        let text = RunFooter::from_counters("title exited with 0", Duration::from_secs(2)).render();
        assert!(
            text.contains("duration_s=2 frames=7 fps_mean=3.5"),
            "{text}"
        );
        assert!(text.lines().all(|line| line.starts_with("STOP ")), "{text}");
        assert!(!text.contains("STOP cache "), "a directory is not walked: {text}");
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
