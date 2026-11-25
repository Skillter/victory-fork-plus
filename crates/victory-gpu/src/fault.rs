//! What limits the copies out of title memory: the process's page faults and
//! its memory, printed beside the rate the copies ran at.
//!
//! 96 MB in 1.4 s (68 MB/s) is not a copy: `ReadProcessMemory` of our own
//! process runs at about 13 GB/s on pages that are in memory. The rest is work
//! the copy makes the kernel do, and there are three kinds: a source page that
//! is not resident (it comes back from the pagefile), a destination page that
//! is not (the first write into a fresh window, or a page trimmed out of one),
//! and whatever each page costs in crossing the boundary. The fault count and
//! one `K32QueryWorkingSetEx` over a sample of the source and of the
//! destination pages, taken before a copy and again after it, tell them apart
//! in one run without measuring any of it twice.
//!
//! Costs: two kernel calls a window for the process's memory, three relaxed
//! atomics a copy of [`TIME_AT`] and more, and, for one copy in
//! [`PROBE_EVERY`] of [`PROBE_AT`] and more, two more for the sample. Nothing
//! here reads the memory it asks about, so the sample cannot fault in what it
//! measures.
//!
//! Counters are totals since the process started and are never drained: a
//! reader takes the difference it needs, so the 5 s line and the end of a run
//! each get the window they serve.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// A copy of this many bytes and more is timed: two clock reads beside a copy
/// of a few pages would be a fifth of it, and the bytes are in the big ones.
const TIME_AT: usize = 64 << 10;

/// A copy of this many bytes and more has its pages sampled, one copy in
/// [`PROBE_EVERY`].
const PROBE_AT: usize = 256 << 10;
const PROBE_EVERY: u64 = 16;

/// Pages asked about in one copy, at most: the sample is spread over the
/// range, so each page stands for `len / pages` bytes of it.
const PROBE_PAGES: usize = 64;

/// Counters since the process started.
enum C {
    /// Calls of the kernel copy, and of them the ones big enough to time.
    Calls,
    TimedCalls,
    /// Bytes and microseconds of the timed calls: their ratio is MB/s.
    TimedBytes,
    TimedUs,
    /// Calls whose whole range could not be read at once (the sparse read
    /// ran).
    Fallbacks,
    /// Pages of the sampled copies, and how many were in the working set
    /// before the read and after it.
    SrcPages,
    SrcThere,
    SrcAfter,
    DstPages,
    DstThere,
    DstAfter,
    /// Bytes of the sampled copies, and of those the part whose source bytes
    /// were not in memory before the read.
    SampledBytes,
    SampledAbsent,
    /// Pages the working-set question was asked about, and how many of them
    /// it found outside the working set: each of those is a byte read through
    /// the kernel, and a page fault with it.
    Asked,
    Absent,
}
const COUNTERS: usize = 15;
static COUNT: [AtomicU64; COUNTERS] = [const { AtomicU64::new(0) }; COUNTERS];

fn add(c: C, n: u64) {
    COUNT[c as usize].fetch_add(n, Relaxed);
}

fn load(c: C) -> u64 {
    COUNT[c as usize].load(Relaxed)
}

/// One question put to the kernel about `pages` pages
/// ([`crate::texture::committed`]), and one page of them found outside the
/// working set, which is a byte read through the kernel and a fault with it.
pub(crate) fn asked(pages: usize) {
    add(C::Asked, pages as u64);
}

pub(crate) fn absent() {
    add(C::Absent, 1);
}

/// Whether this big copy is one of the sampled ones.
fn sampled() -> bool {
    static ASKED: AtomicU64 = AtomicU64::new(0);
    ASKED.fetch_add(1, Relaxed).is_multiple_of(PROBE_EVERY)
}

/// One copy out of title memory: its clock, and the page sample taken before
/// the read. Its [`Drop`] records what it saw on every way out of the copy,
/// so no return path can leave the counters a copy behind.
pub(crate) struct Copying {
    len: usize,
    started: Option<Instant>,
    probe: Option<Probe>,
    fell_back: bool,
}

