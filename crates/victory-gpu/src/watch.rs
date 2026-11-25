//! Which title memory the title wrote since a host copy took it (decision
//! 0007, "Copies of title memory"). The runtime reserves title GPU memory
//! with write watching (`victory-hle` xmem), so the kernel keeps a dirty bit
//! per 4 KiB page; asking for a range's written pages also clears them. Host
//! copies of the same memory can overlap (nested buffers, textures in
//! buffers), so what one asks is kept here for the others: a table of each
//! 4 KiB page's last write, numbered by the asking that found it.
//!
//! The kernel's price is per call, not per page (30-70 microseconds for one
//! that resets written pages), and it answers per reservation: the whole
//! committed, write-watched run of a reservation is asked about once per
//! sync point ([`ask_ahead`], [`ask_once`]), and every range inside it is
//! then answered from the table, with no kernel call. A page written in a
//! sync point nothing refreshed there keeps its asking: the table is
//! monotonic, and a page's entry moves only when a later asking finds that
//! page written again.
//!
//! Physical-page views (xmem's sections) cannot be write-watched. There a
//! copy leaves each 64 KiB block it took read-only, and our vectored handler
//! takes the first write to it: it notes the block as written and gives the
//! write access back. The runtime tells us when it maps or unmaps views
//! ([`title_memory_remapped`]), which resets their protection. Blocks found
//! wholly uncommitted are skipped while they remain so; one query can
//! confirm a whole region, including ordinary reservations between views.
//!
//! shortcut: writes into physical pages through another view (xmem's
//! `write_physical`, DirectStorage's page destinations) are not seen at
//! the guarded addresses; they go to the texture pool, whose textures are
//! hashed, not guarded. A system call writing into a guarded block fails
//! (none is known to).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// The unit tracked and copied: 64 KiB, the console's page (the kernel's
/// 4 KiB pages fold into it). Finer units made the checks of the world's
/// big buffers cost more than the copies they saved.
pub(crate) const PAGE: usize = 64 << 10;
/// The kernel's page, the unit of its answers.
const KERNEL_PAGE: usize = 4096;
/// The kernel's pages in one of ours.
const KERNEL_PAGES: usize = PAGE / KERNEL_PAGE;

// The kernel askings this thread made: what the tests that count them
// read. A thread of its own, so that tests running side by side do not
// count each other's.
#[cfg(test)]
thread_local! {
    static ASKINGS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Askings made and kernel pages walked, since the last take: a call costs
/// with the pages it covers, so a frame line that names one without the
/// other cannot tell a big asking from many small ones.
static MADE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static WALKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The askings made and the kernel pages they walked since the last call.
pub(crate) fn take_askings() -> (u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (MADE.swap(0, Relaxed), WALKED.swap(0, Relaxed))
}

struct Journal {
    /// Numbers the askings; 0 is "never".
    asked: u64,
    /// The kernel's page number → the asking that last found it written.
    /// Monotonic: an entry goes only where title memory is remapped.
    written: BTreeMap<usize, u64>,
    /// Room for the kernel's answer.
    found: Vec<usize>,
    /// Page ranges (ours) the standing asking covered ([`ask_ahead`],
    /// [`ask_once`]) and the asking that did: `writes` answers for a range
    /// inside one from the table, without another kernel call.
    ahead: Vec<(usize, usize)>,
    ahead_asked: u64,
    /// The ahead asking stands for a whole submission ([`ask_once`]): no
    /// other ahead asking is made, and `ahead` stays, until
    /// [`ask_once_end`].
    kept: bool,
}

static JOURNAL: Mutex<Journal> = Mutex::new(Journal {
    asked: 0,
    written: BTreeMap::new(),
    found: Vec::new(),
    ahead: Vec::new(),
    ahead_asked: 0,
    kept: false,
});

/// The most one asking covers (its answer needs 8 bytes per 4 KiB page): a
/// run larger than this is asked about in pieces of it.
const AHEAD_MAX: usize = (1 << 30) / PAGE;

/// Asks the kernel, once, about the whole run of write-watched memory each
/// of `ranges` `(start, end)` lies in, before the [`writes`] of each: the
/// ranges inside one run are covered by its one asking, and `writes` then
/// answers from the table until [`ask_ahead_done`], with no kernel call.
///
/// The kernel answers per reservation, once, whatever the range: a call
/// that resets written pages costs 30-70 microseconds. The world's
/// submissions refresh thousands of host copies (6,664 askings a frame,
/// 456 ms, in run 14; 2,644, 219 ms, in run 75), and asking per range,
/// however near the ranges were, was most of that time. Ranges that are not
/// write-watched (physical-page views), or that lie in no run of one
/// reservation, are left to `writes`.
pub(crate) fn ask_ahead(ranges: &[(usize, usize)]) {
    if ranges.iter().filter(|r| r.0 < r.1).count() < 2 {
        return;
    }
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    // The submission's one asking stands ([`ask_once`]): what it does not
    // hold is left to `writes`.
    if j.kept {
        return;
    }
    j.asked += 1;
    j.ahead_asked = j.asked;
    j.ahead.clear();
    for &(start, end) in ranges {
        if start < end {
            ask_run(j, start, end);
        }
    }
}

/// Since the last frame line: the kernel askings that took a millisecond
/// or more, their microseconds, and the longest asking's. An asking walks
/// page tables, some ten microseconds for most ranges; in run 75 the 2,644
/// askings of a frame took 219 ms in the lines where the pass over the
/// heap's views ran long, 25-50 ms in the others. These numbers say
/// whether a few askings wait for something or all of them are slow.
#[cfg(feature = "diagnostics")]
static SLOW: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

#[cfg(feature = "diagnostics")]
fn slow(us: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    if us >= 1000 {
        SLOW[0].fetch_add(1, Relaxed);
        SLOW[1].fetch_add(us, Relaxed);
    }
    SLOW[2].fetch_max(us, Relaxed);
}

/// [`SLOW`], taken.
#[cfg(feature = "diagnostics")]
pub(crate) fn slow_asks() -> [u64; 3] {
    [0, 1, 2].map(|i| SLOW[i].swap(0, std::sync::atomic::Ordering::Relaxed))
}

/// The submission's refreshes are done: `writes` asks the kernel again.
pub(crate) fn ask_ahead_done() {
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    if !j.kept {
        j.ahead.clear();
    }
}

/// Whether a submission asks the kernel once, before its first refresh,
/// about every range its lists refresh (`VICTORY_ASK_ONCE=1`; off by
/// default).
///
/// A submission's refreshes are done in groups, between the title's own
/// buffer copies (each copy flushes the refreshes recorded before it), and
/// every group asked the kernel again: 2,644 askings a frame, 70 a
/// submission. With the switch the journal answers all of them from one
/// asking made when the submission starts.
///
/// What changes: memory the title's other threads write while the
/// submission is being prepared is seen by the next submission's asking,
/// not by this one's later groups. What this submission's lists read was
/// written before they were submitted, so before the asking. Our own
/// writes into title memory while preparing are told to the journal
/// ([`wrote`]) or end the asking ([`ask_once_end`]).
pub(crate) fn once() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // On by default since run 81 (with card buffers: 375-450 ms a frame
    // against 573-647, the picture unchanged over four minutes);
    // `VICTORY_ASK_ONCE=0` asks per range, as before.
    *ON.get_or_init(|| std::env::var_os("VICTORY_ASK_ONCE").is_none_or(|v| v != "0"))
}

