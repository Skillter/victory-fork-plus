//! Named counters and timers for the run loop.
//!
//! The frame line in `victory_gpu::host` accounts for copies and textures; a
//! 4.2 s stall hid in the rest of the frame because no line named that rest.
//! A [`Meter`] gives every step a name, so a number that grows can be found by
//! grepping its name instead of by reading the code that measures the others.
//!
//! Cost, so nobody has to guess before putting one on a per-draw path:
//!
//! * a name resolves through a 256-slot open-addressed table, one 64-bit hash
//!   of the name's bytes and one to three acquire loads (no lock, no
//!   allocation; a name registers on its first call, once per process);
//! * [`Meter::count`] is then one relaxed add — a counter has one number;
//! * [`Meter::observe`] and [`Meter::time`] are four relaxed operations on one
//!   cache line (samples, sum, max, last), because a duration needs all four;
//! * [`Meter::take_window`] is cold: it locks, drains and allocates.
//!
//! A name that finds no free slot is dropped: a table that grows would need a
//! lock on the path this exists to measure.

use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Slots in the table. Every counter the GPU, the loader and the HLE modules
/// want fits with room to spare; the array is 256 × 48 bytes, static.
const SLOTS: usize = 256;

/// How a name is recorded, fixed by its first call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// [`Meter::count`]: the numbers add up to one total (`draws_n=1740`).
    Counter,
    /// [`Meter::observe`] and [`Meter::time`]: a sample of a value that has a
    /// worst case (`preparing_ms.max=420`).
    Observed,
}

/// A registered name, published once through its slot's [`OnceLock`].
#[derive(Clone, Copy)]
struct Name {
    text: &'static str,
    kind: Kind,
}

/// The four numbers of one name, on one cache line.
struct Cell {
    /// Samples taken, or the total for a [`Kind::Counter`].
    count: AtomicU64,
    sum: AtomicU64,
    max: AtomicU64,
    last: AtomicU64,
}

impl Cell {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            sum: AtomicU64::new(0),
            max: AtomicU64::new(0),
            last: AtomicU64::new(0),
        }
    }

    /// Empties the window; whatever another thread adds during this is counted
    /// in the next one, which is what a 5 s window is for.
    fn drain(&self) -> (u64, u64, u64, u64) {
        let relaxed = Ordering::Relaxed;
        (
            self.count.swap(0, relaxed),
            self.sum.swap(0, relaxed),
            self.max.swap(0, relaxed),
            self.last.swap(0, relaxed),
        )
    }
}

/// One name's numbers for one window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metric {
    pub name: &'static str,
    pub kind: Kind,
    /// Samples ([`Kind::Observed`]) or the accumulated total ([`Kind::Counter`]).
    pub count: u64,
    pub sum: u64,
    pub max: u64,
    pub last: u64,
}

impl Metric {
    /// `name=count` for a counter; `name.sum=/max=/last=/n=` for a sample.
    ///
    /// The unit lives in the name (`_ms`, `_mb`, `_b`, `_n`), never in the
    /// value, and every number is an integer, so a line from one run compares
    /// textually with the line from another.
    pub fn fields(&self) -> String {
        match self.kind {
            Kind::Counter => format!("{}={}", self.name, self.count),
            Kind::Observed => format!(
                "{}.sum={} {}.max={} {}.last={} {}.n={}",
                self.name, self.sum, self.name, self.max, self.name, self.last, self.name, self.count
            ),
        }
    }
}

/// Everything one window recorded, sorted by name.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Time from the previous drain (or from [`Meter`] creation).
    pub window: Duration,
    /// Metrics with activity in this window, sorted by name.
    pub metrics: Vec<Metric>,
}

impl Snapshot {
    /// The `key=value` fields, without the marker, so a footer can reuse them.
    pub fn fields(&self) -> String {
        let mut out = format!("window_ms={}", self.window.as_millis());
        for metric in &self.metrics {
            out.push(' ');
            out.push_str(&metric.fields());
        }
        out
    }

    /// `WINDOW window_ms=5003 draws_n=1740 frame_ms.sum=4998 …`, one line.
    ///
    /// The marker is the word an agent greps; the fields are stable, ordered,
    /// integer, and the same set for the same set of names.
    pub fn line(&self) -> String {
        format!("WINDOW {}", self.fields())
    }
}

/// When this meter began, and when the open window began.
struct Clock {
    created: Instant,
    window: Instant,
}