/// The pages sampled before a copy: where they are, and what they held.
struct Probe {
    src: usize,
    dst: usize,
    len: usize,
    src_pages: u32,
    src_there: u32,
    dst_pages: u32,
    dst_there: u32,
}

/// A copy of `len` bytes from `src` (title memory) to `dst` is about to be
/// asked of the kernel.
pub(crate) fn copying(src: usize, dst: usize, len: usize) -> Copying {
    add(C::Calls, 1);
    Copying {
        len,
        started: (len >= TIME_AT).then(Instant::now),
        probe: (len >= PROBE_AT && sampled()).then(|| {
            let (src_pages, src_there) = sample(src, len);
            let (dst_pages, dst_there) = sample(dst, len);
            Probe {
                src,
                dst,
                len,
                src_pages,
                src_there,
                dst_pages,
                dst_there,
            }
        }),
        fell_back: false,
    }
}

impl Copying {
    /// The whole range could not be read at once, so the sparse read ran.
    pub(crate) fn fell_back(&mut self) {
        self.fell_back = true;
    }
}

impl Drop for Copying {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            add(C::TimedCalls, 1);
            add(C::TimedBytes, self.len as u64);
            add(C::TimedUs, started.elapsed().as_micros() as u64);
        }
        if self.fell_back {
            add(C::Fallbacks, 1);
        }
        let Some(probe) = &self.probe else { return };
        let (_, src_after) = sample(probe.src, probe.len);
        let (_, dst_after) = sample(probe.dst, probe.len);
        add(C::SrcPages, u64::from(probe.src_pages));
        add(C::SrcThere, u64::from(probe.src_there));
        add(C::SrcAfter, u64::from(src_after));
        add(C::DstPages, u64::from(probe.dst_pages));
        add(C::DstThere, u64::from(probe.dst_there));
        add(C::DstAfter, u64::from(dst_after));
        add(C::SampledBytes, probe.len as u64);
        let absent = u64::from(probe.src_pages.saturating_sub(probe.src_there));
        let stand_for = probe.len as u64 * absent / u64::from(probe.src_pages.max(1));
        add(C::SampledAbsent, stand_for);
    }
}

/// Of at most [`PROBE_PAGES`] pages spread over `[at, at + len)`: how many
/// were asked about, and how many are in this process's working set.
///
/// One call to the kernel, which reports what it holds: nothing is read, so
/// the question does not fault in the page it asks about. A page of memory
/// that is not mapped at all answers "not there" as a trimmed one does, which
/// is what a copy that falls back is about either way.
fn sample(at: usize, len: usize) -> (u32, u32) {
    use windows_sys::Win32::System::ProcessStatus::{
        K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    const PAGE: usize = 4096;
    // SAFETY: plain data.
    let mut info: [PSAPI_WORKING_SET_EX_INFORMATION; PROBE_PAGES] = unsafe { std::mem::zeroed() };
    let pages = len.div_ceil(PAGE).max(1);
    let n = pages.min(PROBE_PAGES);
    // Rounded to a page, and no wider than the range: the last one asked
    // about is inside it.
    let stride = (len / n).max(PAGE) & !(PAGE - 1);
    for (i, entry) in info[..n].iter_mut().enumerate() {
        entry.VirtualAddress = ((at + i * stride) & !(PAGE - 1)) as *mut _;
    }
    // SAFETY: `n` entries of the size given, in our own process.
    let asked = unsafe {
        K32QueryWorkingSetEx(
            GetCurrentProcess(),
            info.as_mut_ptr().cast(),
            (n * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32,
        )
    } != 0;
    let mut there = 0;
    for entry in &info[..n] {
        // SAFETY: plain data the query filled in; bit 0 of the answer: the
        // page is in the working set.
        if asked && unsafe { entry.VirtualAttributes.Flags } & 1 != 0 {
            there += 1;
        }
    }
    (n as u32, there)
}

/// The process's memory, as the kernel reports it.
#[derive(Clone, Copy, Default)]
pub(crate) struct Footprint {
    /// Soft and hard page faults together: the kernel keeps one count, and
    /// the sample below says which of them had to come from the pagefile.
    pub(crate) faults: u64,
    pub(crate) working: u64,
    pub(crate) private: u64,
    /// Of `private`, the part backed by the pagefile.
    pub(crate) pagefile: u64,
}

fn footprint() -> Footprint {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process, and a structure of the size given in `cb`,
    // which is the one it is filled as.
    unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            std::ptr::addr_of_mut!(counters).cast::<PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        );
    }
    Footprint {
        faults: u64::from(counters.PageFaultCount),
        working: counters.WorkingSetSize as u64,
        private: counters.PrivateUsage as u64,
        pagefile: counters.PagefileUsage as u64,
    }
}