/// With [`once`]: asks about `ranges` (everything a submission's lists
/// refresh) as [`ask_ahead`] does, and lets that asking stand until
/// [`ask_once_end`].
pub(crate) fn ask_once(ranges: &[(usize, usize)]) {
    if !once() {
        return;
    }
    ask_once_end();
    ask_ahead(ranges);
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    if j.ahead.is_empty() {
        // `ask_ahead` asked nothing (fewer than two ranges).
        j.asked += 1;
        j.ahead_asked = j.asked;
    }
    // `ask_ahead` leaves a range that lies in no run of one reservation
    // (fewer than two ranges, or no write-watch) to `writes`, which asks
    // once per flush: with one asking for the submission that was every
    // such range asked again at each of its 70 flushes (run 76: 2,300
    // askings a frame with the switch on, as without it). They are asked
    // here, once; what is not write-watched stays with `writes`.
    for &(start, end) in ranges {
        if start >= end {
            continue;
        }
        ask_run(j, start, end);
    }
    j.kept = !j.ahead.is_empty();
}

/// The submission's one asking stands no longer: its refreshes are done,
/// or title memory was written in a way the journal was not told.
pub(crate) fn ask_once_end() {
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    if j.kept {
        j.kept = false;
        j.ahead.clear();
    }
}

/// We wrote title memory `[at, at + len)` while a submission's one asking
/// stands (the title's buffer copies, done at submission): its pages count
/// as written after that asking, so the refreshes that follow copy them.
pub(crate) fn wrote(at: usize, len: usize) {
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    if !j.kept || len == 0 {
        return;
    }
    j.asked += 1;
    let now = j.asked;
    for page in at / KERNEL_PAGE..at.saturating_add(len).div_ceil(KERNEL_PAGE) {
        j.written.insert(page, now);
    }
}

/// One kernel asking for the run `(start, end)` (bytes) of `ranges`, before
/// the range is refreshed: the whole run, once, and the coverage noted for
/// the standing asking. Nothing if the range is not write-watched memory of
/// one reservation, or that run is covered already.
fn ask_run(j: &mut Journal, start: usize, end: usize) {
    let Some((run_start, run_end)) = run_of(start, end) else {
        return;
    };
    if covered(j, run_start / PAGE, run_end.div_ceil(PAGE)).is_some() {
        return;
    }
    if ask_writes(j, run_start, run_end, j.ahead_asked) {
        j.ahead.push((run_start / PAGE, run_end.div_ceil(PAGE)));
    }
}

/// Asks the kernel about bytes `[run_start, run_end)` — one run of one
/// reservation ([`run_of`]) — and notes the pages it finds written under the
/// asking `now`; false if the kernel will not answer about the memory
/// (memory that is not write-watched: view memory). The asking covers the
/// whole run: the kernel's price is per call, not per page.
fn ask_writes(j: &mut Journal, run_start: usize, run_end: usize, now: u64) -> bool {
    let mut at = run_start;
    while at < run_end {
        let to = at.saturating_add(AHEAD_MAX * PAGE).min(run_end);
        if !ask_kernel(j, at, to, now) {
            return false;
        }
        at = to;
    }
    true
}

/// Asks the kernel for the written pages of bytes `[start, end)` and notes
/// them in the table under the asking `now`; false (and nothing changed) if
/// the kernel will not answer about the range.
fn ask_kernel(j: &mut Journal, start: usize, end: usize, now: u64) -> bool {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    #[cfg(test)]
    ASKINGS.with(|n| n.set(n.get() + 1));
    let mut count = (end - start) / KERNEL_PAGE;
    {
        use std::sync::atomic::Ordering::Relaxed;
        MADE.fetch_add(1, Relaxed);
        WALKED.fetch_add(count as u64, Relaxed);
    }
    j.found.resize(count.max(j.found.len()), 0);
    let mut granularity = 0;
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    // SAFETY: the answer buffer holds `count` entries.
    let failed = unsafe {
        GetWriteWatch(
            WRITE_WATCH_FLAG_RESET,
            start as *const _,
            end - start,
            j.found.as_mut_ptr().cast(),
            &mut count,
            &mut granularity,
        )
    } != 0;
    if failed {
        return false;
    }
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
        slow(started.elapsed().as_micros() as u64);
    }
    for k in 0..count {
        let page = j.found[k] / KERNEL_PAGE;
        j.written.insert(page, now);
    }
    true
}

