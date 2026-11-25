//! Shared plumbing for every VIctory crate and tool.
//!
//! * [`config`]: where the user's files live (never inside the repository).
//! * [`policy`]: the runtime half of rule #1, refusing to write
//!   game-derived data into the source tree.
//! * [`launch`]: the user's `launch.toml`, read at every launch: focus the
//!   title's window, press Enter for it, or neither.
//! * [`logging`]: one tracing setup for all binaries, plus the records a
//!   filter cannot hide: the run header, the run footer and `warn_once`.
//! * [`meter`]: named counters and timers, cheap enough for a per-draw path.
//! * [`report`]: how the header and the footer are rendered, from plain
//!   values, so both are testable without a run.

pub mod config;
pub mod launch;
pub mod logging;
pub mod meter;
pub mod policy;
pub mod report;

pub use config::Config;
pub use policy::{PolicyError, ensure_outside_repo};
