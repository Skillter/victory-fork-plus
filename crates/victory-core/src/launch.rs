//! The launch policy: whether a launched title is brought to the front and
//! has Enter pressed for it, so a run can get past its menus unattended.
//!
//! The file is the user's, not the build's: `<data-dir>/launch.toml`, that is
//! `%LOCALAPPDATA%\victory\data\launch.toml` on Windows, never a file in the
//! repository ([`ensure_outside_repo`]). [`load`] is called inside `run`, so
//! editing it changes the next run and nothing has to be rebuilt, and a run
//! started from any directory reads the same file.
//!
//! ```toml
//! focus = true          # bring the title's window to the front once it exists
//! enter = true          # press Enter for the title
//! enter_after_s = 35    # seconds after the window exists before the first press
//! enter_every_s = 20    # seconds between presses
//! enter_times = 12      # presses at most
//! ```
//!
//! A missing file is not an error: both features stay off ([`load`] answers
//! `Ok(None)`), and the caller says where the file would be.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::policy::{PolicyError, ensure_outside_repo};

/// The file's name inside the data directory.
pub const FILE_NAME: &str = "launch.toml";

/// Points at another file, for tests. Documented in `README.md`.
pub const ENV_PATH: &str = "VICTORY_LAUNCH_TOML";

/// Seconds after the window exists before the first press.
pub const DEFAULT_ENTER_AFTER_S: u64 = 35;
/// Seconds between presses.
pub const DEFAULT_ENTER_EVERY_S: u64 = 20;
/// Presses at most.
pub const DEFAULT_ENTER_TIMES: u32 = 12;

/// What a run does with the title's window, from `launch.toml`.
///
/// Every key is optional and defaults to off (or to the schedule's defaults),
/// so a file naming one key leaves the rest off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LaunchPolicy {
    /// Bring the title's window to the front once it exists.
    pub focus: bool,
    /// Press Enter for the title.
    pub enter: bool,
    /// Seconds after the window exists before the first press.
    pub enter_after_s: u64,
    /// Seconds between presses.
    pub enter_every_s: u64,
    /// Presses at most.
    pub enter_times: u32,
}

impl Default for LaunchPolicy {
    fn default() -> Self {
        Self {
            focus: false,
            enter: false,
            enter_after_s: DEFAULT_ENTER_AFTER_S,
            enter_every_s: DEFAULT_ENTER_EVERY_S,
            enter_times: DEFAULT_ENTER_TIMES,
        }
    }
}

impl LaunchPolicy {
    /// Whether this policy does anything at all.
    pub fn active(&self) -> bool {
        self.focus || self.enter
    }

    /// The presses this policy asks for, measured from the moment the window
    /// exists. Pure arithmetic, so the schedule is testable without a window.
    pub fn schedule(&self) -> Schedule {
        Schedule {
            first: Duration::from_secs(self.enter_after_s),
            every: Duration::from_secs(self.enter_every_s),
            times: self.enter_times,
        }
    }
}

/// When the schedule presses Enter: `times` presses, the first at `first`,
/// `every` apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// When the first press is due.
    pub first: Duration,
    /// The gap between one press and the next.
    pub every: Duration,
    /// How many presses the schedule asks for.
    pub times: u32,
}

impl Schedule {
    /// When press `index` (0-based) is due. `None` past the last press.
    pub fn at(&self, index: u32) -> Option<Duration> {
        (index < self.times).then(|| self.every.saturating_mul(index).saturating_add(self.first))
    }

    /// Every press, in order: what the worker waits for.
    pub fn presses(&self) -> impl Iterator<Item = Duration> + '_ {
        (0..self.times).filter_map(|index| self.at(index))
    }
}

/// Where the policy is read from: [`ENV_PATH`], else `<data_dir>/launch.toml`.
pub fn path(data_dir: &Path) -> PathBuf {
    path_in(data_dir, |key| std::env::var_os(key))
}

/// [`path`] with the environment passed in, so a test need not set one.
pub fn path_in(data_dir: &Path, env: impl Fn(&str) -> Option<std::ffi::OsString>) -> PathBuf {
    match env(ENV_PATH).filter(|value| !value.is_empty()) {
        Some(explicit) => PathBuf::from(explicit),
        None => data_dir.join(FILE_NAME),
    }
}