/// The run of one reservation's memory that bytes `(start, end)` lie in, as
/// byte bounds: what `GetWriteWatch` answers about in one call. `None` for
/// memory that is not committed, nothing at all, or a range that reaches out
/// of its run.
///
/// Asked of the kernel each time a range needs it, and nothing of it kept: a
/// run's bounds are worth a `VirtualQuery` (some 2 microseconds) beside the
/// asking they save, and anything kept would have to learn of every free and
/// every commit that put other memory there, which only the titles know.
fn run_of(start: usize, end: usize) -> Option<(usize, usize)> {
    let (run_start, run_end) = run_at(start);
    (start >= run_start && end <= run_end && run_start < run_end).then_some((run_start, run_end))
}

/// The run of one reservation's memory holding `at`: where it starts and
/// ends. A run is the committed memory of one allocation, and stops at a
/// hole (a reservation may be committed in parts): the range one
/// `GetWriteWatch` call can cover.
fn run_at(at: usize) -> (usize, usize) {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    fn query(at: usize) -> Option<MEMORY_BASIC_INFORMATION> {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        (n != 0).then_some(info)
    }
    let Some(info) = query(at) else {
        return (at, at);
    };
    let base = info.AllocationBase as usize;
    let same =
        |i: &MEMORY_BASIC_INFORMATION| i.AllocationBase as usize == base && i.State == MEM_COMMIT;
    // A run of a reservation committed in many pieces would cost a query a
    // piece: the walk stops after `RUN_REGIONS` either way. A shorter run is
    // only a smaller asking: a range outside it is asked about separately.
    const RUN_REGIONS: usize = 256;
    let mut start = info.BaseAddress as usize;
    let mut end = start.saturating_add(info.RegionSize);
    for _ in 0..RUN_REGIONS {
        if start <= base {
            break;
        }
        let Some(before) = query(start - 1).filter(same) else {
            break;
        };
        start = before.BaseAddress as usize;
    }
    for _ in 0..RUN_REGIONS {
        let Some(after) = query(end).filter(same) else {
            break;
        };
        end = (after.BaseAddress as usize).saturating_add(after.RegionSize);
    }
    (start, end)
}

/// The asking that stands for pages `[first, last)` (ours), if the standing
/// asking covered them: the table answers `writes` and `asked` from it,
/// with no kernel call.
fn covered(j: &Journal, first: usize, last: usize) -> Option<u64> {
    j.ahead
        .iter()
        .any(|&(a, b)| a <= first && last <= b)
        .then_some(j.ahead_asked)
}

/// The pages of `[first, last)` (ours) the kernel has found written:
/// `(page, asking)` in page order, from the table of its 4 KiB pages. A
/// page is as written as its last written 4 KiB page, and carries that
/// asking.
fn pages(j: &Journal, first: usize, last: usize) -> Vec<(usize, u64)> {
    let mut pages: Vec<(usize, u64)> = Vec::new();
    for (&page, &asking) in j.written.range(first * KERNEL_PAGES..last * KERNEL_PAGES) {
        match pages.last_mut() {
            Some((last, at)) if *last == page / KERNEL_PAGES => *at = (*at).max(asking),
            _ => pages.push((page / KERNEL_PAGES, asking)),
        }
    }
    pages
}

/// What [`writes`] would say of `[start, end)` from the table alone, if the
/// range lies in one run the standing asking covered ([`ask_ahead`],
/// [`ask_once`]): the pages written since tracking began and the asking
/// that last found each written, with the number of the asking that stands
/// (as [`writes`] gives it). `None` if the kernel would have to be asked.
pub(crate) fn asked(start: usize, end: usize) -> Option<(u64, Vec<(usize, u64)>)> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let now = covered(&j, first, last)?;
    Some((now, pages(&j, first, last)))
}

/// The pages of `[start, end)` (whole pages) as last written, for pages
/// written since tracking began: `(page number, asking)` in page order, and
/// this asking's number, which is higher than any in the list but those the
/// kernel just reported. `None` if the memory is not write-watched (nor,
/// with `guarded`, view memory, which is then guarded).
pub(crate) fn writes(start: usize, end: usize, guarded: bool) -> Option<(u64, Vec<(usize, u64)>)> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    // Asked with the submission's other ranges: the table has it.
    if let Some(now) = covered(j, first, last) {
        return Some((now, pages(j, first, last)));
    }
    // The kernel answers about one reservation at a time: ask it, once, for
    // the whole run this range lies in, and answer from the table.
    if let Some((run_start, run_end)) = run_of(start, end) {
        j.asked += 1;
        let now = j.asked;
        if ask_writes(j, run_start, run_end, now) {
            return Some((now, pages(j, first, last)));
        }
    }
    if !guarded {
        return None;
    }
    #[cfg(feature = "diagnostics")]
    let guarding = std::time::Instant::now();
    let written = guard(first * PAGE, last * PAGE)?;
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::GuardUs,
        guarding.elapsed().as_micros() as u64,
    );
    j.asked += 1;
    let now = j.asked;
    for page in written {
        j.written.insert(page * KERNEL_PAGES, now);
    }
    Some((now, pages(j, first, last)))
}

/// Guarded blocks of view memory.
const BLOCK: usize = PAGE;

struct Guards {
    /// Blocks we made read-only.
    protected: BTreeSet<usize>,
    /// Of those, the ones not written since a copy took them.
    clean: BTreeSet<usize>,
    /// Blocks already copied as zeros. While they remain wholly uncommitted,
    /// the copy can skip hundreds of MB of failing kernel reads per world frame.
    absent: BTreeSet<usize>,
}

static GUARDS: Mutex<Guards> = Mutex::new(Guards {
    protected: BTreeSet::new(),
    clean: BTreeSet::new(),
    absent: BTreeSet::new(),
});

