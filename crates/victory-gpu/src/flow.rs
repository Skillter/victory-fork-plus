//! Where a frame's bytes go between title memory and ours: which call site
//! moved them, how many bytes, and how long it took, logged every second
//! beside the frame rate in every build.
//!
//! The frame line splits a frame into `copies`, `textures` and `copying
//! back`; this says which of the copies' call sites took the time, and at
//! what rate the bytes moved (a `memcpy` on the machine this was measured
//! on does 14.1 GB/s, a 64 KiB compare 15.0 GB/s: `copystat`'s doc). The
//! ranges and the memory kinds behind them are `copystat`'s to name, and
//! that module is in diagnostics builds only.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// A call site that moves bytes of title memory, or reads them to compare.
#[derive(Clone, Copy)]
pub(crate) enum Site {
    /// Host memory into title memory at submission (`host`'s `Step::Copy`
    /// and `Step::Fill`, through `copy_mapped`; the copies of one run, whose
    /// destinations carry on, are one figure).
    IntoTitle,
    /// Title memory into a host copy in place, under the guard that stops
    /// the runtime changing what it shows (`Shadow::read_in_place`).
    InPlace,
    /// Title memory into a host copy through the kernel (`copy_mapped`,
    /// after a page of the range was not there to read in place).
    Kernel,
    /// Of [`Site::InPlace`] and [`Site::Kernel`]: the bytes of a copy the
    /// write watch says nothing about, copied whole (`Shadow::update_free`).
    /// Counted in both, so it is a part of them, not a site of its own.
    Whole,
    /// Title memory read and found equal to the copy's bytes, nothing
    /// copied (`Shadow::copy_unless_there`): the read is counted in
    /// [`Site::InPlace`] or [`Site::Kernel`], this in what it was for.
    Kept,
    /// A copy's bytes kept before a submission, to find what shaders wrote
    /// (`Shadow::writing`).
    Snapshot,
    /// A copy compared with the bytes kept before the submission
    /// (`Shadow::copy_back_with`, `shadow::find_all`).
    Compared,
    /// What shaders wrote put back into title memory (`Shadow::copy_back`,
    /// `Shadow::relay_merge`, `Shadow::card_merge`).
    PutBack,
    /// Pieces fetched from copies in the card's memory to ours
    /// (`Host::relay_fetch_merge`): the bytes, and the fetch's own run on the
    /// card (not the waiting behind work sent before it).
    Fetched,
}

/// One label per [`Site`], in its order. `of` marks a part of the sites
/// above it rather than a site of its own.
const LABELS: [(Site, &str); 9] = [
    (Site::IntoTitle, "into title memory"),
    (Site::InPlace, "out of title memory in place"),
    (Site::Kernel, "out of title memory through the kernel"),
    (Site::Whole, "of those, whole (the write watch says nothing)"),
    (Site::Kept, "of those, read and found equal (nothing copied)"),
    (Site::Snapshot, "snapshotted for shader writes"),
    (Site::Compared, "compared with the snapshot"),
    (Site::PutBack, "put back into title memory"),
    (Site::Fetched, "fetched from the card's copies"),
];

const SITES: usize = LABELS.len();
/// Calls, bytes, nanoseconds per site.
const SLOTS: usize = SITES * 3;
static COUNT: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

/// `bytes` were moved at `site` in `nanos`.
pub(crate) fn put(site: Site, bytes: usize, nanos: u64) {
    let at = site as usize * 3;
    COUNT[at].fetch_add(1, Relaxed);
    COUNT[at + 1].fetch_add(bytes as u64, Relaxed);
    COUNT[at + 2].fetch_add(nanos, Relaxed);
}

/// [`put`], timing from `started`.
pub(crate) fn took(site: Site, bytes: usize, started: std::time::Instant) {
    put(site, bytes, started.elapsed().as_nanos() as u64);
}

/// The line: every site that moved bytes since the last one, per frame.
pub(crate) fn line(frames: f64) {
    if let Some(text) = take(frames) {
        tracing::info!("where the frame's bytes went, per frame: {text}");
    }
}

/// [`line`]'s text, taken from the counters: `None` when nothing moved.
fn take(frames: f64) -> Option<String> {
    let v: Vec<f64> = COUNT.iter().map(|c| c.swap(0, Relaxed) as f64 / frames).collect();
    if v.iter().all(|&n| n == 0.0) {
        return None;
    }
    let parts: Vec<String> = LABELS
        .iter()
        .map(|&(site, label)| {
            let at = site as usize * 3;
            let (calls, bytes, nanos) = (v[at], v[at + 1], v[at + 2]);
            if calls == 0.0 {
                return format!("{label} none");
            }
            // Bytes a nanosecond is 1 GB/s.
            let rate = if nanos > 0.0 { bytes / nanos } else { 0.0 };
            format!("{label} {calls:.0}x {:.2} MB {:.2} ms ({rate:.2} GB/s)", bytes / 1e6, nanos / 1e6)
        })
        .collect();
    Some(parts.join("; "))
}

#[cfg(test)]
mod tests {
    use super::{COUNT, LABELS, SITES, Site, put};

    /// A site's bytes and time add up, and the labels name every site once.
    #[test]
    fn every_site_is_labelled_once() {
        let mut seen: Vec<usize> = LABELS.iter().map(|&(site, _)| site as usize).collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..SITES).collect::<Vec<_>>());
    }

    /// A count that is put is the one the line would take, and the line
    /// clears what it took.
    #[test]
    fn a_put_reaches_the_line_and_is_taken() {
        let at = Site::PutBack as usize * 3;
        put(Site::PutBack, 4096, 1000);
        put(Site::PutBack, 1024, 3000);
        assert_eq!(COUNT[at].load(super::Relaxed), 2);
        assert_eq!(COUNT[at + 1].load(super::Relaxed), 5120);
        assert_eq!(COUNT[at + 2].load(super::Relaxed), 4000);
        // No frames since: the line divides by one, so the bytes survive.
        let text = super::take(1.0).expect("something moved");
        assert!(text.contains("put back into title memory 2x 0.01 MB 0.00 ms"), "{text}");
        assert!(text.contains("fetched from the card's copies none"), "{text}");
        assert_eq!(COUNT[at].load(super::Relaxed), 0);
        assert_eq!(COUNT[at + 1].load(super::Relaxed), 0);
        assert_eq!(COUNT[at + 2].load(super::Relaxed), 0);
        // Nothing moved: no line at all.
        assert!(super::take(1.0).is_none());
    }

    /// A site's rate is its bytes a nanosecond: 14 MB in 1 ms is 14 GB/s,
    /// the rate a `memcpy` on the machine measured does.
    #[test]
    fn the_rate_is_bytes_a_nanosecond() {
        put(Site::InPlace, 14_000_000, 1_000_000);
        let text = super::take(1.0).expect("something moved");
        assert!(text.contains("out of title memory in place 1x 14.00 MB 1.00 ms (14.00 GB/s)"), "{text}");
    }
}