/// Reads the policy at `path`.
///
/// `Ok(None)` when the file is not there: both features are off and the caller
/// says where the file would be. `Err` when it cannot be read, does not parse,
/// or lies inside the source tree; the caller warns and boots the title with
/// both features off, because a bad config never stops a run.
pub fn load(path: &Path) -> Result<Option<LaunchPolicy>, LaunchError> {
    ensure_outside_repo(path)?;
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(LaunchError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let policy: LaunchPolicy = toml::from_str(&text).map_err(|source| LaunchError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(policy))
}

/// What one read of the policy decided, in the words the run's log states.
///
/// One `RUN launch ...` line always says what the run applies, so a log reader
/// knows whether focus and Enter were asked for; a file that cannot be used
/// adds the warning that names it and says why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readout {
    /// The policy the run applies. Both features off when the file is absent
    /// or unusable: a bad config never stops the title from booting.
    pub policy: LaunchPolicy,
    /// The `RUN launch ...` line.
    pub info: String,
    /// The warning, when the file is there and is not a policy.
    pub warn: Option<String>,
}

/// Reads `path` and words the result for the run's log.
pub fn read(path: &Path) -> Readout {
    let (policy, note, warn) = match load(path) {
        Ok(Some(policy)) => (policy, "", None),
        Ok(None) => (
            LaunchPolicy::default(),
            " (not found: focus and enter off)",
            None,
        ),
        Err(error) => (
            LaunchPolicy::default(),
            " (unusable: focus and enter off)",
            Some(format!("launch policy: {error}; focus and enter are off")),
        ),
    };
    Readout {
        policy,
        info: format!(
            "RUN launch focus={} enter={} after={} every={} times={} file={}{note}",
            policy.focus,
            policy.enter,
            policy.enter_after_s,
            policy.enter_every_s,
            policy.enter_times,
            path.display()
        ),
        warn,
    }
}