fn guards() -> std::sync::MutexGuard<'static, Guards> {
    GUARDS.lock().unwrap_or_else(|e| e.into_inner())
}

/// For view memory: the pages of `[start, end)`'s blocks written since
/// they were last guarded (all of a block's pages), guarding them again
/// before the caller copies them. `None` if the memory is not a view.
fn guard(start: usize, end: usize) -> Option<Vec<usize>> {
    // A range whose first block the guards hold already (read-only by our
    // hand, or noted absent) is a view: no question to Windows. The
    // question walks the page tables to the end of the pages that share the
    // first one's state, and a guarded view is one long run of read-only
    // pages: 0.6 ms for the title's 174 MB heap, 16 times a frame, and most
    // of the 68 ms a frame of "guards" (run 58).
    // `VICTORY_GUARD_ASK=1` asks every time, as before.
    static ASK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let known =
        !*ASK.get_or_init(|| std::env::var_os("VICTORY_GUARD_ASK").is_some_and(|v| v == "1")) && {
            let g = guards();
            let first = start / BLOCK;
            g.protected.contains(&first) || g.absent.contains(&first)
        };
    if !known && !is_view(start, end) {
        return None;
    }
    install();
    let mut g = guards();
    let mut written = Vec::new();
    let mut run: Option<usize> = None;
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    // The range's clean blocks, in order: read off the set in one pass,
    // not looked up one block at a time. A frame asks about some 10 GB of
    // view memory, 150,000 blocks, nearly all clean: the lookups were most
    // of the 60-90 ms a frame of "guards" (run 54). What the loop adds to
    // the set meanwhile is the block it stands at or ones before it.
    let clean: Vec<usize> = g.clean.range(first..last).copied().collect();
    if clean.len() == last - first {
        // Nothing written, mapped or new since the range was last taken.
        return Some(written);
    }
    let mut next_clean = 0;
    let mut uncommitted_until = first;
    for block in first..=last {
        while next_clean < clean.len() && clean[next_clean] < block {
            next_clean += 1;
        }
        let is_clean = next_clean < clean.len() && clean[next_clean] == block;
        let mut open = block < last && !is_clean;
        if open && g.absent.contains(&block) {
            // Ordinary commits need no runtime mapping notification. Reuse
            // this asking's query for the following blocks in the same region.
            if block >= uncommitted_until {
                uncommitted_until = uncommitted_end(block);
            }
            open = block >= uncommitted_until;
            if open {
                g.absent.remove(&block);
            }
        }
        if open {
            written.extend(block * BLOCK / PAGE..(block + 1) * BLOCK / PAGE);
            if g.protected.contains(&block) {
                // Written through the runtime, still read-only.
                g.clean.insert(block);
            } else {
                run.get_or_insert(block);
                continue;
            }
        }
        if let Some(from) = run.take() {
            protect(&mut g, from, block);
        }
    }
    Some(written)
}

/// Whether `[start, end)` is view memory. The title's views lie in
/// placeholders, mapped and unmapped block by block as it streams, so a
/// range can begin in a block that is not mapped right now: the first
/// pages of the range that are there decide. A range with nothing there
/// counts as a view (its blocks are noted absent, and looked at again when
/// the runtime maps them).
///
/// Looking at the first address only took such a range for "not a view":
/// its copy was marked not watched for good and copied whole at every
/// refresh, around its unmapped pages (run 54: 300 such ranges, 570 MB
/// copied a frame for them, 255 copies a frame reading around holes).
/// `VICTORY_GUARD_HOLES=0` looks at the first address only, as before.
fn is_view(start: usize, end: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    static HOLES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let holes =
        *HOLES.get_or_init(|| std::env::var_os("VICTORY_GUARD_HOLES").is_none_or(|v| v != "0"));
    let mut at = start;
    loop {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        if n == 0 {
            return false;
        }
        if info.Type == MEM_MAPPED {
            return true;
        }
        // Committed memory that is no view (the title's heaps, ours), or
        // the answer as it was before.
        if info.State == MEM_COMMIT || !holes {
            return false;
        }
        // A hole (a placeholder, or free): on to what follows it.
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        if next <= at || next >= end {
            return true;
        }
        at = next;
    }
}

/// Makes blocks `[from, to)` read-only and notes them guarded and clean;
/// block by block if they span views, leaving out any that fail, and
/// noting those not committed (unmapped) absent.
fn protect(g: &mut Guards, from: usize, to: usize) {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
    let set = |from: usize, to: usize| {
        let mut old = 0;
        // SAFETY: title view memory; only its protection changes.
        unsafe {
            VirtualProtect(
                (from * BLOCK) as *const _,
                (to - from) * BLOCK,
                PAGE_READONLY,
                &mut old,
            ) != 0
        }
    };
    let note = |g: &mut Guards, from: usize, to: usize| {
        g.protected.extend(from..to);
        g.clean.extend(from..to);
    };
    if set(from, to) {
        note(g, from, to);
        return;
    }
    for block in from..to {
        if set(block, block + 1) {
            note(g, block, block + 1);
        } else if block < uncommitted_end(block) {
            g.absent.insert(block);
        }
    }
}

/// The end (exclusive) of the whole uncommitted blocks starting at `block`.
fn uncommitted_end(block: usize) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_FREE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery((block * BLOCK) as *const _, &mut info, size_of_val(&info)) };
    if n != 0 && matches!(info.State, MEM_RESERVE | MEM_FREE) {
        (info.BaseAddress as usize).saturating_add(info.RegionSize) / BLOCK
    } else {
        block
    }
}

/// Gives write access back to the guarded blocks of `[start, end)` and
/// notes them written; true if any was guarded.
fn open(g: &mut Guards, start: usize, end: usize) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    let blocks: Vec<usize> = g
        .protected
        .range(start / BLOCK..end.div_ceil(BLOCK))
        .copied()
        .collect();
    for &block in &blocks {
        g.protected.remove(&block);
        g.clean.remove(&block);
        let mut old = 0;
        // SAFETY: a block we made read-only.
        unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READWRITE, &mut old) };
    }
    !blocks.is_empty()
}