/// Named counters and timers. [`meter`] is the process-wide one; a test or a
/// second subsystem makes its own.
pub struct Meter {
    names: [OnceLock<Name>; SLOTS],
    cells: [Cell; SLOTS],
    clock: OnceLock<Mutex<Clock>>,
    /// Everything drained so far, so the end of the run can total the start.
    totals: OnceLock<Mutex<Vec<Metric>>>,
}

impl Default for Meter {
    fn default() -> Self {
        Self::new()
    }
}

impl Meter {
    pub const fn new() -> Self {
        Self {
            names: [const { OnceLock::new() }; SLOTS],
            cells: [const { Cell::new() }; SLOTS],
            clock: OnceLock::new(),
            totals: OnceLock::new(),
        }
    }

    fn clock(&self) -> &Mutex<Clock> {
        self.clock.get_or_init(|| {
            let now = Instant::now();
            Mutex::new(Clock {
                created: now,
                window: now,
            })
        })
    }

    /// The slot for `name`, registering it on first use. `None` when the table
    /// is full, which drops the metric rather than locking the hot path.
    fn slot(&self, name: &'static str, kind: Kind) -> Option<usize> {
        let mut index = (hash(name) as usize) & (SLOTS - 1);
        for _ in 0..SLOTS {
            match self.names[index].get() {
                Some(registered) if registered.text == name => return Some(index),
                Some(_) => {}
                // Two threads that race here both store an equal `Name`, so
                // the loser re-reads the slot and finds the winner's.
                None => {
                    if self.names[index].set(Name { text: name, kind }).is_ok() {
                        // The run's clock starts with its first metric: a
                        // window the end of the run reports is the span the
                        // totals cover, and a clock first read inside
                        // `totals` would date the run from that read. The
                        // 2026-11-25 19:24 run lasted 903 s and its `STOP
                        // meter` line said `window_ms=0`.
                        let _ = self.clock();
                        return Some(index);
                    }
                    continue;
                }
            }
            index = (index + 1) & (SLOTS - 1);
        }
        None
    }