/// Why the policy at a path could not be read.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    /// The file would be inside the source tree (rule #1).
    #[error("{0}")]
    OutsideRepo(#[from] PolicyError),
    /// The file is there and could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The file that was read.
        path: PathBuf,
        /// Why it could not be read.
        source: std::io::Error,
    },
    /// The file was read and is not a policy.
    #[error("{path} does not parse: {source}")]
    Parse {
        /// The file that was read.
        path: PathBuf,
        /// What the parser objected to.
        source: toml::de::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn a_missing_file_leaves_both_features_off() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = load(&tmp.path().join(FILE_NAME)).unwrap();
        assert_eq!(policy, None);
    }

    #[test]
    fn an_empty_file_is_the_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), FILE_NAME, "");
        assert_eq!(load(&path).unwrap(), Some(LaunchPolicy::default()));
    }

    #[test]
    fn the_defaults_are_off_with_the_documented_schedule() {
        let policy = LaunchPolicy::default();
        assert!(!policy.focus);
        assert!(!policy.enter);
        assert!(!policy.active());
        assert_eq!(policy.enter_after_s, 35);
        assert_eq!(policy.enter_every_s, 20);
        assert_eq!(policy.enter_times, 12);
    }

    #[test]
    fn a_full_file_is_read_as_written() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(
            tmp.path(),
            FILE_NAME,
            "focus = true\nenter = true\nenter_after_s = 5\nenter_every_s = 7\nenter_times = 3\n",
        );
        let policy = load(&path).unwrap().unwrap();
        assert!(policy.focus && policy.enter && policy.active());
        assert_eq!(
            policy.schedule(),
            Schedule {
                first: Duration::from_secs(5),
                every: Duration::from_secs(7),
                times: 3,
            }
        );
    }

    #[test]
    fn a_partial_file_keeps_the_other_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), FILE_NAME, "enter = true\n");
        let policy = load(&path).unwrap().unwrap();
        assert!(!policy.focus);
        assert!(policy.enter);
        assert_eq!(policy.enter_after_s, DEFAULT_ENTER_AFTER_S);
        assert_eq!(policy.enter_every_s, DEFAULT_ENTER_EVERY_S);
        assert_eq!(policy.enter_times, DEFAULT_ENTER_TIMES);
    }

    #[test]
    fn an_unknown_key_is_refused_and_named() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(
            tmp.path(),
            FILE_NAME,
            "focus = true\nfocus_on_start = true\n",
        );
        let error = load(&path).unwrap_err();
        let text = error.to_string();
        assert!(matches!(error, LaunchError::Parse { .. }), "{error}");
        assert!(text.contains("focus_on_start"), "{text}");
        assert!(text.contains(&path.display().to_string()), "{text}");
    }

    // The property the feature exists for: the file is read per run, so two
    // runs in one process (and two runs started from anywhere) obey two
    // different files without a rebuild.
    #[test]
    fn two_files_read_in_one_process_give_two_policies() {
        let tmp = tempfile::tempdir().unwrap();
        let off = write(tmp.path(), "off.toml", "focus = false\nenter = false\n");
        let on = write(tmp.path(), "on.toml", "focus = true\nenter = true\n");
        let first = load(&off).unwrap().unwrap();
        let second = load(&on).unwrap().unwrap();
        assert!(!first.active());
        assert!(second.active());
        assert_ne!(first, second);
    }

    #[test]
    fn the_schedule_is_one_press_after_the_other() {
        let schedule = Schedule {
            first: Duration::from_secs(35),
            every: Duration::from_secs(20),
            times: 3,
        };
        let presses: Vec<_> = schedule.presses().collect();
        assert_eq!(
            presses,
            vec![
                Duration::from_secs(35),
                Duration::from_secs(55),
                Duration::from_secs(75)
            ]
        );
        assert_eq!(schedule.at(2), Some(Duration::from_secs(75)));
        assert_eq!(schedule.at(3), None);
    }

    #[test]
    fn no_presses_means_an_empty_schedule() {
        let schedule = LaunchPolicy {
            enter_times: 0,
            ..LaunchPolicy::default()
        }
        .schedule();
        assert_eq!(schedule.presses().count(), 0);
        assert_eq!(schedule.at(0), None);
    }

    #[test]
    fn a_huge_interval_saturates_instead_of_panicking() {
        let schedule = LaunchPolicy {
            enter_after_s: u64::MAX,
            enter_every_s: u64::MAX,
            enter_times: 4,
            ..LaunchPolicy::default()
        }
        .schedule();
        assert_eq!(schedule.at(3), Some(Duration::MAX));
    }

    #[test]
    fn the_path_is_the_data_directory_unless_the_environment_names_one() {
        let data = Path::new("data");
        assert_eq!(path_in(data, |_| None), data.join(FILE_NAME));
        assert_eq!(
            path_in(data, |_| Some(std::ffi::OsString::new())),
            data.join(FILE_NAME)
        );
        let explicit = path_in(data, |key| {
            (key == ENV_PATH).then(|| std::ffi::OsString::from("elsewhere\\launch.toml"))
        });
        assert_eq!(explicit, PathBuf::from("elsewhere\\launch.toml"));
    }

    #[test]
    fn a_file_inside_the_source_tree_is_refused() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join(FILE_NAME);
        assert!(matches!(load(&inside), Err(LaunchError::OutsideRepo(_))));
    }

    // The three cases the log states, one line each: a missing file is not a
    // warning, a policy is stated as read, a file that is not a policy warns
    // and leaves both features off.
    #[test]
    fn a_missing_file_states_where_it_would_be_without_a_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(FILE_NAME);
        let readout = read(&path);
        assert_eq!(readout.policy, LaunchPolicy::default());
        assert_eq!(readout.warn, None);
        assert_eq!(
            readout.info,
            format!(
                "RUN launch focus=false enter=false after=35 every=20 times=12 file={} \
                 (not found: focus and enter off)",
                path.display()
            )
        );
    }

    #[test]
    fn a_policy_is_stated_as_read() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(
            tmp.path(),
            FILE_NAME,
            "focus = true\nenter = true\nenter_after_s = 5\nenter_every_s = 7\nenter_times = 3\n",
        );
        let readout = read(&path);
        assert!(readout.policy.active());
        assert_eq!(readout.warn, None);
        assert_eq!(
            readout.info,
            format!(
                "RUN launch focus=true enter=true after=5 every=7 times=3 file={}",
                path.display()
            )
        );
    }

    #[test]
    fn a_file_that_is_not_a_policy_warns_and_turns_both_off() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), FILE_NAME, "focus = \"yes\"\n");
        let readout = read(&path);
        assert_eq!(readout.policy, LaunchPolicy::default());
        let warn = readout.warn.expect("a warning for an unusable file");
        assert!(warn.contains(&path.display().to_string()), "{warn}");
        assert!(warn.contains("focus and enter are off"), "{warn}");
        assert!(readout.info.ends_with("(unusable: focus and enter off)"));
    }
}