/// Before our own write into title memory `[at, at + len)` through the
/// kernel (which does not fault into our handler): opens its guarded blocks.
/// The runtime calls it (as `kernel_will_write`) before the title's kernel
/// reads into its own memory.
pub fn writing(at: usize, len: usize) {
    // The length is the title's (`fread`'s size times count, saturated).
    open(&mut guards(), at, at.saturating_add(len));
    // A placed texture there changes with no descriptor written.
    crate::descriptor::host::remapped(at, len);
}

/// Held shared by a read of title memory in place, and alone by the runtime
/// while it changes what title memory shows.
///
/// The runtime does not change a mapping in one step. To unmap one page of
/// a view it unmaps the whole view and maps what stays again
/// (`xmem.rs` `unmap`), and a view it maps is a reservation until the next
/// call commits it (`map`, then `commit`). For that moment pages the title
/// never gave up are "reserved, never committed", and a thread reading
/// them in place takes an access violation: run 71 in `Layout::detile`,
/// run 74 in the hash before it, both under `Staging::refresh_now`, which
/// had asked whether the texture was all mapped and been told yes. Asking
/// cannot help; not changing the mapping while such a read is under way
/// does.
static MAPPING: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// What the runtime holds while it changes what title memory shows
/// ([`title_memory_remapping`]).
pub struct Remapping(#[allow(dead_code)] std::sync::RwLockWriteGuard<'static, ()>);

/// The runtime is about to map, unmap or move physical pages of title
/// memory: until what it returns is dropped, no read of title memory in
/// place is under way and none begins ([`title_memory_reading`]). Taken
/// around the whole change, the unmapping and the mapping again.
pub fn title_memory_remapping() -> Remapping {
    Remapping(MAPPING.write().unwrap_or_else(|e| e.into_inner()))
}

/// What a read of title memory in place holds ([`title_memory_reading`]).
pub struct Reading(#[allow(dead_code)] std::sync::RwLockReadGuard<'static, ()>);

/// For reading title memory in place: until what it returns is dropped,
/// the runtime changes no mapping, so memory found mapped stays mapped.
/// Not to be taken twice by one thread, nor held across anything that
/// maps title memory.
pub fn title_memory_reading() -> Reading {
    Reading(MAPPING.read().unwrap_or_else(|e| e.into_inner()))
}

/// [`title_memory_reading`] if it can be had at once: `None` while the
/// runtime changes a mapping (or waits to), for a caller that has another
/// way to read and would rather not wait.
pub fn title_memory_reading_now() -> Option<Reading> {
    match MAPPING.try_read() {
        Ok(guard) => Some(Reading(guard)),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(Reading(e.into_inner())),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// The runtime mapped or unmapped title memory `[at, at + len)`: what it
/// shows is new and read-write, so it is no longer guarded (nor known
/// unmapped).
pub fn title_memory_remapped(at: usize, len: usize) {
    // A placed texture there arrives, goes or changes with no descriptor
    // written.
    crate::descriptor::host::remapped(at, len);
    // The views the runtime told us of there are no longer what it shows.
    crate::inplace::views_gone(at, len);
    // And nothing stays in place over it.
    crate::inplace::title_memory_freeing(at, len);
    // The standing asking's word about a run that changed is not true of
    // what is there now: what it covered is asked about again. What the
    // table found written stays, as the journal kept it: a page written
    // before the remap was written, and the copies that follow it take the
    // new mapping's contents.
    {
        let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        let end = at.saturating_add(len);
        let (from, to) = (at / PAGE, end.div_ceil(PAGE));
        j.ahead.retain(|&(a, b)| b <= from || a >= to);
    }
    let mut g = guards();
    let range = at / BLOCK..at.saturating_add(len).div_ceil(BLOCK);
    let absent: Vec<usize> = g.absent.range(range.clone()).copied().collect();
    for block in absent {
        g.absent.remove(&block);
    }
    let blocks: Vec<usize> = g.protected.range(range).copied().collect();
    for block in blocks {
        g.protected.remove(&block);
        g.clean.remove(&block);
    }
}

/// Registers the handler that takes writes to guarded blocks, once, ahead
/// of the crash reporter's.
fn install() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_write(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        const WRITE: usize = 1;
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            || r.NumberParameters < 2
            || r.ExceptionInformation[0] != WRITE
        {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let at = r.ExceptionInformation[1];
        // Held across both checks: another thread may have opened the block
        // since this one faulted, and a copy may have guarded it again since
        // (then it is ours again, not read-only for some other reason).
        let mut g = guards();
        if open(&mut g, at, at + 1) {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::GuardFaults, 1);
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        if writable(at) {
            EXCEPTION_CONTINUE_EXECUTION
        } else {
            EXCEPTION_CONTINUE_SEARCH
        }
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler only touches our guard state and protections.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_write));
    });
}

/// Forget what the table knows of `[at, at + len)`: what the tests that
/// reuse an address need, an address the table has seen being memory the
/// runtime had there before.
#[cfg(test)]
fn forget(at: usize, len: usize) {
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let (first, last) = (
        at / KERNEL_PAGE,
        at.saturating_add(len).div_ceil(KERNEL_PAGE),
    );
    j.written.retain(|&page, _| page < first || page >= last);
}

/// Whether `at` is committed read-write memory now.
fn writable(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State == MEM_COMMIT && info.Protect == PAGE_READWRITE
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    #[test]
    fn written_pages_are_reported_to_every_asker() {
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        let (start, end) = (p, p + 4 * PAGE);
        // The address may have held another test's memory.
        forget(start, end - start);
        let (_, before) = writes(start, end, false).expect("watched");
        assert!(before.is_empty());
        // SAFETY: committed pages of ours.
        unsafe { ((p + PAGE + 8) as *mut u8).write(1) };
        let (now, after) = writes(start, end, false).expect("watched");
        assert_eq!(after, [(p / PAGE + 1, now)]);
        // Another asker after the kernel's bit was cleared still sees it.
        let (later, again) = writes(p + PAGE, p + 2 * PAGE, false).expect("watched");
        assert!(later > now);
        assert_eq!(again, [(p / PAGE + 1, now)]);
        let mut plain = vec![0u8; PAGE];
        assert!(
            writes(
                plain.as_mut_ptr() as usize,
                plain.as_ptr() as usize + PAGE,
                true
            )
            .is_none()
        );
        forget(start, end - start);
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    /// A kernel read into a guarded view block fails; opened first, it
    /// lands and the block counts as written.
    #[test]
    fn kernel_reads_into_guarded_blocks_after_opening() {
        use std::io::{Read, Seek, Write};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile,
        };
        // SAFETY: a pagefile section of two blocks, mapped read-write.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize
        };
        assert_ne!(view, 0);
        assert!(guard(view, view + 2 * BLOCK).is_some()); // now read-only
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"from the disk").unwrap();
        // `ReadFile` inside: the kernel writes the view.
        let read = |file: &mut std::fs::File| {
            file.rewind().unwrap();
            // SAFETY: 13 bytes of our view, nothing else refers to them.
            let into = unsafe { std::slice::from_raw_parts_mut((view + 100) as *mut u8, 13) };
            file.read(into).is_ok_and(|n| n == 13)
        };
        assert!(
            !read(&mut file),
            "the kernel does not fault into our handler"
        );
        writing(view + 100, 13);
        assert!(read(&mut file));
        // SAFETY: the bytes just read.
        let landed = unsafe { std::slice::from_raw_parts((view + 100) as *const u8, 13) };
        assert_eq!(landed, b"from the disk");
        assert_eq!(
            guard(view, view + 2 * BLOCK),
            Some((view / PAGE..view / PAGE + 1).collect())
        );
        // SAFETY: our view.
        unsafe {
            UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: view as *mut _,
                },
            )
        };
        title_memory_remapped(view, 2 * BLOCK);
    }

    #[test]
    fn writes_in_partly_committed_view_blocks_are_reported() {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            SEC_RESERVE, UnmapViewOfFile,
        };

        const SIZE: usize = 3 * BLOCK;
        struct View(MEMORY_MAPPED_VIEW_ADDRESS);
        impl Drop for View {
            fn drop(&mut self) {
                title_memory_remapped(self.0.Value as usize, SIZE);
                // SAFETY: our view is still mapped.
                unsafe { UnmapViewOfFile(self.0) };
            }
        }
        // SAFETY: a new reserved pagefile section. The view holds its own
        // reference after we close the handle, including on assertion failure.
        let view = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE | SEC_RESERVE,
                0,
                SIZE as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, SIZE);
            CloseHandle(section);
            assert!(!view.Value.is_null());
            View(view)
        };
        let start = view.0.Value as usize;
        // The map may have been given an address another test's view had:
        // what the guards and the table knew of the blocks there is not true
        // of these.
        title_memory_remapped(start, SIZE);
        forget(start, SIZE);
        let commit = |at: usize, len| {
            // SAFETY: these pages belong to our reserved view.
            let committed = unsafe { VirtualAlloc(at as _, len, MEM_COMMIT, PAGE_READWRITE) };
            assert_eq!(committed as usize, at);
        };
        let early = start + BLOCK + 3 * KERNEL_PAGE;
        let late = start + 2 * BLOCK + 3 * KERNEL_PAGE;
        commit(start, BLOCK);
        commit(early, KERNEL_PAGE);
        let refresh = || guard(start, start + SIZE).unwrap();
        assert_eq!(refresh(), [start / PAGE, early / PAGE, late / PAGE]);
        // The last block is still entirely reserved.
        assert!(!refresh().contains(&(late / PAGE)));

        commit(late, KERNEL_PAGE); // Ordinary commit, without a runtime mapping notification.
        for at in [early, late] {
            assert!(writable(at));
            // SAFETY: each address is inside its committed read-write page.
            unsafe { (at as *mut u8).write_volatile(1) };
        }
        assert_eq!(refresh(), [early / PAGE, late / PAGE]);
    }

    /// A range of view memory whose first block is not mapped is a view
    /// all the same: its blocks are reported once, then the ones written
    /// or mapped since.
    #[test]
    fn a_range_that_begins_in_an_unmapped_block_is_guarded() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a fresh placeholder of three blocks and a pagefile
        // section of three; the middle block is mapped first.
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                3 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            // The address may have held another test's memory.
            title_memory_remapped(p, 3 * BLOCK);
            forget(p, 3 * BLOCK);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (3 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            // SAFETY: splits the placeholder at the block and maps the
            // section's block there.
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..3).flat_map(pages).collect();
        // Nothing mapped at all: a view with every block absent.
        assert!(is_view(p, p + 3 * BLOCK));
        let middle = map(1);
        title_memory_remapped(p + BLOCK, BLOCK);
        // The range begins in the placeholder, its middle block is a view.
        assert!(is_view(p, p + 3 * BLOCK));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(all));
        assert_eq!(
            guard(p, p + 3 * BLOCK),
            Some(Vec::new()),
            "nothing written since"
        );
        // SAFETY: inside the mapped middle block.
        unsafe { ((p + BLOCK + 8) as *mut u8).write_volatile(1) };
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(1).collect()));
        let first = map(0);
        title_memory_remapped(p, BLOCK);
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(0).collect()));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
        // Memory of ours that is no view stays none.
        let mut local = [0u8; 16];
        let at = local.as_mut_ptr() as usize;
        assert!(!is_view(at, at + local.len()));
        // SAFETY: our views and placeholder.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), middle, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        title_memory_remapped(p, 3 * BLOCK);
    }

    /// Blocks of a placeholder between views are reported once, then not
    /// again (they cannot be guarded) until the runtime maps them.
    #[test]
    fn unmapped_blocks_are_reported_once_until_mapped() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a fresh placeholder of three blocks and a pagefile
        // section of two; block 0 shows the section's first block, as the
        // runtime maps physical pages.
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                3 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            // The address may have held another test's memory.
            title_memory_remapped(p, 3 * BLOCK);
            forget(p, 3 * BLOCK);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            // SAFETY: splits the placeholder at the block and maps the
            // section's block there.
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let first = map(0);
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..3).flat_map(pages).collect();
        assert_eq!(guard(p, p + 3 * BLOCK), Some(all));
        assert_eq!(
            guard(p, p + 3 * BLOCK),
            Some(Vec::new()),
            "nothing written since"
        );
        let second = map(1);
        title_memory_remapped(p + BLOCK, BLOCK);
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(1).collect()));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
        // SAFETY: our views and placeholder.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), second, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        title_memory_remapped(p, 3 * BLOCK);
    }

    /// The standing ahead asking is one per process (the world makes one
    /// submission at a time): tests that make one take this so that a test
    /// running beside them cannot replace it between their calls.
    static SERIAL: Mutex<()> = Mutex::new(());

    /// The kernel is asked about a reservation once, whatever the number of
    /// ranges in it: the sync point's one asking answers them all, and the
    /// next sync point asks again.
    #[test]
    fn a_reservation_is_asked_about_once_per_sync_point() {
        let _one_at_a_time = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let w = Watched::new(16 * KERNEL_PAGES);
        let ranges: Vec<(usize, usize)> = (0..8)
            .map(|i| {
                let at = w.at + i * 2 * PAGE;
                (at, at + PAGE)
            })
            .collect();
        let asked = || ASKINGS.with(|n| n.get());
        let before = asked();
        for _ in 0..2 {
            ask_ahead(&ranges);
            for &(start, end) in &ranges {
                writes(start, end, false).expect("write-watched");
            }
            ask_ahead_done();
        }
        assert_eq!(
            asked() - before,
            2,
            "one asking per sync point, not per range"
        );
    }

    /// Write-watched memory of `pages` 4 KiB pages, freed when dropped.
    struct Watched {
        at: usize,
        pages: usize,
    }

    impl Watched {
        fn new(pages: usize) -> Self {
            // SAFETY: a fresh write-watched allocation, freed on drop.
            let at = unsafe {
                VirtualAlloc(
                    std::ptr::null(),
                    pages * KERNEL_PAGE,
                    MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                    PAGE_READWRITE,
                )
            } as usize;
            assert_ne!(at, 0);
            // The address may have held another test's memory.
            forget(at, pages * KERNEL_PAGE);
            Self { at, pages }
        }

        fn end(&self) -> usize {
            self.at + self.pages * KERNEL_PAGE
        }
    }

    impl Drop for Watched {
        fn drop(&mut self) {
            title_memory_remapped(self.at, self.pages * KERNEL_PAGE);
            // SAFETY: our allocation.
            unsafe { VirtualFree(self.at as *mut _, 0, MEM_RELEASE) };
        }
    }

    /// The askings' answers that the table has to keep: one page dirtied
    /// twice, a page dirtied while mappings are made and unmade beside it,
    /// and a page dirtied and then remapped.
    #[test]
    fn the_table_reports_what_the_askings_reported() {
        let w = Watched::new(4 * KERNEL_PAGES);
        let (start, end) = (w.at, w.end());
        let page = |n: usize| w.at + n * PAGE;
        let (_, nothing) = writes(start, end, false).expect("watched");
        assert!(nothing.is_empty());
        // A page dirtied twice with no asking between: one page, its last
        // asking.
        // SAFETY: inside our allocation.
        unsafe { (page(1) as *mut u8).write_volatile(1) };
        let (first, once) = writes(start, end, false).expect("watched");
        assert_eq!(once, [(page(1) / PAGE, first)]);
        // SAFETY: as above.
        unsafe { ((page(1) + 8) as *mut u8).write_volatile(2) };
        let (second, twice) = writes(start, end, false).expect("watched");
        assert!(second > first);
        assert_eq!(twice, [(page(1) / PAGE, second)], "dirtied twice, one page");
        // A page dirtied while mappings are made and unmade beside it stays
        // as the asking found it.
        // SAFETY: as above.
        unsafe { (page(2) as *mut u8).write_volatile(3) };
        {
            let _remapping = title_memory_remapping();
            title_memory_remapped(page(3), PAGE);
        }
        let (third, beside) = writes(start, end, false).expect("watched");
        assert!(third > second);
        assert_eq!(
            beside,
            [(page(1) / PAGE, second), (page(2) / PAGE, third)],
            "a mapping beside a written page keeps both"
        );
        // A page dirtied and then remapped is still reported, under the
        // asking that found it written: the journal kept it, and so does the
        // table (the copy that follows takes the new mapping's contents).
        {
            let _remapping = title_memory_remapping();
            title_memory_remapped(page(2), PAGE);
        }
        let (_, after) = writes(start, end, false).expect("watched");
        assert!(
            after.contains(&(page(2) / PAGE, third)),
            "a remapped page keeps the asking that found it written: {after:?}"
        );
    }

    /// Today's asking, as the reference: the kernel asked about each range
    /// as it was refreshed, its answer kept in a table of 64 KiB pages
    /// numbered by the asking that found each. This is `writes` before the
    /// table, and the standard a run must meet.
    struct Reference {
        asked: u64,
        written: BTreeMap<usize, u64>,
        found: Vec<usize>,
    }

    impl Reference {
        fn new() -> Self {
            Self {
                asked: 0,
                written: BTreeMap::new(),
                found: Vec::new(),
            }
        }

        fn writes(&mut self, start: usize, end: usize) -> Option<(u64, Vec<(usize, u64)>)> {
            use windows_sys::Win32::System::Memory::GetWriteWatch;
            const WRITE_WATCH_FLAG_RESET: u32 = 1;
            let (first, last) = (start / PAGE, end.div_ceil(PAGE));
            let mut count = (last - first) * KERNEL_PAGES;
            self.found.resize(count.max(self.found.len()), 0);
            let mut granularity = 0;
            // SAFETY: the answer buffer holds `count` entries.
            let failed = unsafe {
                GetWriteWatch(
                    WRITE_WATCH_FLAG_RESET,
                    start as *const _,
                    end - start,
                    self.found.as_mut_ptr().cast(),
                    &mut count,
                    &mut granularity,
                )
            } != 0;
            if failed {
                return None;
            }
            self.asked += 1;
            let now = self.asked;
            for k in 0..count {
                self.written.insert(self.found[k] / PAGE, now);
            }
            Some((
                now,
                self.written
                    .range(first..last)
                    .map(|(&p, &w)| (p, w))
                    .collect(),
            ))
        }
    }

    /// Over 5,000 frames of the same writes and the same refreshes, the
    /// table reports the pages today's per-range asking reports, and the
    /// copies that follow take the same pages: the byte totals match.
    #[test]
    fn five_thousand_frames_report_what_the_per_range_asking_reports() {
        let _one_at_a_time = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        const PAGES: usize = 128;
        let ours = PAGES / KERNEL_PAGES;
        let watched = Watched::new(PAGES);
        let theirs = Watched::new(PAGES);
        let mut reference = Reference::new();
        let (mut copied, mut ref_copied) = (vec![0u64; ours], vec![0u64; ours]);
        let (mut bytes, mut ref_bytes) = (0usize, 0usize);
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for frame in 0..5_000u64 {
            for _ in 0..(next() % 4 + 1) {
                let at =
                    ((next() as usize) % PAGES) * KERNEL_PAGE + (next() as usize) % KERNEL_PAGE;
                // SAFETY: inside both allocations.
                unsafe {
                    ((watched.at + at) as *mut u8).write_volatile(1);
                    ((theirs.at + at) as *mut u8).write_volatile(1);
                }
            }
            let (mut a, mut b) = ((next() as usize) % ours, (next() as usize) % ours);
            if a > b {
                std::mem::swap(&mut a, &mut b);
            }
            let ranges = [(a * PAGE, (a + 1) * PAGE), (b * PAGE, (b + 1) * PAGE)];
            let batched = frame % 3 != 0;
            if batched {
                let asked: Vec<(usize, usize)> = ranges
                    .iter()
                    .map(|&(x, y)| (watched.at + x, watched.at + y))
                    .collect();
                ask_ahead(&asked);
            }
            for &(start, end) in &ranges {
                let (now, written) =
                    writes(watched.at + start, watched.at + end, false).expect("write-watched");
                let (asking, today) = reference
                    .writes(theirs.at + start, theirs.at + end)
                    .expect("write-watched");
                let reported: Vec<usize> = written.iter().map(|&(p, _)| p).collect();
                let want: Vec<usize> = today
                    .iter()
                    .map(|&(p, _)| p - theirs.at / PAGE + watched.at / PAGE)
                    .collect();
                assert_eq!(reported, want, "frame {frame}: pages reported dirty");
                // As the world copies (shadow.rs): takes a page iff it was
                // written since the copy, and remembers the asking's number.
                for &(page, write) in &written {
                    let at = &mut copied[page - watched.at / PAGE];
                    if *at == 0 || write > *at {
                        *at = now;
                    }
                }
                for &(page, write) in &today {
                    let at = &mut ref_copied[page - theirs.at / PAGE];
                    if *at == 0 || write > *at {
                        *at = asking;
                    }
                }
                bytes += reported.len() * PAGE;
                ref_bytes += want.len() * PAGE;
            }
            if batched {
                ask_ahead_done();
            }
        }
        assert!(bytes > 0, "the frames reported something");
        assert_eq!(
            bytes, ref_bytes,
            "the pages refreshed and the bytes they hold match today's"
        );
    }

    /// A write or mapping that runs past the end of the address space (a
    /// title's `fread` with a huge count) covers up to the end; the sum used
    /// to overflow. Up there no test has guarded blocks.
    #[test]
    fn ranges_past_the_address_space_end_do_not_overflow() {
        let top = usize::MAX - 16;
        writing(top, PAGE);
        title_memory_remapped(top, usize::MAX);
    }
}