/// The PC's memory in bytes: physical memory free, and what is left to
/// commit (the pagefile is the limit the copies must live inside).
fn system() -> (u64, u64) {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    unsafe { GlobalMemoryStatusEx(&mut m) };
    (m.ullAvailPhys, m.ullAvailPageFile)
}

/// What the counters held when the last line was printed.
#[derive(Clone, Copy, Default)]
struct Seen {
    calls: u64,
    timed: u64,
    bytes: u64,
    us: u64,
    fallbacks: u64,
    src_pages: u64,
    src_there: u64,
    src_after: u64,
    dst_pages: u64,
    dst_there: u64,
    dst_after: u64,
    sampled_bytes: u64,
    sampled_absent: u64,
    asked: u64,
    absent: u64,
    faults: u64,
}

impl Seen {
    fn now(faults: u64) -> Self {
        Self {
            calls: load(C::Calls),
            timed: load(C::TimedCalls),
            bytes: load(C::TimedBytes),
            us: load(C::TimedUs),
            fallbacks: load(C::Fallbacks),
            src_pages: load(C::SrcPages),
            src_there: load(C::SrcThere),
            src_after: load(C::SrcAfter),
            dst_pages: load(C::DstPages),
            dst_there: load(C::DstThere),
            dst_after: load(C::DstAfter),
            sampled_bytes: load(C::SampledBytes),
            sampled_absent: load(C::SampledAbsent),
            asked: load(C::Asked),
            absent: load(C::Absent),
            faults,
        }
    }

    /// What happened since `before`.
    fn since(self, before: &Self) -> Self {
        let d = |now: u64, was: u64| now.saturating_sub(was);
        Self {
            calls: d(self.calls, before.calls),
            timed: d(self.timed, before.timed),
            bytes: d(self.bytes, before.bytes),
            us: d(self.us, before.us),
            fallbacks: d(self.fallbacks, before.fallbacks),
            src_pages: d(self.src_pages, before.src_pages),
            src_there: d(self.src_there, before.src_there),
            src_after: d(self.src_after, before.src_after),
            dst_pages: d(self.dst_pages, before.dst_pages),
            dst_there: d(self.dst_there, before.dst_there),
            dst_after: d(self.dst_after, before.dst_after),
            sampled_bytes: d(self.sampled_bytes, before.sampled_bytes),
            sampled_absent: d(self.sampled_absent, before.sampled_absent),
            asked: d(self.asked, before.asked),
            absent: d(self.absent, before.absent),
            faults: d(self.faults, before.faults),
        }
    }
}

static SEEN: Mutex<Option<Seen>> = Mutex::new(None);

/// Megabytes a second from bytes copied and microseconds spent: bytes over
/// microseconds is exactly MB/s, since 10^6 bytes in 10^6 µs is one second.
fn rate(bytes: u64, us: u64) -> f64 {
    if us == 0 { 0.0 } else { bytes as f64 / us as f64 }
}

/// The share of `pages` that were there.
fn percent(there: u64, pages: u64) -> f64 {
    if pages == 0 { 0.0 } else { 100.0 * there as f64 / pages as f64 }
}