    /// Adds `n` to the counter `name`: `count("draws_n", 1)` per draw.
    pub fn count(&self, name: &'static str, n: u64) {
        if let Some(slot) = self.slot(name, Kind::Counter) {
            self.cells[slot].count.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Records one sample of `name`, in the unit the name carries.
    pub fn observe(&self, name: &'static str, value: u64) {
        if let Some(slot) = self.slot(name, Kind::Observed) {
            let cell = &self.cells[slot];
            let relaxed = Ordering::Relaxed;
            cell.count.fetch_add(1, relaxed);
            cell.sum.fetch_add(value, relaxed);
            cell.max.fetch_max(value, relaxed);
            cell.last.store(value, relaxed);
        }
    }

    /// Records one duration, converted to the unit the name ends with: `_ns`,
    /// `_us` (the default), `_ms` or `_s`. A sub-millisecond duration recorded
    /// under a `_ms` name is 0, so a fast path wants `_us`.
    pub fn time(&self, name: &'static str, d: Duration) {
        self.observe(name, in_unit(name, d));
    }

    /// Drains the open window and adds it to [`Meter::totals`].
    pub fn take_window(&self) -> Snapshot {
        let (created, window) = {
            let mut clock = self.clock().lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            let window = now.saturating_duration_since(clock.window);
            clock.window = now;
            (clock.created, window)
        };
        let _ = created;
        let mut metrics = Vec::new();
        for (slot, name) in self.names.iter().enumerate() {
            let Some(name) = name.get() else { continue };
            let cell = &self.cells[slot];
            let (count, sum, max, last) = cell.drain();
            if count == 0 && sum == 0 && max == 0 && last == 0 {
                continue;
            }
            metrics.push(Metric {
                name: name.text,
                kind: name.kind,
                count,
                sum,
                max,
                last,
            });
        }
        metrics.sort_by_key(|metric| metric.name);
        let snapshot = Snapshot { window, metrics };
        self.merge(&snapshot);
        snapshot
    }

    fn merge(&self, snapshot: &Snapshot) {
        let mut totals = self
            .totals
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for metric in &snapshot.metrics {
            match totals.iter_mut().find(|total| total.name == metric.name) {
                Some(total) => {
                    total.count = total.count.saturating_add(metric.count);
                    total.sum = total.sum.saturating_add(metric.sum);
                    total.max = total.max.max(metric.max);
                    total.last = metric.last;
                }
                None => totals.push(*metric),
            }
        }
    }

    /// Everything since this meter was created, draining the open window first.
    /// This is what the end of a run reports.
    pub fn totals(&self) -> Snapshot {
        let _ = self.take_window();
        let created = self
            .clock()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .created;
        let mut metrics = self
            .totals
            .get()
            .map(|totals| totals.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .unwrap_or_default();
        metrics.sort_by_key(|metric| metric.name);
        Snapshot {
            window: Instant::now().saturating_duration_since(created),
            metrics,
        }
    }

    /// The window that just closed, when `interval` has passed since the last
    /// report. Call it from the tick the run loop already has — the 5 s line in
    /// `crates/victory-gpu/src/host.rs` is that tick — and log the result:
    ///
    /// ```ignore
    /// if let Some(window) = victory_core::meter::meter().report_every(FIVE_SECONDS) {
    ///     tracing::info!("{}", window.line());
    /// }
    /// ```
    ///
    /// One thread calls it: two that drain at once each get half a window.
    pub fn report_every(&self, interval: Duration) -> Option<Snapshot> {
        let age = {
            let clock = self.clock().lock().unwrap_or_else(|e| e.into_inner());
            Instant::now().saturating_duration_since(clock.window)
        };
        if age < interval {
            return None;
        }
        Some(self.take_window())
    }
}

/// The process's meter: every crate records into this one, so a single line
/// and the footer describe them all.
pub static METER: Meter = Meter::new();

pub fn meter() -> &'static Meter {
    &METER
}

/// A duration in the unit the name declares, so the value never needs a
/// suffix of its own.
fn in_unit(name: &str, d: Duration) -> u64 {
    let clamped = |value: u128| u64::try_from(value).unwrap_or(u64::MAX);
    if name.ends_with("_ns") {
        clamped(d.as_nanos())
    } else if name.ends_with("_ms") {
        clamped(d.as_millis())
    } else if name.ends_with("_s") {
        d.as_secs()
    } else {
        clamped(d.as_micros())
    }
}

/// FNV-1a: no dependency, fast for the short names a metric has, and stable
/// across runs and compilers (a hash that moves would reorder nothing here,
/// since the lines are sorted by name).
fn hash(name: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn a_window_reports_count_sum_max_last() {
        let meter = Meter::new();
        meter.count("draws_n", 1);
        meter.count("draws_n", 2);
        meter.observe("preparing_ms", 4);
        meter.observe("preparing_ms", 420);
        meter.observe("preparing_ms", 7);
        meter.time("stall_us", MS * 3);

        let first = meter.take_window();
        let metric = |name: &str| {
            *first
                .metrics
                .iter()
                .find(|metric| metric.name == name)
                .unwrap_or_else(|| panic!("{name} in {first:?}"))
        };
        let draws = metric("draws_n");
        assert_eq!((draws.kind, draws.count, draws.sum, draws.max), (Kind::Counter, 3, 0, 0));
        let preparing = metric("preparing_ms");
        assert_eq!(preparing.kind, Kind::Observed);
        // count = samples, sum = total, max = worst, last = most recent.
        assert_eq!((preparing.count, preparing.sum, preparing.max, preparing.last), (3, 431, 420, 7));
        // `_us` keeps the sub-millisecond precision a `_ms` name would lose.
        assert_eq!(metric("stall_us").sum, 3_000);

        // The second window starts empty and counts only what happened after.
        meter.count("draws_n", 5);
        let second = meter.take_window();
        assert_eq!(second.metrics.len(), 1, "{second:?}");
        assert_eq!(second.metrics[0].count, 5);
        assert!(first.window <= second.window || second.window > Duration::ZERO);
    }

    #[test]
    fn the_total_survives_the_windows() {
        let meter = Meter::new();
        for _ in 0..3 {
            meter.observe("frame_ms", 16);
            meter.count("presented_n", 1);
            let _ = meter.take_window();
        }
        meter.observe("frame_ms", 40);
        let totals = meter.totals();
        let frame = totals
            .metrics
            .iter()
            .find(|metric| metric.name == "frame_ms")
            .expect("frame_ms");
        assert_eq!((frame.count, frame.sum, frame.max, frame.last), (4, 88, 40, 40));
        let presented = totals
            .metrics
            .iter()
            .find(|metric| metric.name == "presented_n")
            .expect("presented_n");
        assert_eq!(presented.count, 3);
    }

    /// A run's totals name the run: the span they cover starts with the first
    /// metric, not with the moment they are read. (Before this held, the
    /// 2026-11-25 19:24 run - 903 s, 7,997 frames - printed `window_ms=0` and
    /// 406,883 draws in it.)
    #[test]
    fn the_total_covers_the_time_since_the_first_metric() {
        let meter = Meter::new();
        meter.count("presented_n", 1);
        std::thread::sleep(Duration::from_millis(15));
        let totals = meter.totals();
        assert!(
            totals.window >= Duration::from_millis(15),
            "the totals' window is the run, not the read: {:?}",
            totals.window
        );
    }

    #[test]
    fn the_line_is_exact() {
        let snapshot = Snapshot {
            window: Duration::from_millis(5_003),
            metrics: vec![
                Metric {
                    name: "draws_n",
                    kind: Kind::Counter,
                    count: 1740,
                    sum: 0,
                    max: 0,
                    last: 0,
                },
                Metric {
                    name: "preparing_ms",
                    kind: Kind::Observed,
                    count: 12,
                    sum: 18344,
                    max: 4201,
                    last: 312,
                },
            ],
        };
        assert_eq!(
            snapshot.line(),
            "WINDOW window_ms=5003 draws_n=1740 preparing_ms.sum=18344 preparing_ms.max=4201 \
             preparing_ms.last=312 preparing_ms.n=12"
        );
        // An idle window is still a line, so a gap in the log means a gap in
        // the run and not a silent meter.
        let idle = Snapshot {
            window: Duration::from_millis(5_000),
            metrics: Vec::new(),
        };
        assert_eq!(idle.line(), "WINDOW window_ms=5000");
    }

    #[test]
    fn metrics_come_out_sorted_by_name() {
        let meter = Meter::new();
        for name in ["zeta_n", "alpha_n", "mid_n"] {
            meter.count(name, 1);
        }
        let names: Vec<&str> = meter
            .take_window()
            .metrics
            .iter()
            .map(|metric| metric.name)
            .collect();
        assert_eq!(names, ["alpha_n", "mid_n", "zeta_n"]);
    }

    #[test]
    fn report_every_fires_only_when_the_interval_passed() {
        let meter = Meter::new();
        meter.count("draws_n", 1);
        // The open window is younger than an hour and older than nothing.
        assert!(meter.report_every(Duration::from_secs(3_600)).is_none());
        let now = meter.report_every(Duration::ZERO).expect("a closed window");
        assert_eq!(now.metrics.len(), 1);
        // Draining emptied it, so the next zero-interval report is idle.
        assert!(meter.report_every(Duration::ZERO).expect("idle").metrics.is_empty());
    }

    #[test]
    fn a_full_table_drops_rather_than_grows() {
        let meter = Meter::new();
        for i in 0..SLOTS {
            // Leaked so the names are `'static` like a real caller's.
            let name: &'static str = Box::leak(format!("metric_{i}_n").into_boxed_str());
            meter.count(name, 1);
        }
        meter.count("one_too_many_n", 1);
        let window = meter.take_window();
        assert_eq!(window.metrics.len(), SLOTS, "{window:?}");
        assert!(window.metrics.iter().all(|metric| metric.name != "one_too_many_n"));
    }

    #[test]
    fn a_name_resolves_to_the_same_slot_twice() {
        let meter = Meter::new();
        let first = meter.slot("draws_n", Kind::Counter).expect("slot");
        let second = meter.slot("draws_n", Kind::Observed).expect("slot");
        // The first call fixes the kind: a name is one metric, not two.
        assert_eq!(first, second);
        assert_eq!(meter.names[first].get().expect("registered").kind, Kind::Counter);
    }

    #[test]
    fn durations_take_the_unit_of_the_name() {
        let d = Duration::from_micros(1_500);
        assert_eq!(in_unit("copy_us", d), 1_500);
        assert_eq!(in_unit("copy_ms", d), 1);
        assert_eq!(in_unit("copy_ms", Duration::from_secs(2)), 2_000);
        // Whole seconds, so a sub-second duration is 0: `_s` is for a run, not
        // for a step.
        assert_eq!(in_unit("copy_s", d), 0);
        assert_eq!(in_unit("copy_ns", d), 1_500_000);
    }
}