#[cfg(test)]
mod mapping_tests {
    use super::{title_memory_reading, title_memory_remapping};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    /// A change of mapping waits for a read in place that is under way, and
    /// a read in place waits for a change of mapping.
    #[test]
    fn reads_in_place_and_remappings_wait_for_each_other() {
        // A reader holds; the change does not begin until it lets go.
        let changed = Arc::new(AtomicBool::new(false));
        let reading = title_memory_reading();
        let (began, wait) = mpsc::channel();
        let change = {
            let changed = changed.clone();
            std::thread::spawn(move || {
                began.send(()).unwrap();
                let _changing = title_memory_remapping();
                changed.store(true, Ordering::SeqCst);
            })
        };
        wait.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !changed.load(Ordering::SeqCst),
            "the mapping changed under a read in place"
        );
        drop(reading);
        change.join().unwrap();
        assert!(changed.load(Ordering::SeqCst));
        // A change holds; the read does not begin until it is done.
        let read = Arc::new(AtomicBool::new(false));
        let changing = title_memory_remapping();
        let (began, wait) = mpsc::channel();
        let reader = {
            let read = read.clone();
            std::thread::spawn(move || {
                began.send(()).unwrap();
                let _reading = title_memory_reading();
                read.store(true, Ordering::SeqCst);
            })
        };
        wait.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !read.load(Ordering::SeqCst),
            "a read in place began during a change of mapping"
        );
        drop(changing);
        reader.join().unwrap();
        assert!(read.load(Ordering::SeqCst));
    }
}