/// One line per window: what the copies out of title memory and back into it
/// ran at, beside the process's page faults and memory and what the sampled
/// pages held. `back` is the window's `COPY_BACK` (microseconds, bytes).
pub(crate) fn line(window: Duration, back: [u64; 2]) {
    const MB: f64 = 1e6;
    let secs = window.as_secs_f64().max(1e-9);
    let memory = footprint();
    let (free, left) = system();
    let now = Seen::now(memory.faults);
    let seen = SEEN.lock().unwrap_or_else(|e| e.into_inner()).replace(now).unwrap_or_default();
    let w = now.since(&seen);
    let mb = |bytes: u64| bytes as f64 / MB;
    let copied = mb(w.bytes).max(1e-9);
    let faults = w.faults;
    let (calls, timed) = (w.calls, w.timed);
    tracing::info!(
        "copies out of title memory at {:.1} MB/s: {:.1} MB in {:.0} ms ({calls} calls, {timed} of \
         them of {} KB and more), {:.0} of them fell back to the sparse read; copying back {:.1} MB \
         in {:.0} ms at {:.1} MB/s; page faults {faults} ({:.0} a second, {:.1} a MB copied; the \
         kernel counts soft and hard together), working set {:.0} MB, private {:.0} MB of it {:.0} \
         MB in the pagefile; the PC has {:.0} MB free and {:.0} MB left to commit; pages of the \
         copies sampled, in memory before the read: source {:.0}%, destination {:.0}%, after it \
         source {:.0}%, destination {:.0}% (of {:.1} MB copied in copies of {} KB and more, {:.0}% \
         of the source bytes were not in memory: {:.1} MB, {:.1} MB/s at best from the pagefile); \
         the working-set question asked about {:.0} pages, {:.0} of them outside the working set ({} \
         one-byte reads through the kernel, a fault apiece)",
        rate(w.bytes, w.us),
        mb(w.bytes),
        w.us as f64 / 1e3,
        TIME_AT >> 10,
        w.fallbacks as f64,
        mb(back[1]),
        back[0] as f64 / 1e3,
        rate(back[1], back[0]),
        faults as f64 / secs,
        faults as f64 / copied,
        mb(memory.working),
        mb(memory.private),
        mb(memory.pagefile),
        mb(free),
        mb(left),
        percent(w.src_there, w.src_pages),
        percent(w.dst_there, w.dst_pages),
        percent(w.src_after, w.src_pages),
        percent(w.dst_after, w.dst_pages),
        mb(w.sampled_bytes),
        PROBE_AT >> 10,
        percent(w.sampled_absent, w.sampled_bytes),
        mb(w.sampled_absent),
        mb(w.sampled_absent) / secs,
        w.asked as f64,
        w.absent as f64,
        w.absent,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn megabytes_a_second_are_bytes_over_microseconds() {
        // 96.2 MB in 1,409 ms is 68 MB/s.
        assert!((rate(96_200_000, 1_409_000) - 68.27).abs() < 0.01);
        assert!((rate(96_200_000, 1_409) - 68_275.0).abs() < 1.0);
        // Nothing timed is no rate, not a division by zero.
        assert_eq!(rate(1 << 20, 0), 0.0);
    }

    #[test]
    fn a_written_buffer_is_in_memory() {
        // One byte a page: every page of the buffer is written, and so in the
        // working set, which is the whole of what the sample asks about.
        let mut buffer = vec![0u8; 1 << 20];
        for page in buffer.chunks_mut(4096) {
            page[0] = 1;
        }
        let (pages, there) = sample(buffer.as_ptr() as usize, buffer.len());
        assert_eq!(pages, PROBE_PAGES as u32);
        assert_eq!(there, pages, "every page of a buffer just written");
    }

    #[test]
    fn a_copy_is_counted_whatever_len_it_took() {
        let before = (load(C::Calls), load(C::TimedCalls), load(C::SampledBytes));
        let buffer = vec![0u8; PROBE_AT];
        {
            // Under the sample's size: counted and timed, never probed.
            let mut copy = copying(0, buffer.as_ptr() as usize, TIME_AT);
            copy.fell_back();
        }
        assert!(load(C::Calls) > before.0);
        assert!(load(C::TimedCalls) > before.1);
        assert_eq!(load(C::SampledBytes), before.2, "a copy under the probe's size is not sampled");
    }

    #[test]
    fn a_page_sample_stands_for_the_bytes_it_could_not_find() {
        // Every page sampled stands for `len / pages` bytes: an absence is
        // the whole of what its page stands for, and half the pages gone is
        // half the bytes.
        let now = Seen { sampled_bytes: 1 << 20, sampled_absent: 1 << 19, ..Seen::default() };
        let seen = now.since(&Seen::default());
        assert_eq!(seen.sampled_bytes, 1 << 20);
        assert_eq!(seen.sampled_absent, 1 << 19);
        assert!((percent(seen.sampled_absent, seen.sampled_bytes) - 50.0).abs() < 0.001);
        // Nothing sampled is no percentage, not a division by zero.
        assert_eq!(percent(0, 0), 0.0);
    }

    #[test]
    fn the_working_set_question_is_counted_page_by_page() {
        let before = load(C::Asked);
        let buffer = vec![0u8; 8 << 10];
        assert!(crate::texture::committed(buffer.as_ptr(), buffer.len()));
        // The buffer is not page-aligned: it can span one page more than 8 KiB
        // divided by 4 KiB.
        let (at, len) = (buffer.as_ptr() as usize, buffer.len());
        let pages = 1 + (at + len - 1) / 4096 - at / 4096;
        assert_eq!(load(C::Asked) - before, pages as u64, "a question about every page of it");
    }

    /// What a copy out of pages that are not in memory costs, and what one in
    /// small pieces costs, measured here: run by hand, because it empties the
    /// process's working set while it runs.
    ///
    /// `cargo test -p victory-gpu --lib --locked what_a_copy_costs -- --ignored --nocapture`
    ///
    /// Hypothesis 1 (the source pages are not resident, so the copy pays the
    /// pagefile) shows as the second read dropping to tens of MB/s with one
    /// fault a page; hypothesis 3 (a page crossing the process boundary costs
    /// what it costs) shows as the page-by-page copy falling far below the
    /// whole-range one even with every page in memory.
    #[test]
    #[ignore = "empties the working set: run by hand"]
    fn what_a_copy_costs() {
        use windows_sys::Win32::System::ProcessStatus::K32EmptyWorkingSet;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        const LEN: usize = 64 << 20;
        const PAGE: usize = 4096;
        let src = vec![7u8; LEN];
        let mut whole = vec![0u8; LEN];
        let started = Instant::now();
        assert!(crate::host::read_into(src.as_ptr(), &mut whole));
        let hot = started.elapsed();
        // The same bytes, one page a call.
        let mut page = [0u8; PAGE];
        let started = Instant::now();
        let mut at = 0;
        while at < LEN {
            // SAFETY: `at < LEN`, so the page is inside the buffer.
            assert!(crate::host::read_into(unsafe { src.as_ptr().add(at) }, &mut page));
            at += PAGE;
        }
        let pages = started.elapsed();
        let faults = footprint().faults;
        // SAFETY: our own process; this reads and writes nothing, it only
        // takes our pages out of the working set.
        unsafe { K32EmptyWorkingSet(GetCurrentProcess()) };
        // Into a buffer of its own that fits in memory, so the faults counted
        // are the source's: the destination's first write is one page of it.
        let mut piece = vec![0u8; 4 << 20];
        let started = Instant::now();
        let mut at = 0;
        while at < LEN {
            // SAFETY: `at < LEN` and the piece is `len` bytes long.
            assert!(crate::host::read_into(unsafe { src.as_ptr().add(at) }, &mut piece));
            at += piece.len();
        }
        let trimmed = started.elapsed();
        let paid = footprint().faults - faults;
        let (per_call, pages_ms) = (pages.as_nanos() as f64 / 1e3 / (LEN / PAGE) as f64, pages.as_secs_f64() * 1e3);
        println!(
            "64 MB in one call: {:.1} GB/s hot; the same one page a call: {:.0} MB/s ({per_call:.1} us a \
             call, {pages_ms:.0} ms); after the pages were emptied out of the working set: {:.0} MB/s \
             ({:.0} ms) with {paid} faults, {:.2} a page",
            LEN as f64 / 1e9 / hot.as_secs_f64(),
            LEN as f64 / 1e6 / (pages.as_secs_f64()),
            LEN as f64 / 1e6 / (trimmed.as_secs_f64()),
            trimmed.as_secs_f64() * 1e3,
            paid as f64 / (LEN / PAGE) as f64,
        );
        // Whatever it cost, the counters must have seen the faults.
        assert!(paid >= (LEN / PAGE) as u64 / 2, "the fault count rose by the pages it fetched");
    }
}
