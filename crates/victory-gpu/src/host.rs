//! The host GPU: one D3D12 device and direct queue (decision 0007), and a
//! window to present on.
//!
//! shortcut: every submission is waited for, so CPU and GPU never overlap
//! (unless `VICTORY_LATE_WAIT=1`, see [`late`]);
//! overlap them with per-submission fences once more than clears run.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_12_0, D3D_FEATURE_LEVEL_12_2};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCWSTR, w};

/// Resource states without the console's own bits (compression hints from
/// bit 25 up), which the host rejects.
pub(crate) fn pc_state(state: u32) -> u32 {
    state & 0x01ff_ffff
}

/// A console state as a host texture state: PC bits only, and none of the
/// buffer-only ones the console allows on textures (vertex, constant and
/// index buffer, stream out, indirect arguments, as in "generic read").
pub(crate) fn texture_state(state: u32) -> u32 {
    const BUFFER_ONLY: u32 = 0x1 | 0x2 | 0x100 | 0x200;
    pc_state(state) & !BUFFER_ONLY
}

/// Render target views live only until their command is recorded, so a
/// ring of host descriptors is enough.
const RTV_RING: u32 = 1024;

pub(crate) struct Host {
    pub(crate) device: ID3D12Device,
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    /// The last value signalled on `fence`, behind the submission lock.
    submitted: Mutex<u64>,
    event: HANDLE,
    /// The copies of title memory into its copies in the card's memory, on
    /// a queue of their own ([`copy_queue`]); `None` when the switch is off
    /// or the device would not make the queue, and the copies then go in the
    /// direct list ahead of the submission that reads them.
    copies: Option<CopyQueue>,
    rtvs: ID3D12DescriptorHeap,
    rtv_size: u32,
    /// A ring of depth views, used as `rtvs`.
    dsvs: ID3D12DescriptorHeap,
    dsv_size: u32,
    next_dsv: AtomicU32,
    /// With `VICTORY_D3D12_DEBUG` set (diagnostics builds): the debug
    /// layer's messages.
    messages: Option<ID3D12InfoQueue>,
    next_rtv: AtomicU32,
    presenter: Mutex<Option<Presenter>>,
    /// The adapter the device is on, for its memory budget
    /// ([`Host::video_memory`]); `None` if the system does not name it.
    adapter: Option<IDXGIAdapter3>,
    /// The pixel shading rate every host graphics list carries
    /// (`VICTORY_SHADING_RATE`); `None` when the switch is off or the device
    /// has no variable shading rate above tier 1 ([`shading_rate`]).
    shading: Option<ShadingRate>,
}

// SAFETY: D3D12 devices, queues, fences and descriptor heaps are
// free-threaded; the event handle is only waited on under `submitted`.
unsafe impl Send for Host {}
// SAFETY: as above.
unsafe impl Sync for Host {}

/// Host objects the title let go of while a submission may still use them
/// (a texture its descriptors name, a descriptor slot's old resource), by
/// the submission count they wait for. The title's descriptor writes and
/// releases reach the host at once, from its own threads, while the host
/// runs a submission: freeing there is a GPU page fault on the freed
/// texture (a device hang).
static RETIRED: Mutex<Vec<(u64, Retired)>> = Mutex::new(Vec::new());
/// Submissions started and finished (waited for).
static STARTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FINISHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A retired object, dropped on whatever thread finishes the submission.
struct Retired(#[allow(dead_code)] Box<dyn std::any::Any>);
// SAFETY: what is retired is D3D12 objects (free-threaded) and the
// reference-counted wrappers holding them, dropped once.
unsafe impl Send for Retired {}

/// Milliseconds since the first call.
pub(crate) fn now_ms() -> u64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// This moment, for telling later how long ago it was ([`long_ago`]): the
/// frames begun so far (high half, `crate::frame::current`) and tenths of a
/// second since the start. Host copies of title memory, staging buffers and
/// host textures note when they were last used with it.
pub(crate) fn moment() -> u64 {
    (crate::frame::current() << 32) | ((now_ms() / 100) & 0xffff_ffff)
}

/// Whether `moment` lies at least `ms` milliseconds and `frames` frames
/// back. Both: at seconds per frame, what every frame uses is not touched
/// for seconds, and at many frames per second not for many frames. (A title
/// that never waits for frames has no frame count: nothing is then idle.)
pub(crate) fn long_ago(moment: u64, ms: u64, frames: u64) -> bool {
    let now = self::moment();
    let tenths = (now & 0xffff_ffff).saturating_sub(moment & 0xffff_ffff);
    (now >> 32).saturating_sub(moment >> 32) >= frames && tenths * 100 >= ms
}

/// Available physical RAM on the host in bytes, from `GlobalMemoryStatusEx`.
pub(crate) fn available_physical_ram() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    let ok = unsafe { GlobalMemoryStatusEx(&mut m) } != 0;
    ok.then_some(m.ullAvailPhys)
}

/// Effective idle timeout (in milliseconds) for VRAM-backed host textures,
/// scaled down under VRAM pressure. Never returns less than 1,000 ms so
/// handles not yet recorded into a list or slot remain protected.
pub(crate) fn vram_idle_ms(video_memory: Option<[u64; 4]>) -> u64 {
    const NORMAL_MS: u64 = 5_000;
    const MODERATE_MS: u64 = 2_500;
    const HIGH_MS: u64 = 1_000;
    let Some([budget, used, ..]) = video_memory else {
        return NORMAL_MS;
    };
    if budget == 0 {
        return NORMAL_MS;
    }
    let used_pct = (u128::from(used) * 100) / u128::from(budget);
    if used_pct >= 85 {
        HIGH_MS
    } else if used_pct >= 75 {
        MODERATE_MS
    } else {
        NORMAL_MS
    }
}

/// Effective idle timeout (in milliseconds) for system-RAM caches (staging
/// buffers, loose shadows, and the mapped pipeline library), scaled down
/// when free physical RAM is low. Never returns less than 2,000 ms.
pub(crate) fn ram_idle_ms(avail_phys: Option<u64>) -> u64 {
    const GIB: u64 = 1024 * 1024 * 1024;
    const NORMAL_MS: u64 = 10_000;
    const MODERATE_MS: u64 = 5_000;
    const HIGH_MS: u64 = 2_000;
    let Some(avail) = avail_phys else {
        return NORMAL_MS;
    };
    if avail < 2 * GIB {
        HIGH_MS
    } else if avail < 4 * GIB {
        MODERATE_MS
    } else {
        NORMAL_MS
    }
}

/// How many references are held on a host object (its COM count): ours
/// all, since the host's views and commands hold none. A buffer or texture
/// with one is held by its owner alone: no descriptor slot keeps it
/// (`HostHeap::kept`) and no list (`HostList::keep`).
pub(crate) fn references(object: &impl Interface) -> u32 {
    type Method = unsafe extern "system" fn(*mut std::ffi::c_void) -> u32;
    let raw = object.as_raw();
    // SAFETY: a live COM object, whose first three methods are those of
    // `IUnknown` (query, add a reference, release one); the pair leaves the
    // count as it was, and the release returns it. The first is not called.
    unsafe {
        let methods = *raw.cast::<*const [Method; 3]>();
        ((*methods)[1])(raw);
        ((*methods)[2])(raw)
    }
}

/// Once a second, before a submission starts: lets go of what nothing
/// names any more (copies of windows of title memory, staging buffers,
/// host copies of textures the title fills).
/// `VICTORY_SWEEP=0` keeps everything, as before.
fn sweep() {
    // The in-place probe and census, if asked for (`VICTORY_INPLACE_PROBE`).
    crate::inplace::tick();
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("VICTORY_SWEEP").is_none_or(|v| v != "0")) {
        return;
    }
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = now_ms();
    let last = LAST.load(Ordering::Relaxed);
    if now < last + 1000
        || LAST
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    crate::pipeline::trim_library();
    crate::shadow::let_go_idle();
    crate::texture::let_go_idle();
    crate::resource::let_go_idle();
}

/// Drops `object` once every submission running now has finished: at once
/// if none is.
pub(crate) fn retire<T: 'static>(object: T) {
    let started = STARTED.load(Ordering::Acquire);
    if started <= FINISHED.load(Ordering::Acquire) {
        RETIREMENTS[0].fetch_add(1, Ordering::Relaxed);
        drop(object);
        return;
    }
    RETIREMENTS[1].fetch_add(1, Ordering::Relaxed);
    RETIRED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((started, Retired(Box::new(object))));
}

/// Drops what waited for submissions up to `finished`.
fn bury(finished: u64) {
    let done: Vec<(u64, Retired)> = {
        let mut retired = RETIRED.lock().unwrap_or_else(|e| e.into_inner());
        if retired.is_empty() {
            return;
        }
        let (done, keep) = std::mem::take(&mut *retired)
            .into_iter()
            .partition(|(at, _)| *at <= finished);
        *retired = keep;
        done
    };
    drop(done);
}

/// Objects [`retire`] dropped at once (no submission was running) and
/// kept until a running submission finished, since the start.
static RETIREMENTS: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// What the layer did to the host's objects lately, newest last (sweeps
/// that let host copies go, pipeline library calls, saves and trims): said
/// once when the host device is removed ([`log_removal_context`]), with
/// what the submitting thread and the pipeline workers were doing at that
/// moment. A device the driver removes "internally" names no command, and
/// the pass that happens to notice first names only the view it was at:
/// the three such losses of runs 72, 73 and 82 each came within 5 ms of a
/// pipeline library call that no line of the log tied to them.
/// `VICTORY_TRAIL=0`: nothing is kept and nothing is said.
static TRAIL: Mutex<std::collections::VecDeque<(u64, u32, String)>> = Mutex::new(std::collections::VecDeque::new());
/// The entries kept.
const TRAIL_MOST: usize = 96;

fn trail_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TRAIL").is_none_or(|v| v != "0"))
}

/// Microseconds since the first call.
pub(crate) fn now_us() -> u64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_micros() as u64
}

/// The calling thread, as the log's other lines name threads.
pub(crate) fn thread_id() -> u32 {
    // SAFETY: a plain query.
    unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }
}

/// Notes something done to the host's objects ([`TRAIL`]); `what` is only
/// run when the trail is kept.
pub(crate) fn trail(what: impl FnOnce() -> String) {
    if !trail_on() {
        return;
    }
    let entry = (now_us(), thread_id(), what());
    let mut trail = TRAIL.lock().unwrap_or_else(|e| e.into_inner());
    if trail.len() >= TRAIL_MOST {
        trail.pop_front();
    }
    trail.push_back(entry);
}

/// What the submitting thread is doing (0 nothing: between submissions,
/// 1 preparing one, 2 waiting for the card to run it, 3 copying back what
/// it wrote), the lists of that submission, and since when ([`now_us`]).
static SUBMITTING: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

fn submitting(phase: u64, lists: usize) {
    SUBMITTING[1].store(lists as u64, Ordering::Relaxed);
    SUBMITTING[2].store(now_us(), Ordering::Relaxed);
    SUBMITTING[0].store(phase, Ordering::Relaxed);
}

/// What was going on when the host device went: whether a submission was
/// on the card, the pipeline library's state and the calls running in it,
/// and the trail.
pub(crate) fn removal_context() -> String {
    use std::fmt::Write;
    let now = now_us();
    let ms = |at: u64| now.saturating_sub(at) as f64 / 1000.0;
    let phase = match SUBMITTING[0].load(Ordering::Relaxed) {
        0 => "between submissions (nothing of ours was running on the card)",
        1 => "preparing a submission (nothing of ours was running on the card)",
        2 => "waiting for the card to run a submission",
        _ => "copying back what a submission wrote (nothing of ours was running on the card)",
    };
    let mut text = format!(
        "the submitting thread was {phase}, of {} lists, for {:.1} ms; {}; objects let go at once {} and after a running submission {}; submissions started {} finished {}",
        SUBMITTING[1].load(Ordering::Relaxed),
        ms(SUBMITTING[2].load(Ordering::Relaxed)),
        crate::pipeline::library_removal_note(),
        RETIREMENTS[0].load(Ordering::Relaxed),
        RETIREMENTS[1].load(Ordering::Relaxed),
        STARTED.load(Ordering::Relaxed),
        FINISHED.load(Ordering::Relaxed),
    );
    let trail = TRAIL.lock().unwrap_or_else(|e| e.into_inner());
    let _ = write!(text, "; the last {} things done, newest last:", trail.len());
    for (at, thread, what) in trail.iter() {
        let _ = write!(text, "\n  {:.1} ms before, thread {thread}: {what}", ms(*at));
    }
    text
}

/// Says [`removal_context`] in the log, once: called by whoever finds the
/// host device removed.
pub(crate) fn log_removal_context() {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !trail_on() || SAID.swap(true, Ordering::Relaxed) {
        return;
    }
    tracing::error!("when the host device went: {}", removal_context());
}

/// Time spent in submissions since the last frame-rate line, in
/// microseconds: preparing them on the CPU (title memory copies, texture
/// detiling), then waiting for the GPU.
static SUBMISSION_US: [std::sync::atomic::AtomicU64; 2] =
    [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];

/// Time the texture refresh thread spent waiting for title memory to hold
/// still, in microseconds, since the last frame-rate line. Wall-clock time of
/// the run that the preparation split cannot show: it is spent on another
/// thread, while the title's remapping is in flight.
static REFRESH_STILL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// After submissions: shader-written ranges compared and copied back into
/// title memory (microseconds, bytes that differed), since the last
/// frame-rate line.
static COPY_BACK: [std::sync::atomic::AtomicU64; 2] =
    [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];

/// The card's clock, for the frame line of every run, in microseconds since
/// the last one: how long the card had nothing of ours queued; how much of
/// that while the thread that holds the submission lock was working (the
/// card idle behind us: preparing and copying back are done with the card
/// standing); and how long that thread stood waiting for the card.
///
/// Every send of lists to the queue is followed by a signal on a fence of
/// the clock's own ([`Host::card_sent`]). A thread of the clock waits for
/// the newest signal and notes the moment the card reached it with nothing
/// newer sent; the next send ends that idle stretch.
static CARD_CLOCK_US: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Of the thread that holds the submission lock: the microseconds it has
/// worked so far (preparing, copying back; not its waits for the card), and
/// since when it works now ([`now_us`] plus one; 0: it does not).
static WORKING: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// The thread that holds the submission lock starts (`on`) or stops
/// working; whether it worked until now.
fn working(on: bool) -> bool {
    let now = now_us() + 1;
    let since = WORKING[1].swap(if on { now } else { 0 }, Ordering::Relaxed);
    if since != 0 {
        WORKING[0].fetch_add(now.saturating_sub(since), Ordering::Relaxed);
    }
    since != 0
}

/// [`WORKING`] as of now: microseconds worked so far.
fn worked() -> u64 {
    let since = WORKING[1].load(Ordering::Relaxed);
    let now = if since != 0 { (now_us() + 1).saturating_sub(since) } else { 0 };
    WORKING[0].load(Ordering::Relaxed) + now
}

/// The thread that holds the submission lock stood waiting for the card
/// from `since` until now ([`CARD_CLOCK_US`]).
fn stood(since: std::time::Instant) {
    CARD_CLOCK_US[2].fetch_add(since.elapsed().as_micros() as u64, Ordering::Relaxed);
}

/// What [`CARD_CLOCK_US`] is measured with.
struct CardClock {
    fence: ID3D12Fence,
    /// Signals sent so far; and, while the card has reached the last of
    /// them, since when and what [`worked`] said then.
    state: Mutex<(u64, Option<(std::time::Instant, u64)>)>,
    sent: std::sync::Condvar,
}

// SAFETY: a fence (free-threaded) behind a lock.
unsafe impl Send for CardClock {}
// SAFETY: as above.
unsafe impl Sync for CardClock {}

/// The clock's thread: waits for the newest signal sent and notes when the
/// card got there with nothing newer behind it.
fn card_clock_watch(clock: &'static CardClock) {
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        return;
    };
    let mut seen = 0;
    loop {
        let target = {
            let mut state = clock.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.0 == seen {
                state = clock.sent.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            state.0
        };
        // SAFETY: the clock's fence and this thread's event.
        unsafe {
            if clock.fence.SetEventOnCompletion(target, event).is_ok() {
                WaitForSingleObject(event, INFINITE);
            }
        }
        seen = target;
        let mut state = clock.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.0 == target && state.1.is_none() {
            state.1 = Some((std::time::Instant::now(), worked()));
        }
    }
}

/// For the frame-rate line: [`CARD_CLOCK_US`] per frame, `ms` being the
/// frame's time.
fn card_clock_line(frames: f64, ms: f64) {
    let [idle, behind, stood] = [0, 1, 2].map(|i| CARD_CLOCK_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames);
    tracing::info!(
        "the card's clock per frame: nothing of ours queued {idle:.1} ms (the card about {:.0}% busy by this count), \
         {behind:.1} ms of that while the submitting thread was working (the card idle behind us), the rest while the title \
         had not submitted; the submitting thread stood waiting for the card {stood:.1} ms",
        if ms > 0.0 { (100.0 * (1.0 - idle / ms)).clamp(0.0, 100.0) } else { 0.0 }
    );
}

/// Where a submission's preparing goes, in microseconds since the last
/// frame-rate line, always counted: the frame line splits preparing into
/// copies and textures only, and what is left of a 4 s frame is otherwise
/// unattributed. `PHASE_OTHER` is not counted here (it is what the frame
/// line's preparing has over these sums).
static PHASES: [std::sync::atomic::AtomicU64; 7] = [const { std::sync::atomic::AtomicU64::new(0) }; 7];

/// [`PHASES`] in the order the print names them.
const PHASE_NAMES: [&str; 7] = ["asking", "recording", "flush", "snapshots", "card", "send", "settle"];

const PHASE_ASKING: usize = 0;
const PHASE_RECORDING: usize = 1;
const PHASE_FLUSH: usize = 2;
const PHASE_SNAPSHOTS: usize = 3;
const PHASE_CARD: usize = 4;
const PHASE_SEND: usize = 5;
const PHASE_SETTLE: usize = 6;

/// Adds what passed since `since` to a [`PHASES`] entry.
fn phase(i: usize, since: std::time::Instant) {
    PHASES[i].fetch_add(since.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
}

/// Of the preparing: title memory copies (microseconds, bytes) and texture
/// refreshes (microseconds, bytes hashed), since the last frame-rate line.
pub(crate) static PREPARING: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// What a submission's deferred copies into title memory cost, by stage,
/// since the last frame line: the steps' own time ([`ST_STEP`], the number
/// [`PREPARING`] is given) and the parts of it measured apart — the flush
/// before a copy ([`ST_FLUSH`], which [`Batch::update`] counts again as its
/// own), opening the guards over what the copy writes ([`ST_GUARD`]), the
/// kernel's read ([`ST_READ`]), noting the bytes written ([`ST_WROTE`]) —
/// the copies and the runs of them ([`ST_STEPS`], [`ST_RUNS`]), and the
/// host copies a step forced up to date ([`ST_UPDATE`]).
///
/// The frame line's `copies` is `ST_UPDATE` plus the steps' time with
/// `ST_FLUSH` inside it: the flush of a copy is refresh work once, and
/// counted twice while it ran inside the step that asked for it.
static COPY_STAGES: [std::sync::atomic::AtomicU64; 8] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 8];

const ST_STEP: usize = 0;
const ST_FLUSH: usize = 1;
const ST_GUARD: usize = 2;
const ST_READ: usize = 3;
const ST_WROTE: usize = 4;
const ST_UPDATE: usize = 5;
const ST_STEPS: usize = 6;
const ST_RUNS: usize = 7;

/// Adds `micros` to a [`COPY_STAGES`] entry.
fn staged(at: usize, micros: u64) {
    COPY_STAGES[at].fetch_add(micros, Ordering::Relaxed);
}

/// Host buffers made since the start, as (count, bytes): in the upload heap
/// (staging, list scratch), shared with the processor (host copies of title
/// memory) and the graphics card's own (indirect arguments, twins' copies).
/// For the memory line: against what is alive, they show what was let go.
pub(crate) static MADE: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];

/// What of that is alive now, for the memory line (`MADE` counts what was
/// ever made: 42,607 upload buffers, 77.9 GB, in five minutes of run 60,
/// and nothing said how many were alive): host lists; the lists' scratch
/// chunks and their bytes; of those, the chunks no list holds (kept for the
/// next list, [`Spare`]) and their bytes; chunks taken from those kept
/// instead of made, since the start; and the lists' other upload buffers
/// (texture data copied from buffers, scratch too big for a chunk) and
/// their bytes.
pub(crate) static ALIVE: [std::sync::atomic::AtomicU64; 8] = [const { std::sync::atomic::AtomicU64::new(0) }; 8];

/// About host lists, for the memory line: lists made by the host since the
/// start; preludes made (a list's second allocator and list, made when it
/// first needs one, [`lazy_prelude`]); and, in diagnostics builds, the
/// bytes the process's private memory grew by while lists were being made
/// (what a list costs: host objects with the driver's command memory; 631
/// lists were alive in run 65, each with a prelude it may never use).
/// Then the lists alive that were never reset since they were made: the
/// host sides of title lists the title made and has not used (a host list
/// cost 0.68 MB of private memory in run 69: 431 MB for the 632 the title
/// makes at its start).
pub(crate) static LISTS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// The process's private bytes (diagnostics: around the making of lists).
#[cfg(feature = "diagnostics")]
fn private_bytes() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    c.PagefileUsage as u64
}

/// What the runtime does when the host GPU fails for good: report it like a
/// crash and end the run. The console has no such state, and carrying on
/// draws nothing at seconds per frame.
static ON_FAILURE: OnceLock<fn(&str) -> !> = OnceLock::new();

/// Sets what a host GPU failure does (the runtime's crash report).
pub fn on_failure(report: fn(&str) -> !) {
    let _ = ON_FAILURE.set(report);
}

/// The console has no removed device: when the host's goes, the error alone
/// says only "suspended". Say the reason `GetDeviceRemovedReason` gives
/// (a TDR, an invalid call, ...) once, at the first call that reports the
/// device gone, in one line with the debug layer's first error if one is
/// kept.
pub(crate) fn note_removal(error: &windows::core::Error) {
    const REMOVED: i32 = 0x887A_0005u32 as i32;
    const RESET: i32 = 0x887A_0006u32 as i32;
    const HUNG: i32 = 0x887A_0007u32 as i32;
    const INVALID_CALL: i32 = 0x887A_0001u32 as i32;
    let code = error.code().0;
    if ![REMOVED, RESET, HUNG, INVALID_CALL].contains(&code) {
        return;
    }
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if SAID.swap(true, Ordering::Relaxed) {
        return;
    }
    let Some(host) = host() else {
        return;
    };
    // SAFETY: a plain query.
    let reason = match unsafe { host.device.GetDeviceRemovedReason() } {
        Ok(()) => "none: the device is not removed".to_owned(),
        Err(e) => e.to_string(),
    };
    let layer = host.log_messages();
    tracing::error!(reason = %reason, first = %error, layer = ?layer, "the host device was removed");
}

/// Ends the run on a host GPU failure: what failed, the removal reason and
/// the debug layer's first error, if any (`VICTORY_D3D12_DEBUG=1` in a
/// diagnostics build names the command).
pub(crate) fn fail(what: std::fmt::Arguments) -> ! {
    let mut text = what.to_string();
    if let Some(host) = host() {
        // SAFETY: a plain query.
        if let Err(e) = unsafe { host.device.GetDeviceRemovedReason() } {
            text.push_str(&format!("; device removed: {e}"));
            log_removal_context();
        }
        match host.log_messages() {
            Some(first) => text.push_str(&format!("; debug layer: {first}")),
            None if host.messages.is_none() => text.push_str(
                "; a diagnostics build with VICTORY_D3D12_DEBUG=1 names the command",
            ),
            None => {}
        }
    }
    tracing::error!("{text}");
    // Tests have no log: the reason goes to their output.
    #[cfg(test)]
    eprintln!("host GPU failure: {text}");
    match ON_FAILURE.get() {
        Some(report) => report(&text),
        None => std::process::abort(),
    }
}

/// The host GPU, created on first use; `None` without one (a run then
/// behaves as before: nothing executes).
pub(crate) fn host() -> Option<&'static Host> {
    static HOST: OnceLock<Option<Host>> = OnceLock::new();
    HOST.get_or_init(|| match create() {
        Ok(host) => Some(host),
        Err(e) => {
            tracing::warn!(error = %e, "no host D3D12 device; command lists will not run");
            None
        }
    })
    .as_ref()
}

/// The `D3D12Core.dll` a redistributed Agility SDK ships, beside the
/// executable in a `D3D12` folder, is 1.<version>.0 and later: the number
/// after the `1.` is what the SDK configuration carries.
const AGILITY_SDK_VERSION: u32 = 614;

/// Points the system `d3d12.dll` at a redistributed D3D12 core, so the
/// process gets the Agility SDK's D3D12 rather than the older one Windows 10
/// ships. That is what provides feature level 12_2, shader model 6.6 and
/// root signatures declaring heap direct indexing. Windows 11's inbox core
/// has them already, so a missing folder is not an error.
///
/// `SetSDKVersion` is unusable here: it must be the process's first D3D12
/// call, and import binding has already run. `CreateDeviceFactory` has no
/// such ordering requirement, and `ApplyToGlobalState` is required or
/// `d3d12.dll`'s own exports (the root-signature serializer among them) keep
/// using the old core.
fn d3d12_factory() -> Option<ID3D12DeviceFactory> {
    use windows::Win32::Graphics::Direct3D12::{
        CLSID_D3D12SDKConfiguration, D3D12GetInterface, ID3D12DeviceFactory,
        ID3D12SDKConfiguration1,
    };

    static FACTORY: OnceLock<Option<ID3D12DeviceFactory>> = OnceLock::new();
    FACTORY
        .get_or_init(|| {
            let folder = std::env::var_os("VICTORY_D3D12_SDK")
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    let exe = std::env::current_exe().ok()?;
                    Some(exe.parent()?.join("D3D12"))
                })?;
            if !folder.join("D3D12Core.dll").is_file() {
                tracing::debug!(
                    folder = %folder.display(),
                    "no redistributed D3D12 core; the system one stays"
                );
                return None;
            }
            let mut config: Option<ID3D12SDKConfiguration1> = None;
            // SAFETY: a documented entry point; the class id and interface
            // both come from the same headers.
            if let Err(e) = unsafe { D3D12GetInterface(&CLSID_D3D12SDKConfiguration, &mut config) }
            {
                tracing::warn!(error = %e, "D3D12 SDK configuration is unavailable");
                return None;
            }
            let config = config?;
            let text = folder.to_string_lossy().to_string();
            for candidate in [text.clone(), format!("{text}\\")] {
                let path: Vec<u8> = candidate.bytes().chain([0]).collect();
                // SAFETY: a NUL-terminated path alive across the call.
                let made: windows::core::Result<ID3D12DeviceFactory> = unsafe {
                    config.CreateDeviceFactory(AGILITY_SDK_VERSION, windows::core::PCSTR(path.as_ptr()))
                };
                match made {
                    Ok(factory) => {
                        // SAFETY: a live factory; the call sets global state.
                        if let Err(e) = unsafe { factory.ApplyToGlobalState() } {
                            tracing::warn!(error = %e, "the new core did not become global");
                        }
                        tracing::info!(
                            version = AGILITY_SDK_VERSION,
                            path = %candidate,
                            "D3D12 Agility SDK selected"
                        );
                        return Some(factory);
                    }
                    Err(e) => tracing::debug!(
                        error = %e,
                        path = %candidate,
                        "D3D12 Agility SDK path refused"
                    ),
                }
            }
            tracing::warn!(
                version = AGILITY_SDK_VERSION,
                folder = %text,
                "D3D12 Agility SDK refused; the system core stays"
            );
            None
        })
        .clone()
}

/// Creates the device through the redistributed core when there is one, else
/// through the system `d3d12.dll`.
fn create_device(
    factory: Option<&ID3D12DeviceFactory>,
    adapter: Option<&IDXGIAdapter>,
    level: D3D_FEATURE_LEVEL,
    device: &mut Option<ID3D12Device>,
) -> windows::core::Result<()> {
    match (factory, adapter) {
        // SAFETY: a live factory and adapter; the device is written once.
        (Some(factory), Some(adapter)) => unsafe { factory.CreateDevice(adapter, level, device) },
        // SAFETY: as above, with the default adapter.
        (Some(factory), None) => unsafe {
            factory.CreateDevice(None::<&windows::core::IUnknown>, level, device)
        },
        (None, Some(adapter)) => {
            // SAFETY: a live adapter; the device is written once.
            unsafe { D3D12CreateDevice(adapter, level, device) }
        }
        (None, None) => {
            // SAFETY: the default adapter; the device is written once.
            unsafe { D3D12CreateDevice(None, level, device) }
        }
    }
}

/// What `VICTORY_BACKGROUND_PROCESSING` asks of the runtime's background work
/// (`create`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Background {
    /// The driver's background compilation and measurements are off.
    Off,
    /// The runtime's own default: background work as it sees fit.
    Normal,
    /// Intrusive measurements, committed when the mode returns.
    Measure,
}

impl Background {
    fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" | "0" => Some(Self::Off),
            "normal" | "on" | "1" => Some(Self::Normal),
            "measure" => Some(Self::Measure),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Normal => "normal",
            Self::Measure => "measure",
        }
    }

    /// The mode, and what the runtime does with the measurements it takes.
    fn mode(self) -> (D3D12_BACKGROUND_PROCESSING_MODE, D3D12_MEASUREMENTS_ACTION) {
        match self {
            Self::Off => (
                D3D12_BACKGROUND_PROCESSING_MODE_DISABLE_BACKGROUND_WORK,
                D3D12_MEASUREMENTS_ACTION_KEEP_ALL,
            ),
            Self::Normal => (
                D3D12_BACKGROUND_PROCESSING_MODE_ALLOWED,
                D3D12_MEASUREMENTS_ACTION_KEEP_ALL,
            ),
            Self::Measure => (
                D3D12_BACKGROUND_PROCESSING_MODE_ALLOW_INTRUSIVE_MEASUREMENTS,
                D3D12_MEASUREMENTS_ACTION_COMMIT_RESULTS,
            ),
        }
    }
}

/// `VICTORY_BACKGROUND_PROCESSING`, default `off`: nothing named is `off`,
/// and text that names no mode is refused to `off` rather than obeyed.
fn background_processing(named: Option<&str>) -> Background {
    match named.and_then(Background::parse) {
        Some(asked) => asked,
        None => {
            if let Some(text) = named {
                tracing::warn!(
                    value = %text,
                    "VICTORY_BACKGROUND_PROCESSING: off, normal or measure; off is used"
                );
            }
            Background::Off
        }
    }
}

/// What `VICTORY_SHADING_RATE` asks of the pixel shading rate: one pixel
/// shader invocation over a rectangle of pixels, on every host graphics list
/// ([`HostList::shade`]). Measured on the world it changed nothing
/// (`MEMORY.md` §5.1): the cost there is per draw, not per pixel, so the
/// switch is off unless asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ShadingRate {
    /// One invocation per pixel, the console's own rate.
    Off,
    X1Y2,
    X2Y1,
    X2Y2,
    X4Y4,
}

impl ShadingRate {
    /// The one spelling of each rate, case aside: text that names none is
    /// refused to `off` by [`shading_rate_asked`] rather than guessed at.
    fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "1x2" => Some(Self::X1Y2),
            "2x1" => Some(Self::X2Y1),
            "2x2" => Some(Self::X2Y2),
            "4x4" => Some(Self::X4Y4),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::X1Y2 => "1x2",
            Self::X2Y1 => "2x1",
            Self::X2Y2 => "2x2",
            Self::X4Y4 => "4x4",
        }
    }

    fn d3d12(self) -> D3D12_SHADING_RATE {
        match self {
            Self::Off => D3D12_SHADING_RATE_1X1,
            Self::X1Y2 => D3D12_SHADING_RATE_1X2,
            Self::X2Y1 => D3D12_SHADING_RATE_2X1,
            Self::X2Y2 => D3D12_SHADING_RATE_2X2,
            Self::X4Y4 => D3D12_SHADING_RATE_4X4,
        }
    }
}

/// `VICTORY_SHADING_RATE`, default `off`: nothing named is `off`, and text
/// that names no rate is warned about once and refused to `off`.
fn shading_rate_asked(named: Option<&str>) -> ShadingRate {
    match named.and_then(ShadingRate::parse) {
        Some(asked) => asked,
        None => {
            if let Some(text) = named {
                victory_core::logging::warn_once("gpu.shading_rate.value", || {
                    format!(
                        "VICTORY_SHADING_RATE: off, 1x2, 2x1, 2x2 or 4x4; off is used; value={text}"
                    )
                });
            }
            ShadingRate::Off
        }
    }
}

/// The rate the lists carry, from the switch and the device's tier. A device
/// whose feature query failed (`None`) or that knows only tier 1 keeps 1x1:
/// `RSSetShadingRate` there is a no-op or refused, and a pipeline or a list
/// must never fail over a switch.
fn shading_rate_in_force(
    asked: ShadingRate,
    tier: Option<D3D12_VARIABLE_SHADING_RATE_TIER>,
) -> Option<ShadingRate> {
    match tier {
        Some(tier) if tier.0 >= D3D12_VARIABLE_SHADING_RATE_TIER_2.0 => match asked {
            ShadingRate::Off => None,
            asked => Some(asked),
        },
        _ => None,
    }
}

/// Reads `VICTORY_SHADING_RATE` and the device's variable shading rate tier
/// once, logs both once (the tier is logged even when the switch is off), and
/// returns the rate every host graphics list carries.
fn shading_rate(device: &ID3D12Device) -> Option<ShadingRate> {
    let named = std::env::var("VICTORY_SHADING_RATE").ok();
    let asked = shading_rate_asked(named.as_deref());
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS6::default();
    // SAFETY: writes `options` of the given size.
    let tier = unsafe {
        device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS6,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
            .is_ok()
    }
    .then_some(options.VariableShadingRateTier);
    let in_force = shading_rate_in_force(asked, tier);
    tracing::info!(
        asked = asked.name(),
        tier = tier.map_or(-1, |tier| tier.0),
        in_force = in_force.map_or("off", |rate| rate.name()),
        "variable shading rate: asked, the device's tier (tier 2 is required), and what the \
         host graphics lists carry"
    );
    in_force
}

fn create() -> windows::core::Result<Host> {
    // SAFETY: plain D3D12 object creation with valid descriptions.
    unsafe {
        // A newer D3D12 core first: the debug layer and device-removed data
        // below are the core's own settings, and before `ApplyToGlobalState`
        // `D3D12GetDebugInterface` hands out the system core's, which the
        // device from the new core never reads (a run with
        // `VICTORY_D3D12_DRED=1` had "no breadcrumbs (0x887A0004)").
        let factory = d3d12_factory();
        // The debug layer only in builds with diagnostics.
        let debug =
            cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_D3D12_DEBUG").is_some();
        if debug {
            let mut layer: Option<ID3D12Debug> = None;
            match D3D12GetDebugInterface(&mut layer) {
                Ok(()) => layer.iter().for_each(|l| l.EnableDebugLayer()),
                Err(e) => tracing::warn!(error = %e, "no D3D12 debug layer"),
            }
            // GPU-based validation (`VICTORY_D3D12_GBV`): out-of-range
            // descriptor heap indices and the like become messages naming
            // the command instead of page faults. Without state tracking
            // (directly indexed textures are not transitioned).
            if std::env::var_os("VICTORY_D3D12_GBV").is_some()
                && let Some(l) = &layer
            {
                if let Ok(d1) = l.cast::<ID3D12Debug1>() {
                    d1.SetEnableGPUBasedValidation(true);
                }
                if let Ok(d2) = l.cast::<ID3D12Debug2>() {
                    d2.SetGPUBasedValidationFlags(
                        D3D12_GPU_BASED_VALIDATION_FLAGS_DISABLE_STATE_TRACKING,
                    );
                }
                tracing::info!("GPU-based validation on");
            }
        }
        // Diagnostics: device-removed data (the command that faulted, the
        // faulting address), read when the device is removed.
        if dred_on() {
            let mut dred: Option<ID3D12DeviceRemovedExtendedDataSettings> = None;
            if D3D12GetDebugInterface(&mut dred).is_ok()
                && let Some(dred) = dred
            {
                dred.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                dred.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
            }
        }
        let mut device: Option<ID3D12Device> = None;
        // Diagnostics: the software adapter, on which a shader's wild access
        // is an access violation in this process (a crash report with the
        // address and instruction) instead of a GPU fault and driver reset.
        if cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_D3D12_WARP").is_some() {
            let adapter = warp_adapter()?;
            create_device(
                factory.as_ref(),
                Some(&adapter),
                D3D_FEATURE_LEVEL_12_2,
                &mut device,
            )?;
            tracing::info!("host device on the software adapter (WARP)");
        } else {
            // Feature level 12_2 is where shader model 6.6 and descriptor-heap
            // direct indexing live, which the title's pipelines and root
            // signatures use. An older host refuses it, and then 12_0 runs.
            if let Err(e) = create_device(
                factory.as_ref(),
                None,
                D3D_FEATURE_LEVEL_12_2,
                &mut device,
            ) {
                tracing::warn!(
                    error = %e,
                    "feature level 12_2 unavailable; falling back to 12_0"
                );
                create_device(
                    factory.as_ref(),
                    None,
                    D3D_FEATURE_LEVEL_12_0,
                    &mut device,
                )?;
            }
        }
        let device = device.ok_or_else(windows::core::Error::empty)?;
        // `VICTORY_BACKGROUND_PROCESSING=off|normal|measure`: what the runtime
        // may do behind the title's back. The driver compiles shaders again on
        // its "D3D Background Thread"s for minutes after pipelines were made
        // or loaded, and while those threads run the same shot took
        // 1,000-1,200 ms a frame against 830-950 ms from the moment they
        // stopped (run 54, 08:32:01 to 08:32:06). `off` is the default.
        //
        // The runtime answers a mode it does not have, so the feature query
        // decides whether the switch was really there; without
        // `BackgroundProcessingSupported` the call is a benign no-op.
        if let Ok(device6) = device.cast::<ID3D12Device6>() {
            // `VICTORY_D3D12_BACKGROUND=0`, the older spelling, asks for
            // what is now the default and needs no reading of its own.
            let named = std::env::var("VICTORY_BACKGROUND_PROCESSING").ok();
            let asked = background_processing(named.as_deref());
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS6::default();
            let supported = device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS6,
                    (&raw mut options).cast(),
                    size_of_val(&options) as u32,
                )
                .is_ok()
                && options.BackgroundProcessingSupported.as_bool();
            let (mode, measurements) = asked.mode();
            let accepted = supported
                && device6
                    .SetBackgroundProcessingMode(mode, measurements, None, None)
                    .is_ok();
            tracing::info!(
                asked = asked.name(),
                accepted,
                BackgroundProcessingSupported = supported,
                "background processing mode (measure is for a menu or a loading screen, not a scene)"
            );
        }
        // the widest wave the GPU runs: 32 (NVIDIA) gets console wave64 compute programs whose
        // cross-lane code stays in 32-lane halves translated for waves of 32
        let mut o1 = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
        if device
            .CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS1, (&raw mut o1).cast(), size_of_val(&o1) as u32)
            .is_ok()
        {
            tracing::info!(min = o1.WaveLaneCountMin, max = o1.WaveLaneCountMax, "host wave lanes");
            victory_shader::emit::set_host_max_wave(o1.WaveLaneCountMax);
        }
        let messages = debug
            .then(|| device.cast::<ID3D12InfoQueue>().ok())
            .flatten();
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        })?;
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        let event = CreateEventW(None, false, false, None)?;
        // The uploads' own queue ([`copy_queue`]): a device that refuses one
        // leaves them on the direct queue, as without the switch.
        let copies = if copy_queue() {
            match make_copy_queue(&device) {
                Ok(copies) => Some(copies),
                Err(e) => {
                    tracing::warn!(error = %e, "no copy queue for the copies into the card's memory; they stay on the direct queue");
                    None
                }
            }
        } else {
            None
        };
        let rtvs: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let rtv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
        let dsvs: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let dsv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_DSV);
        let adapter = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            .and_then(|factory| factory.EnumAdapterByLuid::<IDXGIAdapter3>(device.GetAdapterLuid()))
            .ok();
        // `VICTORY_SHADING_RATE`, and what this device makes of it, before a
        // list exists to carry it.
        let shading = shading_rate(&device);
        tracing::info!("host D3D12 device ready");
        Ok(Host {
            device,
            queue,
            fence,
            submitted: Mutex::new(0),
            event,
            copies,
            rtvs,
            rtv_size,
            dsvs,
            dsv_size,
            next_dsv: AtomicU32::new(0),
            messages,
            next_rtv: AtomicU32::new(0),
            presenter: Mutex::new(None),
            adapter,
            shading,
        })
    }
}

/// DXGI may return hardware for `EnumWarpAdapter` (notably under Proton).
/// Reject it before creating a device in diagnostics' software-only mode.
pub(crate) fn warp_adapter() -> windows::core::Result<IDXGIAdapter> {
    // SAFETY: plain DXGI adapter queries.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter()?;
        let desc = adapter.cast::<IDXGIAdapter1>()?.GetDesc1()?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
            tracing::warn!(
                vendor = format_args!("{:#x}", desc.VendorId),
                flags = format_args!("{:#x}", desc.Flags),
                "DXGI did not identify the WARP adapter as software; refusing device"
            );
            return Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_FAIL,
            ));
        }
        Ok(adapter)
    }
}

#[cfg(test)]
mod tests {
    /// The presents that cannot do what they were asked are counted and say
    /// which occurrence they are ([`victory_core::logging::warn_once`]): a
    /// client area dragged under the swap chain's size refuses on every
    /// present, and a `Once` reported that once and then read as if the window
    /// had stopped being dragged.
    #[test]
    fn a_present_refusal_says_which_occurrence_it_is() {
        use super::{frame_not_float, resize_refused};
        for nth in 1..=10 {
            let logged = frame_not_float(28, 87);
            assert_eq!(logged, nth == 1 || nth == 10, "occurrence {nth} of the present's format");
        }
        assert!(resize_refused("E_FAIL", (640, 480)), "the first refusal is logged");
        assert!(!resize_refused("E_FAIL", (640, 480)), "the second is counted");
        let totals = victory_core::logging::warn_once_totals();
        let count = |key: &str| {
            totals
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, count)| *count)
        };
        assert_eq!(count("present.frame_format"), Some(10), "{totals:?}");
        assert_eq!(count("present.resize"), Some(2), "{totals:?}");
    }

    /// The shading rate switch names five states, case aside, and refuses
    /// anything else to `off`; a device whose query failed or that knows only
    /// tier 1 keeps 1x1, and `off` is 1x1 even on a tier-2 device.
    #[test]
    fn the_shading_rate_switch_names_five_rates() {
        use windows::Win32::Graphics::Direct3D12::*;

        use super::{ShadingRate, shading_rate_asked, shading_rate_in_force};
        assert_eq!(ShadingRate::parse("off"), Some(ShadingRate::Off));
        assert_eq!(ShadingRate::parse(" 1X2 "), Some(ShadingRate::X1Y2));
        assert_eq!(ShadingRate::parse("2x1"), Some(ShadingRate::X2Y1));
        assert_eq!(ShadingRate::parse("2x2"), Some(ShadingRate::X2Y2));
        assert_eq!(ShadingRate::parse("4x4"), Some(ShadingRate::X4Y4));
        assert_eq!(ShadingRate::parse("1x1"), None, "1x1 is spelled off");
        assert_eq!(ShadingRate::parse("4x2"), None, "a rate the switch does not name");
        assert_eq!(ShadingRate::parse("3x3"), None);
        assert_eq!(ShadingRate::parse(""), None);
        assert_eq!(shading_rate_asked(None), ShadingRate::Off, "nothing named is off");
        assert_eq!(shading_rate_asked(Some("typo")), ShadingRate::Off);
        assert_eq!(shading_rate_asked(Some("4x4")), ShadingRate::X4Y4);
        assert_eq!(ShadingRate::X2Y2.name(), "2x2");
        assert_eq!(ShadingRate::X4Y4.d3d12(), D3D12_SHADING_RATE_4X4);
        assert_eq!(ShadingRate::Off.d3d12(), D3D12_SHADING_RATE_1X1);
        let tier = D3D12_VARIABLE_SHADING_RATE_TIER_2;
        assert_eq!(shading_rate_in_force(ShadingRate::X2Y2, None), None, "no query");
        assert_eq!(
            shading_rate_in_force(
                ShadingRate::X2Y2,
                Some(D3D12_VARIABLE_SHADING_RATE_TIER_NOT_SUPPORTED)
            ),
            None
        );
        assert_eq!(
            shading_rate_in_force(ShadingRate::X2Y2, Some(D3D12_VARIABLE_SHADING_RATE_TIER_1)),
            None,
            "tier 1 keeps 1x1"
        );
        assert_eq!(
            shading_rate_in_force(ShadingRate::X2Y2, Some(tier)),
            Some(ShadingRate::X2Y2)
        );
        assert_eq!(
            shading_rate_in_force(ShadingRate::Off, Some(tier)),
            None,
            "off sets nothing"
        );
    }

    /// The switch has three spellings, unknown text is refused to `off`
    /// rather than obeyed, nothing named is `off`, and only `measure` lets
    /// the runtime commit its measurements.
    #[test]
    fn background_processing_names_one_of_three_modes() {
        use windows::Win32::Graphics::Direct3D12::*;

        use super::{Background, background_processing};
        assert_eq!(Background::parse("off"), Some(Background::Off));
        assert_eq!(Background::parse(" Normal "), Some(Background::Normal));
        assert_eq!(Background::parse("MEASURE"), Some(Background::Measure));
        assert_eq!(Background::parse("0"), Some(Background::Off));
        assert_eq!(Background::parse("sometimes"), None);
        assert_eq!(background_processing(None), Background::Off);
        assert_eq!(background_processing(Some("measure")), Background::Measure);
        assert_eq!(background_processing(Some("normal")), Background::Normal);
        assert_eq!(background_processing(Some("typo")), Background::Off);
        assert_eq!(
            Background::Off.mode(),
            (
                D3D12_BACKGROUND_PROCESSING_MODE_DISABLE_BACKGROUND_WORK,
                D3D12_MEASUREMENTS_ACTION_KEEP_ALL
            )
        );
        assert_eq!(
            Background::Normal.mode(),
            (
                D3D12_BACKGROUND_PROCESSING_MODE_ALLOWED,
                D3D12_MEASUREMENTS_ACTION_KEEP_ALL
            )
        );
        assert_eq!(
            Background::Measure.mode(),
            (
                D3D12_BACKGROUND_PROCESSING_MODE_ALLOW_INTRUSIVE_MEASUREMENTS,
                D3D12_MEASUREMENTS_ACTION_COMMIT_RESULTS
            )
        );
    }

    #[test]
    fn sparse_copies_preserve_islands_and_observe_remaps() {
        use windows_sys::Win32::System::Memory::*;
        const N: usize = 5 << 16;
        // SAFETY: we reserve a private region, commit selected pages inside it,
        // and only directly access the pages while they are writable.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), N, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            let mut expected = vec![0; N];
            for (offset, value) in [(0, 9), (2 << 16, 37), (N - 4096, 85)] {
                let at = base.add(offset);
                assert_eq!(VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE), at.cast());
                at.write_bytes(value, 4096);
                expected[offset..offset + 4096].fill(value);
            }
            for remap in [false, true] {
                if remap {
                    let mut old = 0;
                    assert_ne!(VirtualProtect(base.cast(), 4096, PAGE_NOACCESS, &mut old), 0);
                    expected[..4096].fill(0);
                    let at = base.add(1 << 16);
                    assert_eq!(VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE), at.cast());
                    at.write_bytes(0xc7, 4096);
                    expected[1 << 16..(1 << 16) + 4096].fill(0xc7);
                }
                let mut out = vec![0xee; N];
                super::copy_mapped(out.as_mut_ptr().add(27), base.add(27), N - 54, false);
                assert_eq!(out[27..N - 27], expected[27..N - 27]);
                assert!(out[..27].iter().chain(&out[N - 27..]).all(|&v| v == 0xee));
            }
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    use super::*;

    #[test]
    fn warp_adapter_accepts_only_software() {
        // This probes DXGI without creating a D3D12 device or running a title.
        // SAFETY: plain DXGI adapter queries.
        let raw = unsafe {
            let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            else {
                return;
            };
            factory.EnumWarpAdapter::<IDXGIAdapter>()
        };
        let Ok(raw) = raw else { return };
        // SAFETY: EnumWarpAdapter returns a DXGI adapter, which implements
        // IDXGIAdapter1; `raw` owns that interface.
        let desc = unsafe { raw.cast::<IDXGIAdapter1>().unwrap().GetDesc1().unwrap() };
        eprintln!(
            "EnumWarpAdapter: vendor={:#x}, flags={:#x}",
            desc.VendorId, desc.Flags
        );
        assert_eq!(
            warp_adapter().is_ok(),
            desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
        );
    }

    /// A present moves the shown target's tracked state before it does
    /// anything that can fail, and every way out puts that state back: a
    /// minimised window refuses the resize, the back buffer and the pass, and
    /// a target left tracked as the state the pass read it in is a barrier
    /// the rest of the run will not emit.
    #[test]
    fn a_present_puts_back_the_state_it_moved() {
        use std::cell::Cell;
        let moved = Cell::new(0);
        {
            let _restore = Restore::new(|| moved.set(moved.get() + 1));
            assert_eq!(moved.get(), 0, "nothing is put back before the way out");
        }
        assert_eq!(moved.get(), 1, "the way out puts back what the present moved");
        // The present that gets the whole way restores the state itself, and
        // the guard then has nothing left to do: a second restore would take
        // the target out of the state the list just moved it to.
        let moved = Cell::new(0);
        {
            let mut restore = Restore::new(|| moved.set(moved.get() + 1));
            moved.set(moved.get() + 10);
            restore.done();
        }
        assert_eq!(moved.get(), 10, "a finished present is not restored twice");
    }
}

/// A host command list and its allocator, always used together.
pub(crate) struct HostList {
    allocator: ID3D12CommandAllocator,
    pub(crate) list: ID3D12GraphicsCommandList,
    /// The list as `ID3D12GraphicsCommandList5` and the shading rate it
    /// carries (`VICTORY_SHADING_RATE`), `None` when the switch is off or the
    /// interface is missing. A reset returns the rate to 1x1, so it is set
    /// again after every reset ([`HostList::shade`]).
    shading: Option<(ID3D12GraphicsCommandList5, D3D12_SHADING_RATE)>,
    /// Work for submission time, in recorded order.
    deferred: Mutex<Deferred>,
    /// Upload memory for data the list's draws make (expanded vertices),
    /// reused after a reset.
    scratch: Mutex<Scratch>,
    /// Host texture states the list assumes, by host copy: at its start and
    /// at its end. Lists are recorded on many threads and run in submission
    /// order, so the transitions from a texture's real state are made at
    /// submission, in `prelude`. Holding the copies keeps every texture the
    /// list names alive until it is reset, even if the title releases the
    /// resource first.
    states: Mutex<States>,
    /// What the list does with depth textures that have a colour twin, by
    /// twin ([`TwinUse`]).
    twins: Mutex<std::collections::HashMap<usize, TwinUse>>,
    /// The list that runs those transitions just before this one, made
    /// when the list first has such transitions ([`lazy_prelude`]).
    prelude: OnceLock<Option<Prelude>>,
    /// The pipelines the list binds, for reports of slow submissions.
    pipelines: Mutex<std::collections::BTreeSet<String>>,
    /// The work its indirect executions may still do (`indirect`): a
    /// shader-writable word, zeroed when the list is submitted.
    budget: OnceLock<Option<(ID3D12Resource, usize)>>,
    /// Device-local scratch buffer for clamped indirect arguments (`indirect`),
    /// grown to the largest indirect execution recorded on the list.
    indirect_args: Mutex<Option<(ID3D12Resource, u64)>>,
    /// Counts the list in [`ALIVE`] while it lives.
    _alive: Counted,
    /// The texts of the markers `note_op` set, in order (for device-removed
    /// data without marker texts), with the host GPU range a copy reads.
    #[cfg(feature = "diagnostics")]
    notes: Mutex<Vec<Note>>,
    /// With `VICTORY_GPU_TIMING`: a GPU timestamp before each noted command
    /// and at the end, and where they are read back.
    #[cfg(feature = "diagnostics")]
    stamps: OnceLock<Option<(ID3D12QueryHeap, ID3D12Resource)>>,
}

/// A list's prelude: an allocator and a closed list.
type Prelude = (ID3D12CommandAllocator, ID3D12GraphicsCommandList);

/// Whether a host list's prelude is made when the list first needs one. A
/// prelude runs the texture transitions and twin copies a list needs before
/// it; lists that name no texture never have one, and every host list was
/// made with one all the same: twice the host objects per list, for the
/// title's 630 lists and for every list made anew.
/// `VICTORY_LAZY_PRELUDE=0`: made with the list, as before.
fn lazy_prelude() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LAZY_PRELUDE").is_none_or(|v| v != "0"))
}

/// Timestamps per list (diagnostics); commands past them are not timed.
#[cfg(feature = "diagnostics")]
const STAMPS: u32 = 4096;

/// A marker's text and the host range its command reads (diagnostics).
#[cfg(feature = "diagnostics")]
type Note = (String, Option<(u64, u64)>);

type States = std::collections::HashMap<
    usize,
    (u32, u32, std::sync::Arc<crate::resource::HostTexture>),
>;

/// What a list does with a depth texture that has a colour twin
/// ([`crate::resource::Twin`]). Lists are recorded on many threads and run
/// in submission order, so a list cannot know which side an earlier list
/// leaves behind: it notes which sides it uses as it finds them, and its
/// prelude copies one side into the other at submission if that side is
/// behind then. From its first use of a side on, the list knows.
struct TwinUse {
    /// The depth texture (the title resource's host copy) and its twin.
    depth: std::sync::Arc<crate::resource::HostTexture>,
    twin: std::sync::Arc<crate::resource::Twin>,
    /// The sides (depth texture, twin) the list uses as it finds them.
    needs: [bool; 2],
    /// Whether each side lacks what the other was written last, where the
    /// list is now; `None`: as the list found it.
    behind: [Option<bool>; 2],
    /// The draw or dispatch being prepared writes the twin.
    writing: bool,
    /// The depth plane's twin holds what the plane lacks although the list
    /// copied the twins since: the plane's pass waits for the list's close.
    late: bool,
}

/// The pass that draws the twin of a depth plane into the plane: one
/// triangle over each mip of each slice, its pixel shader writing the
/// twin's texel as the depth, with the depth test "always" and no stencil
/// (the stencil plane is copied through a buffer and not touched here).
///
/// A 16-bit twin holds whole steps of 1/65535, and the depth written is
/// the step times the float nearest 1/65535 (0x37800080). That is how
/// The title's tiled lighting computes the depth of the
/// rectangles it then draws with the test "equal", one class of materials
/// a draw (all six of its vertex shaders multiply the class number by that
/// constant), so the pass and the draw give the host the same float, and
/// the host turns the same float into the same 24 bits. The step divided
/// by 65535 is another float for 512 of the 65,536 steps (none under 257),
/// and lands in another 24-bit step for 127 of them (none under 385).
const PLANE_CORNERS: &str = r#"
float4 corner(uint i : SV_VertexID) : SV_Position {
    return float4(float(i & 1) * 4.0 - 1.0, float(i >> 1) * 4.0 - 1.0, 0.0, 1.0);
}
"#;
const PLANE_STEPS: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL)"
Texture2DArray<uint> twin : register(t0);
[RootSignature(RS)]
float plane(float4 p : SV_Position) : SV_Depth {
    return float(twin.Load(int4(int2(p.xy), 0, 0))) * asfloat(0x37800080);
}
"#;
const PLANE_FLOATS: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL)"
Texture2DArray<float> twin : register(t0);
[RootSignature(RS)]
float plane(float4 p : SV_Position) : SV_Depth {
    return twin.Load(int4(int2(p.xy), 0, 0));
}
"#;

/// Views of twins the pass reads, in a heap of ours used as a ring.
/// shortcut: a view must last until its list has run; with one or two
/// passes a frame the ring is far from coming round.
const PLANE_VIEWS: u32 = 256;

/// What the pass draws with: the heap of its views of twins, and per kind
/// of twin (16-bit steps or floats) and depth format the root signature
/// and pipeline, made when first needed (`None`: the host refused).
struct PlanePass {
    views: ID3D12DescriptorHeap,
    cpu: usize,
    gpu: u64,
    step: u32,
    next: AtomicU32,
    made: Mutex<Vec<(bool, i32, Option<PlanePipeline>)>>,
}

/// The pass's root signature and pipeline for one kind of twin and one
/// depth format.
type PlanePipeline = (ID3D12RootSignature, ID3D12PipelineState);

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for PlanePass {}
// SAFETY: as above.
unsafe impl Sync for PlanePass {}

fn plane_pass() -> Option<&'static PlanePass> {
    static PASS: OnceLock<Option<PlanePass>> = OnceLock::new();
    PASS.get_or_init(|| {
        let host = host()?;
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        // SAFETY: plain heap creation and queries.
        unsafe {
            let views: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: PLANE_VIEWS,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .ok()?;
            Some(PlanePass {
                cpu: views.GetCPUDescriptorHandleForHeapStart().ptr,
                gpu: views.GetGPUDescriptorHandleForHeapStart().ptr,
                step: host.device.GetDescriptorHandleIncrementSize(kind),
                views,
                next: AtomicU32::new(0),
                made: Mutex::default(),
            })
        }
    })
    .as_ref()
}

/// Compiles function `entry` of `source` for `target` with the system's
/// compiler, as the indirect-argument clamp is (`crate::indirect`).
pub(crate) fn compile(
    source: &str,
    entry: windows::core::PCSTR,
    target: windows::core::PCSTR,
) -> windows::core::Result<Vec<u8>> {
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            windows::core::s!("victory"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                )
            };
            tracing::warn!("a shader of ours: {}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes.
    let bytes = unsafe {
        std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
    };
    Ok(bytes.to_vec())
}

impl PlanePass {
    /// The root signature and pipeline for a twin of 16-bit steps (`steps`)
    /// or of floats and the depth format `format`.
    fn pipeline(&self, steps: bool, format: DXGI_FORMAT) -> Option<PlanePipeline> {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, _, pipeline)) = made.iter().find(|m| (m.0, m.1) == (steps, format.0)) {
            return pipeline.clone();
        }
        let pipeline = match Self::make(steps, format) {
            Ok(pipeline) => Some(pipeline),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    steps,
                    format = format.0,
                    "no pipeline to draw a depth plane from its twin: such planes stay as they are"
                );
                None
            }
        };
        made.push((steps, format.0, pipeline.clone()));
        pipeline
    }

    fn make(steps: bool, format: DXGI_FORMAT) -> windows::core::Result<PlanePipeline> {
        use windows::core::s;
        let device = &host().ok_or_else(windows::core::Error::empty)?.device;
        let vs = compile(PLANE_CORNERS, s!("corner"), s!("vs_5_1"))?;
        let source = if steps { PLANE_STEPS } else { PLANE_FLOATS };
        let ps = compile(source, s!("plane"), s!("ps_5_1"))?;
        let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
            pShaderBytecode: bytes.as_ptr().cast(),
            BytecodeLength: bytes.len(),
        };
        // SAFETY: the root signature is inside the pixel shader; the
        // description points at live data and never releases its pointer.
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &ps[..])?;
            let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                VS: code(&vs[..]),
                PS: code(&ps[..]),
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                // Every texel is written; stencil stays off.
                DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                    DepthEnable: true.into(),
                    DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
                    DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                    ..Default::default()
                },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                DSVFormat: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            let pipeline: ID3D12PipelineState = device.CreateGraphicsPipelineState(&desc)?;
            Ok((root, pipeline))
        }
    }
}

/// Records into `list` the pass that draws `plane`, the twin of the depth
/// plane of `depth`, into that plane: `depth` in the depth-write state,
/// `plane` readable by pixel shaders. The pass sets the list's descriptor
/// heap, graphics root signature, pipeline, depth target, viewport,
/// scissor and topology and puts none of them back: it is for a list that
/// records nothing of the title's after it.
fn record_plane_pass(
    list: &ID3D12GraphicsCommandList,
    depth: &ID3D12Resource,
    plane: &ID3D12Resource,
) {
    let (Some(host), Some(pass)) = (host(), plane_pass()) else {
        return;
    };
    // SAFETY: live resources.
    let (of, twin) = unsafe { (depth.GetDesc(), plane.GetDesc()) };
    // An R16 typeless twin (53) holds steps, read as R16_UINT (57); an R32
    // typeless one floats, read as R32_FLOAT (41). The depth view names
    // the depth-stencil format of the texture's family (44: 45, 19: 20).
    let steps = twin.Format.0 == 53;
    let format = DXGI_FORMAT(match of.Format.0 {
        44 => 45,
        19 => 20,
        typed => typed,
    });
    let Some((root, pipeline)) = pass.pipeline(steps, format) else {
        return;
    };
    for slice in 0..u32::from(of.DepthOrArraySize) {
        for mip in 0..u32::from(of.MipLevels) {
            let slot = pass.next.fetch_add(1, Ordering::Relaxed) % PLANE_VIEWS;
            let view = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT(if steps { 57 } else { 41 }),
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2DArray: D3D12_TEX2D_ARRAY_SRV {
                        MostDetailedMip: mip,
                        MipLevels: 1,
                        FirstArraySlice: slice,
                        ArraySize: 1,
                        ..Default::default()
                    },
                },
            };
            let target = D3D12_DEPTH_STENCIL_VIEW_DESC {
                Format: format,
                ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
                    Texture2DArray: D3D12_TEX2D_ARRAY_DSV {
                        MipSlice: mip,
                        FirstArraySlice: slice,
                        ArraySize: 1,
                    },
                },
                ..Default::default()
            };
            let target = host.dsv(depth, Some(&target));
            let (width, height) = ((of.Width >> mip).max(1) as u32, (of.Height >> mip).max(1));
            let cpu = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: pass.cpu + (slot * pass.step) as usize,
            };
            let gpu = D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: pass.gpu + u64::from(slot * pass.step),
            };
            // SAFETY: an open list; a slot of our heap; live resources and
            // objects (the pass's own live as long as the process).
            unsafe {
                host.device
                    .CreateShaderResourceView(plane, Some(&view), cpu);
                list.SetDescriptorHeaps(&[Some(pass.views.clone())]);
                list.SetGraphicsRootSignature(&root);
                list.SetPipelineState(&pipeline);
                list.SetGraphicsRootDescriptorTable(0, gpu);
                list.OMSetRenderTargets(0, None, false, Some(&target));
                list.RSSetViewports(&[D3D12_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: width as f32,
                    Height: height as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                }]);
                list.RSSetScissorRects(&[RECT {
                    left: 0,
                    top: 0,
                    right: width as i32,
                    bottom: height as i32,
                }]);
                list.IASetPrimitiveTopology(
                    windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY(4),
                );
                list.DrawInstanced(3, 1, 0, 0);
            }
            crate::resource::PLANES[0].fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Records into `list` the copy of a depth texture into its colour twin
/// (`colour`) or of the twin into the depth texture, both already in their
/// copy states: whole textures of one size in one format family, which the
/// host copies as they are; or, for the twin of a stencil plane, that plane
/// alone, through the twin's buffer.
fn record_twin_copy(
    list: &ID3D12GraphicsCommandList,
    depth: &ID3D12Resource,
    twin: &crate::resource::Twin,
    colour: bool,
) {
    let (from, to) = if colour {
        (depth, &twin.colour.texture)
    } else {
        (&twin.colour.texture, depth)
    };
    crate::resource::TWINS[2 + usize::from(colour)].fetch_add(1, Ordering::Relaxed);
    // shortcut: the depth plane's twin is not given the plane (that is a
    // pass the other way: the plane read by a shader, the twin as its
    // target). The twin then lacks what draws and clears wrote to the
    // plane since shaders last wrote the twin. In the title
    // nothing writes that plane but the twin's pass (its 192 draws against
    // it only test, run 29), so the twin lacks nothing. Counted.
    if colour && twin.plane.is_some() {
        crate::resource::PLANES[1].fetch_add(1, Ordering::Relaxed);
    }
    let Some(through) = &twin.through else {
        // SAFETY: an open list and two live textures.
        unsafe { list.CopyResource(to, from) };
        return;
    };
    // A stencil plane and its twin: each mip and slice of the plane goes
    // into the buffer, laid out as the host lays the plane out for copies,
    // and from the buffer into the other texture, whole subresources both
    // times. In the depth texture the stencil plane's subresources follow
    // the depth plane's; the twin has the one plane.
    let Some(host) = host() else { return };
    // SAFETY: a live texture.
    let of = unsafe { depth.GetDesc() };
    let planes = u32::from(of.MipLevels) * u32::from(of.DepthOrArraySize);
    let mut rows = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); planes as usize];
    // SAFETY: a plain query writing `planes` footprints.
    unsafe {
        host.device.GetCopyableFootprints(
            &of,
            planes,
            planes,
            0,
            Some(rows.as_mut_ptr()),
            None,
            None,
            None,
        );
    }
    let (first_from, first_to) = if colour { (planes, 0) } else { (0, planes) };
    let subresource = |texture: &ID3D12Resource, index: u32| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: index,
        },
    };
    let placed = |row: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(through) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: *row,
        },
    };
    let (common, source, dest) = (
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    // SAFETY: an open list and live resources; the buffer was made as big
    // as these footprints, and rests in the common state between copies.
    unsafe {
        list.ResourceBarrier(&[transition(through, common, dest)]);
        for (i, row) in rows.iter().enumerate() {
            let index = first_from + i as u32;
            list.CopyTextureRegion(&placed(row), 0, 0, 0, &subresource(from, index), None);
        }
        list.ResourceBarrier(&[transition(through, dest, source)]);
        for (i, row) in rows.iter().enumerate() {
            let index = first_to + i as u32;
            list.CopyTextureRegion(&subresource(to, index), 0, 0, 0, &placed(row), None);
        }
        list.ResourceBarrier(&[transition(through, source, common)]);
    }
}

/// Whether device-removed data is on (`VICTORY_D3D12_DRED`, diagnostics
/// builds).
pub(crate) fn dred_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_D3D12_DRED").is_some()
    })
}

/// Indirect mesh dispatches since the last frame line: the largest group
/// count a record asked for, and the groups asked in all (from the lists'
/// budget buffers, a run late).
static MESH_GROUPS: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Whether each list of a submission runs and is timed on its own, slow
/// ones logged with their commands (`VICTORY_GPU_TIMING`, diagnostics
/// builds; the commands are noted with `VICTORY_D3D12_DRED` on).
#[cfg(feature = "diagnostics")]
fn timing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GPU_TIMING").is_some())
}

/// Names a host object for the device-removed report, which names the
/// allocations a page fault hit (only with device-removed data on).
pub(crate) fn name(object: &impl Interface, text: impl FnOnce() -> String) {
    if dred_on()
        && let Ok(object) = object.cast::<ID3D12Object>()
    {
        // SAFETY: a plain call on a live object.
        let _ = unsafe { object.SetName(&windows::core::HSTRING::from(text())) };
    }
}

/// One in [`ALIVE`]`[0]` while it lives (a host list), and one in
/// [`LISTS`]`[3]` until it is first reset.
struct Counted {
    /// Not reset since it was made.
    fresh: std::sync::atomic::AtomicBool,
}

impl Counted {
    fn new() -> Counted {
        ALIVE[0].fetch_add(1, Ordering::Relaxed);
        LISTS[3].fetch_add(1, Ordering::Relaxed);
        Counted {
            fresh: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// The list is reset: it is in use.
    fn used(&self) {
        if self.fresh.swap(false, Ordering::Relaxed) {
            LISTS[3].fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        ALIVE[0].fetch_sub(1, Ordering::Relaxed);
        self.used();
    }
}

/// A chunk of upload memory for lists' scratch data: the buffer, its
/// mapping and its host GPU address.
struct Chunk {
    _buffer: ID3D12Resource,
    data: usize,
    gpu: u64,
}

// SAFETY: a D3D12 resource (free-threaded) and its persistent mapping,
// written only by the list that holds the chunk.
unsafe impl Send for Chunk {}

/// Upload chunks handed out front to back: the chunks (each with its
/// bytes), the one in use and the bytes used in it.
#[derive(Default)]
struct Scratch {
    chunks: Vec<(Chunk, u64)>,
    current: usize,
    used: u64,
}

// SAFETY: the chunk pointers are persistent mappings of upload buffers.
unsafe impl Send for Scratch {}

impl Drop for Scratch {
    fn drop(&mut self) {
        give_chunks(std::mem::take(&mut self.chunks));
    }
}

const SCRATCH_CHUNK: u64 = 4 << 20;
/// A list's first chunk when the chunks are shared ([`scratch_pool`]): most
/// lists put a few constants or a rectangle's corners there, and every
/// list that did took 4 MiB of the PC's memory for it.
const SCRATCH_FIRST: u64 = 256 << 10;
/// The most bytes of chunks kept while no list holds them.
const SPARE_MOST: u64 = 128 << 20;

/// Whether lists share their scratch chunks: a list takes a chunk when it
/// first needs one, a small one first, and gives all of them back when it
/// is reset or dropped, for the next list. Before, a list kept every chunk
/// it ever took (4 MiB each) for as long as it lived, used or not, and the
/// host lists of held submissions, which live for one submission, each
/// made theirs anew: some 57 upload buffers of 4 MiB made, zeroed and
/// freed per frame of gameplay (run 60).
/// `VICTORY_SCRATCH_POOL=0`: chunks of 4 MiB kept by their list, as before.
fn scratch_pool() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SCRATCH_POOL").is_none_or(|v| v != "0"))
}

/// Things of some sizes that nothing holds now, kept for whoever next asks
/// for one of the same size.
struct Spare<T> {
    kept: Vec<(T, u64)>,
    bytes: u64,
}

impl<T> Spare<T> {
    const fn new() -> Self {
        Spare { kept: Vec::new(), bytes: 0 }
    }

    /// A kept thing of `size` bytes, the one given last.
    fn take(&mut self, size: u64) -> Option<T> {
        let at = self.kept.iter().rposition(|(_, s)| *s == size)?;
        self.bytes -= size;
        Some(self.kept.remove(at).0)
    }

    /// Keeps `thing` (`size` bytes) unless that takes the kept bytes past
    /// `most`: then it comes back.
    fn give(&mut self, thing: T, size: u64, most: u64) -> Option<T> {
        if self.bytes + size > most {
            return Some(thing);
        }
        self.bytes += size;
        self.kept.push((thing, size));
        None
    }
}

/// Scratch chunks no list holds.
static SPARE: Mutex<Spare<Chunk>> = Mutex::new(Spare::new());

/// A scratch chunk of `size` bytes: one kept ([`SPARE`]) or a new one.
fn take_chunk(host: &Host, size: u64) -> Option<Chunk> {
    if scratch_pool()
        && let Some(chunk) = SPARE.lock().unwrap_or_else(|e| e.into_inner()).take(size)
    {
        ALIVE[3].fetch_sub(1, Ordering::Relaxed);
        ALIVE[4].fetch_sub(size, Ordering::Relaxed);
        ALIVE[5].fetch_add(1, Ordering::Relaxed);
        return Some(chunk);
    }
    let (buffer, data) = host.upload_buffer(size)?;
    name(&buffer, || "list scratch".into());
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    ALIVE[1].fetch_add(1, Ordering::Relaxed);
    ALIVE[2].fetch_add(size, Ordering::Relaxed);
    Some(Chunk {
        _buffer: buffer,
        data: data as usize,
        gpu,
    })
}

/// A list is done with `chunks` (it was reset after its submissions ran,
/// or dropped): kept for the next list, or freed past [`SPARE_MOST`] and
/// without the pool.
fn give_chunks(chunks: Vec<(Chunk, u64)>) {
    if chunks.is_empty() {
        return;
    }
    let mut freed = Vec::new();
    {
        let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
        for (chunk, size) in chunks {
            let back = if scratch_pool() {
                spare.give(chunk, size, SPARE_MOST)
            } else {
                Some(chunk)
            };
            match back {
                None => {
                    ALIVE[3].fetch_add(1, Ordering::Relaxed);
                    ALIVE[4].fetch_add(size, Ordering::Relaxed);
                }
                Some(chunk) => {
                    ALIVE[1].fetch_sub(1, Ordering::Relaxed);
                    ALIVE[2].fetch_sub(size, Ordering::Relaxed);
                    freed.push(chunk);
                }
            }
        }
    }
    // Freed outside the lock.
    drop(freed);
}

/// Whether the copies of title memory into its copies in the card's memory
/// go on a COPY queue of their own (`VICTORY_COPY_QUEUE=1`) instead of the
/// direct queue ahead of the submission that reads them (**off** by
/// default).
///
/// Those copies are one system-memory-to-card DMA of ~140 MB a frame (the
/// card line's "sent up"), and on the direct queue it sits between the
/// flights before a submission and the submission's own lists. A copy queue
/// can run it beside them, but only where nothing reads the copies: the
/// copies write the buffers the title's shaders read, so every list that
/// reads them waits for the copy fence, and the copy queue in turn waits for
/// the flights already sent, which read those buffers with the bytes from
/// before. What the split buys is one submission's DMA running while the
/// flights before it finish, and nothing more, which is why it is off: the
/// win it cannot give needs the copies double buffered, or the wait narrowed
/// to the flights that named the copies being uploaded (those numbers are
/// in `crate::shadow`'s flights, today only with the relay).
/// [`COPY_QUEUE_STATS`] is what a run of it reports, and its third figure is
/// whether anything overlapped at all.
fn copy_queue() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_COPY_QUEUE").is_ok_and(|v| v == "1"))
}

/// The run's copies into the card's copies on their own queue
/// ([`copy_queue`]): batches sent, their bytes, batches whose run had
/// finished by the time the next batch was ready (they ran beside the direct
/// queue), and batches still running then. Every batch that a later one
/// arrived behind lands in exactly the third or the fourth, so a window's
/// third and fourth add up to its first less the batches still in the air
/// when it ended.
static COPY_QUEUE_STATS: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// The direct queue's relay fetch in flight: its fence and the value that
/// fetch waits on the copy queue for. A fetch reads the copies the title's
/// shaders read ([`Host::relay_card_fetch`]) with no list of its own on the
/// copy queue, so the next batch of copies waits for the value the fetch's
/// wait ends at before it writes one of those buffers ([`Host::card_copies`]).
///
/// Written and cleared only under [`CopyLists`]' lock *and* [`QUEUE_ORDER`]
/// ([`Host::relay_card_fetch`]): a batch reads it under the same two, so a
/// batch that sees it is one the fetch's value is ordered against, and one
/// recorded after a failed fetch is cleared before it can see it.
static FETCH_BARRIER: Mutex<Option<(ID3D12Fence, u64)>> = Mutex::new(None);

/// What the copy queue and the direct queue know of each other: every batch
/// of copies is signalled on the copy fence ([`CopyOrder::sending`]), the
/// direct queue waits for that value before any list that reads a copy
/// ([`CopyOrder::wait`]), and the copy queue waits for the direct queue's
/// last send before it writes one ([`CopyOrder::direct_sent`]). One queue
/// needs none of this: the order of that queue says it.
#[derive(Default)]
struct CopyOrder {
    /// The last value a batch was numbered with, signalled or not: each batch
    /// takes one above it, so a value is used once however its signal went.
    /// Never a value to wait for (`last_signalled` is).
    sent: u64,
    /// The last value a batch's `Signal` actually carried: the newest value the
    /// copy fence can reach. Every wait on that fence names this or an older
    /// value. A number between the two is one no signal will carry and no
    /// completion will raise, so a wait for it never returns (a hang on the
    /// direct queue, and the timeout that follows); `sent` advanced past a
    /// failed signal is exactly how that used to be reached.
    last_signalled: u64,
    /// The last value the direct queue was told to wait for.
    waited: u64,
    /// The last value signalled on the direct queue's fence.
    direct: u64,
}

impl CopyOrder {
    /// A batch is going to the copy queue: the value it is signalled with.
    fn sending(&mut self) -> u64 {
        self.sent += 1;
        self.sent
    }

    /// The batch numbered `value` got its signal: that value is on the fence,
    /// and the completion of every batch before it is named by it.
    fn signalled(&mut self, value: u64) {
        self.last_signalled = self.last_signalled.max(value);
    }

    /// The value the direct queue must wait for before a list that reads a
    /// copy, if any batch was signalled whose completion it has not waited for.
    fn wait(&self) -> Option<u64> {
        (self.last_signalled > self.waited).then_some(self.last_signalled)
    }

    /// The direct queue waits for `value`: every batch sent before it is in
    /// the card's copies.
    fn waited(&mut self, value: u64) {
        self.waited = self.waited.max(value);
    }

    /// The direct queue signalled `value`.
    fn direct_sent(&mut self, value: u64) {
        self.direct = value;
    }
}

/// The state a copy destination is in when the direct queue's list reads it
/// after the copies into it: `COMMON` when they ran on the copy queue (a
/// resource accessed on a copy queue decays to `COMMON` when that
/// `ExecuteCommandLists` completes, and a copy queue may touch a resource
/// only in `COMMON` or a copy state), `COPY_DEST` when they are in the same
/// list, which promoted it and left it there.
fn copied_state(on_copy_queue: bool) -> D3D12_RESOURCE_STATES {
    if on_copy_queue {
        D3D12_RESOURCE_STATE_COMMON
    } else {
        D3D12_RESOURCE_STATE_COPY_DEST
    }
}

/// The copy queue ([`copy_queue`]), the fence a batch is signalled on, and
/// the event for the one wait a full pool makes on this thread.
struct CopyQueue {
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    event: HANDLE,
    lists: Mutex<CopyLists>,
}

/// The most batches the copy queue holds in the air at once: below it a batch
/// is recorded in a list of its own and the two queues overlap, at it the copy
/// queue is already behind and the next batch waits for the oldest — which is
/// the figure [`COPY_QUEUE_STATS`] reports as still running.
const COPY_LISTS: usize = 4;

/// Where the copy queue's next batch records ([`CopyLists::take`]): in a list
/// whose batch the card has finished, in a new one while fewer than
/// [`COPY_LISTS`] are in the air, or in the oldest batch's, waited for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListFrom {
    Idle,
    Fresh,
    Wait,
}

fn list_from(idle: usize, flying: usize) -> ListFrom {
    if idle > 0 {
        ListFrom::Idle
    } else if flying < COPY_LISTS {
        ListFrom::Fresh
    } else {
        ListFrom::Wait
    }
}

/// Which of [`COPY_QUEUE_STATS`]'s last two a batch counts in, from the value
/// of the batch before it and the fence when this one was ready: still
/// running (3) when the card had not reached it, finished (2) when it had.
fn overlap_slot(before: u64, done: u64) -> usize {
    2 + usize::from(done < before)
}

/// A window of [`COPY_QUEUE_STATS`]: the batches sent so far and, per frame of
/// the window, the megabytes the window sent and the batches of the window
/// that finished before the next batch was ready or were still running then.
/// The two batch counts are swapped, not read: a total divided by one window's
/// frames would report the whole run as if it happened in this window.
fn copy_queue_window(
    stats: &[std::sync::atomic::AtomicU64; 4],
    frames: f64,
) -> (u64, f64, f64, f64) {
    let batches = stats[0].load(Ordering::Relaxed);
    let mb = stats[1].swap(0, Ordering::Relaxed) as f64 / 1e6 / frames;
    let finished = stats[2].swap(0, Ordering::Relaxed) as f64 / frames;
    let running = stats[3].swap(0, Ordering::Relaxed) as f64 / frames;
    (batches, mb, finished, running)
}

/// Whether a batch has run, from the value its own signal carries (`None` when
/// that signal failed) and the first value signalled after it: one queue runs
/// its batches in the order they were sent, so a later value says an
/// unsignalled batch before it has run too, and a batch with no value anywhere
/// is one nothing can wait for.
fn batch_done(value: Option<u64>, after: Option<u64>, done: u64) -> bool {
    match value {
        Some(value) => value <= done,
        None => after.is_some_and(|after| after <= done),
    }
}

/// The value a relay fetch makes the direct queue wait for, from the copy
/// queue's order: the newest value the copy fence carries, and nothing when no
/// batch has been signalled at all (`Host::relay_card_fetch`). Never
/// [`CopyOrder::sent`]: the newest batch's signal may have failed, and a wait
/// on the direct queue for a value no signal carries never returns.
fn fetch_waits(order: &CopyOrder) -> Option<u64> {
    (order.last_signalled > 0).then_some(order.last_signalled)
}

/// The copy queue's lists, and the order the two queues keep
/// ([`CopyOrder`]). A list is recorded, sent, and given back only when the
/// batch it recorded has completed: an allocator cannot be reset while the
/// card runs it.
#[derive(Default)]
struct CopyLists {
    order: CopyOrder,
    /// Lists whose batch has completed: free to record again.
    idle: Vec<Prelude>,
    /// Batches sent and not completed: the value signalled after each (`None`
    /// for a batch whose signal failed, which no value in the fence ever
    /// names) and the list that recorded it.
    flying: std::collections::VecDeque<(Option<u64>, Prelude)>,
}

impl CopyLists {
    /// Gives back the lists of the batches the card has finished.
    fn free(&mut self, fence: &ID3D12Fence) {
        // SAFETY: a plain query of our fence.
        let done = unsafe { fence.GetCompletedValue() };
        while self.front_done(done) {
            if let Some((_, prelude)) = self.flying.pop_front() {
                self.idle.push(prelude);
            }
        }
    }

    /// Whether the oldest batch in the air has run ([`batch_done`]).
    fn front_done(&self, done: u64) -> bool {
        let Some((value, _)) = self.flying.front() else {
            return false;
        };
        // A batch whose signal failed is behind the first value after it on the
        // queue, which names its completion too.
        let after = value.is_none().then(|| self.after_front()).flatten();
        batch_done(*value, after, done)
    }

    /// The first value signalled after the oldest batch in the air.
    fn after_front(&self) -> Option<u64> {
        self.flying.iter().skip(1).find_map(|(value, _)| *value)
    }

    /// The value a wait must be for to know the oldest batch has run: `None`
    /// when every batch in the air has no value of its own, a queue no wait
    /// can be told about.
    fn wait_value(&self) -> Option<u64> {
        self.flying.iter().find_map(|(value, _)| *value)
    }

    /// A batch whose signal failed, drained here: the batch is already on the
    /// copy queue, so a value signalled after it names its completion, and this
    /// thread waits for that value on the fence before anything reads the
    /// copies it wrote. It is the caller's `last_signalled` too, so the direct
    /// queue's waits can name it.
    ///
    /// `None` when that signal fails as well: no wait can be ordered against
    /// the batch then, and the copies its list wrote are read by the
    /// submission's own list, unsynchronised. A fence that refuses two signals
    /// in a row is one the device has removed.
    fn drain(
        &mut self,
        queue: &ID3D12CommandQueue,
        fence: &ID3D12Fence,
        event: HANDLE,
    ) -> Option<u64> {
        let value = self.order.sending();
        // SAFETY: our copy queue, its fence, and the event of its one wait.
        let signalled = unsafe {
            (|| -> windows::core::Result<()> {
                fence.SetEventOnCompletion(value, event)?;
                queue.Signal(fence, value)?;
                WaitForSingleObject(event, INFINITE);
                Ok(())
            })()
        };
        match signalled {
            Ok(()) => {
                self.order.signalled(value);
                Some(value)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "no value to drain the copies into the card's memory with: a list of the \
                     direct queue copies them instead"
                );
                None
            }
        }
    }

    /// A list to record the next batch in: one whose batch completed, a new one
    /// while fewer than [`COPY_LISTS`] batches are in the air (they overlap the
    /// direct queue rather than being waited for), or the oldest batch's when
    /// the pool is full.
    fn take(&mut self, fence: &ID3D12Fence, event: HANDLE) -> Option<Prelude> {
        self.free(fence);
        match list_from(self.idle.len(), self.flying.len()) {
            ListFrom::Idle => self.idle.pop(),
            // A list of its own: `None` is the caller's `make_copy_prelude`.
            ListFrom::Fresh => None,
            ListFrom::Wait => {
                let value = self.wait_value()?;
                // SAFETY: our fence and event; the batch is on the card.
                unsafe {
                    fence.SetEventOnCompletion(value, event).ok()?;
                    WaitForSingleObject(event, INFINITE);
                }
                self.free(fence);
                self.idle.pop()
            }
        }
    }
}

/// The copy queue, made or not: a device that will not give a COPY queue
/// leaves the copies on the direct queue, as without the switch.
fn make_copy_queue(device: &ID3D12Device) -> windows::core::Result<CopyQueue> {
    // SAFETY: plain object creation; the event is ours.
    unsafe {
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_COPY,
            ..Default::default()
        })?;
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        let event = CreateEventW(None, false, false, None)?;
        Ok(CopyQueue {
            queue,
            fence,
            event,
            lists: Mutex::new(CopyLists::default()),
        })
    }
}

/// What the host keeps for copies in the card's own memory
/// ([`crate::shadow::cards`]): three lists of ours and the compare.
struct CardWork {
    /// Before a submission's lists: what changed on the copies' CPU side
    /// goes to their card side. Those not in use: a plan takes one and
    /// gives it back when its submission has run (`Host::card_done`), so
    /// that submissions sent ahead ([`late`]) each have their own.
    sync: Vec<Prelude>,
    /// After them, in the same submission: the compare of the declared
    /// ranges with their bytes from before. As `sync`.
    check: Vec<Prelude>,
    /// After it ran: the pieces that differ come down.
    fetch: Prelude,
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    /// One dword per piece compared, set where the piece differs: PC
    /// memory, mapped (buffer, mapping, bytes).
    differs: (ID3D12Resource, usize, u64),
    /// Where the pieces that differ are fetched to: PC memory, mapped.
    fetched: Option<(ID3D12Resource, usize, u64)>,
    /// The dwords of `differs` taken by plans whose results were not read
    /// yet.
    taken: u32,
    /// Plans made and not done with (`Host::card_done`).
    out: u32,
}

// SAFETY: D3D12 objects (free-threaded) and persistent mappings, used only
// under the submission lock.
unsafe impl Send for CardWork {}

static CARD_WORK: Mutex<Option<CardWork>> = Mutex::new(None);

/// The compare of a declared range of a copy in the card's memory with
/// its bytes from before, one thread per 64 KiB piece (cut at multiples of
/// 64 KiB of title memory, as `Shadow::card_piece` cuts them): `differs`
/// gets a dword per piece, 1 where a byte differs. The offsets are widened
/// to 16 bytes; buffers are multiples of 64 KiB, and a piece flagged for a
/// byte just outside the range costs a fetch that finds nothing.
pub(crate) const CARD_COMPARE: &str = r#"
#define RS "RootConstants(num32BitConstants=6, b0), SRV(t0), UAV(u0), UAV(u1)"
cbuffer Span : register(b0) {
    uint first;    // the first piece
    uint skew;     // where the copy starts in its piece 0
    uint size;     // the copy's buffer, in bytes (a multiple of 64 KiB)
    uint pieces;
    uint out_at;   // the first piece's dword in `differs`, in bytes
    uint compare;  // 0: the hashes are stored; else compared with those stored (2: and then stored)
};
ByteAddressBuffer now : register(t0);
// 16 bytes per piece of the copy: the hash of its bytes when they were
// last sent up (or hashed whole, the first time).
RWByteAddressBuffer hashes : register(u0);
RWByteAddressBuffer differs : register(u1);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= pieces) return;
    uint piece = first + id.x;
    uint lo = piece * 65536u;
    lo = lo > skew ? lo - skew : 0u;
    uint hi = min((piece + 1u) * 65536u - skew, size);
    lo = lo & ~15u;
    hi = (hi + 15u) & ~15u;
    uint4 h = uint4(0x9E3779B9u, 0x85EBCA6Bu, 0xC2B2AE35u, 0x27D4EB2Fu);
    [loop]
    for (uint at = lo; at < hi; at += 16u) {
        h = (h ^ now.Load4(at)) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
    }
    if (compare != 0u) {
        differs.Store(out_at + id.x * 4u, any(h != hashes.Load4(piece * 16u)) ? 1u : 0u);
        // With the copy-back on its own thread: the next compare of the
        // piece finds what is written from here on.
        if (compare == 2u) hashes.Store4(piece * 16u, h);
    } else {
        hashes.Store4(piece * 16u, h);
    }
}
"#;

/// Whether the pieces hashed again after ranges were sent up to a copy in
/// the card's memory are hashed in runs of pieces that touch, each piece
/// once (on by default; `VICTORY_CARD_REHASH_RUNS=0` makes a pass per range
/// sent up).
///
/// A pass walks its 64 KiB pieces 16 bytes at a time, 4,096 steps one after
/// the other, so it takes the time of one piece however few its pieces are:
/// run 20261125-055341 sent up ranges that made 246 passes over 1,118
/// pieces, and the ranges lie next to one another (they are views of the
/// same title heap). Joined, the same pieces are hashed, plus the pieces
/// between ranges that touch: those are hashed to what the card holds now,
/// and the compare that reads them runs in queue order before this pass, so
/// a write it must flag is flagged first. A piece hashed when it need not be
/// costs a fetch that finds nothing, and loses no shader write.
fn rehash_runs() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CARD_REHASH_RUNS").is_none_or(|v| v != "0"))
}

/// Since the last frame-rate line, of our own passes over copies in the
/// card's memory: hash passes after ranges were sent up, the pieces they
/// hashed, and compare passes after the lists; then the ranges and pieces a
/// pass would have taken without [`rehash_runs`] joining them.
static CARD_PASSES: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// Whether what shaders wrote to copies in the card's memory is merged
/// into title memory on several threads (`VICTORY_WIDE_MERGE=1`; off by
/// default). The pieces the card's compare found differing come down in
/// one fetch and were then merged one after the other on the thread that
/// settles: about 630 pieces and 40 MB a frame (run 104), each compared
/// with the copy's CPU side byte by byte and put into title memory through
/// the kernel. A piece is its own bytes of its copy and of title memory,
/// so eight threads take an eighth of them each.
fn wide_merge() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WIDE_MERGE").is_some_and(|v| v == "1"))
}

/// One declared range of a copy in the card's memory, in a submission.
struct CardRange {
    shadow: std::sync::Arc<crate::shadow::Shadow>,
    /// As declared (title addresses), for the statistics.
    declared: (usize, usize),
    span: crate::shadow::CardSpan,
    /// Its first piece's dword in the compare's results.
    at: u32,
    /// Whether the card compares it; else every piece is fetched.
    compared: bool,
}

/// What a submission does for copies in the card's memory.
struct CardPlan {
    ranges: Vec<CardRange>,
    /// The compare, run after the submission's lists.
    check: Option<ID3D12CommandList>,
    /// The copies whose card side was brought up to date, alive until the
    /// submission has run.
    _synced: Vec<std::sync::Arc<crate::shadow::Shadow>>,
    /// The lists of ours the plan recorded (whether the compare, the
    /// list), given back by `Host::card_done`.
    lists: Vec<(bool, Prelude)>,
}

/// One piece of submission-time work, in recorded order.
enum Step {
    /// A copy into title memory (destination, source, bytes): the title's
    /// buffer copies, which later refreshes must see.
    Copy(usize, usize, usize),
    /// A copy of title memory into the list's own upload memory.
    Fill(usize, usize, usize),
    /// Brings a range (address, bytes) of a buffer's host copy up to date:
    /// once per submission, whichever lists ask, until a `Copy` or `Run`
    /// intervenes.
    Refresh(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Rectangles a draw expands from a title vertex stream into the list's
    /// scratch memory (`crate::primitive::expand_rects`).
    Rectangles(Rectangles),
    /// Work that may write title memory.
    Run(Box<dyn Fn() + Send>),
    /// Work that writes host memory only.
    Host(Box<dyn Fn() + Send>),
    /// Shaders write a range (address, bytes) of a buffer's host copy.
    GpuWrite(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Brings a texture's staging buffer up to date: once per submission,
    /// whichever lists sample it, until a `Copy` or `Run` intervenes.
    Texture(std::sync::Arc<crate::texture::Staging>),
}

/// Rectangles a draw expands into a list's scratch memory when the list is
/// submitted, held as data: one boxed closure per vertex stream per
/// rectangle draw was a heap allocation and an indirect call each.
pub(crate) struct Rectangles {
    /// Title memory the vertices are read from, checked when recorded.
    from: usize,
    /// The list's scratch memory, `6` vertices for every `3` read.
    to: usize,
    stride: usize,
    /// Rectangles.
    count: usize,
    /// The stream's elements as the input layout packs them.
    elements: [(u32, u32); Rectangles::CAP],
    named: usize,
}

impl Rectangles {
    /// Elements one stream may name and stay a `Rectangles`: a longer one
    /// keeps the boxed closure of [`HostList::defer_host`].
    pub(crate) const CAP: usize = 8;

    pub(crate) fn new(
        from: usize,
        to: usize,
        stride: usize,
        count: usize,
        elements: &[(u32, u32)],
    ) -> Option<Rectangles> {
        if elements.len() > Self::CAP {
            return None;
        }
        let mut held = [(0, 0); Self::CAP];
        held[..elements.len()].copy_from_slice(elements);
        Some(Rectangles {
            from,
            to,
            stride,
            count,
            elements: held,
            named: elements.len(),
        })
    }

    /// Expands them, as [`crate::primitive::expand_rects`] does.
    fn run(&self) {
        expand_rects_now(
            self.from,
            self.to,
            self.stride,
            self.count,
            &self.elements[..self.named],
        );
    }

    /// The title memory it reads: where, how many bytes.
    fn source(&self) -> (usize, usize) {
        (self.from, self.count * 3 * self.stride)
    }
}

/// Rectangles read from title memory `from` and written expanded to `to`
/// when the list is submitted: the body [`Rectangles`] runs, and what the
/// boxed closure of a stream naming more elements than [`Rectangles::CAP`]
/// runs.
pub(crate) fn expand_rects_now(
    from: usize,
    to: usize,
    stride: usize,
    count: usize,
    elements: &[(u32, u32)],
) {
    // The vertices are read in place, at submission: no change of mapping
    // under the read (see `crate::watch::title_memory_reading`).
    let _mapped = crate::watch::title_memory_reading();
    // SAFETY: the vertices lie in a title buffer (checked when recorded)
    // and the scratch memory is the list's.
    let (src, dst) = unsafe {
        (
            std::slice::from_raw_parts(from as *const u8, count * 3 * stride),
            std::slice::from_raw_parts_mut(to as *mut u8, count * 6 * stride),
        )
    };
    crate::primitive::expand_rects(src, dst, stride, elements);
}

/// What a list does on the CPU when it is submitted: copies between host
/// memory regions (title buffers, upload buffers and buffer copies), and
/// the upload buffers its GPU commands read, kept until the list is reset.
#[derive(Default)]
struct Deferred {
    /// Destination, source, bytes.
    copies: Vec<Step>,
    uploads: Vec<ID3D12Resource>,
    /// The bytes of `uploads` (counted in [`ALIVE`] while they are held).
    upload_bytes: u64,
    /// Host objects the list's commands name (pipelines, root signatures,
    /// descriptor heaps): the title may release its objects before it
    /// submits the list.
    kept: Vec<windows::core::IUnknown>,
    /// Buffer ranges refreshed since the last title copy (host copy,
    /// address, bytes): refreshing them again would find the same bytes.
    refreshed: std::collections::HashSet<(usize, usize, usize)>,
    /// Textures whose contents the list brings up to date.
    textures: std::collections::HashSet<usize>,
    /// Bottom-of-pipe writes, done once the list has run: address, what,
    /// bytes.
    writes: Vec<(usize, crate::queue::Written, usize)>,
    /// Textures read back (texture to buffer copies), done once the list
    /// has run: the host buffer the list copies the texels into, the title
    /// memory they go to, the bytes of a row, the rows, the title's row
    /// pitch.
    backs: Vec<(ID3D12Resource, usize, [usize; 3])>,
    /// Whether a step zeroes the indirect-work budget.
    budget_zeroed: bool,
}

impl Deferred {
    /// Holds `buffer` (`size` bytes of upload memory) until the list is
    /// reset.
    fn hold_upload(&mut self, buffer: ID3D12Resource, size: u64) {
        ALIVE[6].fetch_add(1, Ordering::Relaxed);
        ALIVE[7].fetch_add(size, Ordering::Relaxed);
        self.upload_bytes += size;
        self.uploads.push(buffer);
    }
}

impl Drop for Deferred {
    fn drop(&mut self) {
        ALIVE[6].fetch_sub(self.uploads.len() as u64, Ordering::Relaxed);
        ALIVE[7].fetch_sub(self.upload_bytes, Ordering::Relaxed);
    }
}

/// Whether a queue's submissions go to the card without a wait for each
/// (`VICTORY_LATE_WAIT=1`; off by default, and off in the timing run, which
/// waits for every list on its own).
///
/// Every submission was prepared, sent, waited for and copied back before
/// the next one was looked at, so the card stood idle while we prepared
/// and we stood idle while it ran: in run 86 a frame of 446 ms is 134 ms of
/// preparing, 194 ms of waiting and 78 ms of copying back, one after the
/// other, on the one thread that submits.
///
/// With the switch a queue's submission is sent and joins a line of
/// "flights" ([`Flight`]); the thread goes on to the next one while the
/// card runs. The flights are settled together ([`Host::settle`]: one wait
/// for the card, what their shaders wrote copied back once per byte, then
/// their bottom-of-pipe writes and the fence signals behind them, in
/// order) at the next point where the title, or our own preparing, can
/// observe what they do. Those points, exactly:
///
/// 1. a synchronous submission of ours ([`Host::execute`]): a present, a
///    read-back or map of a texture, a clear or upload run at once;
/// 2. the next submission reads title memory that a flight's shaders may
///    write: a title buffer copy, a fill of upload memory, a texture's
///    memory, any step that runs code of ours on title memory
///    ([`late_blocked`]);
/// 3. the next submission refreshes a range of a host copy that a flight's
///    shaders may write, and the title wrote a page of it since it was last
///    copied ([`stale_under_flights`]): the refresh leaves such a range
///    alone until the flight's writes are back, as it always did within a
///    submission;
/// 4. a queue stands at a wait for a memory value that no flight's
///    bottom-of-pipe write is known to satisfy, at an address a flight may
///    write ([`late_met`]);
/// 5. the title blocks on a fence without an event;
/// 6. the card has run every flight (a thread of ours waits for that:
///    [`late_helper`]), or the flights hold [`late_most`] host lists.
///
/// What the title is told (bottom-of-pipe writes, fence values) is told
/// when the card has really run the work, later than before and never
/// earlier; a fence signalled on a queue while flights are in the air is
/// set behind them. A wait for a memory value that a flight's
/// bottom-of-pipe write satisfies counts as met: the waiting part goes to
/// the card behind that flight, which is the order the wait asks for.
///
/// What it assumes beyond today: memory the title rewrites between two of
/// its submissions, without waiting for the first, may be seen by the
/// first's shaders (as on the console, where the card reads title memory
/// when it runs, not when the list is submitted).
pub(crate) fn late() -> bool {
    late_stage() > 0
}

/// `VICTORY_LATE_WAIT`: 0 (off), 1, or 2 ([`late_exact`]).
fn late_stage() -> u8 {
    static STAGE: OnceLock<u8> = OnceLock::new();
    *STAGE.get_or_init(|| {
        if std::env::var_os("VICTORY_GPU_TIMING").is_some() {
            return 0;
        }
        match std::env::var("VICTORY_LATE_WAIT").as_deref() {
            Ok("1") => 1,
            Ok("2") => 2,
            Ok("3") => 3,
            // The relay alone: on top of stage 3.
            Err(_) if crate::shadow::relay() => 3,
            _ => 0,
        }
    })
}

/// Stage 2 of [`late`] (`VICTORY_LATE_WAIT=2`): point 3 of its list is no
/// longer a point. Where the title wrote a page that lies in a range a
/// flight's shaders may write, the bytes the title changed are found (the
/// page as it is now against what the copy held when the flight was
/// prepared) and put into the copy, and only those: what the flight's
/// shaders write beside them stays (`Shadow::merge_under_writes`). The
/// flights are settled for it only where that cannot be done: memory that
/// is not write-watched, or a copy in the card's memory with more changed
/// runs than a submission should send up one by one.
///
/// In run 92 (stage 1) this point settled the flights five times a frame
/// of twelve, each time with the card's work still to wait for: 172 ms a
/// frame of waiting were left of 200.
///
/// What differs from settling first: a byte both the title and a flight's
/// shader wrote is the title's if the shader wrote it first, the shader's
/// if it wrote it afterwards; settled first it was always the shader's.
/// The title writing a byte a shader of its own is writing is a race on
/// the console too.
fn late_exact() -> bool {
    late_stage() > 1
}

/// Stage 3 of [`late`] (`VICTORY_LATE_WAIT=3`: stage 2, and the present is
/// no longer a point where flights are settled). The copy to the back
/// buffer is sent as a flight of its own (`Host::execute_own`, a list of
/// its own for each present in the air) and the swap chain is told to
/// present behind it; the queue's call returns, and the card finishes the
/// frame while the title records the next one. What the title is told
/// about the frame (bottom-of-pipe writes, fences) is told when the card
/// has run it, by the settling thread.
///
/// What is left out with it: the look whether the presented frame is black
/// (diagnostics; it reads the frame back at once). A frame that is saved
/// or listed is presented the old way. Once every second and a half a
/// submission settles the flights all the same, for the sweep that lets
/// idle host objects go (it needs nothing in the air).
pub(crate) fn late_free_present() -> bool {
    late_stage() > 2
}

/// The relay's first stage (`VICTORY_RELAY=1`; off by default; it turns
/// [`late`] on at stage 3 unless `VICTORY_LATE_WAIT` says another stage,
/// and is off with it, in the timing run and with kept snapshots).
///
/// With [`late`] alone every settle does three things on the thread that
/// holds the submission lock: it waits for everything in the air, copies
/// back with the card standing, and only then goes on preparing. The frame
/// stays a relay: the card works about 242 ms of it and one thread
/// prepares, waits and copies back for about 220.
///
/// With the switch a thread of its own ([`relay_worker`]) lands flights as
/// soon as the card has run them, in the order they were sent and without
/// the submission lock: it fetches and merges what their shaders wrote to
/// copies in the card's memory, compares and merges the other copies,
/// frees the flights' ranges, and then does their read-backs, their
/// bottom-of-pipe writes and the fence signals behind them, in the line's
/// order. The submitting thread meanwhile prepares and sends the next
/// submissions. [`Host::settle`] no longer settles anything: it waits until
/// that thread has landed everything in the air, at the same points as
/// before (the list in [`late`]), so whoever can observe title memory, a
/// fence or a present still does so only after the flights before it are
/// merged.
///
/// What makes that safe is in `crate::shadow::relay` (ranges counted per
/// flight; a merge that never stores into memory the card may be writing;
/// sends to card copies numbered) and here: flights land in the order of
/// [`LINE`] by one thread, so a later flight's results never reach title
/// memory before an earlier flight's; the compare on the card stores the
/// hash it made, so each flight's compare flags exactly what changed since
/// the compare before it, however late the pieces are fetched; the fetch
/// goes to the queue under [`QUEUE_ORDER`], which says which sends of
/// title bytes the card had received when it ran.
pub(crate) fn relay() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| late_stage() > 0 && crate::shadow::relay())
}

/// The relay's second stage (`VICTORY_RELAY=2`): a submission that reads
/// title memory a flight's shaders may write (a title buffer copy, a fill
/// of upload memory, a texture) waits only until the flights that declared
/// that memory have landed, not for everything in the air; and at
/// [`late_most`] lists in the air it waits until there are fewer. At `1`
/// both wait for everything.
fn relay_exact() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    // Stage 2 unless `VICTORY_RELAY=1` asks for the first one (the default
    // since the walls of the first room were found drawn in every run with
    // this set and in fewer than half without it).
    *ON.get_or_init(|| relay() && std::env::var_os("VICTORY_RELAY").is_none_or(|v| v == "2"))
}

/// Told when something joins [`LINE`], for the thread that lands flights
/// ([`relay`]); [`LINE_MOVED`] stays the word for the thread that looks at
/// what queues hold.
static RELAY_MOVED: std::sync::Condvar = std::sync::Condvar::new();

/// Told when flights have landed ([`relay`]): who waits for that looks
/// again.
static LINE_LANDED: std::sync::Condvar = std::sync::Condvar::new();

/// With [`relay`], the order of what we put on the queue: the number of
/// sends of changed ranges to copies in the card's memory so far
/// (`Host::card_before`). Held from the taking of the ranges until their
/// list is on the queue, and while a fetch of pieces goes to the queue: a
/// fetch sent with the count at N ran behind sends 1..N and ahead of every
/// later one.
///
/// **Lock order: this one first, [`CopyLists`] second.** Both sites that take
/// both take this one first (`Host::card_before`, which then reaches
/// `Host::card_copies`, and `Host::relay_card_fetch`), and neither takes this
/// one while the lists' is held. That is not tidiness: a batch's number and
/// the [`FETCH_BARRIER`] it reads are one step only because no batch can be
/// numbered while a fetch holds both (`Host::relay_card_fetch` says what the
/// inversion deadlocks).
static QUEUE_ORDER: Mutex<u64> = Mutex::new(0);

/// Since the last frame-rate line, of the thread that lands flights
/// ([`relay`]): landings, flights landed, microseconds it waited for the
/// card with nothing to land, microseconds it waited for fetches of card
/// copies' pieces, microseconds of landing in all, and of the fetches'
/// waiting the part the card spent running work sent before the fetch.
static RELAY_STATS: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];

/// Of the line's things in order (a flight: the value signalled on the
/// host's fence behind it; a write or a signal behind a flight: `None`):
/// how many, from the front, can be landed when the card has reached
/// `completed`: everything ahead of the first flight it has not run. A
/// later flight never lands before an earlier one, and what follows a
/// flight (its bottom-of-pipe writes, signals) never before the flight.
fn landable(line: &[Option<u64>], completed: u64) -> usize {
    line.iter().position(|f| f.is_some_and(|f| f > completed)).unwrap_or(line.len())
}

/// For one plan of copies in the card's memory, with [`relay`]: `flags`
/// holds, for every piece of every range of the plan, the copy, the
/// piece's number in the copy and whether the compare flagged it there (or
/// did not run). The pieces to fetch, under whichever range: all of them
/// flagged under any. Two ranges of a plan may hold parts of the same
/// 64 KiB piece; the compare hashes the whole piece for each and stores
/// the hash, so the second of them may find it stored already and not
/// flag it.
fn flagged_pieces(flags: &[(usize, u32, bool)]) -> std::collections::HashSet<(usize, u32)> {
    flags.iter().filter(|f| f.2).map(|f| (f.0, f.1)).collect()
}

#[cfg(test)]
mod relay_tests {
    use super::{flagged_pieces, landable};

    /// Two flights and what follows each: nothing of the second lands
    /// before the card has run it, and nothing behind a flight lands
    /// before the flight.
    #[test]
    fn flights_land_in_the_order_they_were_sent() {
        // Flight (fence 5), its write, a signal, flight (fence 6), its write.
        let line = [Some(5), None, None, Some(6), None];
        assert_eq!(landable(&line, 4), 0);
        assert_eq!(landable(&line, 5), 3);
        assert_eq!(landable(&line, 6), 5);
        assert_eq!(landable(&line, u64::MAX), 5);
        // Signals with no flight ahead land at once; behind one they wait.
        assert_eq!(landable(&[None, None], 0), 2);
        assert_eq!(landable(&[None, Some(9), None], 8), 1);
        assert_eq!(landable(&[], 3), 0);
        // After the first three landed, the rest when its turn comes.
        assert_eq!(landable(&line[3..], 5), 0);
        assert_eq!(landable(&line[3..], 6), 2);
    }

    /// A piece flagged under one range of a plan is fetched under every
    /// range that holds a part of it; pieces of other copies are not.
    #[test]
    fn a_piece_flagged_under_one_range_is_fetched_under_all() {
        let (a, b) = (0x1000usize, 0x2000usize);
        // Copy a: piece 3 under two ranges, flagged under the first only;
        // piece 4 not flagged; copy b: piece 3 not flagged, piece 9 flagged.
        let flags = [(a, 3, true), (a, 4, false), (a, 3, false), (b, 3, false), (b, 9, true)];
        let fetch = flagged_pieces(&flags);
        assert!(fetch.contains(&(a, 3)));
        assert!(!fetch.contains(&(a, 4)));
        assert!(!fetch.contains(&(b, 3)));
        assert!(fetch.contains(&(b, 9)));
        assert_eq!(fetch.len(), 2);
    }
}

/// The most host lists in the air before the flights are settled
/// (`VICTORY_LATE_WAIT_LISTS`, 256; `=128` is the value before: with the
/// relay it stopped the submitting thread twice a frame for 92-103 ms in
/// all while the card stood idle 64-113 ms a frame, runs 158, 159 and 162;
/// 32 settled the flights three times a
/// frame in run 92 and 64 still 1.3-1.7 times in run 99, with 12,919 tables
/// asked and none refused; watch "tables asked, N refused"): their dynamic
/// descriptor slots, upload
/// memory and scratch chunks are held until then, and a frame's lists held
/// all at once ran the slots out before (run 45).
fn late_most() -> usize {
    static MOST: OnceLock<usize> = OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("VICTORY_LATE_WAIT_LISTS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(256).max(1)
    })
}

/// Whether a submission with bottom-of-pipe writes may fly
/// (`VICTORY_LATE_WAIT_WRITES=0`: it is settled as soon as it is sent, so
/// the title is told at once, as before; for telling a title that polls
/// such a value from one that does not).
fn late_writes() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LATE_WAIT_WRITES").is_none_or(|v| v != "0"))
}

/// Per host copy, the ranges a submission's shaders may write, as declared
/// (address, end).
type Declared = (std::sync::Arc<crate::shadow::Shadow>, Vec<(usize, usize)>);

/// What [`Host::submit`] hands back for a submission it sent and did not
/// wait for.
struct Sent {
    /// Its number among the submissions started.
    number: u64,
    card: Option<CardPlan>,
    back: Vec<Declared>,
    /// Its bottom-of-pipe writes.
    writes: Vec<(usize, crate::queue::Written, usize)>,
}

/// A submission on the card that has not been waited for ([`late`]): its
/// host lists, taken from the title's (which it may reset meanwhile), and
/// what is to be done once it has run.
struct Flight {
    number: u64,
    /// The value signalled on the host's fence behind it.
    fence: u64,
    lists: Vec<crate::list::Detached>,
    order: Vec<usize>,
    card: Option<CardPlan>,
    back: Vec<Declared>,
}

/// One thing in the line of what waits for the card, in order.
enum Late {
    Run(Flight),
    /// A bottom-of-pipe write of the flight before it: address, what, bytes.
    Write(usize, crate::queue::Written, usize),
    /// A fence (its address) signalled behind the flights before it.
    Signal(usize, u64),
}

/// The title memory the flights in the air, and the flights whose merge is
/// held back ([`lazy_merge`]), may write, per declaration: each range with
/// the submissions that declared it and have not merged it yet.
///
/// A range two flights declared stays held after the first of them merges
/// (`merged` is per submission): it is closed to refreshes until the last
/// does, as `shadow::Flights` keeps it. A held range is what [`blocked_by`]
/// tests, and what a settle waits to be gone.
#[derive(Default)]
struct Held {
    /// Each declared range, with the submissions that declared it and have
    /// not merged it.
    ranges: Vec<((usize, usize), Vec<u64>)>,
    /// The union of `ranges`, in order and joined: what is tested.
    joined: Vec<(usize, usize)>,
}

impl Held {
    const fn new() -> Held {
        Held { ranges: Vec::new(), joined: Vec::new() }
    }

    /// Submission `number` declares `ranges` (each once): each is held for
    /// it, however many times the submission declares it.
    fn declare(&mut self, ranges: impl IntoIterator<Item = (usize, usize)>, number: u64) {
        let mut any = false;
        for range in ranges {
            if range.0 >= range.1 {
                continue;
            }
            match self.ranges.iter_mut().find(|(r, _)| *r == range) {
                Some((_, held)) => {
                    if !held.contains(&number) {
                        held.push(number);
                    }
                }
                None => self.ranges.push((range, vec![number])),
            }
            any = true;
        }
        if any {
            self.rejoin();
        }
    }

    /// Submission `number`'s shaders' writes are in title memory: the
    /// ranges no other submission still holds are free again, and the ones
    /// it still holds are not.
    fn merged(&mut self, number: u64) {
        if !self.ranges.iter().any(|(_, held)| held.contains(&number)) {
            return;
        }
        self.ranges.retain_mut(|(_, held)| {
            held.retain(|&n| n != number);
            !held.is_empty()
        });
        self.rejoin();
    }

    /// Whether a range that no submission merged yet touches `[from, to)`.
    fn touches(&self, from: usize, to: usize) -> bool {
        touches(&self.joined, from, to)
    }

    /// The held ranges, in order and joined: [`blocked_by`]'s argument.
    fn joined(&self) -> &[(usize, usize)] {
        &self.joined
    }

    fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    fn clear(&mut self) {
        self.ranges.clear();
        self.joined.clear();
    }

    fn rejoin(&mut self) {
        let all: Vec<(usize, usize)> = self.ranges.iter().map(|(range, _)| *range).collect();
        self.joined.clear();
        join_ranges(&mut self.joined, all);
    }
}

/// A flight whose shaders' writes are in copies in the PC's memory and not
/// in title memory yet, and that told the title nothing when it landed
/// ([`lazy_merge`]): merged at the first point that reads those bytes, at
/// the next landing, or at the next settle, whichever comes first.
struct HeldBack {
    /// Its number among the submissions started (what [`Held::merged`] and
    /// `Shadow::copy_back_done` end).
    number: u64,
    /// The ranges its shaders may write, per copy, as declared.
    back: Vec<Declared>,
}

/// Whether what a flight's shaders wrote in a copy in the PC's memory is
/// merged at the first point that reads it rather than when it lands
/// (`VICTORY_LAZY_MERGE=1`; off by default).
///
/// A flight that tells the title nothing when it lands (no bottom-of-pipe
/// write, no fence signal behind it) leaves bytes in title memory that only
/// a later submission's preparation, a present, a fence or a refresh can
/// observe ([`late`]'s points). Until one of them does, the merge is work
/// on the landing thread that no one is waiting for, and the landing is
/// what the submitting thread waits for ([`relay_wait`]): holding it back
/// lands the flight sooner and lets the merge run beside the card, or in
/// the next landing with the flights after it.
///
/// Off unless the card copies' fetch runs on a thread of its own
/// ([`relay`]: without it the merging thread is the submitting one and
/// holding a merge back only moves it), and never together with a pinned
/// `VICTORY_LATE_WAIT`, whose runs this must not change.
fn lazy_merge() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("VICTORY_LAZY_MERGE").is_some_and(|v| v != "0")
            && relay()
            && std::env::var_os("VICTORY_LATE_WAIT").is_none()
    })
}

/// Whether a landing holds a flight's merge back ([`lazy_merge`]): it wrote
/// something in a copy in the PC's memory, and the items after it in this
/// landing tell the title nothing (`Late::Write`, `Late::Signal`), which
/// would put its bytes in the title's hands unwritten.
fn holds_back(wrote: bool, after: &[Late]) -> bool {
    wrote && !after.iter().any(|item| matches!(item, Late::Write(..) | Late::Signal(..)))
}

struct Line {
    line: Vec<Late>,
    /// The title memory the flights' shaders may write, and the flights
    /// that have not merged theirs yet (`Held`).
    held: Held,
    /// With [`lazy_merge`]: the flights that landed and told nobody, whose
    /// shaders' writes title memory does not hold yet.
    deferred: Vec<HeldBack>,
    /// Threads waiting in [`Host::settle`]: a landing holds nothing back
    /// while one is, a settle being one of the points the title must see
    /// title memory at.
    settles: usize,
    /// Host lists in the flights.
    lists: usize,
    /// The value signalled on the host's fence behind the last flight.
    target: u64,
    /// A settle has taken the line and is not done: a signal still goes
    /// behind it.
    settling: bool,
    /// Merges held back ([`HeldBack`]) that a thread has taken out of
    /// `deferred` and is merging: the line is empty of them while their bytes
    /// are not in title memory yet, so a settle waits for this and not only
    /// for `deferred` ([`settled`]).
    draining: usize,
    /// Something was settled away from a queue's own calls: the helper
    /// thread looks at what queues hold.
    poke: bool,
    /// Host lists of settled flights, reset and given back outside the
    /// submission lock ([`land`]: a settle may run under a caller's own
    /// locks).
    landed: Vec<(Vec<crate::list::Detached>, Vec<usize>)>,
    /// With [`relay`]: the bottom-of-pipe writes of the flights being
    /// landed (taken out of `line`, not done yet), for [`late_met`].
    in_hand: Vec<(usize, crate::queue::Written, usize)>,
}

// SAFETY: D3D12 objects (free-threaded) and host lists nothing records
// into; touched under the lock.
unsafe impl Send for Line {}

static LINE: Mutex<Line> = Mutex::new(Line {
    line: Vec::new(),
    held: Held::new(),
    deferred: Vec::new(),
    settles: 0,
    lists: 0,
    target: 0,
    settling: false,
    draining: 0,
    poke: false,
    landed: Vec::new(),
    in_hand: Vec::new(),
});
/// Told when a flight joins [`LINE`] or it is poked.
static LINE_MOVED: std::sync::Condvar = std::sync::Condvar::new();

fn line() -> std::sync::MutexGuard<'static, Line> {
    LINE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Host lists in the flights in the air ([`late`]): the runs of dynamic
/// descriptor slots a refusal cannot get are held by them until they have
/// run (`descriptor::host` names the count when it refuses a table).
pub(crate) fn lists_in_air() -> usize {
    if !late() {
        return 0;
    }
    line().lists
}

/// Why flights were settled ([`LATE_STATS`]' places from 2 on).
const WHY_SYNC: usize = 2;
const WHY_CARD_DONE: usize = 3;
const WHY_VALUE: usize = 4;
const WHY_STALE: usize = 5;
const WHY_READ: usize = 6;
const WHY_RUN: usize = 7;
const WHY_MOST: usize = 8;
const WHY_WRITES: usize = 9;
const WHY_FENCE: usize = 10;
/// A descriptor table the ring had no room for
/// (`descriptor::host::take_slots`): the flight in the air holds the slots,
/// and it is worth a wait for the card.
pub(crate) const WHY_DYNAMIC: usize = 18;

/// With [`lazy_merge`]: flights a landing did not merge, by index in
/// [`LATE_STATS`].
const DEFERRED: usize = 19;
/// Since the last frame-rate line: submissions sent without a wait;
/// settles; settles by reason ([`WHY_SYNC`]..); flights settled;
/// microseconds settles waited for the card; value waits met by a flight's
/// bottom-of-pipe write; fence signals set behind flights.
/// Then: the settles for title memory read, by what read it (a buffer
/// copy, a fill of upload memory, a texture), and the merges held back
/// ([`DEFERRED`]).
static LATE_STATS: [std::sync::atomic::AtomicU64; 20] = [const { std::sync::atomic::AtomicU64::new(0) }; 20];

/// Since the last frame-rate line: the microseconds settles waited for the
/// card, by why they settled (the `WHY_` places).
static LATE_WAITED: [std::sync::atomic::AtomicU64; 19] = [const { std::sync::atomic::AtomicU64::new(0) }; 19];

/// Whether `[from, to)` touches one of `ranges` (in order, joined).
fn touches(ranges: &[(usize, usize)], from: usize, to: usize) -> bool {
    if from >= to {
        return false;
    }
    let at = ranges.partition_point(|r| r.1 <= from);
    ranges.get(at).is_some_and(|r| r.0 < to)
}

/// `ranges` with `more`, in order, joined where they overlap or touch.
fn join_ranges(ranges: &mut Vec<(usize, usize)>, more: impl IntoIterator<Item = (usize, usize)>) {
    ranges.extend(more.into_iter().filter(|r| r.0 < r.1));
    ranges.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for &(from, to) in ranges.iter() {
        match out.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    *ranges = out;
}

/// Whether anything is in the air.
fn flying() -> bool {
    if !late() {
        return false;
    }
    let line = line();
    // (With the relay: also while flights are being landed.)
    line.settling || line.line.iter().any(|l| matches!(l, Late::Run(_)))
}

/// With flights in the air ([`late`]): why `runs` cannot be prepared
/// before they are settled, if so. Preparing reads title memory on the
/// CPU (the title's buffer copies, fills of upload memory, textures) and
/// runs steps of ours that may read or write it anywhere; what the
/// flights' shaders write is not there yet.
fn late_blocked(runs: &[&HostList]) -> Option<usize> {
    let held = {
        let line = line();
        if line.held.is_empty() {
            return None;
        }
        line.held.joined().to_vec()
    };
    blocked_by(&held, runs, true)
}

/// [`late_blocked`] against `held`, the title memory flights' shaders may
/// write (in order, joined); `count` whether the kind of read is
/// counted for the frame line.
fn blocked_by(held: &[(usize, usize)], runs: &[&HostList], count: bool) -> Option<usize> {
    if held.is_empty() {
        return None;
    }
    for list in runs {
        for step in &list.deferred().copies {
            match step {
                &Step::Copy(dst, src, len) => {
                    if touches(held, src, src + len) || touches(held, dst, dst + len) {
                        if count {
                            LATE_STATS[15].fetch_add(1, Ordering::Relaxed);
                        }
                        return Some(WHY_READ);
                    }
                }
                &Step::Fill(_, src, len) => {
                    if touches(held, src, src + len) {
                        if count {
                            LATE_STATS[16].fetch_add(1, Ordering::Relaxed);
                        }
                        return Some(WHY_READ);
                    }
                }
                Step::Texture(staging) => {
                    let (at, len) = staging.memory();
                    if touches(held, at, at + len) {
                        if count {
                            LATE_STATS[17].fetch_add(1, Ordering::Relaxed);
                        }
                        return Some(WHY_READ);
                    }
                }
                Step::Run(_) => return Some(WHY_RUN),
                // Rectangles are read out of title memory at submission, as
                // a fill is (the closure this replaced was not told apart
                // from work that writes host memory only).
                Step::Rectangles(work) => {
                    let (at, len) = work.source();
                    if touches(held, at, at + len) {
                        if count {
                            LATE_STATS[16].fetch_add(1, Ordering::Relaxed);
                        }
                        return Some(WHY_READ);
                    }
                }
                Step::Refresh(..) | Step::GpuWrite(..) | Step::Host(_) => {}
            }
        }
    }
    None
}

/// With flights in the air, after the submission's write-watch asking:
/// whether `runs` refresh a range of a host copy that a flight's shaders
/// may write and that the title wrote since it was last copied. The
/// refresh leaves ranges shaders are writing alone (`Shadow::update`), so
/// the title's bytes there would reach the card a submission late.
fn stale_under_flights(runs: &[&HostList]) -> bool {
    use std::sync::Arc;
    // Per copy, the span of what is refreshed (a flush brings that much
    // up to date at most).
    let mut spans: Vec<(Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
    for list in runs {
        for step in &list.deferred().copies {
            if let Step::Refresh(shadow, address, len) = step
                && shadow.writes_pending()
            {
                match spans.iter_mut().find(|s| Arc::ptr_eq(&s.0, shadow)) {
                    Some(s) => (s.1, s.2) = (s.1.min(*address), s.2.max(*address + *len)),
                    None => spans.push((shadow.clone(), *address, *address + *len)),
                }
            }
        }
    }
    if late_exact() {
        // The title's bytes are put in beside the flights' writes; a
        // settle is still due where that could not be done. At most this
        // many changed runs go up to copies in the card's memory, each a
        // copy of its own there.
        let mut runs_left = 512;
        return spans.iter().any(|(shadow, from, to)| shadow.merge_under_writes(*from, *to - *from, &mut runs_left));
    }
    spans.iter().any(|(shadow, from, to)| shadow.stale_under_writes(*from, *to - *from))
}

/// A fence signal on a queue ([`late`]): goes behind the flights in the
/// air, if there are any, and is set when they are settled. Whether it
/// did; if not the caller sets the fence now.
pub(crate) fn late_signal(fence: usize, value: u64) -> bool {
    if !late() {
        return false;
    }
    let mut line = line();
    if line.line.is_empty() && !line.settling {
        return false;
    }
    line.line.push(Late::Signal(fence, value));
    LATE_STATS[14].fetch_add(1, Ordering::Relaxed);
    drop(line);
    // A signal alone (behind a settle under way) has no flight to wake
    // the helper thread for it.
    LINE_MOVED.notify_one();
    RELAY_MOVED.notify_one();
    true
}

/// A queue stands at `wait`, which memory does not meet now ([`late`]):
/// whether it is met once the flights in the air have run. A flight's
/// bottom-of-pipe write of a value that meets it says yes without a wait
/// for the card: the part behind the wait goes to the card behind that
/// flight, which is the order the wait asks for. If the flights may write
/// the value in a way not known here (shaders, a timestamp, another
/// width), they are settled and memory is asked again.
pub(crate) fn late_met(wait: &crate::list::ValueWait) -> bool {
    let (address, bytes) = wait.place();
    {
        let line = line();
        if line.line.is_empty() && line.in_hand.is_empty() {
            return false;
        }
        let mut unsure = line.held.touches(address, address + bytes);
        // (With the relay: first the writes of flights being landed.)
        let writes = line.in_hand.iter().map(|(at, what, n)| (at, what, n)).chain(line.line.iter().filter_map(|item| match item {
            Late::Write(at, what, n) => Some((at, what, n)),
            _ => None,
        }));
        for (at, what, n) in writes {
            if *at >= address + bytes || address >= *at + *n {
                continue;
            }
            match what {
                crate::queue::Written::Value(v) if *at == address && *n >= bytes => {
                    if wait.holds_for(*v) {
                        LATE_STATS[13].fetch_add(1, Ordering::Relaxed);
                        return true;
                    }
                }
                _ => unsure = true,
            }
        }
        if !unsure {
            return false;
        }
    }
    settle_now(WHY_VALUE);
    wait.met()
}

/// Settles the flights in the air, if any ([`Host::settle`]), for reason
/// `why` (one of the `WHY_` places). Not from inside a submission.
pub(crate) fn settle_now(why: usize) {
    if !late() {
        return;
    }
    let Some(host) = host() else {
        return;
    };
    let mut submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
    host.settle(&mut submitted, why);
}

/// [`settle_now`] at a present, and for a fence the title blocks on.
pub(crate) fn settle_for_present() {
    settle_now(WHY_SYNC);
}
pub(crate) fn settle_for_fence() {
    settle_now(WHY_FENCE);
}

/// Resets the host lists of settled flights and keeps them for reuse, as
/// a queue does for the lists it ran (`crate::list::recycle`).
fn land() {
    let landed = std::mem::take(&mut line().landed);
    for (lists, order) in landed {
        crate::list::recycle(lists, &order);
    }
}

/// The thread that settles the flights once the card has run them all
/// ([`late`]): without it a bottom-of-pipe write would reach the title only
/// at the next call that settles, and a title thread that polls such a
/// value while the submitting thread waits for that thread would never
/// see it.
fn late_helper() {
    let Some(host) = host() else {
        return;
    };
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        tracing::warn!("no event for the thread that settles submissions sent ahead");
        return;
    };
    loop {
        let target = {
            let mut line = line();
            loop {
                if line.poke {
                    line.poke = false;
                    break None;
                }
                // (With the relay the flights are another thread's.)
                if !relay() && !line.line.is_empty() && !line.settling {
                    break Some(line.target);
                }
                line = LINE_MOVED.wait(line).unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some(target) = target else {
            land();
            crate::queue::release_held(false);
            continue;
        };
        // The card first, without the submission lock: the submitting
        // thread prepares and sends meanwhile.
        // SAFETY: our fence and event.
        unsafe {
            if host.fence.SetEventOnCompletion(target, event).is_ok() {
                WaitForSingleObject(event, INFINITE);
            }
        }
        {
            let mut submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
            // Something newer went up meanwhile: the card is waited for
            // again, the submitting thread is not held up for it.
            if line().target != target {
                continue;
            }
            host.settle(&mut submitted, WHY_CARD_DONE);
        }
        land();
        crate::queue::release_held(false);
    }
}

/// Starts the threads of [`late`], once: the one that settles flights when
/// the card has run them and looks at what queues hold, and with [`relay`]
/// the one that lands flights.
fn late_threads() {
    static HELPER: std::sync::Once = std::sync::Once::new();
    HELPER.call_once(|| {
        if let Err(e) = std::thread::Builder::new().name("victory-late-wait".into()).spawn(late_helper) {
            tracing::warn!(error = %e, "no thread to settle submissions sent ahead: they are settled by the calls that follow");
        }
        tracing::info!(
            most = late_most(),
            writes = late_writes(),
            "submissions are sent to the card without a wait for each (VICTORY_LATE_WAIT=1)"
        );
        if relay() {
            // Without it nothing would ever land.
            if let Err(e) = std::thread::Builder::new().name("victory-relay".into()).spawn(relay_worker) {
                fail(format_args!("no thread to land submissions sent ahead (VICTORY_RELAY=1): {e}"));
            }
            tracing::info!(
                exact = relay_exact(),
                "what shaders wrote is copied back by a thread of its own, beside the submitting thread (VICTORY_RELAY=1)"
            );
        }
    });
}

/// With [`relay`]: waits until `done` says so of the line, looked at each
/// time flights have landed. Counted as a settle for `why`, and its time
/// as waiting for the card. The caller may hold the submission lock: the
/// thread that lands flights never takes it.
fn relay_wait(why: usize, done: impl Fn(&Line) -> bool) {
    let mut line = line();
    if done(&line) {
        return;
    }
    LATE_STATS[1].fetch_add(1, Ordering::Relaxed);
    LATE_STATS[why].fetch_add(1, Ordering::Relaxed);
    let was_working = working(false);
    let waiting = std::time::Instant::now();
    while !done(&line) {
        line = LINE_LANDED.wait(line).unwrap_or_else(|e| e.into_inner());
    }
    drop(line);
    stood(waiting);
    working(was_working);
    let us = waiting.elapsed().as_micros() as u64;
    LATE_STATS[12].fetch_add(us, Ordering::Relaxed);
    LATE_WAITED[why.min(10)].fetch_add(us, Ordering::Relaxed);
    SUBMISSION_US[1].fetch_add(us, Ordering::Relaxed);
}

/// What the thread that lands flights fetches card copies' pieces with
/// ([`relay`]): a list, a fence and an event of its own (the host's are
/// behind the submission lock), and the memory the pieces come down to.
struct RelayFetch {
    list: Prelude,
    fence: ID3D12Fence,
    event: HANDLE,
    /// The last value signalled on `fence`.
    sent: u64,
    /// Buffer, mapping, bytes.
    buffer: Option<(ID3D12Resource, usize, u64)>,
}

/// A fetch of card copies' pieces sent to the card and not waited for yet
/// ([`Host::relay_card_fetch`]): what it will merge, where it lands, and
/// the signals its waits are for.
struct FetchRun {
    /// The piece of each copy, its offsets in that copy, and where its
    /// bytes come down.
    pieces: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize, u64)>,
    /// The mapping of the memory the pieces come down to.
    data: usize,
    /// Bytes fetched.
    total: u64,
    /// The fetch's number on [`RelayFetch::fence`] (values `2n-1` and `2n`).
    sent: u64,
    /// The queue's count when the fetch was sent: a send to a card copy
    /// made after it is not named by what the fetch brings.
    after: u64,
    /// The copies the fetch was made in.
    runs: usize,
}

/// The thread that lands flights ([`relay`]): as soon as the card has run
/// the first flights of the line it takes them, with what follows them,
/// and does what they left to do ([`Host::relay_land`]); else it waits for
/// the card, or for a flight. It takes neither the submission lock nor
/// anything a caller of [`Host::settle`] may hold: resetting the landed
/// lists and looking at what queues hold are left to [`late_helper`].
fn relay_worker() {
    let Some(host) = host() else {
        return;
    };
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        fail(format_args!("no event for the thread that lands submissions sent ahead (VICTORY_RELAY=1)"));
    };
    let mut fetch: Option<RelayFetch> = None;
    loop {
        let next = {
            let mut line = line();
            loop {
                if line.line.is_empty() {
                    line = RELAY_MOVED.wait(line).unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                let fences: Vec<Option<u64>> = line
                    .line
                    .iter()
                    .map(|l| match l {
                        Late::Run(flight) => Some(flight.fence),
                        _ => None,
                    })
                    .collect();
                // SAFETY: a plain query.
                let completed = unsafe { host.fence.GetCompletedValue() };
                let n = landable(&fences, completed);
                if n == 0 {
                    // The first thing is a flight the card has not run.
                    break Err(fences[0].unwrap_or(0));
                }
                line.settling = true;
                let items: Vec<Late> = line.line.drain(..n).collect();
                line.in_hand = items
                    .iter()
                    .filter_map(|l| match l {
                        Late::Write(at, what, bytes) => Some((*at, *what, *bytes)),
                        _ => None,
                    })
                    .collect();
                break Ok(items);
            }
        };
        match next {
            Ok(items) => host.relay_land(items, &mut fetch),
            Err(target) => {
                let waiting = std::time::Instant::now();
                // SAFETY: the host's fence and this thread's event.
                let waited = unsafe {
                    host.fence.SetEventOnCompletion(target, event).map(|()| {
                        WaitForSingleObject(event, INFINITE);
                    })
                };
                if waited.is_err() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                RELAY_STATS[2].fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
        }
    }
}

/// [`relay`]: what shaders wrote to the declared ranges `back` of copies
/// in the PC's memory goes into title memory. As [`hand_back`], each byte
/// once, the pieces that differ found on several threads; but the merge is
/// `Shadow::relay_merge`, which does not write the copy, and the ranges
/// are not done with here (each flight's are, afterwards).
fn relay_hand_back(back: &[Declared]) {
    let mut work: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
    for (shadow, declared) in back {
        if shadow.carded() {
            continue;
        }
        let mut spans: Vec<(usize, usize)> = Vec::new();
        join_ranges(&mut spans, declared.iter().copied());
        for &(from, to) in &spans {
            shadow.relay_compared(from, to - from);
        }
        work.extend(spans.into_iter().map(|(from, to)| (shadow.clone(), from, to)));
    }
    if work.is_empty() {
        return;
    }
    let scanning = std::time::Instant::now();
    // Looked through without a lock: a piece that changes meanwhile (a
    // later flight writing, the title's bytes going in) is found now or by
    // that flight's landing, and the merge looks again under the lock.
    let found = crate::shadow::find_all(&work);
    crate::flow::took(crate::flow::Site::Compared, work.iter().map(|(_, from, to)| to - from).sum(), scanning);
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::BackScanUs, scanning.elapsed().as_micros() as u64);
    let (mut now, mut title) = (Vec::new(), Vec::new());
    for ((shadow, from, to), found) in work.iter().zip(found) {
        let merging = std::time::Instant::now();
        let mut written = 0;
        for (at, n) in found {
            written += shadow.relay_merge(at, n, &mut now, &mut title);
        }
        COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
        crate::flow::took(crate::flow::Site::PutBack, written, merging);
        #[cfg(feature = "diagnostics")]
        crate::copystat::compared(*from, to - from, written);
        let _ = (from, to);
    }
}

/// Merges what a landing held back ([`lazy_merge`], [`HeldBack`]): what the
/// shaders of flights that landed and told nobody wrote in copies in the
/// PC's memory, which title memory does not hold yet.
///
/// Called at the first point that must see those bytes (a preparation that
/// reads them, a `Map`, a refresh that would write over them), at every
/// settle, and at the start of every landing. Merges all of them: each is
/// about to be read, or the landing is about to tell the title its flight
/// is done. The entries are taken out of the line first, so exactly one
/// thread merges each.
fn drain_deferred() -> usize {
    let entries = {
        let mut line = line();
        let entries = std::mem::take(&mut line.deferred);
        // Counted under the same lock as the take: a settle that looked at the
        // line between the two would find nothing left to merge and return
        // while these bytes are still in the copies in the PC's memory.
        if !entries.is_empty() {
            line.draining += 1;
        }
        entries
    };
    if entries.is_empty() {
        return 0;
    }
    let mut back: Vec<Declared> = Vec::new();
    for entry in &entries {
        for (shadow, ranges) in &entry.back {
            match back.iter_mut().find(|(s, _)| std::sync::Arc::ptr_eq(s, shadow)) {
                Some((_, all)) => {
                    for range in ranges {
                        if !all.contains(range) {
                            all.push(*range);
                        }
                    }
                }
                None => back.push((shadow.clone(), ranges.clone())),
            }
        }
    }
    relay_hand_back(&back);
    for entry in &entries {
        for (shadow, ranges) in &entry.back {
            shadow.copy_back_done(ranges);
        }
    }
    {
        let mut line = line();
        for entry in &entries {
            line.held.merged(entry.number);
        }
        // After `held.merged`: the ranges stay held until the bytes are in
        // title memory, and a settle that woke on this one sees both.
        line.draining -= 1;
    }
    LINE_LANDED.notify_all();
    entries.len()
}

/// Whether a settle has nothing left to wait for: no flight in the line, none
/// being landed, and no merge held back or being merged. The line alone does
/// not say it — [`drain_deferred`] takes what a landing held back out of
/// `deferred` before it writes those bytes into title memory, so a second
/// settle that looked at the line while that ran would find it empty and
/// return with the title's memory still holding the copies' bytes from before
/// ([`lazy_merge`]).
fn settled(line: &Line) -> bool {
    line.line.is_empty() && !line.settling && line.deferred.is_empty() && line.draining == 0
}

/// For the frame-rate line, with [`late`]: what the line of flights did
/// per frame since the last one.
fn late_line(frames: f64) {
    if !late() {
        return;
    }
    let s: Vec<f64> = LATE_STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    tracing::info!(
        "submissions sent ahead per frame: {:.1} in {:.1} settles ({:.1} flights each time), {:.1} ms waiting for the card in them; \
         settled for: a synchronous submission or present {:.1}, the card had run them {:.1}, a wait for a memory value {:.1}, \
         title memory changed under shader writes {:.1}, title memory read {:.1} (by a buffer copy {:.1}, a fill of upload memory {:.1}, \
         a texture {:.1}), a step of ours {:.1}, {} lists in the air {:.1}, \
         bottom-of-pipe writes (VICTORY_LATE_WAIT_WRITES=0) {:.1}, a blocking fence wait {:.1}, \
         a descriptor table with no room {:.1}; \
         {:.1} waits for memory values met by a flight's bottom-of-pipe write, {:.1} fence signals set behind flights",
        s[0],
        s[1],
        if s[1] > 0.0 { s[11] / s[1] } else { 0.0 },
        s[12] / 1000.0,
        s[WHY_SYNC],
        s[WHY_CARD_DONE],
        s[WHY_VALUE],
        s[WHY_STALE],
        s[WHY_READ],
        s[15],
        s[16],
        s[17],
        s[WHY_RUN],
        late_most(),
        s[WHY_MOST],
        s[WHY_WRITES],
        s[WHY_FENCE],
        s[WHY_DYNAMIC],
        s[13],
        s[14],
    );
    {
        let w: Vec<f64> = LATE_WAITED.iter().map(|w| w.swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames).collect();
        tracing::info!(
            "of the waiting for the card in settles, per frame, by what settled: a synchronous submission or present {:.1} ms, \
             the card had run them {:.1}, a wait for a memory value {:.1}, title memory changed under shader writes {:.1}, \
             title memory read {:.1}, a step of ours {:.1}, lists in the air {:.1}, bottom-of-pipe writes {:.1}, a blocking fence wait {:.1}, \
             a descriptor table with no room {:.1}",
            w[WHY_SYNC],
            w[WHY_CARD_DONE],
            w[WHY_VALUE],
            w[WHY_STALE],
            w[WHY_READ],
            w[WHY_RUN],
            w[WHY_MOST],
            w[WHY_WRITES],
            w[WHY_FENCE],
            w[WHY_DYNAMIC],
        );
    }
    if relay() {
        let r: Vec<f64> = RELAY_STATS.iter().map(|r| r.swap(0, Ordering::Relaxed) as f64 / frames).collect();
        tracing::info!(
            "the relay per frame (VICTORY_RELAY=1): {:.1} flights landed in {:.1} landings by the thread that copies back, \
             beside the submitting thread; that thread waited {:.1} ms for the card with nothing to land and {:.1} ms for \
             fetches of card copies' pieces (of them {:.1} ms while the card finished work sent before the fetch, the rest \
             the fetch's own copies), and was {:.1} ms landing in all (the frame line's \"copying back\" is its time; \
             \"settles\" above are waits of other threads for it)",
            r[1],
            r[0],
            r[2] / 1000.0,
            r[3] / 1000.0,
            r[5] / 1000.0,
            r[4] / 1000.0,
        );
    }
    if lazy_merge() {
        tracing::info!(
            "merges held back per frame (VICTORY_LAZY_MERGE=1): {:.1} flights landed and told nobody, \
             whose shaders' writes in card copies go into title memory at the first read, present, \
             fence or landing after them",
            s[DEFERRED]
        );
    }
    if late_exact() {
        let [pages, runs, bytes] = crate::shadow::merged_stats().map(|n| n as f64 / frames);
        tracing::info!(
            "title bytes put in under flights' shader writes per frame (VICTORY_LATE_WAIT=2): {pages:.0} pages looked at, \
             {runs:.0} changed runs, {:.2} MB",
            bytes / (1 << 20) as f64
        );
    }
}

#[cfg(test)]
mod late_tests {
    use super::{Held, Line, continues, join_ranges, settled, touches};

    /// A line with nothing in it.
    fn empty_line() -> Line {
        Line {
            line: Vec::new(),
            held: Held::new(),
            deferred: Vec::new(),
            settles: 0,
            lists: 0,
            target: 0,
            settling: false,
            draining: 0,
            poke: false,
            landed: Vec::new(),
            in_hand: Vec::new(),
        }
    }

    /// A settle returns only when title memory holds what the flights wrote:
    /// merges held back in `deferred`, and the merges a thread has taken out of
    /// it and is writing (`draining`), are both in the way. Without the second,
    /// a second thread returns from `settle` while the first is still relaying
    /// those writes into title memory.
    #[test]
    fn a_settle_waits_for_the_merges_being_relayed() {
        let mut line = empty_line();
        assert!(settled(&line));
        // A merge held back has not landed.
        line.deferred.push(super::HeldBack {
            number: 0,
            back: Vec::new(),
        });
        assert!(!settled(&line), "a missing merge is one a settle waits for");
        line.deferred.clear();
        // A thread that took the merges out of `deferred` and is writing them:
        // the line is empty of them, title memory is not holding them yet.
        line.draining = 1;
        assert!(
            !settled(&line),
            "a drain in hand is one a settle waits for, as a merge still in `deferred` is"
        );
        line.draining = 0;
        assert!(settled(&line));
        // A settle in progress is still in the way, and a flight too.
        line.settling = true;
        assert!(!settled(&line));
        line.settling = false;
        assert!(settled(&line));
    }

    /// Copies join only when both ends continue and the whole does not
    /// overlap itself.
    #[test]
    fn copies_join_only_end_to_end_and_apart() {
        // (destination, source, bytes)
        assert!(continues((1000, 5000, 16), (1016, 5016, 32)));
        assert!(continues((5000, 1000, 16), (5016, 1016, 32)));
        // A gap, or only one side continues.
        assert!(!continues((1000, 5000, 16), (1020, 5016, 32)));
        assert!(!continues((1000, 5000, 16), (1016, 5020, 32)));
        assert!(!continues((1000, 5000, 16), (1000, 5000, 16)));
        // Together they would overlap: the second reads what the first wrote.
        assert!(!continues((1000, 1016, 16), (1016, 1032, 16)));
        assert!(!continues((1016, 1000, 16), (1032, 1016, 16)));
        // They touch without overlapping.
        assert!(continues((1000, 1032, 16), (1016, 1048, 16)));
        assert!(!continues((usize::MAX - 8, 0, 8), (0, 8, 8)));
    }

    /// Ranges are found by what they hold, not by where they start, and a
    /// range that only touches another's end does not touch it.
    #[test]
    fn ranges_touch_where_they_overlap() {
        let mut ranges = Vec::new();
        join_ranges(&mut ranges, [(40, 50), (10, 20), (20, 25), (45, 60), (7, 7)]);
        assert_eq!(ranges, [(10, 25), (40, 60)]);
        join_ranges(&mut ranges, [(25, 40)]);
        assert_eq!(ranges, [(10, 60)]);
        let ranges = [(10usize, 25usize), (40, 60), (100, 101)];
        for (from, to, hit) in [
            (0, 10, false),
            (0, 11, true),
            (24, 25, true),
            (25, 40, false),
            (25, 41, true),
            (59, 200, true),
            (60, 100, false),
            (100, 101, true),
            (101, 500, false),
            (30, 30, false),
            (50, 50, false),
        ] {
            assert_eq!(touches(&ranges, from, to), hit, "{from}..{to}");
        }
        assert!(!touches(&[], 0, usize::MAX));
    }

    /// A range a flight declared is held until that flight's merge, however
    /// many times it declares it; the ranges of two flights join without
    /// touching, and merging one number leaves the other's as they were.
    #[test]
    fn a_flight_holds_its_ranges_until_it_merges() {
        use super::Held;
        let mut held = Held::new();
        held.declare([(100, 200), (100, 200)], 1);
        held.declare([(160, 300)], 2);
        assert_eq!(held.joined(), [(100, 300)]);
        held.merged(1);
        assert_eq!(held.joined(), [(160, 300)]);
        assert!(held.touches(150, 170));
        assert!(!held.touches(100, 120));
        // A number that declared nothing ends nothing.
        held.merged(9);
        assert_eq!(held.joined(), [(160, 300)]);
        held.merged(2);
        assert!(held.is_empty());
        assert!(held.joined().is_empty());
    }

    /// A range two flights declared stays held after the first of them
    /// merges, and is free only when the last does: a refresh may not write
    /// over what the second flight's shaders wrote.
    #[test]
    fn a_range_two_flights_declared_stays_held_until_both_merge() {
        use super::Held;
        let mut held = Held::new();
        held.declare([(100, 200)], 1);
        held.declare([(100, 200)], 2);
        assert!(held.touches(150, 160));
        held.merged(1);
        assert!(held.touches(150, 160), "the second flight still holds it");
        assert_eq!(held.joined(), [(100, 200)]);
        held.merged(2);
        assert!(!held.touches(150, 160));
        assert!(held.is_empty());
    }

    /// A settle — a present, a fence the title blocks on, a read — merges
    /// what a landing held back: its flight's ranges are free afterwards,
    /// and a second one finds nothing left to merge. The entry is taken out
    /// of the line once, so no other thread merges the same flight.
    #[test]
    fn a_settle_drains_what_a_landing_held_back() {
        use super::{HeldBack, drain_deferred, line};
        {
            let mut guard = line();
            guard.held.declare([(4096, 8192)], 7);
            guard.deferred.push(HeldBack { number: 7, back: Vec::new() });
        }
        assert!(line().held.touches(5000, 6000), "held while its merge waits");
        assert_eq!(drain_deferred(), 1);
        assert!(line().deferred.is_empty());
        assert!(!line().held.touches(5000, 6000), "free after the merge");
        assert_eq!(drain_deferred(), 0);
    }

    /// A landing holds a flight's merge back only when the flight wrote
    /// something in a copy in the PC's memory and the line after it in the
    /// same landing tells the title nothing: a bottom-of-pipe write or a
    /// fence signal behind it puts its bytes in the title's hands, and what
    /// the title does after that may read them.
    #[test]
    fn a_merge_is_held_back_only_when_the_title_is_told_nothing() {
        use super::{Late, holds_back};
        assert!(holds_back(true, &[]));
        assert!(!holds_back(false, &[]));
        assert!(!holds_back(true, &[Late::Signal(0, 1)]));
        assert!(!holds_back(true, &[Late::Signal(0, 1), Late::Signal(0, 2)]));
    }
}

/// Whether a title buffer copy that continues the one recorded before it
/// in its list becomes one step with it (`VICTORY_JOIN_COPIES=1`; off by
/// default).
///
/// Each copy is done at submission through the kernel, which reports a
/// page that is not there instead of faulting (`copy_mapped`), tells the
/// write watch and the guards, and ends the refreshes gathered so far: a
/// call into the kernel for some 480 bytes, 14,042 times a frame in run 86
/// (6.8 MB; "moves 19 ms" with the refreshes' own reads). How many of them
/// continue the one before is counted either way and said on its own line
/// with the frame rate; the switch is worth that many kernel calls.
///
/// Joined, the same bytes move: [`continues`] joins only copies whose
/// ranges, taken together, do not overlap, and nothing is recorded between
/// two steps that follow each other. What differs: a page that is not
/// mapped now fails the joined copy as a whole (it then goes page by page,
/// zeros for what cannot be read, as a single copy does), where before
/// only the copy that held it did.
fn join_copies() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_JOIN_COPIES").is_some_and(|v| v == "1"))
}

/// Since the last frame-rate line: title buffer copies recorded, and those
/// that continue the one before ([`join_copies`]).
static COPIES_JOINED: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Whether the copy `next` (destination, source, bytes) goes on exactly
/// where `last` ended, source and destination both, and the two as one do
/// not overlap (source over destination): then one copy of both moves the
/// bytes the two would move in turn.
fn continues(last: (usize, usize, usize), next: (usize, usize, usize)) -> bool {
    let ((d, s, n), (dst, src, len)) = (last, next);
    let Some(all) = n.checked_add(len) else {
        return false;
    };
    d.checked_add(n) == Some(dst)
        && s.checked_add(n) == Some(src)
        && (d.checked_add(all).is_some_and(|end| end <= s) || s.checked_add(all).is_some_and(|end| end <= d))
}

/// The deferred steps from `at` that one copy group runs: the `Copy` there
/// and those after it whose destination carries on exactly where the one
/// before it ended, and where the group's destination ends. Nothing is
/// recorded between steps that follow each other, so a group is run in the
/// order it was recorded and the copies of it still go one after another,
/// each failing on its own; what a group shares is the work around them —
/// the flush before them, opening the guards over all their destinations,
/// noting them written, and their accounting. Returned with their count, at
/// least `1`.
///
/// The bytes a group moves are the bytes a submission used to move in the
/// same order: a copy's source may lie in another's destination, and the
/// copies are not reordered for that (the title emits tile mappings whose
/// destinations are adjacent, ~0.5 KB each, 13,034 of them a frame).
fn copy_run(steps: &[Step], at: usize) -> (usize, usize) {
    let Some(&Step::Copy(dst, _, len)) = steps.get(at) else {
        return (1, 0);
    };
    let mut end = dst.saturating_add(len);
    let mut taken = 1;
    while let Some(&Step::Copy(next, _, len)) = steps.get(at + taken) {
        if len == 0 || next != end {
            break;
        }
        end = end.saturating_add(len);
        taken += 1;
    }
    (taken, end)
}

/// Runs the copies of the group at `at` ([`copy_run`]), one after another, to
/// `copy` (destination, source, bytes); the bytes they move. The group's
/// guards are opened over all of their destinations before this is called.
fn group_copies(steps: &[Step], at: usize, mut copy: impl FnMut(usize, usize, usize)) -> usize {
    let (taken, _) = copy_run(steps, at);
    let mut bytes = 0;
    for step in &steps[at..at + taken] {
        // `copy_run` took the copies only, and the one at `at` is one.
        let Step::Copy(dst, src, len) = *step else {
            break;
        };
        copy(dst, src, len);
        bytes += len;
    }
    bytes
}

/// Where a copy's time went, for [`COPY_STAGES`]: nanoseconds in the
/// kernel's read.
#[derive(Default)]
struct Stages {
    read: u64,
}

/// What a copy into title memory has opened for it before it reads.
enum Open {
    /// Not into title memory (`Step::Fill`, and the reads out of it).
    Nothing,
    /// Into title memory, on its own: the guards over its range, when the
    /// copy is made ([`crate::watch::writing`]).
    Own,
    /// Into title memory, in a group that opened the guards over the whole
    /// group's destination before its first copy ([`copy_run`]).
    Group,
}

impl Open {
    /// Whether the copy writes title memory.
    fn writes_title(&self) -> bool {
        !matches!(self, Open::Nothing)
    }
}

#[cfg(test)]
mod copy_tests {
    use super::{Step, copy_run, group_copies};

    /// The steps of a title's copies, as recorded, into a "title memory" that
    /// starts at `base`: (destination, source, bytes) as offsets.
    fn steps_of(base: usize, copies: &[(usize, usize, usize)]) -> Vec<Step> {
        copies
            .iter()
            .map(|&(dst, src, len)| Step::Copy(base + dst, base + src, len))
            .collect()
    }

    /// Every byte a run's copies write is inside the one range the run opens
    /// its guard over and notes as written, and that range starts where the
    /// first copy's destination does: no copy of a run is left unopened or
    /// unreported, and none reaches outside what was opened for it.
    fn covered(steps: &[Step], at: usize) -> bool {
        let (taken, end) = copy_run(steps, at);
        let first = match &steps[at] {
            &Step::Copy(first, _, _) => first,
            _ => return true,
        };
        (0..taken).all(|k| match &steps[at + k] {
            &Step::Copy(dst, _, len) => dst >= first && dst <= end && len <= end - dst,
            _ => false,
        })
    }

    /// A run takes the copies whose destinations carry on from one another,
    /// and stops at a gap in them or at any other step: a `Fill`, or work
    /// recorded between two copies of the title's, which must not be moved
    /// past either of them.
    #[test]
    fn a_run_is_the_copies_whose_destinations_carry_on() {
        let base = 0x1000;
        let mut steps = steps_of(base, &[(0, 400, 16), (16, 950, 16), (32, 980, 16)]);
        steps.push(Step::Fill(0, base + 900, 16));
        steps.extend(steps_of(base, &[(64, 900, 16), (80, 900, 16)]));
        steps.push(Step::Host(Box::new(|| {})));
        steps.extend(steps_of(base, &[(64, 900, 16)]));
        steps.extend(steps_of(base, &[(0, 400, 16), (32, 950, 16)]));

        assert_eq!(copy_run(&steps, 0), (3, base + 48));
        assert!(covered(&steps, 0));
        // A fill ends the run whether or not destinations would carry on: the
        // step after it is no `Copy` at all.
        assert_eq!(copy_run(&steps, 3), (1, 0));
        // Two copies whose destinations carry on are one run of two.
        assert_eq!(copy_run(&steps, 4), (2, base + 96));
        assert!(covered(&steps, 4));
        // Work recorded between two copies ends a run: it may read what they
        // wrote, and must not be moved past them.
        assert_eq!(copy_run(&steps, 6), (1, 0));
        assert_eq!(copy_run(&steps, 7), (1, base + 80));
        // Destinations that do not carry on are two runs, not one.
        assert_eq!(copy_run(&steps, 8), (1, base + 16));
        assert_eq!(copy_run(&steps, 9), (1, base + 48));
        assert!(covered(&steps, 8));
        // Past the end: one step, nothing to cover.
        assert_eq!(copy_run(&steps, steps.len()), (1, 0));
    }

    /// A run moves the bytes the copies would move one at a time, in the
    /// order they were recorded: a copy that reads another's destination sees
    /// what it wrote, so nothing may be reordered or merged into one copy.
    #[test]
    fn a_run_moves_the_same_bytes_in_the_same_order() {
        let base = 0x2000;
        let mut title: Vec<u8> = (0..512).map(|i| (i % 251) as u8).collect();
        // The second copy reads what the first wrote.
        let steps = steps_of(base, &[(0, 400, 16), (16, 8, 16), (32, 416, 16)]);
        let one_by_one = {
            let mut bytes = title.clone();
            for step in &steps {
                let &Step::Copy(dst, src, len) = step else { continue };
                let (dst, src) = (dst - base, src - base);
                assert!(dst + len <= bytes.len() && src + len <= bytes.len());
                bytes.copy_within(src..src + len, dst);
            }
            bytes
        };
        let mut at = 0;
        let mut order = Vec::new();
        while at < steps.len() {
            let (taken, end) = copy_run(&steps, at);
            let first = match &steps[at] {
                &Step::Copy(dst, _, _) => dst,
                _ => unreachable!("a run starts at a copy"),
            };
            // The one range the run opens its guard over.
            assert!(end >= first && covered(&steps, at));
            group_copies(&steps, at, |dst, src, len| {
                order.push(dst - base);
                title.copy_within(src - base..src - base + len, dst - base);
            });
            at += taken;
        }
        assert_eq!(title, one_by_one);
        assert_eq!(order, [0, 16, 32]);
    }

    /// A run that takes one copy takes the copy before it in the same place:
    /// the group's bytes are the step's own when nothing carries on.
    #[test]
    fn a_run_of_one_is_the_step_itself() {
        let base = 0x3000;
        let steps = steps_of(base, &[(0, 400, 16), (100, 400, 16)]);
        assert_eq!(group_copies(&steps, 0, |_, _, _| {}), 16);
        assert_eq!(group_copies(&steps, 1, |_, _, _| {}), 16);
    }
}

/// The ranges the shaders of `runs` may write, per host copy, as declared.
/// One list's lock at a time: a submission may name a list twice.
fn declared_writes(runs: &[&HostList]) -> Vec<Declared> {
    let mut back: Vec<Declared> = Vec::new();
    for list in runs {
        for step in &list.deferred().copies {
            if let Step::GpuWrite(shadow, address, len) = step {
                let range = (*address, *address + *len);
                match back.iter_mut().find(|(s, _)| std::sync::Arc::ptr_eq(s, shadow)) {
                    Some((_, ranges)) => {
                        if !ranges.contains(&range) {
                            ranges.push(range);
                        }
                    }
                    None => back.push((shadow.clone(), vec![range])),
                }
            }
        }
    }
    back
}

/// After `list` has run: the texels its read-backs copied into host
/// buffers of ours go to the title's memory (`HostList::defer_read_back`).
///
/// A submission that is waited for did this itself. One sent ahead
/// ([`late`]) did not, at any stage: the flights' lists were settled
/// without it, so the title read a buffer nobody wrote, as before read-backs
/// were made (the exposure metering's two small float targets, every frame).
fn read_backs(list: &HostList) {
    for (buffer, at, [row, rows, pitch]) in &list.deferred().backs {
        let mut data = std::ptr::null_mut();
        // SAFETY: a read-back buffer of ours, written by the list that has
        // run; the title's destination holds every row (checked when the
        // copy was recorded).
        unsafe {
            if buffer.Map(0, None, Some(&mut data)).is_ok() && !data.is_null() {
                let from = data.cast::<u8>().cast_const();
                for line in 0..*rows {
                    std::ptr::copy_nonoverlapping(from.add(line * row.next_multiple_of(256)), (*at as *mut u8).add(line * pitch), *row);
                }
                buffer.Unmap(0, None);
            }
        }
    }
}

/// After the card ran what declared `back`: what shaders wrote there goes
/// into title memory, and the ranges are done with.
fn hand_back(back: &[Declared]) {
    if crate::shadow::wide_back() {
        // Every range first (the pieces that differ, found on several
        // threads), then the merges, in order, as below.
        let mut work: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
        for (shadow, declared) in back {
            if shadow.carded() {
                continue;
            }
            let mut sorted = declared.clone();
            sorted.sort_unstable();
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for (from, to) in sorted {
                match spans.last_mut() {
                    Some(last) if from <= last.1 => last.1 = last.1.max(to),
                    _ => spans.push((from, to)),
                }
            }
            work.extend(spans.into_iter().map(|(from, to)| (shadow.clone(), from, to)));
        }
        #[cfg(feature = "diagnostics")]
        let scanning = std::time::Instant::now();
        let found = crate::shadow::find_all(&work);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::BackScanUs, scanning.elapsed().as_micros() as u64);
        for ((shadow, from, to), found) in work.iter().zip(found) {
            let written = shadow.copy_back_with(*from, to - from, Some(found));
            COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(*from, to - from, written);
        }
        for (shadow, declared) in back {
            shadow.copy_back_done(declared);
        }
        return;
    }
    for (shadow, declared) in back {
        if shadow.carded() {
            // Done before (`Host::card_back`).
            shadow.copy_back_done(declared);
            continue;
        }
        // Each byte compared once: the world declares dozens of views
        // of its 174 MB heap that overlap almost entirely, and comparing
        // every one whole was gigabytes a frame (0.3-0.5 s) for the few
        // MB shaders wrote. Only ranges that overlap or touch are
        // joined: what lies between two was not given to shaders (the
        // snapshot does not hold it).
        let mut sorted = declared.clone();
        sorted.sort_unstable();
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for (from, to) in sorted {
            match spans.last_mut() {
                Some(last) if from <= last.1 => last.1 = last.1.max(to),
                _ => spans.push((from, to)),
            }
        }
        for (from, to) in spans {
            let written = shadow.copy_back(from, to - from);
            COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(from, to - from, written);
        }
        shadow.copy_back_done(declared);
    }
}

impl Host {
    /// Whether the host runs mesh shaders at shader model 6.6 (translated
    /// mesh shaders need both); logged once.
    pub(crate) fn mesh_shaders(&self) -> bool {
        static MESH: OnceLock<bool> = OnceLock::new();
        *MESH.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
            let mut model = D3D12_FEATURE_DATA_SHADER_MODEL {
                HighestShaderModel: D3D_SHADER_MODEL_6_6,
            };
            // SAFETY: each query writes its own structure of the given size.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS7,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
                    && self
                        .device
                        .CheckFeatureSupport(
                            D3D12_FEATURE_SHADER_MODEL,
                            (&raw mut model).cast(),
                            size_of_val(&model) as u32,
                        )
                        .is_ok()
            };
            let mesh = ok
                && options.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED
                && model.HighestShaderModel.0 >= D3D_SHADER_MODEL_6_6.0;
            tracing::info!(mesh, "host mesh shader support");
            mesh
        })
    }

    /// Whether the host GPU supports the depth bounds test; logged once.
    pub(crate) fn depth_bounds(&self) -> bool {
        static BOUNDS: OnceLock<bool> = OnceLock::new();
        *BOUNDS.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS2::default();
            // SAFETY: writes `options` of the given size.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS2,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
            };
            let supported = ok && options.DepthBoundsTestSupported.as_bool();
            tracing::info!(supported, "host depth bounds support");
            supported
        })
    }

    /// A new list, open for recording.
    pub(crate) fn new_list(&self) -> windows::core::Result<HostList> {
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        let before = private_bytes();
        let list = self.make_list();
        LISTS[0].fetch_add(1, Ordering::Relaxed);
        // (May shrink while a list is made, on another thread's account:
        // added as it is, the sum is what counts.)
        #[cfg(feature = "diagnostics")]
        LISTS[2].fetch_add(private_bytes().wrapping_sub(before), Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            add(C::HostLists, 1);
            add(C::HostListNs, started.elapsed().as_nanos() as u64);
        }
        list
    }

    /// An allocator and a closed list for a prelude.
    ///
    /// No shading rate: a prelude carries texture transitions and the twin
    /// copies, whose work is per-pixel writes, so it keeps 1x1 even with
    /// `VICTORY_SHADING_RATE` on.
    fn make_prelude(&self) -> Option<Prelude> {
        // SAFETY: plain object creation.
        let made: windows::core::Result<Prelude> = unsafe {
            (|| {
                let allocator: ID3D12CommandAllocator =
                    self.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                let list: ID3D12GraphicsCommandList =
                    self.device
                        .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                list.Close()?;
                Ok((allocator, list))
            })()
        };
        LISTS[1].fetch_add(1, Ordering::Relaxed);
        match made {
            Ok(prelude) => Some(prelude),
            Err(e) => {
                tracing::warn!(error = %e, "no host list for a prelude");
                None
            }
        }
    }

    /// An allocator and a closed list on the copy queue ([`copy_queue`]).
    fn make_copy_prelude(&self) -> Option<Prelude> {
        // SAFETY: plain object creation.
        unsafe {
            let allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)
                .ok()?;
            let list: ID3D12GraphicsCommandList =
                self.device
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)
                    .ok()?;
            list.Close().ok()?;
            Some((allocator, list))
        }
    }

    fn make_list(&self) -> windows::core::Result<HostList> {
        // SAFETY: plain object creation.
        unsafe {
            let allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator,
                None,
            )?;
            let prelude = OnceLock::new();
            if !lazy_prelude() {
                let _ = prelude.set(self.make_prelude());
            }
            let shading = self.shading.and_then(|rate| {
                match list.cast::<ID3D12GraphicsCommandList5>() {
                    Ok(list5) => {
                        list5.RSSetShadingRate(rate.d3d12(), None);
                        Some((list5, rate.d3d12()))
                    }
                    Err(e) => {
                        victory_core::logging::warn_once("gpu.shading_rate.interface", || {
                            format!(
                                "no ID3D12GraphicsCommandList5 for VICTORY_SHADING_RATE ({e}); \
                                 1x1 is used"
                            )
                        });
                        None
                    }
                }
            });
            Ok(HostList {
                allocator,
                list,
                shading,
                deferred: Mutex::default(),
                scratch: Mutex::default(),
                states: Mutex::default(),
                twins: Mutex::default(),
                prelude,
                pipelines: Mutex::default(),
                budget: OnceLock::new(),
                indirect_args: Mutex::default(),
                _alive: Counted::new(),
                #[cfg(feature = "diagnostics")]
                notes: Mutex::default(),
                #[cfg(feature = "diagnostics")]
                stamps: OnceLock::new(),
            })
        }
    }

    /// The card's clock ([`CARD_CLOCK_US`]), made at the first send.
    fn card_clock(&self) -> Option<&'static CardClock> {
        static CLOCK: OnceLock<Option<CardClock>> = OnceLock::new();
        static WATCH: std::sync::Once = std::sync::Once::new();
        let clock = CLOCK
            .get_or_init(|| {
                // SAFETY: a plain fence on our device.
                let fence = unsafe { self.device.CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE) }.ok()?;
                Some(CardClock {
                    fence,
                    state: Mutex::new((0, None)),
                    sent: std::sync::Condvar::new(),
                })
            })
            .as_ref()?;
        WATCH.call_once(|| {
            if let Err(e) = std::thread::Builder::new().name("victory-card-clock".into()).spawn(move || card_clock_watch(clock)) {
                tracing::warn!(error = %e, "no thread for the card's clock: the frame line's idle times stay 0");
            }
        });
        Some(clock)
    }

    /// Lists of ours just went to the queue: the clock's signal goes
    /// behind them, and an idle stretch of the card, if one was under way,
    /// ends here ([`CARD_CLOCK_US`]).
    fn card_sent(&self) {
        let Some(clock) = self.card_clock() else {
            return;
        };
        let mut state = clock.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0 += 1;
        // SAFETY: our queue and the clock's fence.
        let _ = unsafe { self.queue.Signal(&clock.fence, state.0) };
        if let Some((since, worked_then)) = state.1.take() {
            CARD_CLOCK_US[0].fetch_add(since.elapsed().as_micros() as u64, Ordering::Relaxed);
            CARD_CLOCK_US[1].fetch_add(worked().saturating_sub(worked_then), Ordering::Relaxed);
        }
        drop(state);
        clock.sent.notify_one();
    }

    /// Whether the host has what copies in the card's memory need (three
    /// lists and the compare); made at the first ask. A copy goes to the
    /// card's memory only if it has.
    pub(crate) fn card_ready(&self) -> bool {
        static READY: OnceLock<bool> = OnceLock::new();
        *READY.get_or_init(|| match self.make_card_work() {
            Ok(work) => {
                *CARD_WORK.lock().unwrap_or_else(|e| e.into_inner()) = Some(work);
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "no host copies in the card's memory: the compare could not be made");
                false
            }
        })
    }

    fn make_card_work(&self) -> windows::core::Result<CardWork> {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        let mut code: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call; the out
        // pointer is ours.
        unsafe {
            D3DCompile(
                CARD_COMPARE.as_ptr().cast(),
                CARD_COMPARE.len(),
                windows::core::s!("card compare"),
                None,
                None,
                windows::core::s!("main"),
                windows::core::s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )?;
        }
        let code = code.ok_or_else(windows::core::Error::empty)?;
        let list = || self.make_prelude().ok_or_else(windows::core::Error::empty);
        // A megabyte of results: 262,144 pieces, 16 GB of declared ranges
        // in one submission.
        const RESULTS: u64 = 1 << 20;
        let (differs, data) = self.shared_buffer(RESULTS).ok_or_else(windows::core::Error::empty)?;
        name(&differs, || "card compare results".into());
        // SAFETY: the blob's own bytes; the root signature is inside the
        // shader.
        unsafe {
            let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
            let root: ID3D12RootSignature = self.device.CreateRootSignature(0, bytes)?;
            let pso: ID3D12PipelineState = self.device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: bytes.as_ptr().cast(),
                    BytecodeLength: bytes.len(),
                },
                ..Default::default()
            })?;
            Ok(CardWork {
                sync: vec![list()?],
                check: vec![list()?],
                fetch: list()?,
                root,
                pso,
                differs: (differs, data as usize, RESULTS),
                fetched: None,
                taken: 0,
                out: 0,
            })
        }
    }

    /// The copies of the dirty ranges into their copies in the card's
    /// memory, on the copy queue ([`copy_queue`]), and the wait that makes
    /// the direct queue see them: `direct` is the last value signalled on
    /// the direct queue's fence, which the batch waits for (the flights
    /// already sent read those copies with the bytes from before it), and
    /// the value returned is the one signalled after it, which every list
    /// the direct queue runs next waits for ([`copied_state`] for the state
    /// the batch leaves a copy in). `None` with the switch off, no copy
    /// queue, or no list to record in: the caller copies in its own list. A
    /// batch whose signal failed is drained on this thread
    /// ([`CopyLists::drain`]) and returns the value of that drain; a batch
    /// whose drain failed too returns `None`, and the caller copies in its own
    /// list with the batch's copies unordered against it.
    fn card_copies(
        &self,
        dirty: &[(std::sync::Arc<crate::shadow::Shadow>, Vec<(usize, usize)>)],
        direct: u64,
    ) -> Option<u64> {
        let copies = self.copies.as_ref()?;
        let mut lists = copies.lists.lock().unwrap_or_else(|e| e.into_inner());
        let (allocator, list) = lists
            .take(&copies.fence, copies.event)
            .or_else(|| self.make_copy_prelude())?;
        // How the copy queue stood when this batch was ready: the value of the
        // batch before it, and what the card had reached. A batch the card had
        // not reached was still running while this one was recorded, which is
        // what the split buys or does not ([`COPY_QUEUE_STATS`], counted once
        // the batch is on the queue).
        let before = lists.flying.back().and_then(|(value, _)| *value).unwrap_or(0);
        // SAFETY: a plain query of our fence.
        let done = unsafe { copies.fence.GetCompletedValue() };
        let mut bytes = 0u64;
        let mut ranges = 0u64;
        // SAFETY: a list whose last batch has completed (`CopyLists::take`),
        // live buffers, and ranges that lie in them.
        let recorded = unsafe {
            (|| -> windows::core::Result<()> {
                allocator.Reset()?;
                list.Reset(&allocator, None)?;
                if direct > lists.order.direct {
                    copies.queue.Wait(&self.fence, direct)?;
                    lists.order.direct_sent(direct);
                }
                // A relay fetch reads these buffers on the direct queue,
                // which this queue does not otherwise wait for: a batch
                // recorded over one of them while that read runs overwrites
                // what it is copying out. The fetch publishes the value its
                // own wait on this queue ends at, and this batch waits for it
                // ([`FETCH_BARRIER`]) before it writes anything.
                let barrier = FETCH_BARRIER
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if let Some((fetch_fence, fetch_value)) = barrier {
                    copies.queue.Wait(&fetch_fence, fetch_value)?;
                }
                for (shadow, dirty) in dirty {
                    let Some((staging, _, _)) = shadow.card_sides() else {
                        continue;
                    };
                    for &(a, b) in dirty {
                        list.CopyBufferRegion(
                            &shadow.buffer,
                            a as u64,
                            staging,
                            a as u64,
                            (b - a) as u64,
                        );
                        bytes += (b - a) as u64;
                        ranges += 1;
                    }
                    // A copy queue may touch a copy only in `COMMON` or a
                    // copy state, and the next reader is on the direct
                    // queue, which cannot take it in `COPY_DEST` from here:
                    // the batch leaves it in `COMMON`, where a buffer is
                    // read and written without a transition (D3D12's state
                    // decay, [`copied_state`]).
                    if !dirty.is_empty() {
                        list.ResourceBarrier(&[transition(
                            &shadow.buffer,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            D3D12_RESOURCE_STATE_COMMON,
                        )]);
                    }
                }
                list.Close()?;
                copies.queue.ExecuteCommandLists(&[Some(list.cast()?)]);
                Ok(())
            })()
        };
        if let Err(e) = recorded {
            // Nothing reached the card: the list is closed again, free to
            // record, and the caller copies in its own list.
            // SAFETY: a list of ours, recorded in part or not at all.
            let _ = unsafe { list.Close() };
            lists.idle.push((allocator, list));
            tracing::warn!(error = %e, "the copies into the card's memory failed on the copy queue; they go back to the direct queue");
            return None;
        }
        let value = lists.order.sending();
        // SAFETY: our copy queue and its fence; the batch is on the queue.
        let carried = match unsafe { copies.queue.Signal(&copies.fence, value) } {
            Ok(()) => {
                lists.order.signalled(value);
                Some(value)
            }
            // The value this batch would have been named by never reaches the
            // fence, so no wait may name it (`last_signalled`): the copies are
            // drained here instead, before the hash passes below read them.
            Err(e) => {
                tracing::warn!(error = %e, "no signal for the copies into the card's memory");
                lists.drain(&copies.queue, &copies.fence, copies.event)
            }
        };
        // Whether the batch before this one ran beside the direct queue while
        // this one was recorded: one of the two counts for every batch that a
        // later one arrived behind, and neither for the batches still in the
        // air when the run ends.
        if before > 0 {
            COPY_QUEUE_STATS[overlap_slot(before, done)].fetch_add(1, Ordering::Relaxed);
        }
        // A batch with no value at all (its signal and the drain's both
        // failed) is `None`: nothing names its completion, so no wait can be
        // told about it, and [`CopyLists::front_done`] reads it as run behind
        // the first value after it instead of waiting forever on a value that
        // never comes.
        lists.flying.push_back((carried, (allocator, list)));
        crate::shadow::CARD_STATS[0].fetch_add(bytes, Ordering::Relaxed);
        crate::shadow::CARD_STATS[1].fetch_add(ranges, Ordering::Relaxed);
        COPY_QUEUE_STATS[0].fetch_add(1, Ordering::Relaxed);
        COPY_QUEUE_STATS[1].fetch_add(bytes, Ordering::Relaxed);
        // The direct queue waits for the batch before its next list: the
        // hash passes below read the copies it wrote, and so does the
        // submission. A batch nothing can be told about waits for nothing:
        // its copies are then ordered only by the submissions that follow.
        if carried.is_some()
            && let Some(value) = lists.order.wait()
        {
            // SAFETY: our copy queue, its fence, and the direct queue.
            let waited = unsafe { self.queue.Wait(&copies.fence, value) };
            if let Err(e) = waited {
                fail(format_args!("the direct queue could not wait for the copies into the card's memory ({e})"));
            }
            lists.order.waited(value);
        }
        // `None` when the copies are not known to have run: the caller copies
        // them in its own list, and reads them as `COPY_DEST`.
        carried
    }

    /// Before a submission's lists run, for copies in the card's memory
    /// ([`crate::shadow::cards`]): what changed on their CPU side since
    /// the last submission goes to their card side (a list of ours, run
    /// now; the submission's lists follow it on the queue), and the ranges
    /// this submission's shaders may write get a compare to run after its
    /// lists. `None` if there is nothing to do.
    ///
    /// `flown`: the submission is sent ahead with [`relay`]. Its compare
    /// then stores the hashes it makes (see [`flagged_pieces`] and
    /// `relay`). `direct`: the last value signalled on the direct queue's
    /// fence, which the copy queue waits for before it writes a copy
    /// ([`Host::card_copies`]).
    fn card_before(&self, runs: &[&HostList], flown: bool, direct: u64) -> Option<CardPlan> {
        use std::sync::Arc;
        if !crate::shadow::cards() {
            return None;
        }
        // The ranges this submission's shaders may write, per copy.
        let mut declared: Vec<(Arc<crate::shadow::Shadow>, Vec<(usize, usize)>)> = Vec::new();
        for list in runs {
            for step in &list.deferred().copies {
                if let Step::GpuWrite(shadow, address, len) = step
                    && shadow.carded()
                {
                    let range = (*address, *address + *len);
                    match declared.iter_mut().find(|(s, _)| Arc::ptr_eq(s, shadow)) {
                        Some((_, ranges)) => {
                            if !ranges.contains(&range) {
                                ranges.push(range);
                            }
                        }
                        None => declared.push((shadow.clone(), vec![range])),
                    }
                }
            }
        }
        // With the relay: the order of the queue is held from here until
        // what is taken is on the queue, and the taking is numbered.
        let mut order = relay().then(|| QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()));
        let sent_as = match order.as_mut() {
            Some(order) => {
                **order += 1;
                **order
            }
            None => 0,
        };
        let mut dirty = crate::shadow::card_dirty(sent_as);
        if declared.is_empty() && dirty.is_empty() {
            return None;
        }
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let work = work.as_mut()?;
        // A copy whose shader writes are declared for the first time gets
        // its bytes from before, whole.
        let mut whole: Vec<Arc<crate::shadow::Shadow>> = Vec::new();
        for (shadow, _) in &declared {
            if shadow.card_make_before(|size| {
                let before = self.uav_buffer(crate::shadow::card_hash_bytes(size))?;
                name(&before, || "title buffer on the card, hashes of its pieces".into());
                Some(before)
            }) {
                whole.push(shadow.clone());
                if !dirty.iter().any(|(s, _)| Arc::ptr_eq(s, shadow)) {
                    dirty.push((shadow.clone(), Vec::new()));
                }
            }
        }
        let mut used: Vec<(bool, Prelude)> = Vec::new();
        if !dirty.is_empty() {
            // The copies of the dirty ranges: on the copy queue, beside the
            // flights that are still reading those copies with the bytes
            // from before, or (the switch off, or no list for it) in this
            // list, ahead of the hash passes that read them.
            let copied = self.card_copies(&dirty, direct);
            let Some(sync) = work.sync.pop().or_else(|| self.make_prelude()) else {
                fail(format_args!("no host list to bring host copies in the card's memory up to date"));
            };
            let (allocator, list) = &sync;
            // Where the copies left the copies: `COMMON` after the copy
            // queue's run ([`copied_state`]), `COPY_DEST` in this list.
            let from_state = copied_state(copied.is_some());
            // SAFETY: the list's last run finished (it ran ahead of a
            // submission that was waited for, and came back after that);
            // live buffers, and the ranges lie in them.
            let recorded = unsafe {
                (|| -> windows::core::Result<()> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    for (shadow, ranges) in &dirty {
                        let Some((staging, twin, size)) = shadow.card_sides() else {
                            continue;
                        };
                        if copied.is_none() {
                            for &(a, b) in ranges {
                                list.CopyBufferRegion(&shadow.buffer, a as u64, staging, a as u64, (b - a) as u64);
                                crate::shadow::CARD_STATS[0].fetch_add((b - a) as u64, Ordering::Relaxed);
                            }
                            crate::shadow::CARD_STATS[1].fetch_add(ranges.len() as u64, Ordering::Relaxed);
                        }
                        let Some(before) = twin else {
                            continue;
                        };
                        // No twin (it cost the card the copy's size again,
                        // 399 MB in run 75): `before` holds a 128-bit hash
                        // of each 64 KiB piece. The pieces just sent up are
                        // hashed again, all of them the first time; the
                        // compare after the lists hashes the declared
                        // pieces and flags those whose hash is another.
                        if !ranges.is_empty() {
                            list.ResourceBarrier(&[transition(
                                &shadow.buffer,
                                from_state,
                                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                            )]);
                        }
                        list.SetComputeRootSignature(&work.root);
                        list.SetPipelineState(&work.pso);
                        list.SetComputeRootShaderResourceView(1, shadow.buffer.GetGPUVirtualAddress());
                        list.SetComputeRootUnorderedAccessView(2, before.GetGPUVirtualAddress());
                        list.SetComputeRootUnorderedAccessView(3, work.differs.0.GetGPUVirtualAddress());
                        let runs: Vec<crate::shadow::CardSpan> = if whole.iter().any(|s| Arc::ptr_eq(s, shadow)) {
                            vec![shadow.card_run(0, usize::MAX)]
                        } else {
                            ranges.iter().map(|&(a, b)| shadow.card_run(a, b)).collect()
                        };
                        // The same pieces in fewer passes (`rehash_runs`).
                        CARD_PASSES[3].fetch_add(runs.len() as u64, Ordering::Relaxed);
                        CARD_PASSES[4].fetch_add(runs.iter().map(|r| u64::from(r.pieces)).sum::<u64>(), Ordering::Relaxed);
                        let runs = if rehash_runs() { crate::shadow::joined_piece_runs(runs) } else { runs };
                        CARD_PASSES[0].fetch_add(runs.len() as u64, Ordering::Relaxed);
                        CARD_PASSES[1].fetch_add(runs.iter().map(|r| u64::from(r.pieces)).sum::<u64>(), Ordering::Relaxed);
                        for run in runs {
                            let constants = [run.first, run.skew, size as u32, run.pieces, 0, 0];
                            list.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
                            list.Dispatch(run.pieces.div_ceil(64), 1, 1);
                        }
                    }
                    list.Close()?;
                    self.queue.ExecuteCommandLists(&[Some(list.cast()?)]);
                    Ok(())
                })()
            };
            if let Err(e) = recorded {
                fail(format_args!("host copies in the card's memory could not be brought up to date ({e})"));
            }
            used.push((false, sync));
        }
        drop(order);
        let mut plan = CardPlan {
            ranges: Vec::new(),
            check: None,
            _synced: dirty.into_iter().map(|(shadow, _)| shadow).collect(),
            lists: used,
        };
        // Each byte once, as the copy-back joins the declared ranges. The
        // results go behind those of plans still in the air.
        let mut at = work.taken;
        for (shadow, ranges) in &declared {
            let mut sorted = ranges.clone();
            sorted.sort_unstable();
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for (from, to) in sorted {
                match spans.last_mut() {
                    Some(last) if from <= last.1 => last.1 = last.1.max(to),
                    _ => spans.push((from, to)),
                }
            }
            let twin = shadow.card_sides().is_some_and(|(_, before, _)| before.is_some());
            for (from, to) in spans {
                let Some(span) = shadow.card_span(from, to) else {
                    continue;
                };
                let compared = twin && u64::from(at + span.pieces) * 4 <= work.differs.2;
                plan.ranges.push(CardRange {
                    shadow: shadow.clone(),
                    declared: (from, to),
                    span,
                    at,
                    compared,
                });
                if compared {
                    at += span.pieces;
                }
            }
        }
        work.taken = at;
        if plan.ranges.iter().any(|r| r.compared) {
            let Some(check) = work.check.pop().or_else(|| self.make_prelude()) else {
                fail(format_args!("no host list for the compare of host copies in the card's memory"));
            };
            let (allocator, list) = &check;
            // SAFETY: the list's last run finished (its submission was
            // waited for, and it came back after that); live buffers; the
            // compare's own root signature.
            let recorded = unsafe {
                (|| -> windows::core::Result<ID3D12CommandList> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    // What the submission's shaders wrote, first.
                    list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                        Anonymous: D3D12_RESOURCE_BARRIER_0 {
                            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                                pResource: std::mem::ManuallyDrop::new(None),
                            }),
                        },
                    }]);
                    list.SetComputeRootSignature(&work.root);
                    list.SetPipelineState(&work.pso);
                    list.SetComputeRootUnorderedAccessView(3, work.differs.0.GetGPUVirtualAddress());
                    for r in plan.ranges.iter().filter(|r| r.compared) {
                        let Some((_, Some(before), size)) = r.shadow.card_sides() else {
                            continue;
                        };
                        let constants = [r.span.first, r.span.skew, size as u32, r.span.pieces, r.at * 4, if flown { 2 } else { 1 }];
                        list.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
                        list.SetComputeRootShaderResourceView(1, r.shadow.buffer.GetGPUVirtualAddress());
                        list.SetComputeRootUnorderedAccessView(2, before.GetGPUVirtualAddress());
                        list.Dispatch(r.span.pieces.div_ceil(64), 1, 1);
                        CARD_PASSES[2].fetch_add(1, Ordering::Relaxed);
                    }
                    list.Close()?;
                    list.cast()
                })()
            };
            match recorded {
                Ok(list) => plan.check = Some(list),
                Err(e) => fail(format_args!("the compare of host copies in the card's memory could not be recorded ({e})")),
            }
            plan.lists.push((true, check));
        }
        work.out += 1;
        Some(plan)
    }

    /// With [`relay`]: whether the compare's result area is half taken by
    /// plans in the air. It starts from the beginning only when no plan is
    /// out, so the caller lets the flights land first.
    fn card_results_low(&self) -> bool {
        CARD_WORK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|work| u64::from(work.taken) * 8 > work.differs.2)
    }

    /// The plan's submission has run and its results were read
    /// ([`Host::card_back`]): its lists are free for the next plan. Called
    /// when nothing of ours is on the card, so the compare's results start
    /// from the beginning again.
    fn card_done(&self, plan: CardPlan) {
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(work) = work.as_mut() else {
            return;
        };
        for (check, list) in plan.lists {
            if check { work.check.push(list) } else { work.sync.push(list) }
        }
        work.out = work.out.saturating_sub(1);
        // With the relay other plans may be on the card, their results
        // not read yet: the area starts again only when none is out.
        if !relay() || work.out == 0 {
            work.taken = 0;
        }
    }

    /// The relay's own list, fence, event for [`Host::relay_card_fetch`].
    fn make_relay_fetch(&self) -> Option<RelayFetch> {
        let list = self.make_prelude()?;
        // SAFETY: plain objects of our own.
        unsafe {
            let fence: ID3D12Fence = self.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
            let event = CreateEventW(None, false, false, None).ok()?;
            Some(RelayFetch {
                list,
                fence,
                event,
                sent: 0,
                buffer: None,
            })
        }
    }

    /// [`relay`], on the thread that lands flights: for the plans of the
    /// flights being landed (the card has run them), the pieces their
    /// compares flagged are fetched, once each, with a list and a wait of
    /// this thread's own, and what shaders wrote in them goes into title
    /// memory (`Shadow::card_merge_relay`). As [`Host::card_back`], but:
    /// no submission lock; a piece flagged under one range of a plan is
    /// fetched under all ([`flagged_pieces`]); the fetch runs behind
    /// whatever was sent meanwhile, so it may bring a later flight's
    /// bytes, which that flight's landing then finds merged already; and
    /// nothing is sent back up.
    fn relay_card_fetch(&self, plans: &[&CardPlan], fetch: &mut Option<RelayFetch>) -> Option<FetchRun> {
        let differs = {
            let work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
            work.as_ref().map(|work| (work.differs.1, (work.differs.2 / 4) as usize))
        };
        let mut pieces: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize, u64)> = Vec::new();
        if let Some((differs, dwords)) = differs {
            // SAFETY: the mapping of the results. The compares of these
            // plans have run, and their dwords are given to no other plan
            // before these are done with (`card_done`).
            let differs = unsafe { std::slice::from_raw_parts(differs as *const u32, dwords) };
            let mut seen = std::collections::HashSet::new();
            for plan in plans {
                let flags: Vec<(usize, u32, bool)> = plan
                    .ranges
                    .iter()
                    .flat_map(|r| {
                        (0..r.span.pieces).map(move |p| {
                            let flagged = !r.compared || differs.get((r.at + p) as usize).is_none_or(|&d| d != 0);
                            (std::sync::Arc::as_ptr(&r.shadow) as usize, r.span.first + p, flagged)
                        })
                    })
                    .collect();
                let fetched = flagged_pieces(&flags);
                for r in &plan.ranges {
                    let copy = std::sync::Arc::as_ptr(&r.shadow) as usize;
                    for p in 0..r.span.pieces {
                        if !fetched.contains(&(copy, r.span.first + p)) {
                            continue;
                        }
                        let (lo, hi) = r.shadow.card_piece(&r.span, p);
                        if lo < hi && seen.insert((copy, lo, hi)) {
                            pieces.push((r.shadow.clone(), lo, hi, 0));
                        }
                    }
                    crate::shadow::CARD_STATS[2].fetch_add(u64::from(r.span.pieces), Ordering::Relaxed);
                }
            }
        }
        // The pieces of one copy that touch or overlap are one copy on the
        // card (`shadow::fetch_runs`), and each piece still merges where it
        // lies: nothing is stored into the card's memory, and what a run
        // reads past its pieces is never merged.
        let who: Vec<(usize, usize, usize)> = pieces
            .iter()
            .map(|(shadow, lo, hi, _)| (std::sync::Arc::as_ptr(shadow) as usize, *lo, *hi))
            .collect();
        let (runs, at_of, total) = crate::shadow::fetch_runs(&who);
        for (piece, at) in pieces.iter_mut().zip(at_of) {
            piece.3 = at;
        }
        // Each copy's card side, by the identity the runs carry.
        let mut sides: Vec<(usize, &ID3D12Resource)> = Vec::new();
        for (shadow, _, _, _) in &pieces {
            let id = std::sync::Arc::as_ptr(shadow) as usize;
            if !sides.iter().any(|(k, _)| *k == id) {
                sides.push((id, &shadow.buffer));
            }
        }
        if pieces.is_empty() {
            // Nothing to fetch: every send so far is older than the next fetch.
            crate::shadow::relay_fetched(*QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()));
            return None;
        }
        if fetch.is_none() {
            *fetch = self.make_relay_fetch();
        }
        let Some(f) = fetch.as_mut() else {
            fail(format_args!("no list, fence or event to fetch what shaders wrote to the card's memory (VICTORY_RELAY=1)"));
        };
        if f.buffer.as_ref().is_none_or(|b| b.2 < total) {
            let size = total.next_multiple_of(16 << 20);
            f.buffer = self.shared_buffer(size).map(|(buffer, data)| (buffer, data as usize, size));
        }
        let Some((buffer, data, _)) = f.buffer.clone() else {
            fail(format_args!("no memory to fetch what shaders wrote to the card's memory ({total} bytes, VICTORY_RELAY=1)"));
        };
        f.sent += 1;
        let ((allocator, list), fence, sent) = (f.list.clone(), f.fence.clone(), f.sent);
        // The copy queue's batches write the same card-side buffers this fetch
        // reads, and it is a queue of its own: the fetch waits for the last
        // batch signalled so far ([`CopyOrder`]), and the batches recorded from
        // here on wait for this fetch, published as [`FETCH_BARRIER`].
        //
        // Both under [`QUEUE_ORDER`], held from the publication to this fetch's
        // own signal, list included: a batch takes its number and reads that
        // barrier while it holds the order ([`Host::card_before`]), so no batch
        // is recorded between the two. A batch recorded before the publication
        // waited for nothing newer than the value this fetch waits for; one
        // recorded after it waits for this fetch, whose list — the only thing
        // that signals the value that batch waits for — is then already on the
        // direct queue, ahead of the direct queue's own wait for that batch.
        //
        // Taking the order only after the publication gave the lists back is
        // what used to break that: a batch recorded in the gap waited for this
        // fetch, the direct queue then waited for that batch, and this fetch's
        // list was enqueued behind that wait. Both queues waited for each
        // other, and neither moved again (a TDR).
        let copies = self.copies.as_ref();
        // SAFETY: the list's last run was waited for by this thread; live
        // buffers (the copies are held by the plans); the fence and the
        // event are this thread's.
        let ran = unsafe {
            (|| -> windows::core::Result<u64> {
                allocator.Reset()?;
                list.Reset(&allocator, None)?;
                for &(copy, lo, hi, at) in &runs {
                    if let Some(from) = sides.iter().find(|(k, _)| *k == copy).map(|(_, b)| *b) {
                        list.CopyBufferRegion(&buffer, at, from, lo as u64, (hi - lo) as u64);
                    }
                }
                list.Close()?;
                let lists = [Some(list.cast::<ID3D12CommandList>()?)];
                // In the order of the queue: the sends of title bytes so
                // far are ahead of this fetch, every later one behind it.
                let order = QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner());
                let mut published = false;
                let enqueued = (|| -> windows::core::Result<()> {
                    if let Some(copies) = copies {
                        // The copy queue's last signalled batch, and the value
                        // this fetch ends at, in one hold of its lists: a batch
                        // recorded from here on waits for the fetch before it
                        // writes a buffer the fetch reads, whether or not a
                        // batch was sent before this one.
                        let wait = {
                            let lists = copies.lists.lock().unwrap_or_else(|e| e.into_inner());
                            let wait = fetch_waits(&lists.order);
                            *FETCH_BARRIER.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some((fence.clone(), sent * 2));
                            wait
                        };
                        published = true;
                        if let Some(wait) = wait {
                            self.queue.Wait(&copies.fence, wait)?;
                        }
                    }
                    // A signal before the fetch: when it completes, everything
                    // sent before the fetch has run, and the time until it did
                    // is the waiting behind that work, not the fetch's own.
                    self.queue.Signal(&fence, sent * 2 - 1)?;
                    self.queue.ExecuteCommandLists(&lists);
                    // The fetch's own value, last: a batch waits for it, so it
                    // must be on the queue before the order is let go.
                    self.queue.Signal(&fence, sent * 2)?;
                    Ok(())
                })();
                let after = match enqueued {
                    Ok(()) => *order,
                    Err(e) => {
                        // No fetch reached the queue, or the value a batch would
                        // wait for never will: the barrier goes back, and under
                        // the order, so that no batch is recorded against it.
                        if published {
                            let mut barrier =
                                FETCH_BARRIER.lock().unwrap_or_else(|e| e.into_inner());
                            if barrier.as_ref().is_some_and(|(_, value)| *value == sent * 2) {
                                *barrier = None;
                            }
                        }
                        return Err(e);
                    }
                };
                drop(order);
                self.card_sent();
                Ok(after)
            })()
        };
        let after = match ran {
            Ok(after) => after,
            Err(e) => {
                fail(format_args!("what shaders wrote to the card's memory could not be fetched ({e}, VICTORY_RELAY=1)"))
            }
        };
        Some(FetchRun { pieces, data, total, sent, after, runs: runs.len() })
    }

    /// [`Host::relay_card_fetch`]'s wait: the card is waited for, and what
    /// shaders wrote in the fetched pieces goes into title memory
    /// (`Shadow::card_merge_relay`). Runs after the copies in the PC's
    /// memory are merged, so the card runs the fetch while the landing
    /// thread has work of its own.
    fn relay_fetch_merge(&self, run: FetchRun, fetch: &Option<RelayFetch>) {
        let FetchRun { pieces, data, total, sent, after, runs } = run;
        let Some(fetched) = fetch.as_ref() else {
            return;
        };
        let (fence, event) = (fetched.fence.clone(), fetched.event);
        // SAFETY: this thread's fence and event, and the two signals the
        // waits are for were sent by `relay_card_fetch`.
        //
        // Each wait is timed on its own: between the two, this thread has
        // merged copies of its own, which is not time the card kept it.
        let waiting = std::time::Instant::now();
        if let Err(e) = unsafe { fence.SetEventOnCompletion(sent * 2 - 1, event) } {
            fail(format_args!("what shaders wrote to the card's memory could not be waited for ({e}, VICTORY_RELAY=1)"));
        }
        unsafe { WaitForSingleObject(event, INFINITE) };
        let drained = waiting.elapsed();
        // The fetch's own run: what the card takes for the copies, once the
        // work sent before it is done (nothing if it had run by the time
        // the signal before it completed).
        let running = std::time::Instant::now();
        // SAFETY: this thread's fence and event.
        let waited = unsafe { fence.SetEventOnCompletion(sent * 2, event) }
            .map(|()| unsafe { WaitForSingleObject(event, INFINITE) });
        if let Err(e) = waited {
            fail(format_args!("the fetch of what shaders wrote to the card's memory could not be waited for ({e}, VICTORY_RELAY=1)"));
        }
        let own = running.elapsed();
        let us = (drained + own).as_micros() as u64;
        RELAY_STATS[3].fetch_add(us, Ordering::Relaxed);
        RELAY_STATS[5].fetch_add(drained.as_micros() as u64, Ordering::Relaxed);
        crate::flow::put(crate::flow::Site::Fetched, total as usize, own.as_nanos() as u64);
        {
            use crate::shadow::CARD_STATS;
            CARD_STATS[3].fetch_add(pieces.len() as u64, Ordering::Relaxed);
            CARD_STATS[4].fetch_add(total, Ordering::Relaxed);
            CARD_STATS[5].fetch_add(1, Ordering::Relaxed);
            CARD_STATS[6].fetch_add(us, Ordering::Relaxed);
            CARD_STATS[7].fetch_add(runs as u64, Ordering::Relaxed);
        }
        let mut title = Vec::new();
        let mut written = 0;
        for (shadow, lo, hi, at) in &pieces {
            // SAFETY: the mapping of the fetched bytes, `total` of them;
            // the copy into it has run.
            let gpu = unsafe { std::slice::from_raw_parts((data + *at as usize) as *const u8, hi - lo) };
            written += shadow.card_merge_relay(*lo, gpu, after, &mut title);
        }
        COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
        // Sends up to `after` are of no use to any later merge.
        crate::shadow::relay_fetched(after);
    }

    /// [`relay`], on the thread that lands flights: what their shaders
    /// wrote in copies in the PC's memory goes into title memory
    /// (`relay_hand_back`), then their ranges are done with, each by its
    /// own number: a range another flight in the air declared too stays
    /// closed to refreshes. `run` is the card copies' fetch this landing
    /// made, if any: it is waited for between the two, so the card runs it
    /// while this thread compares and merges the others.
    fn merge_flights(&self, flights: &[&Flight], run: Option<FetchRun>, fetch: &Option<RelayFetch>) {
        if flights.is_empty() {
            // The fetch was sent all the same, and its list must not be
            // reset until it has run.
            if let Some(run) = run {
                self.relay_fetch_merge(run, fetch);
            }
            return;
        }
        let mut back: Vec<Declared> = Vec::new();
        for flight in flights {
            for (shadow, ranges) in &flight.back {
                match back.iter_mut().find(|(s, _)| std::sync::Arc::ptr_eq(s, shadow)) {
                    Some((_, all)) => {
                        for range in ranges {
                            if !all.contains(range) {
                                all.push(*range);
                            }
                        }
                    }
                    None => back.push((shadow.clone(), ranges.clone())),
                }
            }
        }
        relay_hand_back(&back);
        if let Some(run) = run {
            self.relay_fetch_merge(run, fetch);
        }
        for flight in flights {
            for (shadow, ranges) in &flight.back {
                shadow.copy_back_done(ranges);
            }
        }
        let mut line = line();
        for flight in flights {
            line.held.merged(flight.number);
        }
    }

    /// [`relay`], on the thread that lands flights: `items` are the first
    /// things of the line, flights the card has run and what follows them.
    /// What their shaders wrote goes into title memory (copies in the
    /// card's memory first, then the others, each byte once whichever
    /// flights declared it); then every flight's ranges are done with;
    /// then, in the line's order, each flight's read-backs, its
    /// bottom-of-pipe writes and the fence signals behind it. Title memory
    /// is complete before the title is told anything.
    ///
    /// With [`lazy_merge`], a flight whose landing tells the title nothing
    /// (no bottom-of-pipe write, no fence signal after it in this landing)
    /// keeps its ranges held and is merged later ([`drain_deferred`]): the
    /// copies in the PC's memory are the ones that wait.
    fn relay_land(&self, items: Vec<Late>, fetch: &mut Option<RelayFetch>) {
        let landing = std::time::Instant::now();
        // What an earlier landing held back is merged first: this is one of
        // the points a read, a present or a fence comes by, and the items
        // of this landing may tell the title its writes are there.
        drain_deferred();
        {
            let flights: Vec<&Flight> = items
                .iter()
                .filter_map(|l| match l {
                    Late::Run(flight) => Some(flight),
                    _ => None,
                })
                .collect();
            if !flights.is_empty() {
                // SAFETY: a plain query.
                if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                    let runs: Vec<&HostList> = flights.iter().flat_map(|f| f.order.iter().map(|&i| f.lists[i].host())).collect();
                    fail(format_args!(
                        "host GPU device removed after {} submissions sent ahead (VICTORY_RELAY=1) of {} lists{}{}",
                        flights.len(),
                        runs.len(),
                        self.removal_data(&runs),
                        last_notes(&runs)
                    ));
                }
                let plans: Vec<&CardPlan> = flights.iter().filter_map(|f| f.card.as_ref()).collect();
                // The fetch goes to the queue first and is waited for
                // after the copies in the PC's memory are merged: the card
                // runs it while this thread compares and merges its own.
                let run = self.relay_card_fetch(&plans, fetch);
                // The flights this landing does not merge ([`lazy_merge`]):
                // those the line after them in this landing tells the title
                // nothing about. They keep their ranges held, so no refresh
                // writes over what their shaders wrote before it is in
                // title memory.
                let mut held_back: Vec<u64> = Vec::new();
                if lazy_merge() {
                    for (i, item) in items.iter().enumerate() {
                        if let Late::Run(flight) = item
                            && holds_back(!flight.back.is_empty(), &items[i + 1..])
                        {
                            held_back.push(flight.number);
                        }
                    }
                }
                let (merging, later): (Vec<&Flight>, Vec<&Flight>) =
                    flights.iter().partition(|f| !held_back.contains(&f.number));
                self.merge_flights(&merging, run, fetch);
                // What this landing does not merge, pushed now or merged
                // now: `settles` is read and the entries are pushed in one
                // lock section, so a settle is either counted there (and
                // its flights are merged here) or drains them itself.
                let hold = {
                    let mut line = line();
                    if line.settles > 0 {
                        false
                    } else {
                        for flight in &later {
                            line.deferred.push(HeldBack {
                                number: flight.number,
                                back: flight.back.clone(),
                            });
                        }
                        LATE_STATS[DEFERRED].fetch_add(later.len() as u64, Ordering::Relaxed);
                        true
                    }
                };
                if !hold {
                    self.merge_flights(&later, None, fetch);
                }
                COPY_BACK[0].fetch_add(landing.elapsed().as_micros() as u64, Ordering::Relaxed);
                RELAY_STATS[0].fetch_add(1, Ordering::Relaxed);
                RELAY_STATS[1].fetch_add(flights.len() as u64, Ordering::Relaxed);
            }
        }
        // In the order given: what tells the title the card is done.
        let mut last = 0;
        let mut landed = Vec::new();
        for item in items {
            match item {
                Late::Run(flight) => {
                    last = flight.number;
                    for &i in &flight.order {
                        read_backs(flight.lists[i].host());
                    }
                    if let Some(card) = flight.card {
                        self.card_done(card);
                    }
                    landed.push((flight.lists, flight.order));
                }
                Late::Write(at, what, bytes) => what.write(at as *mut u8, bytes),
                Late::Signal(fence, value) => crate::fence::set(fence as crate::object::This, value),
            }
        }
        if last > 0 {
            FINISHED.store(last, Ordering::Release);
            bury(last);
        }
        self.log_messages();
        RELAY_STATS[4].fetch_add(landing.elapsed().as_micros() as u64, Ordering::Relaxed);
        {
            let mut line = line();
            let Line {
                line: items,
                lists,
                settling,
                poke,
                landed: kept,
                in_hand,
                ..
            } = &mut *line;
            kept.extend(landed);
            in_hand.clear();
            // What is still in the air, from the flights that are. What
            // they declared is held already (`Held`), each by its number,
            // and what this landing merged is not (`Held::merged`); what
            // it held back is, until its own merge (`Line::deferred`).
            *lists = 0;
            for item in items.iter() {
                if let Late::Run(flight) = item {
                    *lists += flight.order.len().max(1);
                }
            }
            *settling = false;
            // The landed lists are reset, and what queues hold looked at,
            // by the helper thread (they take locks a waiter may hold).
            *poke = true;
        }
        LINE_MOVED.notify_all();
        LINE_LANDED.notify_all();
    }

    /// After a submission ran, for copies in the card's memory: the pieces
    /// of the declared ranges that the compare found differing (all of a
    /// range it did not compare) are fetched, with one more wait for the
    /// card, and what shaders wrote in them goes into title memory.
    /// `submitted` is the last value signalled on the fence. `seen` holds
    /// the pieces fetched already since the card was last waited for
    /// (several submissions settled together name the same ones).
    fn card_back(
        &self,
        plan: &CardPlan,
        submitted: &mut u64,
        seen: &mut std::collections::HashSet<(usize, usize, usize)>,
    ) {
        if plan.ranges.is_empty() {
            return;
        }
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(work) = work.as_mut() else {
            return;
        };
        // SAFETY: the mapping of the results, which the compare (it has
        // run) wrote.
        let differs = unsafe { std::slice::from_raw_parts(work.differs.1 as *const u32, (work.differs.2 / 4) as usize) };
        // The pieces to fetch: the range, the offsets in its copy, and
        // where the bytes come down. Pieces of one copy that touch or
        // overlap are one copy on the card (`shadow::fetch_runs`), and each
        // piece still merges where it lies: a piece the title wrote and the
        // card did not keeps the title's value, and the bytes a run reaches
        // past its pieces are read and never merged.
        let mut pieces: Vec<(usize, usize, usize, u64)> = Vec::new();
        for (i, r) in plan.ranges.iter().enumerate() {
            for p in 0..r.span.pieces {
                if r.compared && differs[(r.at + p) as usize] == 0 {
                    continue;
                }
                let (lo, hi) = r.shadow.card_piece(&r.span, p);
                if lo < hi && seen.insert((std::sync::Arc::as_ptr(&r.shadow) as usize, lo, hi)) {
                    pieces.push((i, lo, hi, 0));
                }
            }
            crate::shadow::CARD_STATS[2].fetch_add(u64::from(r.span.pieces), Ordering::Relaxed);
        }
        // What the runs name a copy by, and where each piece's bytes land.
        let who: Vec<(usize, usize, usize)> = pieces
            .iter()
            .map(|&(i, lo, hi, _)| (std::sync::Arc::as_ptr(&plan.ranges[i].shadow) as usize, lo, hi))
            .collect();
        let (runs, at_of, total) = crate::shadow::fetch_runs(&who);
        for (piece, at) in pieces.iter_mut().zip(at_of) {
            piece.3 = at;
        }
        // Each copy's card side, by the identity the runs carry: two ranges
        // of one copy name one buffer.
        let mut sides: Vec<(usize, &ID3D12Resource)> = Vec::new();
        for r in &plan.ranges {
            let id = std::sync::Arc::as_ptr(&r.shadow) as usize;
            if !sides.iter().any(|(k, _)| *k == id) {
                sides.push((id, &r.shadow.buffer));
            }
        }
        let mut written = vec![0usize; plan.ranges.len()];
        if !pieces.is_empty() {
            if work.fetched.as_ref().is_none_or(|f| f.2 < total) {
                let size = total.next_multiple_of(16 << 20);
                work.fetched = self.shared_buffer(size).map(|(buffer, data)| (buffer, data as usize, size));
            }
            let Some((fetched, data, _)) = &work.fetched else {
                fail(format_args!("no memory to fetch what shaders wrote to the card's memory ({total} bytes)"));
            };
            let waiting = std::time::Instant::now();
            let (allocator, list) = &work.fetch;
            // SAFETY: the list's last run was waited for; live buffers; the
            // fence and event are ours, under the submission lock.
            let ran = unsafe {
                (|| -> windows::core::Result<()> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    for &(copy, lo, hi, at) in &runs {
                        if let Some(buffer) = sides.iter().find(|(k, _)| *k == copy).map(|(_, b)| *b) {
                            list.CopyBufferRegion(fetched, at, buffer, lo as u64, (hi - lo) as u64);
                        }
                    }
                    list.Close()?;
                    self.queue.ExecuteCommandLists(&[Some(list.cast()?)]);
                    self.card_sent();
                    *submitted += 1;
                    self.queue.Signal(&self.fence, *submitted)?;
                    self.fence.SetEventOnCompletion(*submitted, self.event)?;
                    let was_working = working(false);
                    WaitForSingleObject(self.event, INFINITE);
                    stood(waiting);
                    working(was_working);
                    Ok(())
                })()
            };
            if let Err(e) = ran {
                fail(format_args!("what shaders wrote to the card's memory could not be fetched ({e})"));
            }
            {
                use crate::shadow::CARD_STATS;
                CARD_STATS[3].fetch_add(pieces.len() as u64, Ordering::Relaxed);
                CARD_STATS[4].fetch_add(total, Ordering::Relaxed);
                CARD_STATS[5].fetch_add(1, Ordering::Relaxed);
                CARD_STATS[6].fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
                CARD_STATS[7].fetch_add(runs.len() as u64, Ordering::Relaxed);
            }
            if wide_merge() && pieces.len() >= 8 {
                // The pieces are apart from one another (each its own bytes
                // of its copy and of title memory): several at a time.
                let fetched = *data;
                let counts: std::sync::Arc<Vec<std::sync::atomic::AtomicUsize>> =
                    std::sync::Arc::new((0..plan.ranges.len()).map(|_| std::sync::atomic::AtomicUsize::new(0)).collect());
                let each = pieces.len().div_ceil(8);
                let jobs: Vec<Box<dyn FnOnce() + Send>> = pieces
                    .chunks(each)
                    .map(|chunk| {
                        let chunk: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize, usize, u64)> =
                            chunk.iter().map(|&(i, lo, hi, at)| (plan.ranges[i].shadow.clone(), i, lo, hi, at)).collect();
                        let counts = counts.clone();
                        Box::new(move || {
                            let mut title = Vec::new();
                            for (shadow, i, lo, hi, at) in chunk {
                                // SAFETY: the mapping of the fetched bytes;
                                // the copy into it has run, and the caller
                                // waits for this job.
                                let gpu = unsafe { std::slice::from_raw_parts((fetched + at as usize) as *const u8, hi - lo) };
                                counts[i].fetch_add(shadow.card_merge(lo, gpu, &mut title), Ordering::Relaxed);
                            }
                        }) as Box<dyn FnOnce() + Send>
                    })
                    .collect();
                crate::shadow::wide_run(jobs);
                for (written, count) in written.iter_mut().zip(counts.iter()) {
                    *written += count.load(Ordering::Relaxed);
                }
            } else {
                let mut title = Vec::new();
                for &(i, lo, hi, at) in &pieces {
                    // SAFETY: the mapping of the fetched bytes, `total` of
                    // them; the copy into it has run.
                    let gpu = unsafe { std::slice::from_raw_parts((*data + at as usize) as *const u8, hi - lo) };
                    written[i] += plan.ranges[i].shadow.card_merge(lo, gpu, &mut title);
                }
            }
        }
        for (r, written) in plan.ranges.iter().zip(written) {
            COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
            let (from, to) = r.declared;
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(from, to - from, written);
            let _ = (from, to);
        }
    }

    /// Runs closed lists and waits for them, and with [`late`] for
    /// everything sent to the card before them, which is settled first.
    pub(crate) fn execute(&self, runs: &[&HostList]) {
        let _ = self.submit(runs, false);
    }

    /// A queue's submission with [`late`]: the host lists `lists`, taken
    /// from the title's, go to the card in `order` and are not waited for.
    /// They join the line of flights and are settled, with the others in
    /// the air, at the next point where what they do can be observed
    /// ([`late`] lists those points); their host lists are reset and kept
    /// for reuse then, as a queue does for the lists it ran.
    pub(crate) fn execute_late(&self, lists: Vec<crate::list::Detached>, order: Vec<usize>) {
        if order.is_empty() {
            crate::list::recycle(lists, &order);
            return;
        }
        late_threads();
        let sent = {
            let runs: Vec<&HostList> = order.iter().map(|&i| lists[i].host()).collect();
            self.submit(&runs, true)
        };
        let Some((mut submitted, sent)) = sent else {
            // (Not reached: a submission sent ahead always comes back.)
            crate::list::recycle(lists, &order);
            return;
        };
        let told = !sent.writes.is_empty();
        let most = {
            let mut line = line();
            let Line { line: items, held, lists: flying, target, .. } = &mut *line;
            held.declare(sent.back.iter().flat_map(|(_, ranges)| ranges.iter().copied()), sent.number);
            *flying += order.len();
            *target = *submitted;
            items.push(Late::Run(Flight {
                number: sent.number,
                fence: *target,
                lists,
                order,
                card: sent.card,
                back: sent.back,
            }));
            items.extend(sent.writes.into_iter().map(|(at, what, bytes)| Late::Write(at, what, bytes)));
            *flying >= late_most()
        };
        LATE_STATS[0].fetch_add(1, Ordering::Relaxed);
        RELAY_MOVED.notify_one();
        if told && !late_writes() {
            self.settle(&mut submitted, WHY_WRITES);
        } else if most && relay_exact() {
            // Until there are fewer, not until there are none.
            relay_wait(WHY_MOST, |line| line.lists < late_most());
        } else if most {
            self.settle(&mut submitted, WHY_MOST);
        } else {
            LINE_MOVED.notify_one();
        }
        drop(submitted);
        land();
    }

    /// Sends `list`, a closed list of ours (the present's copy), as a
    /// flight ([`late_free_present`]): not waited for, settled with the
    /// others. The caller keeps the list and does not reset it before the
    /// submission whose number comes back has finished ([`FINISHED`]); 0 if
    /// it ran and was waited for after all.
    fn execute_own(&self, list: &HostList) -> u64 {
        late_threads();
        let Some((submitted, sent)) = self.submit(&[list], true) else {
            return 0;
        };
        let number = sent.number;
        {
            let mut line = line();
            let Line { line: items, held, lists: flying, target, .. } = &mut *line;
            held.declare(sent.back.iter().flat_map(|(_, ranges)| ranges.iter().copied()), number);
            *flying += 1;
            *target = *submitted;
            items.push(Late::Run(Flight {
                number,
                fence: *target,
                lists: Vec::new(),
                order: Vec::new(),
                card: sent.card,
                back: sent.back,
            }));
            items.extend(sent.writes.into_iter().map(|(at, what, bytes)| Late::Write(at, what, bytes)));
        }
        LATE_STATS[0].fetch_add(1, Ordering::Relaxed);
        LINE_MOVED.notify_one();
        RELAY_MOVED.notify_one();
        drop(submitted);
        number
    }

    /// Waits for the card to run every flight in the air ([`late`]) and
    /// does what they left to do, as [`Host::execute`] does for one
    /// submission after its wait: the pieces their shaders wrote in copies
    /// in the card's memory are fetched and merged (each once), the ranges
    /// they declared in the other copies are compared and copied back
    /// (each byte once, whichever flights declared it), and then, in the
    /// order they were given: their bottom-of-pipe writes and the fence
    /// signals behind them. `submitted` is the submission lock's value;
    /// `why` one of the `WHY_` places. Nothing happens if nothing flies.
    ///
    /// Everything in the air is settled, never the first flights alone:
    /// the merge writes the copies' CPU side, which a flight still on the
    /// card may be writing too.
    fn settle(&self, submitted: &mut u64, why: usize) {
        if !late() {
            return;
        }
        if relay() {
            // Nothing is settled here: the thread that lands flights does
            // it, in order, and this waits until nothing is in the air and
            // nothing is being landed. Every point that settled before
            // comes here, so each still sees title memory, fences and
            // bottom-of-pipe values as after a settle.
            //
            // Counted before anything is looked at: a landing that would
            // hold a merge back ([`lazy_merge`]) merges it instead when it
            // sees this, and one that has held it back already has left it
            // in the line for this to drain, so either order ends with the
            // bytes in title memory before the wait below is done.
            {
                let mut line = line();
                line.settles += 1;
            }
            drain_deferred();
            relay_wait(why, settled);
            {
                let mut line = line();
                line.settles -= 1;
            }
            return;
        }
        let was_working = working(true);
        let mut counted = false;
        loop {
            let items = {
                let mut line = line();
                if line.line.is_empty() {
                    line.settling = false;
                    break;
                }
                line.settling = true;
                line.held.clear();
                line.lists = 0;
                std::mem::take(&mut line.line)
            };
            let flights = items.iter().filter(|i| matches!(i, Late::Run(_))).count();
            if !counted {
                counted = true;
                LATE_STATS[1].fetch_add(1, Ordering::Relaxed);
                LATE_STATS[why].fetch_add(1, Ordering::Relaxed);
            }
            LATE_STATS[11].fetch_add(flights as u64, Ordering::Relaxed);
            if flights > 0 {
                submitting(2, flights);
                let waiting = std::time::Instant::now();
                // SAFETY: the fence and event are ours, under the
                // submission lock; `submitted` is the last value signalled.
                unsafe {
                    match self.fence.SetEventOnCompletion(*submitted, self.event) {
                        Ok(()) => {
                            working(false);
                            WaitForSingleObject(self.event, INFINITE);
                            stood(waiting);
                            working(true);
                        }
                        Err(e) => tracing::warn!(error = %e, "submissions sent ahead not waited for"),
                    }
                }
                let gpu = waiting.elapsed();
                LATE_STATS[12].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
                LATE_WAITED[why.min(WHY_DYNAMIC)].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
                SUBMISSION_US[1].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
                #[cfg(feature = "diagnostics")]
                crate::copystat::card_waited(gpu.as_micros() as u64, || {
                    format!("{flights} submissions sent ahead, settled together")
                });
                // SAFETY: a plain query.
                if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                    let runs: Vec<&HostList> = items
                        .iter()
                        .filter_map(|i| match i {
                            Late::Run(f) => Some(f),
                            _ => None,
                        })
                        .flat_map(|f| f.order.iter().map(|&i| f.lists[i].host()))
                        .collect();
                    fail(format_args!(
                        "host GPU device removed after {flights} submissions sent ahead (VICTORY_LATE_WAIT=1) of {} lists{}{}",
                        runs.len(),
                        self.removal_data(&runs),
                        last_notes(&runs)
                    ));
                }
                submitting(3, flights);
                let copying = std::time::Instant::now();
                let mut seen = std::collections::HashSet::new();
                let mut back: Vec<Declared> = Vec::new();
                for item in &items {
                    let Late::Run(flight) = item else {
                        continue;
                    };
                    if let Some(card) = &flight.card {
                        self.card_back(card, submitted, &mut seen);
                    }
                    for (shadow, ranges) in &flight.back {
                        match back.iter_mut().find(|(s, _)| std::sync::Arc::ptr_eq(s, shadow)) {
                            Some((_, all)) => {
                                for range in ranges {
                                    if !all.contains(range) {
                                        all.push(*range);
                                    }
                                }
                            }
                            None => back.push((shadow.clone(), ranges.clone())),
                        }
                    }
                }
                hand_back(&back);
                COPY_BACK[0].fetch_add(copying.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
            // In the order given: what tells the title the card is done.
            let mut last = 0;
            let mut landed = Vec::new();
            for item in items {
                match item {
                    Late::Run(flight) => {
                        last = flight.number;
                        for &i in &flight.order {
                            read_backs(flight.lists[i].host());
                        }
                        if let Some(card) = flight.card {
                            self.card_done(card);
                        }
                        landed.push((flight.lists, flight.order));
                    }
                    Late::Write(at, what, bytes) => what.write(at as *mut u8, bytes),
                    Late::Signal(fence, value) => crate::fence::set(fence as crate::object::This, value),
                }
            }
            if last > 0 {
                FINISHED.store(last, Ordering::Release);
                bury(last);
            }
            line().landed.extend(landed);
        }
        if counted {
            self.log_messages();
            submitting(0, 0);
            // What was settled may be what a queue stands at, and the
            // caller may not be one that looks (a read-back, a present).
            if why != WHY_CARD_DONE {
                line().poke = true;
                LINE_MOVED.notify_one();
            }
        }
        working(was_working);
    }

    /// [`Host::execute`], or with `late` the first half of
    /// [`Host::execute_late`]: the lists are prepared and sent, and what is
    /// to be done once they have run comes back with the submission lock
    /// still held.
    fn submit<'a>(&'a self, runs: &[&HostList], late: bool) -> Option<(std::sync::MutexGuard<'a, u64>, Sent)> {
        #[cfg(feature = "diagnostics")]
        let called = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        crate::copystat::submission_begins();
        // One submission at a time from preparation to copy-back: snapshots
        // and the ranges shaders write are per host copy, not per submission.
        let mut submitted = self.submitted.lock().unwrap_or_else(|e| e.into_inner());
        let was_working = working(true);
        submitting(1, runs.len());
        // Flights in the air ([`late`]): a submission that is waited for
        // settles them first, one sent ahead only if preparing it reads
        // what they write.
        if !late {
            self.settle(&mut submitted, WHY_SYNC);
        } else if let Some(why) = late_blocked(runs) {
            if relay_exact() && why == WHY_READ {
                // What an earlier landing held back ([`lazy_merge`]) is
                // merged here, on this thread, because this is the read that
                // must see it. Then only the flights still in the air are
                // waited for, and only until the ones that may write what is
                // read have landed: their copy-back's end, not the card's
                // other work.
                //
                // Announced before the entries are looked at, as a settle
                // is: a landing that would hold a merge back merges it
                // instead while this waits, and one that has already done so
                // has left it in the line for the drain below, so this
                // cannot wait for bytes no one is going to merge.
                {
                    let mut line = line();
                    line.settles += 1;
                }
                drain_deferred();
                relay_wait(why, |line| blocked_by(line.held.joined(), runs, false).is_none());
                {
                    let mut line = line();
                    line.settles -= 1;
                }
            } else {
                self.settle(&mut submitted, why);
            }
        }
        let behind = late && flying();
        // Before this submission counts as started: what the sweep lets go
        // of is named by no list, and nothing runs now (behind flights the
        // sweep waits for a submission that finds none in the air: there is
        // one at every present).
        static SWEPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if !behind {
            sweep();
            SWEPT.store(now_ms(), Ordering::Relaxed);
        } else if late_free_present() && now_ms() > SWEPT.load(Ordering::Relaxed) + 1500 {
            // With the present sent ahead nothing else brings a moment
            // with nothing in the air: one is made for the sweep.
            self.settle(&mut submitted, WHY_SYNC);
            sweep();
            SWEPT.store(now_ms(), Ordering::Relaxed);
        }
        let behind = late && flying();
        // The slots that waited for a texture's host copy, before the
        // lists that may read them run.
        crate::descriptor::make_waiting();
        let this_submission = STARTED.fetch_add(1, Ordering::AcqRel) + 1;
        // The ranges shaders write are counted for it (with the relay).
        crate::shadow::preparing(this_submission);
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        let lock = (started - called).as_micros() as u64;
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::Submissions, 1);
        let mut lists: Vec<Option<ID3D12CommandList>> = Vec::with_capacity(2 * runs.len());
        // Where each run's lists start in `lists`.
        let mut firsts = Vec::with_capacity(runs.len());
        let mut batch = Batch::default();
        // The time a settle of flights takes inside this preparing: the
        // wait for the card and the copy-back have their own places on the
        // frame line (run 92 showed them twice, "preparing" 250 ms for 135).
        let mut settled = std::time::Duration::ZERO;
        // One write-watch asking for everything the submission refreshes
        // (`VICTORY_ASK_ONCE`): per copy, the span of its refreshed ranges.
        if crate::watch::once() {
            // Each range the submission refreshes, as its lists name them:
            // the asking costs with the pages it walks, so the span of a
            // copy's ranges (a whole buffer, from its first to its last)
            // is not what to ask about (run 94: 1,150 ms a frame).
            let mut ranges: Vec<(usize, usize)> = Vec::new();
            for list in runs {
                for step in &list.deferred().copies {
                    if let Step::Refresh(shadow, address, len) = step
                        && !shadow.unwatched()
                    {
                        ranges.push((*address, address.saturating_add(*len)));
                    }
                }
            }
            let asking = std::time::Instant::now();
            crate::watch::ask_once(&ranges);
            phase(PHASE_ASKING, asking);
            // Behind flights: the title wrote memory that a flight's shaders
            // may write too, and this submission refreshes it. The flights
            // are settled, and the asking made again: the copy-back wrote
            // title memory, which other copies of it must see.
            if behind && stale_under_flights(runs) {
                let settling = std::time::Instant::now();
                self.settle(&mut submitted, WHY_STALE);
                settled += settling.elapsed();
                phase(PHASE_SETTLE, settling);
                crate::watch::ask_once(&ranges);
            }
        } else if behind && stale_under_flights(runs) {
            let settling = std::time::Instant::now();
            self.settle(&mut submitted, WHY_STALE);
            settled += settling.elapsed();
            phase(PHASE_SETTLE, settling);
        }
        let recording = std::time::Instant::now();
        for list in runs {
            list.run_deferred(&mut batch);
            firsts.push(lists.len());
            if let Some(prelude) = list.prelude() {
                lists.push(Some(prelude));
            }
            lists.push(list.list.cast().ok());
        }
        phase(PHASE_RECORDING, recording);
        let flushing = std::time::Instant::now();
        batch.flush();
        crate::watch::ask_once_end();
        phase(PHASE_FLUSH, flushing);
        // The snapshots of the ranges shaders may write, if they were left
        // for now (`crate::shadow::wide_back`): the refreshes are done, the
        // lists have not gone to the card.
        {
            let snapping = std::time::Instant::now();
            #[cfg(feature = "diagnostics")]
            let snapshots = std::time::Instant::now();
            crate::shadow::take_snapshots();
            phase(PHASE_SNAPSHOTS, snapping);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::SnapshotUs, snapshots.elapsed().as_micros() as u64);
        }
        // Copies in the card's memory: what changed on their CPU side goes
        // up ahead of the lists, and a compare of what the shaders may
        // write follows them in the same submission.
        let flown = late && relay();
        if flown && self.card_results_low() {
            let settling = std::time::Instant::now();
            self.settle(&mut submitted, WHY_MOST);
            settled += settling.elapsed();
            phase(PHASE_SETTLE, settling);
        }
        let carding = std::time::Instant::now();
        let card = self.card_before(runs, flown, *submitted);
        phase(PHASE_CARD, carding);
        if let Some(check) = card.as_ref().and_then(|c| c.check.clone()) {
            lists.push(Some(check));
        }
        let prepared = std::time::Instant::now();
        if late {
            *submitted += 1;
            let sending = std::time::Instant::now();
            // SAFETY: closed lists; the fence is ours.
            let signalled = unsafe {
                self.queue.ExecuteCommandLists(&lists);
                self.queue.Signal(&self.fence, *submitted)
            };
            self.card_sent();
            phase(PHASE_SEND, sending);
            if let Err(e) = signalled {
                fail(format_args!("a submission sent ahead could not be followed by a signal ({e})"));
            }
            let back = declared_writes(runs);
            let writes = runs.iter().flat_map(|l| l.deferred().writes.clone()).collect();
            SUBMISSION_US[0].fetch_add((prepared - started).saturating_sub(settled).as_micros() as u64, Ordering::Relaxed);
            submitting(0, 0);
            #[cfg(feature = "diagnostics")]
            crate::copystat::submission_ends(lock);
            let sent = Sent {
                number: this_submission,
                card,
                back,
                writes,
            };
            working(was_working);
            return Some((submitted, sent));
        }
        // Diagnostics: each list on its own, timed (`VICTORY_GPU_TIMING`).
        #[cfg(feature = "diagnostics")]
        let groups: Vec<&[Option<ID3D12CommandList>]> = if timing() {
            firsts
                .iter()
                .zip(firsts.iter().skip(1).chain([&lists.len()]))
                .map(|(&a, &b)| &lists[a..b])
                .collect()
        } else {
            vec![&lists[..]]
        };
        #[cfg(not(feature = "diagnostics"))]
        let groups = [&lists[..]];
        submitting(2, runs.len());
        for (i, group) in groups.iter().enumerate() {
            *submitted += 1;
            let started = std::time::Instant::now();
            // SAFETY: closed lists; the fence and event are ours.
            unsafe {
                self.queue.ExecuteCommandLists(group);
                self.card_sent();
                let waited = self
                    .queue
                    .Signal(&self.fence, *submitted)
                    .and_then(|()| self.fence.SetEventOnCompletion(*submitted, self.event));
                match waited {
                    Ok(()) => {
                        working(false);
                        WaitForSingleObject(self.event, INFINITE);
                        stood(started);
                        working(true);
                    }
                    Err(e) => tracing::warn!(error = %e, "host submission not waited for"),
                }
            }
            // With timing on each group is one of `runs` (and the compare
            // of copies in the card's memory, with the last): every
            // command's time on the card goes to the sums by pipeline.
            #[cfg(feature = "diagnostics")]
            if timing() {
                // SAFETY: a plain query.
                let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                for (ms, op) in runs[i].slowest(frequency, usize::MAX) {
                    crate::copystat::command_took(&op, ms);
                }
            }
            // A submission of one list was never named here (`groups.len()
            // > 1` was the test): the two 50 ms lists of the gameplay
            // frame, each a submission of its own, printed nothing.
            #[cfg(feature = "diagnostics")]
            if timing() && started.elapsed() > std::time::Duration::from_millis(50) {
                // SAFETY: a plain query.
                let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                let commands = runs[i].notes.lock().unwrap_or_else(|e| e.into_inner()).len();
                tracing::warn!(ms = started.elapsed().as_millis(), commands, "slow host list");
                for (ms, op) in runs[i].slowest(frequency, 6) {
                    tracing::warn!("slow list command, {ms:.1} ms: {op}");
                }
            }
            let _ = (i, started);
        }
        let _ = &firsts;
        FINISHED.store(this_submission, Ordering::Release);
        bury(this_submission);
        let gpu = prepared.elapsed();
        SUBMISSION_US[0].fetch_add((prepared - started).as_micros() as u64, Ordering::Relaxed);
        SUBMISSION_US[1].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        crate::copystat::card_waited(gpu.as_micros() as u64, || {
            let pipelines: std::collections::BTreeSet<String> = runs
                .iter()
                .flat_map(|l| l.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clone())
                .collect();
            let text = format!("{} lists, pipelines {pipelines:?}", runs.len());
            text.chars().take(1200).collect()
        });
        // The host resets a device whose submission runs for about 2 s.
        if gpu > std::time::Duration::from_millis(250) {
            let pipelines: std::collections::BTreeSet<String> = runs
                .iter()
                .flat_map(|l| l.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clone())
                .collect();
            tracing::warn!(ms = gpu.as_millis(), lists = runs.len(), ?pipelines, "slow host submission");
        }
        // SAFETY: a plain query.
        if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
            fail(format_args!(
                "host GPU device removed after a submission of {} lists{}{}",
                runs.len(),
                self.removal_data(runs),
                last_notes(runs)
            ));
        }
        submitting(3, runs.len());
        // What shaders wrote goes back before the bottom-of-pipe writes,
        // which tell the title the GPU is done; each range once per
        // submission, however many dispatches declared it (the world's
        // passes declare the same big ranges dozens of times).
        let copying = std::time::Instant::now();
        // For copies in the card's memory the compare has run there: the
        // pieces that differ are fetched and merged.
        if let Some(card) = &card {
            self.card_back(card, &mut submitted, &mut std::collections::HashSet::new());
        }
        let back = declared_writes(runs);
        hand_back(&back);
        if let Some(card) = card {
            self.card_done(card);
        }
        #[cfg(feature = "diagnostics")]
        if !back.is_empty() {
            crate::copystat::add(crate::copystat::C::BackSubmissions, 1);
        }
        // Nothing of this submission goes back to the title: no range of
        // shader writes, no value written when it is done. The wait for
        // such a one is only for order and for what the card still holds.
        #[cfg(feature = "diagnostics")]
        if back.is_empty() && runs.iter().all(|l| l.deferred().writes.is_empty()) {
            use crate::copystat::{C, add};
            add(C::NothingBack, 1);
            add(C::NothingBackUs, gpu.as_micros() as u64);
        }
        COPY_BACK[0].fetch_add(copying.elapsed().as_micros() as u64, Ordering::Relaxed);
        for list in runs {
            // Read backs first: a value written at the bottom of the pipe
            // may be what tells the title that they are there.
            read_backs(list);
            for &(at, what, bytes) in &list.deferred().writes {
                what.write(at as *mut u8, bytes);
            }
        }
        self.log_messages();
        submitting(0, 0);
        #[cfg(feature = "diagnostics")]
        crate::copystat::submission_ends(lock);
        // With the relay: nothing is in the air now, so no fetch still to
        // be merged is older than the sends to card copies so far, and
        // their notes can go.
        if relay() {
            crate::shadow::relay_fetched(*QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()));
        }
        working(was_working);
        None
    }

    /// With `VICTORY_D3D12_DRED`: the faulting address and the allocations
    /// it lies in (by the names we give them) or the title memory copy just
    /// below it; for each list that did not finish, the command it stopped
    /// at and what the draws and dispatches in flight there run (or, in
    /// lists that never started, the pipelines they bind).
    fn removal_data(&self, runs: &[&HostList]) -> String {
        let Ok(dred) = self.device.cast::<ID3D12DeviceRemovedExtendedData1>() else {
            return String::new();
        };
        let mut text = String::new();
        let mut fault_va = None;
        // SAFETY: the outputs are the runtime's, valid while the device
        // lives; nodes are walked as linked, histories have their counts.
        unsafe {
            if let Ok(fault) = dred.GetPageFaultAllocationOutput1()
                && fault.PageFaultVA != 0
            {
                let va = fault.PageFaultVA;
                fault_va = Some(va);
                text.push_str(&format!("; page fault at {va:#x}"));
                for (what, head) in [
                    ("inside", fault.pHeadExistingAllocationNode),
                    ("inside freed", fault.pHeadRecentFreedAllocationNode),
                ] {
                    let mut names = Vec::new();
                    let mut node = head;
                    while let Some(n) = node.as_ref()
                        && names.len() < 8
                    {
                        let name = if n.ObjectNameW.is_null() {
                            "unnamed".to_owned()
                        } else {
                            n.ObjectNameW.to_string().unwrap_or_default()
                        };
                        names.push(format!("{name} (type {})", n.AllocationType.0));
                        node = n.pNext;
                    }
                    if !names.is_empty() {
                        text.push_str(&format!("; {what} {names:?}"));
                    }
                }
                text.push_str(&crate::shadow::near(va));
                text.push_str(&crate::descriptor::host::near_heaps(va));
            }
            // The plain output: the extended one (with marker texts) came
            // back without stopped lists on this host.
            let crumbs = match dred.GetAutoBreadcrumbsOutput() {
                Ok(crumbs) => crumbs,
                Err(e) => {
                    text.push_str(&format!("; no breadcrumbs ({e})"));
                    return text;
                }
            };
            // Every list's progress (done/count), in the runtime's order.
            let mut progress = Vec::new();
            let mut node = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = node.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                let ours = p.pCommandList.as_ref().map(|l| l.as_raw());
                let index = runs.iter().position(|r| Some(r.list.as_raw()) == ours);
                progress.push(format!("{done}/{} {index:?}", p.BreadcrumbCount));
                node = p.pNext;
            }
            text.push_str(&format!("; breadcrumbs (done/count, list) {progress:?}"));
            let mut plain = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = plain.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                if done < p.BreadcrumbCount && !p.pCommandHistory.is_null() {
                    let history =
                        std::slice::from_raw_parts(p.pCommandHistory, p.BreadcrumbCount as usize);
                    text.push_str(&format!(
                        "; a list stopped at command {done} of {} (op {})",
                        p.BreadcrumbCount, history[done as usize].0
                    ));
                    let list = p.pCommandList.as_ref().map(|l| l.as_raw());
                    let run = runs.iter().find(|r| Some(r.list.as_raw()) == list);
                    // The k-th marker command is the k-th note.
                    #[cfg(feature = "diagnostics")]
                    let markers: Vec<(u32, String)> = run.map_or_else(Vec::new, |r| {
                        let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
                        history
                            .iter()
                            .enumerate()
                            .filter(|(_, op)| **op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
                            .zip(notes.iter())
                            .map(|((at, _), (note, range))| {
                                let place = fault_va.zip(*range).map(|(va, r)| fault_in(va, r));
                                (at as u32, format!("{note}{}", place.unwrap_or_default()))
                            })
                            .collect()
                    });
                    #[cfg(not(feature = "diagnostics"))]
                    let markers: Vec<(u32, String)> = Vec::new();
                    #[cfg(not(feature = "diagnostics"))]
                    let _ = fault_va;
                    if done > 0 {
                        text.push_str(&in_flight(history, &markers, done as usize));
                    } else if let Some(r) = run {
                        let pipelines = r.pipelines.lock().unwrap_or_else(|e| e.into_inner());
                        text.push_str(&format!(", its pipelines {pipelines:?}"));
                    }
                }
                plain = p.pNext;
            }
        }
        text
    }

    /// Moves the debug layer's messages into our log; returns the first
    /// error among them.
    /// The host device (`crate::cardmeter`).
    pub(crate) fn device(&self) -> &ID3D12Device {
        &self.device
    }

    /// The host queue (`crate::tiles`).
    pub(crate) fn queue(&self) -> &ID3D12CommandQueue {
        &self.queue
    }

    /// Ticks a second of the queue's timestamps; 0 if it has none.
    pub(crate) fn timestamp_frequency(&self) -> u64 {
        // SAFETY: a plain query.
        unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(0)
    }

    pub(crate) fn log_messages(&self) -> Option<String> {
        let Some(queue) = &self.messages else {
            return None;
        };
        let mut first_error = None;
        // SAFETY: each message is read into a buffer of the size the queue
        // reports, then the queue is emptied.
        unsafe {
            for i in 0..queue.GetNumStoredMessages() {
                let mut size = 0;
                if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                    continue;
                }
                let mut buffer = vec![0u64; size.div_ceil(8)];
                let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                    let m = &*message;
                    let text = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                    let text = String::from_utf8_lossy(text);
                    let text = text.trim_end_matches('\0');
                    tracing::warn!(id = m.ID.0, severity = m.Severity.0, "D3D12: {text}");
                    if m.Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 && first_error.is_none() {
                        first_error = Some(text.to_owned());
                    }
                }
            }
            queue.ClearStoredMessages();
        }
        first_error
    }

    /// A host render target view of `resource`, valid until it is recorded.
    pub(crate) fn rtv(
        &self,
        resource: Option<&ID3D12Resource>,
        desc: Option<&D3D12_RENDER_TARGET_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_rtv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource, or none with a
        // description (a typed null view).
        unsafe {
            let mut handle = self.rtvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.rtv_size) as usize;
            self.device
                .CreateRenderTargetView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }

    /// A host depth view of `resource`, valid until it is recorded.
    pub(crate) fn dsv(
        &self,
        resource: &ID3D12Resource,
        desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_dsv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource.
        unsafe {
            let mut handle = self.dsvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.dsv_size) as usize;
            self.device
                .CreateDepthStencilView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }
}

impl HostList {
    /// The timestamp heap and its read-back buffer (diagnostics, timing on).
    #[cfg(feature = "diagnostics")]
    fn stamps(&self) -> Option<&(ID3D12QueryHeap, ID3D12Resource)> {
        if !timing() {
            return None;
        }
        self.stamps
            .get_or_init(|| {
                let device = &host()?.device;
                let mut heap: Option<ID3D12QueryHeap> = None;
                let mut back: Option<ID3D12Resource> = None;
                let readback = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: u64::from(STAMPS) * 8,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                };
                // SAFETY: plain object creation.
                unsafe {
                    device
                        .CreateQueryHeap(
                            &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: STAMPS, NodeMask: 0 },
                            &mut heap,
                        )
                        .ok()?;
                    device
                        .CreateCommittedResource(
                            &readback,
                            D3D12_HEAP_FLAG_NONE,
                            &desc,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            None,
                            &mut back,
                        )
                        .ok()?;
                }
                Some((heap?, back?))
            })
            .as_ref()
    }

    /// The noted commands that took longest on the GPU, longest first, as
    /// (milliseconds, text) (diagnostics, timing on, after the list ran).
    #[cfg(feature = "diagnostics")]
    fn slowest(&self, frequency: u64, n: usize) -> Vec<(f64, String)> {
        let notes = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        let (Some((_, back)), count) = (self.stamps(), notes.len().min(STAMPS as usize - 1)) else {
            return Vec::new();
        };
        let mut data = std::ptr::null_mut();
        let range = D3D12_RANGE { Begin: 0, End: (count + 1) * 8 };
        // SAFETY: a read-back buffer the finished list resolved into.
        let stamps: Vec<u64> = unsafe {
            if back.Map(0, Some(&range), Some(&mut data)).is_err() {
                return Vec::new();
            }
            let s = std::slice::from_raw_parts(data.cast::<u64>(), count + 1).to_vec();
            back.Unmap(0, None);
            s
        };
        let mut times: Vec<(f64, String)> = (0..count)
            .map(|i| {
                let ticks = stamps[i + 1].saturating_sub(stamps[i]);
                (ticks as f64 * 1e3 / frequency.max(1) as f64, notes[i].0.chars().take(300).collect())
            })
            .collect();
        times.sort_by(|a, b| b.0.total_cmp(&a.0));
        times.truncate(n);
        times
    }

    pub(crate) fn close(&self) {
        self.twins_to_depth();
        #[cfg(feature = "diagnostics")]
        if let Some((heap, back)) = self.stamps() {
            let count = (self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32).min(STAMPS - 1);
            // SAFETY: an open list, our heap and read-back buffer.
            unsafe {
                self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, count);
                self.list.ResolveQueryData(heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, count + 1, back, 0);
            }
        }
        // SAFETY: our list.
        if let Err(e) = unsafe { self.list.Close() } {
            fail(format_args!(
                "a host command list did not close ({e}): a command recorded into it was invalid"
            ));
        }
    }

    /// Only after the list's last submission finished (every one is waited
    /// for).
    pub(crate) fn reset(&self) {
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        self._alive.used();
        *self.deferred() = Deferred::default();
        self.states.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.twins.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clear();
        #[cfg(feature = "diagnostics")]
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).clear();
        {
            let mut scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
            (scratch.current, scratch.used) = (0, 0);
            // Every submission of the list has run: nothing reads its
            // scratch memory any more, and the next list to ask takes it.
            if scratch_pool() {
                give_chunks(std::mem::take(&mut scratch.chunks));
            }
        }
        // SAFETY: the list is not executing.
        let reset = unsafe {
            self.allocator
                .Reset()
                .and_then(|()| self.list.Reset(&self.allocator, None))
        };
        if let Err(e) = reset {
            fail(format_args!("a host command list did not reset ({e})"));
        }
        self.shade();
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::ResetNs, started.elapsed().as_nanos() as u64);
    }

    /// Sets the list's pixel shading rate (`VICTORY_SHADING_RATE`); a reset
    /// returns it to 1x1, so this runs after every reset as well as at
    /// creation.
    fn shade(&self) {
        if let Some((list5, rate)) = &self.shading {
            // SAFETY: our list, open for recording.
            unsafe { list5.RSSetShadingRate(*rate, None) };
        }
    }

    /// Moves a title texture's host copy to `wanted` (a host state) in this
    /// list: the host texture and the state it leaves, if that is a change.
    /// The list's first use of a texture records nothing: its prelude brings
    /// the texture there at submission.
    pub(crate) fn transition(
        &self,
        resource: crate::object::This,
        wanted: u32,
    ) -> Option<(&'static ID3D12Resource, u32)> {
        // One ask for the resource's host side: the copy it holds is the key
        // the states are kept by, and it names the texture the barrier moves
        // (a second ask, for `host_resource`, answered the same question).
        let copy = crate::resource::host_copy(resource)?;
        // SAFETY: the list holds `copy` until it is reset, so the texture
        // outlives every barrier the list records through this.
        let texture: &'static ID3D12Resource = unsafe { &*std::ptr::from_ref(&copy.texture) };
        let key = std::sync::Arc::as_ptr(&copy) as usize;
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let held = states.get(&key).map(|(_, last, _)| *last);
        // `wanted` is a console state; `held` one the texture's flags allow.
        match moving(held, wanted, |w| legal_state(texture, w)) {
            Move::Settled => None,
            Move::FirstUse(to) => {
                states.insert(key, (to, to, copy));
                None
            }
            Move::Moved(from, to) => {
                if let Some(held) = states.get_mut(&key) {
                    held.1 = to;
                }
                Some((texture, from))
            }
        }
    }

    /// Moves host texture `copy` to `wanted` (a host state) in this list,
    /// as [`HostList::transition`] does a title resource's host copy, and
    /// records the barrier: for textures that are not a title resource's
    /// host copy (colour twins) and for the copies between the two.
    fn settle(&self, copy: &std::sync::Arc<crate::resource::HostTexture>, wanted: u32) {
        let wanted = legal_state(&copy.texture, wanted);
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let before = match states.entry(std::sync::Arc::as_ptr(copy) as usize) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((wanted, wanted, copy.clone()));
                None
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let before = std::mem::replace(&mut o.get_mut().1, wanted);
                (before != wanted).then_some(before)
            }
        };
        drop(states);
        if let Some(before) = before {
            let barrier = transition(
                &copy.texture,
                D3D12_RESOURCE_STATES(before as i32),
                D3D12_RESOURCE_STATES(wanted as i32),
            );
            // SAFETY: an open list and a live texture.
            unsafe { self.list.ResourceBarrier(&[barrier]) };
        }
    }

    /// A use of title texture `resource` about to be recorded, if it is a
    /// depth texture with a colour twin (false if it is not): of the twin
    /// (`colour`: unordered access) or of the depth texture (depth target,
    /// clear, shader read, copy, resolve), writing it or only reading. The
    /// side used is first brought up to date: by a copy recorded here if
    /// this list wrote the other side since, by the list's prelude if the
    /// list uses the side as it finds it. The twin is left in the
    /// unordered access state; the caller moves the depth texture to the
    /// state its use needs, as for any texture. A twin still ahead when the
    /// list closes is copied then (`twins_to_depth`).
    /// shortcut: every depth target bound for a draw counts as written
    /// (also when the draw only tests), and every unordered access view a
    /// shader names as written; views a shader picks from the heap by a
    /// computed index are not seen here at all.
    pub(crate) fn twin_use(
        &self,
        resource: crate::object::This,
        colour: bool,
        write: bool,
    ) -> bool {
        const UNORDERED_ACCESS: u32 = 0x8;
        // Asked for every texture a draw reads: most have no twin.
        let Some(twin) = crate::resource::twin(resource) else {
            return false;
        };
        let Some(depth) = crate::resource::host_copy(resource) else {
            return false;
        };
        let side = usize::from(colour);
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        let u = twins
            .entry(std::sync::Arc::as_ptr(&twin) as usize)
            .or_insert_with(|| TwinUse {
                depth: depth.clone(),
                twin: twin.clone(),
                needs: [false; 2],
                behind: [None; 2],
                writing: false,
                late: false,
            });
        let copy = match u.behind[side] {
            Some(behind) => behind,
            None => {
                u.needs[side] = true;
                false
            }
        };
        u.behind[side] = Some(false);
        match (write, colour) {
            // Shaders write the twin when the draw or dispatch runs: what
            // the same one reads of the depth texture is prepared before
            // (`twins_written`).
            (true, true) => u.writing = true,
            (true, false) => u.behind[1] = Some(true),
            (false, _) => {}
        }
        // shortcut: the depth plane's twin is not drawn into the plane
        // here: the pass sets a pipeline and a target of ours, which a list
        // still recording the title's commands cannot take. This use sees
        // the plane as it was; the pass runs when the list closes
        // (`twins_to_depth`). Counted.
        if copy && !colour && twin.plane.is_some() {
            u.late = true;
            crate::resource::PLANES[2].fetch_add(1, Ordering::Relaxed);
        }
        drop(twins);
        if copy {
            self.twin_copy(&depth, &twin, colour);
        } else if colour {
            self.settle(&twin.colour, UNORDERED_ACCESS);
        }
        // The depth plane's twin is written through unordered access too.
        if colour && let Some(plane) = &twin.plane {
            self.settle(plane, UNORDERED_ACCESS);
        }
        true
    }

    /// Records the pass that draws the twin of the depth plane of `depth`
    /// into the plane, if `twin` has one, with the moves to the states the
    /// pass needs; the twin is left in the unordered access state. Only
    /// for a list that records nothing of the title's afterwards
    /// (`record_plane_pass`).
    fn pass_plane(
        &self,
        depth: &std::sync::Arc<crate::resource::HostTexture>,
        twin: &crate::resource::Twin,
    ) {
        const UNORDERED_ACCESS: u32 = 0x8;
        const DEPTH_WRITE: u32 = 0x10;
        const PIXEL_SHADER_RESOURCE: u32 = 0x80;
        let Some(plane) = &twin.plane else { return };
        self.settle(depth, DEPTH_WRITE);
        self.settle(plane, PIXEL_SHADER_RESOURCE);
        record_plane_pass(&self.list, &depth.texture, &plane.texture);
        self.settle(plane, UNORDERED_ACCESS);
    }

    /// Records the copy of a depth texture into its colour twin (`colour`)
    /// or of the twin into the depth texture, with the moves to the copy
    /// states. The twin is left in the unordered access state.
    fn twin_copy(
        &self,
        depth: &std::sync::Arc<crate::resource::HostTexture>,
        twin: &crate::resource::Twin,
        colour: bool,
    ) {
        const UNORDERED_ACCESS: u32 = 0x8;
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;
        let (from, to) = if colour {
            (depth, &twin.colour)
        } else {
            (&twin.colour, depth)
        };
        self.settle(from, COPY_SOURCE);
        self.settle(to, COPY_DEST);
        record_twin_copy(&self.list, &depth.texture, twin, colour);
        self.settle(&twin.colour, UNORDERED_ACCESS);
    }

    /// Before the list closes: the depth textures whose twins it wrote last
    /// are given what was written, and left readable by shaders. Shaders
    /// that pick a view from the heap by a computed index are not seen by
    /// `twin_use` and read the depth texture in lists that never name it
    /// (in a captured frame of the title no table names a shadow
    /// map for reading but the pass that fills the next one).
    fn twins_to_depth(&self) {
        const SHADER_RESOURCE: u32 = 0x40 | 0x80;
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        let written: Vec<_> = twins
            .values_mut()
            .filter(|u| u.behind[0] == Some(true) || u.late)
            .map(|u| {
                // Not copied again if the list copied since the last write
                // and only the depth plane's pass is left.
                let copy = u.behind[0] == Some(true);
                u.behind[0] = Some(false);
                u.late = false;
                (u.depth.clone(), u.twin.clone(), copy)
            })
            .collect();
        drop(twins);
        for (depth, twin, copy) in &written {
            if *copy {
                self.twin_copy(depth, twin, false);
            }
            // Nothing of the title's is recorded after this: the pass may
            // leave its own pipeline and target set.
            self.pass_plane(depth, twin);
            self.settle(depth, SHADER_RESOURCE);
        }
    }

    /// The draw or dispatch just prepared is recorded: the twins it writes
    /// now hold what their depth textures lack.
    pub(crate) fn twins_written(&self) {
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        for u in twins.values_mut().filter(|u| u.writing) {
            u.writing = false;
            u.behind[0] = Some(true);
        }
    }

    /// At submission: the list that moves each texture this list uses from
    /// its real state to the one the list starts with; the textures' real
    /// states become those the list ends with. For depth textures with a
    /// colour twin it first copies the side the list uses as it finds it
    /// from the other, if a list submitted earlier left it behind.
    fn prelude(&self) -> Option<ID3D12CommandList> {
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;
        let host_state = |state: u32| D3D12_RESOURCE_STATES(state as i32);
        let states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        // The copies (depth texture, twin, whether into the twin); the
        // sides are then as this list leaves them.
        let mut syncs = Vec::new();
        let twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        for u in twins.values() {
            let real = u.twin.behind.load(Ordering::Relaxed);
            let mut behind = [real & 1 != 0, real & 2 != 0];
            for side in [0, 1] {
                if u.needs[side] && behind[side] {
                    syncs.push((u.depth.clone(), u.twin.clone(), side == 1));
                    behind = [false; 2];
                }
            }
            let left = [0, 1].map(|side| u.behind[side].unwrap_or(behind[side]));
            let left = u32::from(left[0]) | (u32::from(left[1]) << 1);
            u.twin.behind.store(left, Ordering::Relaxed);
        }
        drop(twins);
        // A copy runs with its textures in the copy states: each moves
        // there from its real state, and from there to the state the list
        // starts with (back to its real state if the list does not use it).
        let mut to_copy = Vec::new();
        let mut barriers = Vec::new();
        let mut copying = std::collections::HashMap::new();
        for (depth, twin, colour) in &syncs {
            let (from, to) = if *colour {
                (depth, &twin.colour)
            } else {
                (&twin.colour, depth)
            };
            for (texture, state) in [(from, COPY_SOURCE), (to, COPY_DEST)] {
                let key = std::sync::Arc::as_ptr(texture) as usize;
                let real = texture.state.load(Ordering::Relaxed);
                copying.insert(key, state);
                if real != state {
                    to_copy.push(transition(
                        &texture.texture,
                        host_state(real),
                        host_state(state),
                    ));
                    if !states.contains_key(&key) {
                        barriers.push(transition(
                            &texture.texture,
                            host_state(state),
                            host_state(real),
                        ));
                    }
                }
            }
        }
        barriers.extend(states.iter().filter_map(|(key, (first, last, copy))| {
            let real = copy.state.swap(*last, Ordering::Relaxed);
            let real = copying.get(key).copied().unwrap_or(real);
            (real != *first)
                .then(|| transition(&copy.texture, host_state(real), host_state(*first)))
        }));
        if barriers.is_empty() && syncs.is_empty() {
            return None;
        }
        let Some((allocator, list)) = self
            .prelude
            .get_or_init(|| host().and_then(|h| h.make_prelude()))
            .as_ref()
        else {
            fail(format_args!("no host list for a state prelude"));
        };
        // SAFETY: the prelude is not executing (every submission is waited
        // for); the textures are live.
        let recorded = unsafe {
            allocator.Reset().and_then(|()| {
                list.Reset(allocator, None)?;
                if !to_copy.is_empty() {
                    list.ResourceBarrier(&to_copy);
                }
                for (depth, twin, colour) in &syncs {
                    record_twin_copy(list, &depth.texture, twin, *colour);
                }
                if !barriers.is_empty() {
                    list.ResourceBarrier(&barriers);
                }
                list.Close()
            })
        };
        if let Err(e) = recorded {
            fail(format_args!("a host state prelude did not record ({e})"));
        }
        list.cast().ok()
    }

    /// Notes what the command about to be recorded does, as a marker
    /// command whose text the list keeps (diagnostics, with device-removed
    /// data on): the report pairs the breadcrumbs' marker commands with the
    /// texts in order and names the commands a fault stopped among.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_op(&self, what: impl FnOnce() -> String) {
        self.note(what, None);
    }

    /// As [`HostList::note_op`], for a copy that reads the host GPU range
    /// `(address, bytes)`: the report places the faulting address in it.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_read(&self, what: impl FnOnce() -> String, range: (u64, u64)) {
        self.note(what, Some(range));
    }

    #[cfg(feature = "diagnostics")]
    fn note(&self, what: impl FnOnce() -> String, range: Option<(u64, u64)>) {
        if !dred_on() {
            crate::capture::op(std::ptr::from_ref(self) as usize, what);
            return;
        }
        {
            let started = std::time::Instant::now();
            let at = self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32;
            if let Some((heap, _)) = self.stamps() && at < STAMPS - 1 {
                // SAFETY: an open list and our query heap.
                unsafe { self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, at) };
            }
            let what = what();
            crate::capture::op(std::ptr::from_ref(self) as usize, || what.clone());
            let text: Vec<u16> = what.encode_utf16().chain([0]).collect();
            const UNICODE: u32 = 0;
            // SAFETY: an open list; the marker is copied when recorded.
            unsafe {
                self.list.SetMarker(UNICODE, Some(text.as_ptr().cast()), 2 * text.len() as u32)
            };
            self.notes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((what, range));
            crate::copystat::add(crate::copystat::C::NoteNs, started.elapsed().as_nanos() as u64);
        }
    }

    /// Whether nothing recorded since the list was opened does any work:
    /// no draw or dispatch (the first keeps its pipeline), no copy, clear,
    /// upload or resolve (they defer a step or move a texture's state), no
    /// bottom-of-pipe write. State set on the list does not count.
    pub(crate) fn untouched(&self) -> bool {
        {
            let deferred = self.deferred();
            if !(deferred.copies.is_empty()
                && deferred.uploads.is_empty()
                && deferred.kept.is_empty()
                && deferred.textures.is_empty()
                && deferred.writes.is_empty()
                && deferred.backs.is_empty())
            {
                return false;
            }
        }
        self.states.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
            && self.twins.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// Notes a pipeline the list binds.
    pub(crate) fn used_pipeline(&self, name: &str) {
        let mut pipelines = self.pipelines.lock().unwrap_or_else(|e| e.into_inner());
        if !pipelines.contains(name) {
            pipelines.insert(name.to_owned());
        }
    }

    fn deferred(&self) -> std::sync::MutexGuard<'_, Deferred> {
        self.deferred.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Copies `len` bytes from `src` to `dst` when the list is submitted.
    pub(crate) fn defer_copy(&self, dst: *mut u8, src: *const u8, len: usize) {
        let mut deferred = self.deferred();
        COPIES_JOINED[0].fetch_add(1, Ordering::Relaxed);
        // A copy that goes on where the one recorded just before it ended,
        // source and destination both: with `join_copies` the two are one
        // step.
        if let Some(Step::Copy(d, s, n)) = deferred.copies.last_mut()
            && continues((*d, *s, *n), (dst as usize, src as usize, len))
        {
            COPIES_JOINED[1].fetch_add(1, Ordering::Relaxed);
            if join_copies() {
                *n += len;
                return;
            }
        }
        deferred
            .copies
            .push(Step::Copy(dst as usize, src as usize, len));
        deferred.refreshed.clear();
    }

    /// Copies `len` bytes of title memory at `src` into the list's upload
    /// memory at `dst` when the list is submitted.
    pub(crate) fn defer_fill(&self, dst: *mut u8, src: *const u8, len: usize) {
        self.deferred()
            .copies
            .push(Step::Fill(dst as usize, src as usize, len));
    }

    /// Brings `[address, address + len)` of a buffer's host copy up to date
    /// when the list is submitted (once per range until a title copy
    /// intervenes), keeping the copy alive until the list is reset.
    pub(crate) fn defer_refresh(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        let mut deferred = self.deferred();
        let key = (std::sync::Arc::as_ptr(shadow) as usize, address, len);
        if deferred.refreshed.insert(key) {
            deferred
                .copies
                .push(Step::Refresh(shadow.clone(), address, len));
        }
    }

    /// Shaders of the list write `[address, address + len)` of a buffer's
    /// host copy: copied back into title memory once the list has run.
    pub(crate) fn defer_gpu_write(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        self.deferred()
            .copies
            .push(Step::GpuWrite(shadow.clone(), address, len));
    }

    /// A read back: once the list has run, `rows` rows of `row` bytes go
    /// from `buffer` (rows 256-byte pitched, see `PlacedFootprint::host_rows`)
    /// to title memory at `dst`, pitched `pitch`. The buffer lives until
    /// the list is reset.
    pub(crate) fn defer_read_back(&self, buffer: ID3D12Resource, dst: *mut u8, [row, rows, pitch]: [usize; 3]) {
        self.deferred().backs.push((buffer, dst as usize, [row, rows, pitch]));
    }

    /// Stores `what` (`bytes` of it) at `dst` once the list has run.
    pub(crate) fn defer_write(&self, dst: *mut u8, what: crate::queue::Written, bytes: usize) {
        self.deferred().writes.push((dst as usize, what, bytes));
    }

    /// A mapped upload buffer of `size` bytes, alive until the list is
    /// reset.
    ///
    /// shortcut: one buffer per copy; pool them once uploads are frequent.
    pub(crate) fn upload(&self, host: &Host, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let (buffer, data) = host.upload_buffer(size)?;
        name(&buffer, || format!("list upload {size:#x}"));
        self.deferred().hold_upload(buffer.clone(), size);
        Some((buffer, data))
    }

    /// `len` bytes of upload memory (256-byte aligned) until the list is
    /// reset: the host GPU address and the CPU pointer.
    pub(crate) fn scratch(&self, len: u64) -> Option<(u64, *mut u8)> {
        let host = host()?;
        let mut s = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        let len = len.max(1).next_multiple_of(256);
        if len > SCRATCH_CHUNK {
            let (buffer, data) = host.upload_buffer(len)?;
            name(&buffer, || format!("list scratch {len:#x}"));
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            self.deferred().hold_upload(buffer, len);
            return Some((gpu, data));
        }
        // The chunk in use while `len` fits in what is left of it, else the
        // next one (a list's first chunk may be a small one).
        while s.current < s.chunks.len() && s.used + len > s.chunks[s.current].1 {
            (s.current, s.used) = (s.current + 1, 0);
        }
        if s.current == s.chunks.len() {
            let size = if scratch_pool() && s.chunks.is_empty() && len <= SCRATCH_FIRST {
                SCRATCH_FIRST
            } else {
                SCRATCH_CHUNK
            };
            let chunk = take_chunk(host, size)?;
            s.chunks.push((chunk, size));
            s.used = 0;
        }
        let (data, gpu) = (s.chunks[s.current].0.data, s.chunks[s.current].0.gpu);
        let at = s.used;
        s.used += len;
        Some((gpu + at, (data + at as usize) as *mut u8))
    }

    /// The bytes of scratch chunks the list holds now (after a reset:
    /// none while lists share their chunks).
    pub(crate) fn scratch_kept(&self) -> u64 {
        let scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        scratch.chunks.iter().map(|c| c.1).sum()
    }

    /// The host address of the list's indirect-work budget, zeroed at each
    /// submission of the list from the first use after a reset on.
    pub(crate) fn budget(&self) -> Option<u64> {
        let (buffer, data) = self
            .budget
            .get_or_init(|| {
                let (b, d) = host()?.shared_buffer(256)?;
                Some((b, d as usize))
            })
            .as_ref()?;
        let mut deferred = self.deferred();
        if !deferred.budget_zeroed {
            deferred.budget_zeroed = true;
            let at = *data;
            // SAFETY: the list's own mapped words, written before it runs.
            // The words after the budget hold what the clamp noted about the
            // list's last run (mesh dispatches: the largest group count
            // asked for, the groups asked in all): taken, then zeroed.
            deferred.copies.push(Step::Host(Box::new(move || unsafe {
                let words = at as *mut u32;
                let (largest, all) = (words.add(1).read_volatile(), words.add(2).read_volatile());
                MESH_GROUPS[0].fetch_max(u64::from(largest), Ordering::Relaxed);
                MESH_GROUPS[1].fetch_add(u64::from(all), Ordering::Relaxed);
                for k in 0..3 {
                    words.add(k).write_volatile(0);
                }
            })));
        }
        // SAFETY: a live buffer.
        Some(unsafe { buffer.GetGPUVirtualAddress() })
    }

    /// A device-local unordered-access buffer of at least `size` bytes for
    /// clamped indirect arguments, reused across executions and resets of the
    /// list. Smaller buffers replaced mid-list stay alive until
    /// [`HostList::reset`].
    pub(crate) fn indirect_scratch(&self, size: u64) -> Option<ID3D12Resource> {
        let needed = size.max(64 << 10);
        let mut slot = self.indirect_args.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((buffer, cap)) = &*slot
            && *cap >= needed
        {
            return Some(buffer.clone());
        }
        let prev = slot.as_ref().map_or(0, |(_, cap)| *cap);
        let alloc = prev
            .saturating_mul(2)
            .max(needed)
            .next_multiple_of(64 << 10);
        let buffer = host()?.uav_buffer(alloc)?;
        name(&buffer, || format!("indirect arguments {alloc:#x}"));
        let old = slot.replace((buffer.clone(), alloc));
        drop(slot);
        if let Some((old_buf, _)) = old {
            self.keep(&old_buf);
        }
        Some(buffer)
    }

    /// Keeps `object` alive until the list is reset.
    ///
    /// The reference is taken off the vtable the object already has:
    /// `cast::<IUnknown>()` asked the object for the interface it starts
    /// with (`QueryInterface`, then `AddRef`) on every pipeline change, and
    /// the opening cutscene changes pipelines thousands of times a frame.
    pub(crate) fn keep(&self, object: &impl Interface) {
        let raw = object.as_raw();
        // SAFETY: a live interface the list's commands named; `clone` is
        // the `AddRef` of the reference `kept` gives back at the reset.
        if let Some(unknown) = unsafe { windows::core::IUnknown::from_raw_borrowed(&raw) } {
            self.deferred().kept.push(unknown.clone());
        }
    }

    /// Keeps `a` and `b` alive until the list is reset, in one lock: a
    /// pipeline change keeps its pipeline and its root signature, and the
    /// opening cutscene changes pipelines thousands of times a frame.
    pub(crate) fn keep_pair(&self, a: &impl Interface, b: &impl Interface) {
        let mut deferred = self.deferred();
        for object in [a.as_raw(), b.as_raw()] {
            // SAFETY: a live interface the list's commands named; `clone`
            // is the `AddRef` of the reference `kept` gives back.
            if let Some(unknown) = unsafe { windows::core::IUnknown::from_raw_borrowed(&object) } {
                deferred.kept.push(unknown.clone());
            }
        }
    }

    /// Whether this is the first time since the reset that the list asks
    /// about `key` (a texture to bring up to date).
    pub(crate) fn first_use(&self, key: usize) -> bool {
        self.deferred().textures.insert(key)
    }

    /// Runs `work` when the list is submitted, in order with its copies.
    /// `work` may write title memory (refreshes after it copy again).
    pub(crate) fn defer(&self, work: impl Fn() + Send + 'static) {
        self.deferred().copies.push(Step::Run(Box::new(work)));
        self.deferred().refreshed.clear();
    }

    /// As [`HostList::defer`], for work that writes only host memory.
    pub(crate) fn defer_host(&self, work: impl Fn() + Send + 'static) {
        self.deferred().copies.push(Step::Host(Box::new(work)));
    }

    /// Runs [`Rectangles`] when the list is submitted: as
    /// [`HostList::defer_host`], without the boxed closure.
    pub(crate) fn defer_host_rects(&self, work: Rectangles) {
        self.deferred().copies.push(Step::Rectangles(work));
    }

    /// Brings a texture's staging buffer up to date when the list is
    /// submitted, keeping it alive until the list is reset.
    pub(crate) fn defer_texture(&self, staging: std::sync::Arc<crate::texture::Staging>) {
        self.deferred().copies.push(Step::Texture(staging));
    }

    /// Runs the list's submission-time steps; refreshes go into `batch`
    /// (the title's big buffers, a 174 MB one among them, are read by most
    /// lists of a submission, through many ranges).
    fn run_deferred(&self, batch: &mut Batch) {
        #[cfg(feature = "diagnostics")]
        use crate::copystat::{C, add};
        let list = self.deferred();
        let steps: &[Step] = &list.copies;
        let mut at = 0;
        while at < steps.len() {
            let step = &steps[at];
            let started = std::time::Instant::now();
            // The steps this one runs, and the microseconds of them a flush
            // took (it is refresh work, counted apart as well).
            let mut taken = 1;
            let mut flushed = 0;
            let copied = match step {
                &Step::Copy(dst, _, _) => {
                    // The copies whose destinations carry on from this one's,
                    // run as one group: one flush before them (a copy writes
                    // title memory, so what is pending reads it first), the
                    // guards opened over all of their destinations, one note
                    // that the bytes were written, and each copy in the order
                    // it was recorded — the same bytes in the same order as
                    // one copy at a time, with the work around them shared.
                    let (run, end) = copy_run(steps, at);
                    taken = run;
                    let flushing = std::time::Instant::now();
                    batch.title_written();
                    flushed = flushing.elapsed().as_micros() as u64;
                    staged(ST_FLUSH, flushed);
                    // shortcut: one opening of the guards for the run, not one
                    // a copy. The blocks opened are exactly the ones each
                    // copy would open (its destination's), so nothing beyond
                    // what the run writes is opened; only the moment moves
                    // earlier, and a title write into a later copy's block in
                    // the microseconds between the two copies is not seen by
                    // the guards — the run writes those bytes itself anyway.
                    let guarding = std::time::Instant::now();
                    crate::watch::writing(dst, end - dst);
                    staged(ST_GUARD, guarding.elapsed().as_micros() as u64);
                    let mut stages = Stages::default();
                    let copying = std::time::Instant::now();
                    let bytes = group_copies(steps, at, |at, from, bytes| {
                        copy_staged(at as *mut u8, from as *const u8, bytes, Open::Group, Some(&mut stages));
                    });
                    crate::flow::put(
                        crate::flow::Site::IntoTitle,
                        bytes,
                        copying.elapsed().as_nanos() as u64,
                    );
                    staged(ST_READ, stages.read / 1000);
                    // With the submission's one asking: the journal learns it.
                    let noting = std::time::Instant::now();
                    crate::watch::wrote(dst, end - dst);
                    staged(ST_WROTE, noting.elapsed().as_micros() as u64);
                    staged(ST_STEPS, run as u64);
                    staged(ST_RUNS, 1);
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Copies, run as u64);
                        add(C::CopyBytes, bytes as u64);
                    }
                    bytes
                }
                &Step::Fill(dst, src, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Copies, 1);
                        add(C::CopyBytes, len as u64);
                    }
                    let begun = std::time::Instant::now();
                    copy_mapped(dst as *mut u8, src as *const u8, len, false);
                    crate::flow::took(crate::flow::Site::IntoTitle, len, begun);
                    len
                }
                Step::Refresh(shadow, address, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Refreshes, 1);
                        add(C::RefreshBytes, *len as u64);
                    }
                    batch.add(shadow, *address, *len);
                    at += 1;
                    continue;
                }
                Step::Run(work) | Step::Host(work) => {
                    if matches!(step, Step::Run(_)) {
                        batch.title_written();
                    }
                    work();
                    // It wrote title memory, where is not known here: the
                    // submission's one asking ends, later refreshes ask anew.
                    if matches!(step, Step::Run(_)) {
                        crate::watch::ask_once_end();
                    }
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Runs, 1);
                        add(C::RunUs, started.elapsed().as_micros() as u64);
                    }
                    at += 1;
                    continue;
                }
                Step::Rectangles(work) => {
                    work.run();
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Runs, 1);
                        add(C::RunUs, started.elapsed().as_micros() as u64);
                    }
                    at += 1;
                    continue;
                }
                Step::GpuWrite(shadow, address, len) => {
                    // The shaders start from the title's data: this copy's
                    // pending refresh first. The other copies' wait for
                    // the next flush of all (flushing all of them at every
                    // range a dispatch writes made thousands of write-watch
                    // askings a frame).
                    batch.flush_one(shadow);
                    #[cfg(feature = "diagnostics")]
                    let snapshot = std::time::Instant::now();
                    shadow.writing(*address, *len);
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::GpuWrites, 1);
                        add(C::SnapshotUs, snapshot.elapsed().as_micros() as u64);
                    }
                    at += 1;
                    continue;
                }
                Step::Texture(staging) => {
                    if batch.texture_due(staging) {
                        staging.refresh();
                    }
                    at += 1;
                    continue;
                }
            };
            at += taken;
            // What the steps cost, the flush of a copy aside: `Batch::update`
            // counts its own time too, and it ran inside this step's flush.
            let step_us = started.elapsed().as_micros() as u64;
            let own = step_us.saturating_sub(flushed);
            staged(ST_STEP, own);
            PREPARING[0].fetch_add(own, Ordering::Relaxed);
            PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
        }
    }
}

/// Whether a submission's refreshes of host copies run on several threads,
/// a copy to a thread (`VICTORY_WIDE_REFRESH=1`; off by default).
///
/// A submission brings some sixty copies up to date, one after the other
/// on the submitting thread: for each, the pages the title wrote are asked
/// for and read through the kernel into the copy. That is the frame line's
/// "copies", 45-50 ms a frame, and the copies have nothing to do with one
/// another: each refresh writes its own copy and its own notes, and reads
/// title memory. With the switch the copies of a flush (four or more) are
/// spread over the threads kept for the snapshots (`crate::shadow::wide`).
/// The write-watch journal and the guards are behind their own locks, as
/// they are for the title's threads.
fn wide_refresh() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WIDE_REFRESH").is_some_and(|v| v == "1"))
}

/// A submission's refreshes not done yet, merged per buffer copy into the
/// range that covers them: done before a step that writes title memory or
/// gives a range to shaders, and before the lists run.
///
/// Also the textures brought up to date in the submission so far: most
/// lists of a world submission sample the same textures, and a texture in
/// physical-page memory is hashed whole each time it is checked. Checked
/// once, it is checked again only after a step that writes title memory.
#[derive(Default)]
struct Batch {
    refreshes: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>,
    /// Where each copy (by address) is in `refreshes`: the list that makes
    /// the heap's buffers ready adds some 4,100 copies, and searching them
    /// for every refresh of a submission was tens of millions of
    /// comparisons a frame.
    index: std::collections::HashMap<usize, usize>,
    /// Staging buffers (by address) refreshed since the last title write.
    textures: std::collections::HashSet<usize>,
}

impl Batch {
    fn add(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>, address: usize, len: usize) {
        let end = address + len;
        let key = std::sync::Arc::as_ptr(shadow) as usize;
        match self.index.get(&key) {
            Some(&at) => {
                let r = &mut self.refreshes[at];
                (r.1, r.2) = (r.1.min(address), r.2.max(end));
            }
            None => {
                self.index.insert(key, self.refreshes.len());
                self.refreshes.push((shadow.clone(), address, end));
            }
        }
    }

    /// Before shaders are given a range of `shadow`'s copy: its pending
    /// refresh is done, the others stay pending.
    fn flush_one(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>) {
        let Some(at) = self.index.remove(&(std::sync::Arc::as_ptr(shadow) as usize)) else {
            return;
        };
        let (shadow, from, to) = self.refreshes.swap_remove(at);
        // The last entry took its place.
        if let Some(moved) = self.refreshes.get(at) {
            self.index.insert(std::sync::Arc::as_ptr(&moved.0) as usize, at);
        }
        Self::update(&shadow, from, to);
    }

    /// Brings `[from, to)` of a copy up to date, timed.
    fn update(shadow: &crate::shadow::Shadow, from: usize, to: usize) {
        let started = std::time::Instant::now();
        let copied = shadow.update(from, to - from);
        let us = started.elapsed().as_micros() as u64;
        #[cfg(feature = "diagnostics")]
        crate::copystat::range(from, to - from, us, 0);
        PREPARING[0].fetch_add(us, Ordering::Relaxed);
        PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
        staged(ST_UPDATE, us);
    }

    /// Whether `staging` is still to be refreshed in this submission (and
    /// notes it refreshed).
    fn texture_due<T>(&mut self, staging: &std::sync::Arc<T>) -> bool {
        self.textures.insert(std::sync::Arc::as_ptr(staging) as *const () as usize)
    }

    /// Before a step that writes title memory: pending refreshes are done
    /// first, and textures are checked again after it.
    fn title_written(&mut self) {
        self.flush();
        self.textures.clear();
    }

    fn flush(&mut self) {
        self.index.clear();
        // One write-watch asking for the copies that lie close together,
        // not one per copy.
        let ahead = self.refreshes.len() > 1;
        if ahead {
            let started = std::time::Instant::now();
            let ranges: Vec<(usize, usize)> = self
                .refreshes
                .iter()
                .filter(|r| !r.0.unwatched())
                .map(|r| (r.1, r.2))
                .collect();
            crate::watch::ask_ahead(&ranges);
            PREPARING[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
        if wide_refresh() && self.refreshes.len() >= 4 {
            // Each copy on its own, several at a time: a copy's refresh
            // writes that copy and reads title memory, nothing of another's.
            // The time counted is the time it took, not the threads' sum.
            let started = std::time::Instant::now();
            let before = PREPARING[0].load(Ordering::Relaxed);
            let jobs: Vec<Box<dyn FnOnce() + Send>> = self
                .refreshes
                .drain(..)
                .map(|(shadow, from, to)| Box::new(move || Self::update(&shadow, from, to)) as Box<dyn FnOnce() + Send>)
                .collect();
            crate::shadow::wide_run(jobs);
            let summed = PREPARING[0].load(Ordering::Relaxed).saturating_sub(before);
            PREPARING[0].fetch_sub(summed.saturating_sub(started.elapsed().as_micros() as u64), Ordering::Relaxed);
        } else {
            for (shadow, from, to) in self.refreshes.drain(..) {
                Self::update(&shadow, from, to);
            }
        }
        if ahead {
            crate::watch::ask_ahead_done();
        }
    }
}

/// The copies the title recorded into its own memory, split by what they
/// cost, logged after each frame line: the frame line's `copies` is one
/// number for five costs, and 13,034 copies of ~0.5 KB a frame are their own
/// per-call work rather than the bytes they move (`flow` says which call
/// site moved the bytes; this says what the call cost around them).
fn copy_stages_line(frames: f64) {
    let v: Vec<u64> = COPY_STAGES.iter().map(|c| c.swap(0, Ordering::Relaxed)).collect();
    if v[ST_STEPS] == 0 {
        return;
    }
    let steps = v[ST_STEPS] as f64 / frames;
    let runs = v[ST_RUNS] as f64 / frames;
    let ms = |at: usize| v[at] as f64 / 1e3 / frames;
    let rest = v[ST_STEP].saturating_sub(v[ST_GUARD] + v[ST_READ] + v[ST_WROTE]) as f64 / 1e3 / frames;
    tracing::info!(
        "the copies into title memory, per frame: {steps:.0} copies in {runs:.0} runs of {:.1} (a run's destinations carry on \
         from one another), {:.2} ms in the steps they made: {:.2} flushing before them, {:.2} opening their guards, {:.2} the \
         kernel's reads, {:.2} noting them written, {:.2} the rest; {:.2} ms refreshing the host copies a step forced, which \
         runs inside that step's flush and is counted in the frame line's copies twice",
        steps / runs.max(1.0),
        ms(ST_STEP),
        ms(ST_FLUSH),
        ms(ST_GUARD),
        ms(ST_READ),
        ms(ST_WROTE),
        rest,
        ms(ST_UPDATE),
    );
}

/// Copies title memory at submission. The ranges were checked against their
/// resources when recorded, but the title may since have unmapped pages of
/// them (tiled memory, page mappings), which read as zeros here; a title
/// destination (`title_dst`) that is not all mapped is left alone.
///
/// The source is read through the kernel (`ReadProcessMemory`), which
/// reports an unmapped page instead of faulting. Asking `VirtualQuery` first
/// cost about 2 ms per copy in the title's big pools (it walks the page
/// tables to the region's end), which made copies run at 100-200 MB/s.
pub(crate) fn copy_mapped(dst: *mut u8, src: *const u8, len: usize, title_dst: bool) {
    let open = if title_dst { Open::Own } else { Open::Nothing };
    copy_staged(dst, src, len, open, None);
}

/// [`copy_mapped`], with what is opened for the copy before it reads, and a
/// place to time the kernel's read into (the deferred copies' stage line).
fn copy_staged(dst: *mut u8, src: *const u8, len: usize, open: Open, stages: Option<&mut Stages>) {
    let title_dst = open.writes_title();
    #[cfg(feature = "diagnostics")]
    match crate::copystat::mode() {
        1 => {
            // SAFETY: timing only: the caller vouches for the source.
            unsafe { std::ptr::copy(src, dst, len) };
            return;
        }
        2 => return,
        _ => {}
    }
    if len == 0 {
        return;
    }
    // Every copy out of title memory is counted, and one big enough is timed
    // and its pages sampled before and after the read: the 5 s line divides
    // the bytes by the time and sets the process's page faults beside them
    // (`crate::fault`). The count ends where the copy does, on every path out.
    let mut copying = crate::fault::copying(src as usize, dst as usize, len);
    // A title destination that is not all mapped is left alone. Asking
    // Windows first (`VirtualQuery`) walks the page tables to the end of
    // the destination's region: in the title's big blocks that is
    // milliseconds a copy, and it was a seventh of the submitting thread's
    // time in gameplay (run 60: 71 of 500 looks). The kernel's copy below
    // says the same when it fails, so the question is only asked then.
    // `VICTORY_COPY_ASK=1` asks first, as before.
    static ASK: OnceLock<bool> = OnceLock::new();
    let ask_first = title_dst
        && *ASK.get_or_init(|| std::env::var_os("VICTORY_COPY_ASK").is_some_and(|v| v == "1"));
    if ask_first && !crate::texture::committed(dst, len) {
        return;
    }
    // The kernel writes the destination: it must not be guarded. A copy in a
    // group finds them opened over the whole group's destination already.
    if matches!(open, Open::Own) {
        crate::watch::writing(dst as usize, len);
    }
    #[cfg(feature = "diagnostics")]
    let moved = std::time::Instant::now();
    let timed = stages.as_ref().map(|_| std::time::Instant::now());
    let whole = read(src, dst, len);
    if let (Some(stages), Some(timed)) = (stages, timed) {
        stages.read += timed.elapsed().as_nanos() as u64;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::MoveUs, moved.elapsed().as_micros() as u64);
    if whole {
        return;
    }
    // The copy failed: a source page is not there, or the destination is
    // not all mapped. Then the destination stays as it is (the kernel may
    // have written its first pages).
    if title_dst && !ask_first && !crate::texture::committed(dst, len) {
        return;
    }
    // The sparse read below is the one path a copy can take that is not a
    // copy: counted in every build, so the 5 s line shows it in a release one
    // too (`crate::fault`). The diagnostics table counts it per frame as well,
    // which is the line that has the ranges beside it.
    copying.fell_back();
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::Fallbacks, 1);
    #[cfg(feature = "diagnostics")]
    let sparse = std::time::Instant::now();
    if !title_dst && len >= 64 << 10 {
        read_sparse(src, dst, len);
    } else {
        read_pieces(src, dst, len);
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::SparseUs, sparse.elapsed().as_micros() as u64);
}

/// After a failed bulk read into host memory, skip contiguous holes. Never
/// cache the query across copies: the title may remap the same address.
fn read_sparse(src: *const u8, dst: *mut u8, len: usize) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualQuery,
    };
    let mut offset = 0;
    while offset < len {
        let at = src.wrapping_add(offset);
        let to = dst.wrapping_add(offset);
        let remaining = len - offset;
        // SAFETY: the query inspects an address and fills a local structure.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let size = unsafe { VirtualQuery(at.cast(), &mut info, size_of_val(&info)) };
        let n = (info.BaseAddress as usize).saturating_add(info.RegionSize)
            .saturating_sub(at as usize).min(remaining);
        if size != size_of_val(&info) || n == 0 {
            read_pieces(at, to, remaining);
            break;
        }
        if info.State != MEM_COMMIT || info.Protect == PAGE_NOACCESS {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
            // SAFETY: this path is only used with a writable host destination.
            unsafe { to.write_bytes(0, n) };
        } else if !read(at, to, n) {
            // A mapping may change between query and read.
            read_pieces(at, to, n);
        }
        offset += n;
    }
}

fn read_pieces(src: *const u8, dst: *mut u8, len: usize) {
    // Something is unmapped: page by page (64 KiB, then 4 KiB in a piece
    // that fails), zeros for what cannot be read.
    for_pieces(src as usize, len, 64 << 10, |at, n| {
        let to = dst.wrapping_add(at - src as usize);
        if !read(at as *const u8, to, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                let to = dst.wrapping_add(at - src as usize);
                if !read(at as *const u8, to, n) {
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
                    // SAFETY: inside the destination, which is mapped.
                    unsafe { to.write_bytes(0, n) };
                }
            });
        }
    });
}

/// Reads title memory at `src` into `dst`; false if a page of it is not
/// readable.
pub(crate) fn read_into(src: *const u8, dst: &mut [u8]) -> bool {
    read(src, dst.as_mut_ptr(), dst.len())
}

/// Copies `len` bytes from `src` to `dst` unless a source page is not
/// readable.
fn read(src: *const u8, dst: *mut u8, len: usize) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut done = 0;
    // SAFETY: the destination is mapped writable memory of `len` bytes; the
    // kernel checks the source.
    unsafe { ReadProcessMemory(GetCurrentProcess(), src.cast(), dst.cast(), len, &mut done) != 0 }
}

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    /// The kernel's write into a process's memory, `(process, address,
    /// bytes, length, written out)`: an NTSTATUS, negative when a page of
    /// the range is not there or not writable. It changes no protection.
    fn NtWriteVirtualMemory(
        process: *mut std::ffi::c_void,
        base: *mut std::ffi::c_void,
        buffer: *const std::ffi::c_void,
        size: usize,
        written: *mut usize,
    ) -> i32;
}

/// Whether title memory is written with the kernel's own call
/// (`NtWriteVirtualMemory`) and not `WriteProcessMemory`, which first asks
/// what the pages are (as `VirtualQuery` does: about 2 ms a call in the
/// title's big pools, see [`copy_mapped`]): putting back what shaders wrote
/// cost 1.2-1.9 s a frame for 430-800 pieces, 2 ms each whatever their size
/// (run 24). On by default; `VICTORY_DIRECT_WRITES=0` writes as before.
fn direct_writes() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DIRECT_WRITES").is_none_or(|v| v != "0"))
}

/// Copies host memory into title memory after a submission (what shaders
/// wrote), leaving pages that are not mapped alone.
pub(crate) fn write_mapped(dst: *mut u8, src: *const u8, len: usize) {
    use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let direct = direct_writes();
    let write = |at: usize, n: usize| {
        let from = src.wrapping_add(at - dst as usize);
        let mut done = 0;
        if direct {
            // SAFETY: as below; the kernel checks the destination and
            // writes nothing it cannot (the caller opened its guards:
            // `crate::watch::writing`).
            return unsafe {
                NtWriteVirtualMemory(GetCurrentProcess(), at as _, from.cast(), n, &mut done) >= 0
            };
        }
        // SAFETY: the source is our mapped host copy; the kernel checks the
        // destination.
        unsafe { WriteProcessMemory(GetCurrentProcess(), at as _, from.cast(), n, &mut done) != 0 }
    };
    if len == 0 || write(dst as usize, len) {
        return;
    }
    for_pieces(dst as usize, len, 64 << 10, |at, n| {
        if !write(at, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                write(at, n);
            });
        }
    });
}

/// Calls `f` with the pieces of `[start, start + len)` cut at multiples of
/// `step`.
fn for_pieces(start: usize, len: usize, step: usize, mut f: impl FnMut(usize, usize)) {
    let end = start + len;
    let mut at = start;
    while at < end {
        let next = ((at / step + 1) * step).min(end);
        f(at, next - at);
        at = next;
    }
}

impl Host {
    /// A buffer in the upload heap, mapped for writing.
    /// A buffer of `size` bytes the card writes and we read (a read back),
    /// in the state copies into it need.
    pub(crate) fn read_back_buffer(&self, size: u64) -> Option<ID3D12Resource> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a read-back buffer.
        let made = unsafe {
            self.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
        };
        match made {
            Ok(()) => buffer,
            Err(e) => {
                tracing::warn!(error = %e, size, "no read-back buffer");
                None
            }
        }
    }

    pub(crate) fn upload_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        MADE[0].fetch_add(1, Ordering::Relaxed);
        MADE[1].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of an upload buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no upload buffer");
                note_removal(&e);
                None
            }
        }
    }
}

/// The swap chain formats a presented texture can be copied into: its own
/// family's, in the format a flip chain takes (the four a `DXGI_SWAP_EFFECT_FLIP_*`
/// swap chain offers: 8-bit RGBA and BGRA, 10-bit RGB and 16-bit float).
fn swap_format(format: DXGI_FORMAT) -> Option<DXGI_FORMAT> {
    Some(match format.0 {
        27..=32 => DXGI_FORMAT_R8G8B8A8_UNORM,
        87 | 90 | 91 => DXGI_FORMAT_B8G8R8A8_UNORM,
        23..=25 => DXGI_FORMAT_R10G10B10A2_UNORM,
        9..=14 => DXGI_FORMAT_R16G16B16A16_FLOAT,
        _ => return None,
    })
}

/// Whether `format` is its family's typeless one: the one format a view desc
/// may not name, and the one any typed format of the family may view.
fn typeless(format: DXGI_FORMAT) -> bool {
    let own = format.0 as u32;
    crate::resource::family(own) == own && crate::resource::family(own + 1) == own
}

/// Whether a `Texture2D` sampled as `float4` can name a view of `format`: an
/// integer one samples as `uint4` or `int4`, which the pass's shader cannot
/// read.
fn samples_float(format: DXGI_FORMAT) -> bool {
    const INTEGER: [DXGI_FORMAT; 5] = [
        DXGI_FORMAT_R8G8B8A8_UINT,
        DXGI_FORMAT_R8G8B8A8_SINT,
        DXGI_FORMAT_R16G16B16A16_UINT,
        DXGI_FORMAT_R16G16B16A16_SINT,
        DXGI_FORMAT_R10G10B10A2_UINT,
    ];
    !INTEGER.contains(&format)
}

/// The format the present pass's view of a frame names, for a frame of
/// `format` presented into a swap chain of `swap`.
///
/// A view desc names a typed format its resource allows: a typeless frame takes
/// any typed format of its family, so the swap chain's, and a typed frame takes
/// its own, exactly. D3D12 makes a `_UNORM` view of a typed `_UNORM_SRGB`
/// resource illegal (only a driver's fully typed casting would allow it), and
/// `CreateShaderResourceView` returns nothing, so naming the swap chain's there
/// is a silent black window.
///
/// `None` for a frame the pass cannot read at all — an integer one — which
/// keeps the copy path in place.
fn frame_view(format: DXGI_FORMAT, swap: DXGI_FORMAT) -> Option<DXGI_FORMAT> {
    if !samples_float(format) {
        return None;
    }
    Some(if typeless(format) { swap } else { format })
}

/// The frame's format is not one the present pass reads as float, so the
/// present is DXGI's own stretch: every frame of the run meets it again.
///
/// [`victory_core::logging::warn_once`] says which occurrence it is, so a run
/// whose frames never let the pass read them reports the total instead of a
/// single line a `Once` wrote on the first present.
fn frame_not_float(frame_format: i32, swap_format: i32) -> bool {
    victory_core::logging::warn_once("present.frame_format", || {
        format!(
            "the frame's format is not one the present pass reads as float: DXGI stretches it \
             instead; frame_format={frame_format} swap_format={swap_format}"
        )
    })
}

/// The swap chain could not be resized to the client area: the present is
/// skipped under the size the chain still has, and a window being dragged
/// meets the same refusal on every present until it stops moving.
fn resize_refused(error: impl std::fmt::Display, client: (u32, u32)) -> bool {
    victory_core::logging::warn_once("present.resize", move || {
        format!(
            "swap chain not resized; error={error} client_width={} client_height={}",
            client.0, client.1
        )
    })
}

/// Says once, each way, what the present pass reads a frame with: a view D3D12
/// refuses is silent, so the two formats behind a black window are in the log
/// and nowhere else.
///
/// The stretch is the one case a run meets on every present, so its line comes
/// from `warn_once` and carries the running total ([`frame_not_float`]).
fn note_frame_view(format: DXGI_FORMAT, swap: DXGI_FORMAT, view: Option<DXGI_FORMAT>) {
    static OWN: OnceLock<()> = OnceLock::new();
    match view {
        Some(view) if view != swap && OWN.set(()).is_ok() => tracing::info!(
            frame_format = format.0,
            swap_format = swap.0,
            view_format = view.0,
            "the frame is read with its own format: the swap chain's is not one D3D12 allows for its resource"
        ),
        None => {
            frame_not_float(format.0, swap.0);
        }
        _ => {}
    }
}

/// The largest rectangle of `client` with the shape of `frame`: the frame
/// scaled by the smaller of the two ratios, centred, with bars on the sides or
/// the top and bottom (letterboxed).
///
/// `None` when either size has a zero side: a minimised window has no client
/// area, and a swap chain of a size of zero is not one DXGI makes.
fn fit(frame: (u32, u32), client: (u32, u32)) -> Option<Fit> {
    let ((frame_width, frame_height), (client_width, client_height)) = (frame, client);
    if frame_width == 0 || frame_height == 0 || client_width == 0 || client_height == 0 {
        return None;
    }
    // The smaller ratio decides: whichever of the frame's own sides would leave
    // the other sticking out of the client area.
    let (width, height) = if u64::from(frame_width) * u64::from(client_height)
        <= u64::from(client_width) * u64::from(frame_height)
    {
        let width = u64::from(frame_width) * u64::from(client_height) / u64::from(frame_height);
        (width.min(u64::from(client_width)) as u32, client_height)
    } else {
        let height = u64::from(frame_height) * u64::from(client_width) / u64::from(frame_width);
        (client_width, height.min(u64::from(client_height)) as u32)
    };
    Some(Fit {
        x: (client_width - width) / 2,
        y: (client_height - height) / 2,
        width: width.max(1),
        height: height.max(1),
    })
}

/// Where a frame's picture goes in a window's client area: [`fit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fit {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// Whether shaders may read a texture's texels: the flag that denies it is also
/// the one that denies the read states (`legal_for`).
fn readable(desc: &D3D12_RESOURCE_DESC) -> bool {
    const DENY_SHADER_RESOURCE: u32 = 0x8;
    desc.Flags.0 as u32 & DENY_SHADER_RESOURCE == 0
}

/// The pass that draws a frame into a back buffer the client area's size: one
/// triangle over the rectangle [`fit`] gives, its pixel shader reading the frame
/// through a bilinear sampler.
///
/// The aspect ratio cannot come from the swap chain: DXGI scales a flip chain's
/// back buffer over the whole client area, and `DXGI_SCALING_ASPECT_RATIO_STRETCH`
/// is documented as behaving like `DXGI_SCALING_STRETCH` for a Win32 flip chain
/// (a plain stretch). The rectangle the viewport and the scissor name is what
/// letterboxes the picture. A D3D12 copy cannot scale, so a draw is how the
/// frame reaches a back buffer larger or smaller than itself.
const PRESENT_CORNERS: &str = r#"
struct corner_out {
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
};

corner_out corner(uint i : SV_VertexID) {
    // One triangle twice the viewport's size: (0,0), (2,0), (0,2) in units of
    // the viewport, which the rasteriser and the scissor clip to it.
    float2 t = float2((i << 1) & 2, i & 2);
    corner_out looked;
    looked.position = float4(t.x * 2.0 - 1.0, 1.0 - t.y * 2.0, 0.0, 1.0);
    looked.uv = t;
    return looked;
}
"#;

const PRESENT_FRAME: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL), StaticSampler(s0, filter = FILTER_MIN_MAG_MIP_LINEAR, addressU = TEXTURE_ADDRESS_CLAMP, addressV = TEXTURE_ADDRESS_CLAMP, visibility = SHADER_VISIBILITY_PIXEL)"
Texture2D frame : register(t0);
SamplerState view : register(s0);

[RootSignature(RS)]
float4 present(float4 position : SV_Position, float2 uv : TEXCOORD0) : SV_Target {
    return float4(frame.SampleLevel(view, uv, 0).rgb, 1.0);
}
"#;

/// Views of frames the pass reads, in a heap of ours used as a ring.
/// shortcut: a view must last until its list has run, and presents sent ahead
/// ([`late_free_present`]) hold the last few; a ring this long is well past
/// them.
const PRESENT_VIEWS: u32 = 8;

/// What the pass draws with: the heap of its views of frames, and, per back
/// buffer format, the root signature and pipeline, made when first needed
/// (`None`: the host refused).
struct PresentPass {
    views: ID3D12DescriptorHeap,
    cpu: usize,
    gpu: u64,
    step: u32,
    next: AtomicU32,
    made: Mutex<Vec<(i32, Option<PresentPipeline>)>>,
}

/// The pass's root signature and pipeline for one back buffer format.
type PresentPipeline = (ID3D12RootSignature, ID3D12PipelineState);

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for PresentPass {}
// SAFETY: as above.
unsafe impl Sync for PresentPass {}

fn present_pass() -> Option<&'static PresentPass> {
    static PASS: OnceLock<Option<PresentPass>> = OnceLock::new();
    PASS.get_or_init(|| {
        let host = host()?;
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        // SAFETY: plain heap creation and queries.
        unsafe {
            let views: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: PRESENT_VIEWS,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .ok()?;
            Some(PresentPass {
                cpu: views.GetCPUDescriptorHandleForHeapStart().ptr,
                gpu: views.GetGPUDescriptorHandleForHeapStart().ptr,
                step: host.device.GetDescriptorHandleIncrementSize(kind),
                views,
                next: AtomicU32::new(0),
                made: Mutex::default(),
            })
        }
    })
    .as_ref()
}

impl PresentPass {
    fn pipeline(&self, format: DXGI_FORMAT) -> Option<PresentPipeline> {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, pipeline)) = made.iter().find(|m| m.0 == format.0) {
            return pipeline.clone();
        }
        let pipeline = match Self::make(format) {
            Ok(pipeline) => Some(pipeline),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    format = format.0,
                    "no pipeline to draw the frame into the window: DXGI stretches it instead"
                );
                None
            }
        };
        made.push((format.0, pipeline.clone()));
        pipeline
    }

    fn make(format: DXGI_FORMAT) -> windows::core::Result<PresentPipeline> {
        use windows::core::s;
        let device = &host().ok_or_else(windows::core::Error::empty)?.device;
        let vs = compile(PRESENT_CORNERS, s!("corner"), s!("vs_5_1"))?;
        let ps = compile(PRESENT_FRAME, s!("present"), s!("ps_5_1"))?;
        let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
            pShaderBytecode: bytes.as_ptr().cast(),
            BytecodeLength: bytes.len(),
        };
        // SAFETY: the root signature is inside the pixel shader; the
        // description points at live data and never releases its pointer.
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &ps[..])?;
            let mut targets = [DXGI_FORMAT_UNKNOWN; 8];
            targets[0] = format;
            let (blend, rasterizer) = present_states();
            let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                VS: code(&vs[..]),
                PS: code(&ps[..]),
                BlendState: blend,
                SampleMask: u32::MAX,
                RasterizerState: rasterizer,
                NumRenderTargets: 1,
                RTVFormats: targets,
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            let pipeline: ID3D12PipelineState = device.CreateGraphicsPipelineState(&desc)?;
            Ok((root, pipeline))
        }
    }
}

/// The states the present pass draws with: no blend, every colour channel
/// written, no culling, depth off.
///
/// The write mask is the one that matters: a `Default` blend description
/// writes nothing at all, so the pass would clear the back buffer to black and
/// then draw its triangle into a mask that discards every channel — a black
/// window with no error anywhere. `present_states` is tested for that.
fn present_states() -> (D3D12_BLEND_DESC, D3D12_RASTERIZER_DESC) {
    let mut blend = D3D12_BLEND_DESC::default();
    for target in &mut blend.RenderTarget {
        target.RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
    }
    let raster = D3D12_RASTERIZER_DESC {
        FillMode: D3D12_FILL_MODE_SOLID,
        CullMode: D3D12_CULL_MODE_NONE,
        DepthClipEnable: true.into(),
        ..Default::default()
    };
    (blend, raster)
}

/// What the pass shows, and where.
struct PresentDraw<'a> {
    /// The frame, in a state pixel shaders may read.
    frame: &'a ID3D12Resource,
    /// The back buffer, in the render target state, of the pipeline's format
    /// and of the size the client area names.
    back: &'a ID3D12Resource,
    /// What the view of `frame` names: the frame's own format, or the swap
    /// chain's for a typeless frame ([`frame_view`]).
    view: DXGI_FORMAT,
    /// The rectangle of `back` the frame goes in ([`fit`]).
    rect: Fit,
}

/// Records into `list` the pass that shows `draw`: the back buffer cleared to
/// black and the frame drawn into the rectangle of it. It puts none of the
/// list's state back: it is for a list that records nothing of the title's after
/// it.
fn record_present_pass(
    host: &Host,
    list: &ID3D12GraphicsCommandList,
    pass: &PresentPass,
    pipeline: &PresentPipeline,
    draw: PresentDraw<'_>,
) {
    let PresentDraw {
        frame,
        back,
        view,
        rect,
    } = draw;
    let (root, pipeline) = pipeline;
    let slot = pass.next.fetch_add(1, Ordering::Relaxed) % PRESENT_VIEWS;
    let srv = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: view,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
                ..Default::default()
            },
        },
    };
    let cpu = D3D12_CPU_DESCRIPTOR_HANDLE {
        ptr: pass.cpu + (slot * pass.step) as usize,
    };
    let gpu = D3D12_GPU_DESCRIPTOR_HANDLE {
        ptr: pass.gpu + u64::from(slot * pass.step),
    };
    let target = host.rtv(Some(back), None);
    // SAFETY: an open list; a slot of our heap; live resources and objects (the
    // pass's own live as long as the process).
    unsafe {
        host.device.CreateShaderResourceView(frame, Some(&srv), cpu);
        list.SetDescriptorHeaps(&[Some(pass.views.clone())]);
        // The bars: nothing is drawn outside the rectangle, so they are cleared.
        list.ClearRenderTargetView(target, &[0.0, 0.0, 0.0, 1.0], None);
        list.OMSetRenderTargets(1, Some(&target), false, None);
        list.SetGraphicsRootSignature(root);
        list.SetPipelineState(pipeline);
        list.SetGraphicsRootDescriptorTable(0, gpu);
        list.RSSetViewports(&[D3D12_VIEWPORT {
            TopLeftX: rect.x as f32,
            TopLeftY: rect.y as f32,
            Width: rect.width as f32,
            Height: rect.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        }]);
        list.RSSetScissorRects(&[RECT {
            left: rect.x as i32,
            top: rect.y as i32,
            right: (rect.x + rect.width) as i32,
            bottom: (rect.y + rect.height) as i32,
        }]);
        list.IASetPrimitiveTopology(windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY(4));
        list.DrawInstanced(3, 1, 0, 0);
    }
}

const BUFFERS: u32 = 2;

/// How a frame reaches the window's back buffer.
#[derive(Clone, Copy, Debug)]
enum Show {
    /// `CopyResource`: the frame and the back buffer are one size.
    Copy,
    /// The present pass, drawing the frame into `rect` of the back buffer.
    Drawn(Fit),
}

/// Whether a present of picture `format` into a chain of `swap` may copy it
/// ([`Show::Copy`]): a copy takes the format as it is, so only the chain's
/// own, or a typeless picture that a format of its family stands in for
/// ([`typeless`]). A 64-bit-per-pixel picture copied into a 32-bit back buffer
/// is a `CopyResource` the device refuses, and the pass draws it instead
/// ([`draw_or_copy`]).
fn copy_compatible(format: DXGI_FORMAT, swap: DXGI_FORMAT) -> bool {
    format == swap || (typeless(format) && crate::resource::family(swap.0 as u32) == format.0 as u32)
}

/// Whether a present draws the picture through the pass or copies it: the
/// picture must have the client area's size for a copy (DXGI's stretch of the
/// back buffer is not one of D3D12's), and a copy of another format is none
/// at all, so such a picture is drawn whenever the pass can read it and shown
/// not at all when it cannot — a black window rather than a present the device
/// refuses.
fn draw_or_copy(readable: bool, copyable: bool, same_size: bool) -> Option<bool> {
    // A copy is offered only at the client area's size and in a format the
    // copy takes as it is: the pass stretches, a `CopyResource` does neither.
    let may_copy = copyable && same_size;
    if readable && !same_size {
        Some(true)
    } else if may_copy {
        Some(false)
    } else if readable {
        Some(true)
    } else {
        None
    }
}

/// Runs what the present owes a target it moved when the present returns,
/// however it returns: the state a shown target is tracked in must be the one
/// the pass found it in even on the ways out that present nothing (a minimised
/// window refuses every present, and a target left tracked as a shader read is
/// a barrier the rest of the run will not emit). [`Restore::done`] is for the
/// present that got far enough to put it back itself.
struct Restore<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Restore<F> {
    fn new(restore: F) -> Self {
        Restore(Some(restore))
    }

    /// What it would run is done: nothing on the way out.
    fn done(&mut self) {
        self.0 = None;
    }
}

impl<F: FnOnce()> Drop for Restore<F> {
    fn drop(&mut self) {
        if let Some(restore) = self.0.take() {
            restore();
        }
    }
}

struct Presenter {
    swap_chain: IDXGISwapChain3,
    /// The size of the swap chain's buffers: the window's client area when the
    /// frame is drawn into them ([`fit`]), else the frame's own size.
    size: (u32, u32),
    format: DXGI_FORMAT,
    copy: HostList,
    /// With presents sent ahead ([`late_free_present`]): a list for each
    /// present in the air, and the submission it last flew as (it is used
    /// again once that one has finished).
    ahead: Vec<(HostList, u64)>,
    /// The pass that draws the frame into buffers the client area's size;
    /// without it the buffers are the frame's size and DXGI stretches them.
    pass: Option<&'static PresentPass>,
    hwnd: HWND,
    meter: Meter,
}

/// The frame rate, shown in the window's caption every second and logged
/// every few seconds. The counters the logged block shows are taken once, in
/// that block, so each of them is summed over every second since the last
/// block; [`Meter::reported`] and [`Meter::reported_drawn`] are the frames
/// and the draws of those seconds, which is what they are divided by.
struct Meter {
    since: std::time::Instant,
    frames: u32,
    #[cfg(feature = "diagnostics")]
    copy_stats_frames: u64,
    drawn: u64,
    logged: std::time::Instant,
    /// Frames presented and draws made since `logged`.
    reported: u32,
    reported_drawn: u64,
    /// The caption the window had, once read.
    caption: Option<Vec<u16>>,
}

impl Meter {
    fn new() -> Self {
        let now = std::time::Instant::now();
        let drawn = crate::draw::drawn();
        Meter {
            since: now,
            frames: 0,
            #[cfg(feature = "diagnostics")]
            copy_stats_frames: 0,
            drawn,
            logged: now,
            reported: 0,
            reported_drawn: drawn,
            caption: None,
        }
    }

    /// Counts a presented frame.
    fn frame(&mut self, hwnd: HWND) {
        #[cfg(feature = "diagnostics")]
        tracing::debug!(target: "victory_gpu::frame", token = crate::frame::current(), "present");
        #[cfg(feature = "diagnostics")]
        crate::copystat::presented();
        self.frames += 1;
        self.reported += 1;
        #[cfg(feature = "diagnostics")]
        {
            self.copy_stats_frames += 1;
        }
        let elapsed = self.since.elapsed();
        if elapsed < std::time::Duration::from_secs(1) {
            return;
        }
        let drawn = crate::draw::drawn();
        let frames = f64::from(self.frames);
        let fps = frames / elapsed.as_secs_f64();
        let ms = elapsed.as_secs_f64() * 1000.0 / frames;
        let draws = (drawn - self.drawn) as f64 / frames;
        (self.since, self.frames, self.drawn) = (std::time::Instant::now(), 0, drawn);
        let mesh_largest = MESH_GROUPS[0].swap(0, Ordering::Relaxed);
        let mesh_groups = MESH_GROUPS[1].swap(0, Ordering::Relaxed) as f64 / frames;
        // Every group our translation dispatches promises the program's
        // declared counts whatever the title's culling decided (`emit::lower`'s
        // `mesh_prologue`): this is what that promises per frame, against the
        // vertices the title's culling would have kept.
        let declared = crate::pipeline::host::mesh_declared().map(|((v, p), differ)| {
            let at = if differ { "at most" } else { "at" };
            format!(
                ", {:.0} output vertices and {:.0} primitives {at} {v} of each a group",
                mesh_groups * v as f64,
                mesh_groups * p as f64
            )
        });
        tracing::info!(
            "indirect mesh dispatches: the largest group count asked for {mesh_largest} (run: at most 65535), {mesh_groups:.0} groups asked per frame{}",
            declared.unwrap_or_default()
        );
        late_line(frames);
        card_clock_line(frames, ms);
        crate::shadow::back_line(frames);
        crate::flow::line(frames);
        copy_stages_line(frames);
        {
            let [passes, pieces, compares, ranges, range_pieces] =
                [0, 1, 2, 3, 4].map(|i| CARD_PASSES[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if passes + compares > 0.0 {
                tracing::info!(
                    "our passes over copies in the card's memory per frame: {passes:.0} hash passes over {pieces:.0} pieces after \
                     ranges were sent up, where {ranges:.0} ranges of {range_pieces:.0} pieces were sent up (VICTORY_CARD_REHASH_RUNS \
                     joins ranges that touch into one pass: {}), {compares:.0} compare passes",
                    if rehash_runs() { "on" } else { "off" }
                );
            }
        }
        {
            let waited = [0, 1, 2, 3].map(|i| crate::texture::REFRESH_WAITS[i].swap(0, Ordering::Relaxed));
            // Kept for the 5 s line: the wait is wall-clock time the frame
            // spent, but it happens on the refreshing thread, so it is
            // reported beside the breakdown rather than inside it.
            REFRESH_STILL.fetch_add(waited[3], Ordering::Relaxed);
            let [own, asking, still, all] =
                waited.map(|us| us as f64 / 1000.0 / frames);
            if all >= 1.0 {
                tracing::info!(
                    "texture refreshes per frame, all threads: {all:.1} ms, of them {own:.1} ms waiting for a texture another thread \
                     is refreshing, {asking:.1} ms asking the write watch and the guards, {still:.1} ms waiting for title memory to \
                     hold still (the runtime remapping it); the rest is reading, hashing and detiling"
                );
            }
            let [through, left] = [0, 1].map(|i| crate::texture::UNLOCKED[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if through + left > 0.0 {
                tracing::info!(
                    "texture refreshes that did not wait for title memory to hold still, per frame (VICTORY_TEXTURE_NO_WAIT=1): \
                     {through:.1} read through the kernel, {left:.1} left for the next refresh (a page was not there)"
                );
            }
        }
        {
            let [copies, joined] = [0, 1].map(|i| COPIES_JOINED[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if copies > 0.0 {
                tracing::info!(
                    "title buffer copies recorded per frame: {copies:.0}, of them {joined:.0} continue the one before \
                     (VICTORY_JOIN_COPIES=1 makes those one copy with it: now {})",
                    if join_copies() { "on" } else { "off" }
                );
            }
        }
        // The block printed every few seconds covers every second since the
        // last one printed, so the counters it shows are taken here, once: each
        // is summed over all of those seconds and divided by the frames they
        // presented. Taken a second at a time, only the last second's counts
        // were shown, against the whole window's frame count.
        let printed = self.logged.elapsed();
        if printed >= std::time::Duration::from_secs(5) {
            let frames = f64::from(self.reported);
            let fps = frames / printed.as_secs_f64();
            let ms = printed.as_secs_f64() * 1000.0 / frames;
            let draws = (drawn - self.reported_drawn) as f64 / frames;
            (self.logged, self.reported, self.reported_drawn) =
                (std::time::Instant::now(), 0, drawn);
            // What the copies out of title memory ran at, beside the process's
            // page faults and memory (`crate::fault`): a scene that streams
            // from the pagefile is the one that loses its frame time to copies
            // that are not copies. Taken before the two counters below are
            // drained, so both lines describe one window.
            crate::fault::line(
                printed,
                [COPY_BACK[0].load(Ordering::Relaxed), COPY_BACK[1].load(Ordering::Relaxed)],
            );
            // Of a frame, ms preparing submissions and ms waiting for the GPU.
            let [cpu, gpu] = [0, 1].map(|i| {
                SUBMISSION_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames
            });
            let [copy_ms, copy_mb, texture_ms, texture_mb] = [0, 1, 2, 3].map(|i| {
                let scale = if i % 2 == 0 { 1e3 } else { 1e6 };
                PREPARING[i].swap(0, Ordering::Relaxed) as f64 / scale / frames
            });
            let [back_ms, back_mb] = [0, 1].map(|i| {
                COPY_BACK[i].swap(0, Ordering::Relaxed) as f64 / [1e3, 1e6][i] / frames
            });
            let dropped = crate::draw::take_dropped() as f64 / frames;
            // Wall-clock time of the run that the split above cannot show.
            let still_ms = REFRESH_STILL.swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames;
            // Why those were dropped, split by reason: counted in every build,
            // and taken over the same window as the count above.
            #[cfg(not(feature = "diagnostics"))]
            let drop_reasons = crate::pipeline::take_drop_reasons();
            let indirect = crate::list::take_indirect() as f64 / frames;
            // Passes over the heap's buffer views for shaders that pick them by index.
            let picked = crate::draw::HEAP_BUFFER_PASSES.swap(0, Ordering::Relaxed) as f64 / frames;
            let picked_ms =
                crate::draw::HEAP_BUFFER_MICROS.swap(0, Ordering::Relaxed) as f64 / 1e3 / frames;
            // Copies between depth textures and their colour twins, each way.
            let [twin_to_depth, twin_to_colour] = [2, 3].map(|i| {
                crate::resource::TWINS[i].swap(0, Ordering::Relaxed) as f64 / frames
            });
            // Depth planes drawn from their twins.
            let planes = crate::resource::PLANES[0].swap(0, Ordering::Relaxed) as f64 / frames;
            // Copies in the card's memory (`crate::shadow::cards`).
            let card = crate::shadow::card_stats();
            #[cfg(feature = "diagnostics")]
            let slow = crate::watch::slow_asks();
            // Over the same frames as the counts above.
            #[cfg(feature = "diagnostics")]
            let drops = crate::pipeline::take_drops();
            tracing::info!(
                "placed texture views: {} writes of another view of the texture a slot showed (it kept the old view until the new was made)",
                crate::descriptor::host::KEPT_VIEWS.load(Ordering::Relaxed)
            );
            {
                let [queued, due, away, partly, oldest] =
                    [0, 1, 2, 3, 4].map(|i| crate::draw::HEAP_TEXTURES_QUEUE[i].load(Ordering::Relaxed));
                let [same, parts, tails] =
                    [0, 1, 2].map(|i| crate::draw::HEAP_TEXTURES_PARTLY[i].load(Ordering::Relaxed));
                tracing::info!(
                    "textures picked by index, the queue: {queued} slots, {due} of them due (the oldest written {oldest} frames ago), \
                     {away} waiting for their memory, {partly} uploaded in part; since the start: {} slots found uploaded already by \
                     the draws that read them through tables, {} writes of tables' slots (due some frames later), {} slots taken out \
                     as read through a table, {parts} uploads of textures not all mapped ({tails} of them without their first page), \
                     {same} looks that found no more mapped; {} slots made stale by a newer view of other memory there, \
                     {} ranges of memory mapped, unmapped or written under texture views ({} dropped), {} slots made due by them, \
                     {} made slots found unmapped and made null",
                    crate::draw::HEAP_TEXTURES_TABLED.load(Ordering::Relaxed),
                    crate::descriptor::host::TABLED_WRITES[0].load(Ordering::Relaxed),
                    crate::descriptor::host::TABLED_WRITES[1].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[0].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[1].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[2].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[3].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[4].load(Ordering::Relaxed)
                );
            }
            #[cfg(feature = "diagnostics")]
            tracing::info!(
                "write-watch askings of a millisecond or more: {:.1} a frame, {:.1} ms a frame; the longest asking {:.1} ms",
                slow[0] as f64 / frames,
                slow[1] as f64 / 1e3 / frames,
                slow[2] as f64 / 1e3
            );
            if crate::shadow::cards() {
                let per_frame = |i: usize| card[i] as f64 / frames;
                tracing::info!(
                    "host copies in the card's memory: {} copies {} MB, their bytes from before {} MB; per frame: \
                     {:.1} MB sent up in {:.0} ranges, {:.0} pieces compared on the card, {:.0} of them fetched \
                     ({:.1} MB) in {:.0} copies with {:.1} more waits for the card ({:.1} ms)",
                    card[8],
                    card[9] >> 20,
                    card[10] >> 20,
                    per_frame(0) / 1e6,
                    per_frame(1),
                    per_frame(2),
                    per_frame(3),
                    per_frame(4) / 1e6,
                    per_frame(7),
                    per_frame(5),
                    per_frame(6) / 1e3
                );
            }
            // The uploads on their own queue ([`copy_queue`]): a batch whose
            // run had finished when the next was ready ran beside the direct
            // queue, and that overlap is all the split buys. Per window, as
            // `CARD_STATS` is: the batches' totals divide by this window's
            // frames, so they are drained here.
            if copy_queue() {
                let (batches, mb, finished, running) = copy_queue_window(&COPY_QUEUE_STATS, frames);
                tracing::info!(
                    "copies into the card's memory on the copy queue: {batches} batches so far, \
                     {mb:.1} MB a frame, {finished:.0} finished before the next batch was ready \
                     and {running:.0} still running then, a frame"
                );
            }
            tracing::info!(
                "{fps:.1} fps, {ms:.1} ms/frame ({cpu:.1} preparing: copies {copy_ms:.1} ms \
                 {copy_mb:.1} MB, textures {texture_ms:.1} ms {texture_mb:.1} MB; {gpu:.1} on the \
                 GPU; {back_ms:.1} copying back {back_mb:.1} MB; {still_ms:.1} waiting for title \
                 memory to hold still, on the refreshing thread), {draws:.0} draws/frame, \
                 dropped {dropped:.0} without a host pipeline ({making} being made) and \
                 {indirect:.0} indirect; heap buffers {picked:.1} passes {picked_ms:.1} ms;                  texture slots written so far: {placed} placed, {objects} of objects; \
                 textures picked by index: {picked_textures} uploaded so far ({picked_mb} MB), \
                 {picked_waiting} waiting, {picked_away} passed over unmapped; depth textures with                  unordered access so far: {du0} made, bound as depth {du1}x, cleared as depth {du2}x,                  unordered views refused {du3}x; buffer views left null as the host would refuse them: {refused}",
                picked_textures = crate::draw::HEAP_TEXTURES.load(Ordering::Relaxed),
                picked_mb = crate::draw::HEAP_TEXTURE_BYTES.load(Ordering::Relaxed) >> 20,
                picked_waiting = crate::draw::HEAP_TEXTURES_WAITING.load(Ordering::Relaxed),
                picked_away = crate::draw::HEAP_TEXTURES_AWAY.load(Ordering::Relaxed),
                du0 = crate::resource::DEPTH_UNORDERED[0].load(Ordering::Relaxed),
                du1 = crate::resource::DEPTH_UNORDERED[1].load(Ordering::Relaxed),
                du2 = crate::resource::DEPTH_UNORDERED[2].load(Ordering::Relaxed),
                du3 = crate::resource::DEPTH_UNORDERED[3].load(Ordering::Relaxed),
                refused = format!(
                    "{} so far; {}",
                    crate::descriptor::host::REFUSED_VIEWS.load(Ordering::Relaxed),
                    crate::resource::twin_line(
                        crate::resource::TWINS[0].load(Ordering::Relaxed),
                        crate::resource::TWINS[1].load(Ordering::Relaxed),
                        twin_to_depth,
                        twin_to_colour,
                        planes,
                        crate::resource::PLANES[1].load(Ordering::Relaxed),
                        crate::resource::PLANES[2].load(Ordering::Relaxed)
                    )
                ),
                making = crate::pipeline::making(),
                placed = crate::descriptor::host::PLACED_TEXTURE_SLOTS.load(Ordering::Relaxed),
                objects = crate::descriptor::host::OBJECT_TEXTURE_SLOTS.load(Ordering::Relaxed)
            );
            // Where the preparing went: the frame line splits it into
            // copies and textures, and a frame that spends seconds outside
            // both says nothing about where (run 93: 4.6 s with 6 draws).
            {
                let ms = |i: usize| PHASES[i].swap(0, Ordering::Relaxed) as f64 / 1e3 / frames;
                let parts: Vec<String> = (0..PHASES.len())
                    .map(|i| format!("{} {:.1} ms", PHASE_NAMES[i], ms(i)))
                    .collect();
                let (made, walked, run_us, kernel_us, deferred, refused) =
                    crate::watch::take_askings();
                tracing::info!(
                    "submission phases per frame: {}, asking the kernel {:.1} times for {:.0} pages \
                     ({:.1} ms finding their regions, {:.1} ms in the kernel), {:.0} pages deferred \
                     to the submissions that follow, {:.0} pages the kernel will not answer about",
                    parts.join(", "),
                    made as f64 / frames,
                    walked as f64 / frames,
                    run_us as f64 / 1e3 / frames,
                    kernel_us as f64 / 1e3 / frames,
                    deferred as f64 / frames,
                    refused as f64 / frames
                );
            }
            // Why the host left buffer views null, once there are any: the
            // total is in the line above, the reasons apart.
            if let Some(reasons) = crate::descriptor::host::refused_census() {
                tracing::info!(
                    "buffer views left null as the host would refuse them, by reason: {reasons}"
                );
            }
            // The dynamic descriptor ring: the one number that said a draw
            // lost its table in a release run (run 2026-11-25 04:38 lost
            // 4,359 a frame behind a single line).
            crate::descriptor::host::dynamic_line(frames);
            // How the frame clock paced this window: a menu that runs at half
            // its rate sits through two ticks a frame, and this names it.
            let (waits, slept) = crate::frame::pacing();
            if waits > 0 {
                tracing::info!(
                    "frame waits: {:.1} a frame sleeping {:.2} ms each (all threads)",
                    waits as f64 / frames,
                    slept as f64 / 1e3 / waits as f64
                );
            }
            // The reasons, in the builds that do not print the frame listing:
            // the same drops, split by why each was not run. Diagnostics
            // builds split them by the detail worth formatting there (shader
            // hashes, missing stages), so they are not printed there too.
            #[cfg(not(feature = "diagnostics"))]
            for (why, n) in drop_reasons.iter().take(10) {
                tracing::info!(
                    "dropped {:.1}/frame without a host pipeline: {}",
                    *n as f64 / frames,
                    why.text()
                );
            }
            #[cfg(feature = "diagnostics")]
            {
                crate::copystat::log(std::mem::take(&mut self.copy_stats_frames) as f64);
                for (why, n) in drops.iter().take(40) {
                    tracing::info!(
                        "dropped {:.1}/frame without a host pipeline: {why}",
                        *n as f64 / frames
                    );
                }
            }
        }
        if self.caption.is_none() {
            // The window's own text, read without a message.
            let mut buffer = vec![0u16; 512];
            // SAFETY: a window handle and a buffer of that length.
            let n = unsafe { InternalGetWindowText(hwnd, &mut buffer) };
            buffer.truncate(n.max(0) as usize);
            self.caption = Some(buffer);
        }
        let Some(caption) = &self.caption else { return };
        let text = format!(" | VIctory {fps:.0} fps {ms:.1} ms/frame {draws:.0} draws/frame");
        let mut text: Vec<u16> = caption.iter().copied().chain(text.encode_utf16()).collect();
        text.push(0);
        // Setting the text waits for the window's thread (a title thread,
        // which may not pump for a while), so a thread of ours does it and
        // presenting never waits; it takes the newest text when free.
        static PENDING: Mutex<Option<(usize, Vec<u16>)>> = Mutex::new(None);
        static SETTER: std::sync::Once = std::sync::Once::new();
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((hwnd.0 as usize, text));
        SETTER.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("victory-caption".into())
                .spawn(|| {
                    loop {
                        let next = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
                        match next {
                            // SAFETY: a window handle and a NUL-terminated text.
                            Some((hwnd, text)) => unsafe {
                                let _ = SetWindowTextW(HWND(hwnd as *mut _), PCWSTR(text.as_ptr()));
                            },
                            None => std::thread::sleep(std::time::Duration::from_millis(100)),
                        }
                    }
                });
        });
    }
}

impl Host {
    /// Shows `texture`, which is in `state` (a host state) and stays in it,
    /// as frame `frame` (saved if captures are on). `None` if nothing was
    /// shown; else, in diagnostics builds, whether the frame was black.
    ///
    /// `since` is what the operations since the last present named
    /// ([`crate::present`]): when none of them wrote `texture`, the target it
    /// did write is shown in its place, by the same pass, at its own size and
    /// format.
    pub(crate) fn present(
        &self,
        texture: &ID3D12Resource,
        state: u32,
        frame: u64,
        since: &crate::present::Since,
    ) -> Option<bool> {
        // SAFETY: a live resource.
        let mut desc = unsafe { texture.GetDesc() };
        let Some(format) = swap_format(desc.Format) else {
            tracing::warn!(format = desc.Format.0, "cannot present this format");
            return None;
        };
        // What is shown: the frame itself, or, when nothing wrote it since the
        // last present, the frame's own source that something did write
        // (`crate::present`). Everything below reads the size, the format and
        // the state of whichever it is, so the fallback is shown exactly as a
        // frame of its size would be.
        present_window_line((desc.Width as u32, desc.Height), since);
        let mut picture = texture;
        let mut picture_state = state;
        let mut picture_this = None;
        // Whether the pass will draw a target at all: the presenter is made
        // with the pass only when it has a pipeline for this format
        // ([`Host::presenter`]), and the copy takes a whole resource and nothing
        // else, so a target of more than one mip or slice is showable only
        // through the pass ([`crate::present::showable`]).
        let pass_there = present_pass().is_some_and(|pass| pass.pipeline(format).is_some());
        match crate::present::pick((desc.Width as u32, desc.Height), since, |t| {
            const DEPTH: u32 = 0x2;
            const DENY_SHADER_RESOURCE: u32 = 0x8;
            let candidate = DXGI_FORMAT(t.format);
            t.sample_count <= 1
                && t.flags & (DEPTH | DENY_SHADER_RESOURCE) == 0
                // The pass draws it, or a copy takes it whole: a target
                // neither can show is no substitute (a copy of another format
                // is one the device refuses, and a wrong image is worse than a
                // black one).
                && crate::present::showable(
                    t.mip_levels,
                    t.depth_or_array_size,
                    pass_there && frame_view(candidate, format).is_some(),
                    copy_compatible(candidate, format),
                )
        }) {
            Ok(index) => {
                let shown = since.targets.get(index).and_then(|target| {
                    let at = target.this();
                    crate::resource::host_resource(at).map(|texture| (target, texture))
                });
                match shown {
                    Some((target, texture)) => {
                        // SAFETY: a live host texture.
                        desc = unsafe { texture.GetDesc() };
                        // The state the pass finds it in, read and not taken:
                        // the list moves it from that state, and a way out that
                        // presents nothing leaves it tracked as it was
                        // (`Restore`). Taking it here would leave a target
                        // tracked as the frame's shader read with no barrier
                        // that ever moved it there.
                        let read = D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE.0 as u32;
                        picture_state = crate::resource::host_copy(target.this()).map_or(
                            read,
                            |copy| copy.state.load(Ordering::Relaxed),
                        );
                        picture = texture;
                        picture_this = Some(target.this());
                        if since.cleared {
                            // The case this fix exists for: the title clears
                            // the texture its upscaler would have filled in.
                            victory_core::logging::warn_once("present.fallback.cleared", || {
                                format!(
                                    "presented frame was cleared but never drawn into; showing \
                                     the {}x{} frame it did write",
                                    desc.Width, desc.Height
                                )
                            });
                        } else {
                            victory_core::logging::warn_once("present.fallback", || {
                                format!(
                                    "presented frame was not written; showing the {}x{} frame it \
                                     did write (VICTORY_PRESENT_FALLBACK=1 turns this on)",
                                    desc.Width, desc.Height
                                )
                            });
                        }
                    }
                    // A target `pick` chose that has no host texture any more.
                    None => nothing_written(since),
                }
            }
            Err(crate::present::Why::Competing(sizes)) => {
                // Once: the sizes that were as good as each other, and the
                // black frame left rather than a guess between them.
                victory_core::logging::warn_once("present.fallback.competing", || {
                    let sizes: Vec<String> =
                        sizes.iter().map(|(w, h)| format!("{w}x{h}")).collect();
                    format!(
                        "presented frame was not written and {} targets are as good as each \
                         other ({}): none is shown",
                        sizes.len(),
                        sizes.join(", ")
                    )
                });
            }
            // Nothing was written into the frame and no target of its shape
            // either: once, what the title did write, so the next run says why.
            Err(crate::present::Why::Nothing) => nothing_written(since),
            Err(crate::present::Why::Written) => {}
        }
        let size = (desc.Width as u32, desc.Height);
        // What is shown may be a target of the frame's, moved to be read as a
        // pixel shader resource above. Everything below can leave without
        // presenting (no swap chain, a minimised window, a refused resize, no
        // back buffer), and a target left tracked in a state no barrier moved
        // it to is a texture the rest of the run moves from the wrong one: the
        // guard puts it back on every return, and the present that gets this
        // far puts it back itself and disarms the guard.
        let mut restore = Restore::new(|| {
            if let Some(this) = picture_this {
                crate::resource::host_transition(this, picture_state);
            }
        });
        let mut presenter = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        if presenter.is_none() {
            *presenter = match self.presenter(size, format) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "no swap chain");
                    return None;
                }
            };
        }
        let p = presenter.as_mut()?;
        // The window's client area is the one source of truth for the size: the
        // swap chain's buffers are its size and follow it when the window is
        // resized. A minimised (or closed) window has none, and a swap chain of
        // a size of zero is not one DXGI makes, so nothing is presented while it
        // has none.
        let client = client_size(p.hwnd)?;
        // What the run shows: the frame drawn into the largest part of the client
        // area with its own shape, bars round it ([`fit`]); one `CopyResource`
        // when the frame and the client area are one size; and, without the pass
        // or for a frame whose texture refuses shader reads, DXGI's plain stretch
        // of the frame's own size.
        //
        // The view names a format the frame's own resource allows
        // ([`frame_view`]): the swap chain's only for a typeless frame, the
        // frame's own otherwise. `None` is a frame the pass cannot read, which
        // leaves the copy path below in place rather than draw with a view
        // D3D12 would refuse silently.
        let view = frame_view(desc.Format, format);
        note_frame_view(desc.Format, format, view);
        let drawn = p.pass.filter(|_| readable(&desc)).zip(view).and_then(
            |(pass, view)| pass.pipeline(format).map(|pipeline| (pass, pipeline, view)),
        );
        // Whether a copy can show this picture as it is: the swap chain's own
        // format, at the client area's size ([`copy_compatible`],
        // [`draw_or_copy`]), and a resource of one mip and one slice, which is
        // all a `CopyResource` into a back buffer takes whole — a copy of a mip
        // chain or an array is refused, so such a picture is drawn by the pass
        // instead ([`crate::present::whole`]).
        let copyable = copy_compatible(desc.Format, format)
            && crate::present::whole(desc.MipLevels, desc.DepthOrArraySize);
        let (chain, show) = match draw_or_copy(drawn.is_some(), copyable, client == size) {
            Some(true) => (client, Show::Drawn(fit(size, client)?)),
            Some(false) => (size, Show::Copy),
            None => {
                // Neither the pass nor a copy: nothing is presented, and the
                // next run says why rather than showing a stretched black
                // frame from a `CopyResource` the device refused.
                victory_core::logging::warn_once("present.unshowable", || {
                    format!(
                        "the presented frame is in a format the present pass cannot read as float \
                         and cannot be copied into the swap chain (frame_format={}, \
                         swap_format={}): nothing is shown",
                        desc.Format.0, format.0
                    )
                });
                return None;
            }
        };
        // What the run shows, for the log: the letters, or the stretch.
        let shown = match show {
            Show::Drawn(_) => {
                "the frame is drawn into the largest part of the client area with its own shape: bars at the \
                 sides or the top and bottom"
            }
            Show::Copy if chain == client => "the frame and the client area are one size: the present copies it",
            Show::Copy => {
                "DXGI stretches the frame's own size over the client area: a window of another shape distorts it"
            }
        };
        if (p.size, p.format) != (chain, format) {
            // Presents sent ahead hold back buffers until they have run.
            settle_now(WHY_SYNC);
            // SAFETY: no back buffer is held; every submission was waited for.
            let resized = unsafe {
                p.swap_chain
                    .ResizeBuffers(BUFFERS, chain.0, chain.1, format, DXGI_SWAP_CHAIN_FLAG(0))
            };
            match resized {
                Ok(()) => {
                    (p.size, p.format) = (chain, format);
                    tracing::info!(
                        client_width = client.0,
                        client_height = client.1,
                        swap_chain_width = chain.0,
                        swap_chain_height = chain.1,
                        frame_width = size.0,
                        frame_height = size.1,
                        shown,
                        "the swap chain sized to the window's client area, and what the present shows"
                    );
                }
                Err(e) => {
                    resize_refused(&e, client);
                    return None;
                }
            }
        }
        let state = D3D12_RESOURCE_STATES(picture_state as i32);
        // A draw reads the frame as a pixel shader resource; a copy wants it as
        // a copy source.
        let (source, target) = match show {
            Show::Copy => (D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST),
            Show::Drawn(_) => (
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
            ),
        };
        // SAFETY: our swap chain, list and a live source texture.
        unsafe {
            let index = p.swap_chain.GetCurrentBackBufferIndex();
            let Ok(back) = p.swap_chain.GetBuffer::<ID3D12Resource>(index) else {
                return None;
            };
            // Sent ahead ([`late_free_present`]) unless the frame is to be
            // read back at once (a saved or listed frame): then a list of
            // its own, one whose last flight has finished or a new one.
            #[cfg(feature = "diagnostics")]
            let read_now = crate::capture::wanted(frame) || crate::capture::listed(frame);
            #[cfg(not(feature = "diagnostics"))]
            let read_now = false;
            let slot = if late_free_present() && !read_now {
                let finished = FINISHED.load(Ordering::Acquire);
                p.ahead.iter().position(|(_, flown)| *flown <= finished).or_else(|| {
                    let list = self.new_list().ok()?;
                    list.close();
                    p.ahead.push((list, 0));
                    Some(p.ahead.len() - 1)
                })
            } else {
                None
            };
            let copy = match slot {
                Some(i) => &p.ahead[i].0,
                None => &p.copy,
            };
            copy.reset();
            let list = &copy.list;
            move_to(list, picture, state, source);
            list.ResourceBarrier(&[transition(&back, D3D12_RESOURCE_STATE_PRESENT, target)]);
            match (show, drawn) {
                (Show::Drawn(rect), Some((pass, pipeline, view))) => record_present_pass(
                    self,
                    list,
                    pass,
                    &pipeline,
                    PresentDraw {
                        frame: picture,
                        back: &back,
                        view,
                        rect,
                    },
                ),
                // SAFETY: a source texture and a back buffer of one size.
                _ => list.CopyResource(&back, picture),
            }
            #[cfg(feature = "diagnostics")]
            let capture = crate::capture::wanted(frame)
                .then(|| {
                    // A copy wants the frame as a copy source; the pass reads it
                    // as a pixel shader resource.
                    move_to(list, picture, source, D3D12_RESOURCE_STATE_COPY_SOURCE);
                    let read = crate::capture::read_back(&self.device, list, picture);
                    move_to(list, picture, D3D12_RESOURCE_STATE_COPY_SOURCE, source);
                    read
                })
                .flatten();
            #[cfg(feature = "diagnostics")]
            let probe = if slot.is_some() {
                None
            } else {
                // As the capture: a copy wants the frame as a copy source.
                move_to(list, picture, source, D3D12_RESOURCE_STATE_COPY_SOURCE);
                let probe = crate::capture::probe(&self.device, list, picture);
                move_to(list, picture, D3D12_RESOURCE_STATE_COPY_SOURCE, source);
                probe
            };
            #[cfg(not(feature = "diagnostics"))]
            let _ = frame;
            move_to(list, picture, source, state);
            if let Some(this) = picture_this {
                // The shown target, not the frame: the state it is tracked in
                // is again the one the pass found it in, as `queue::present`
                // puts the frame's back.
                crate::resource::host_transition(this, picture_state);
            }
            // Put back above: nothing left for the way out to do.
            restore.done();
            // SAFETY: a live back buffer of the swap chain.
            list.ResourceBarrier(&[transition(&back, target, D3D12_RESOURCE_STATE_PRESENT)]);
            copy.close();
            match slot {
                Some(i) => {
                    let flown = self.execute_own(copy);
                    p.ahead[i].1 = flown;
                }
                None => self.execute(&[copy]),
            }
            #[cfg(feature = "diagnostics")]
            if let Some((buffer, footprint)) = capture {
                crate::capture::save_read_back(frame, &buffer, &footprint);
            }
            let _ = p.swap_chain.Present(0, DXGI_PRESENT(0));
            p.meter.frame(p.hwnd);
            #[cfg(feature = "diagnostics")]
            return Some(probe.is_some_and(|b| crate::capture::black(&b)));
            #[cfg(not(feature = "diagnostics"))]
            Some(false)
        }
    }

    fn presenter(
        &self,
        frame: (u32, u32),
        format: DXGI_FORMAT,
    ) -> windows::core::Result<Presenter> {
        let hwnd = match title_window() {
            Some(hwnd) => hwnd,
            None => window(frame.0, frame.1)?,
        };
        // The window's client area is the one source of truth for the size the
        // console presents at: the swap chain's buffers are its size. DXGI scales
        // a flip chain's back buffer over the whole client area, and its
        // aspect-ratio scaling mode is documented as behaving like a plain
        // stretch for a Win32 flip chain, so a back buffer of the frame's own
        // size (a console title renders its own resolution setting, not the
        // window) would distort the picture in a window of another shape. The
        // frame is drawn into the client area's buffers instead, letterboxed
        // ([`fit`]); a D3D12 copy cannot scale, so that takes the present pass.
        //
        // shortcut: when the host will not make the pass (no shader compiler, a
        // frame whose texture refuses shader reads), the back buffers are the
        // frame's size and DXGI stretches each present into the window: the
        // picture is distorted in a window of another shape, as it was before the
        // pass existed.
        let client = client_size(hwnd).unwrap_or(frame);
        let pass = present_pass().filter(|pass| pass.pipeline(format).is_some());
        let (width, height) = if pass.is_some() { client } else { frame };
        // SAFETY: plain object creation for our window.
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let desc = |scaling: DXGI_SCALING| DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: BUFFERS,
                Scaling: scaling,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                ..Default::default()
            };
            let made = factory.CreateSwapChainForHwnd(
                &self.queue,
                hwnd,
                &desc(DXGI_SCALING_STRETCH),
                None,
                None,
            )?;
            let swap_chain = made.cast::<IDXGISwapChain3>()?;
            let copy = self.new_list()?;
            copy.close();
            tracing::info!(
                client_width = client.0,
                client_height = client.1,
                swap_chain_width = width,
                swap_chain_height = height,
                frame_width = frame.0,
                frame_height = frame.1,
                format = format.0,
                shown = if pass.is_some() {
                    "the frame is drawn into the largest part of that area with its own shape: bars at the sides or \
                     the top and bottom (the title renders its own resolution setting, not the window)"
                } else {
                    "DXGI stretches the frame's own size over that area: a window of another shape distorts it"
                },
                "presenting to the window's client area, the size of the swap chain"
            );
            Ok(Presenter {
                swap_chain,
                size: (width, height),
                format,
                copy,
                ahead: Vec::new(),
                pass,
                hwnd,
                meter: Meter::new(),
            })
        }
    }
}

/// A window's client area: the size the swap chain's buffers are, and what DXGI
/// scales them into.
fn client_size(hwnd: HWND) -> Option<(u32, u32)> {
    let mut rect = RECT::default();
    // SAFETY: a window handle, and a rectangle that receives its client area.
    unsafe { GetClientRect(hwnd, &mut rect) }.ok()?;
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    (w > 0 && h > 0).then_some((w as u32, h as u32))
}

/// Every couple of seconds: what the present was told the window did, so a black
/// window can be traced to the texture the title really writes. Names the frame's
/// size and, for each target rendered into since the last present, the texture,
/// its shape and flags, and whether a shader read it.
fn present_window_line(frame: (u32, u32), since: &crate::present::Since) {
    use std::sync::atomic::{AtomicU32, Ordering};
    /// Presents between lines, at ~20 fps a line every two seconds.
    const EVERY: u32 = 40;
    static PRESENTS: AtomicU32 = AtomicU32::new(0);
    if !PRESENTS.fetch_add(1, Ordering::Relaxed).is_multiple_of(EVERY) {
        return;
    }
    let (width, height) = frame;
    let mut named: Vec<String> = since
        .targets
        .iter()
        .take(6)
        .map(crate::present::Target::describe)
        .collect();
    if since.targets.len() > named.len() {
        named.push(format!("and {} more", since.targets.len() - named.len()));
    }
    let names = if named.is_empty() {
        "none".to_string()
    } else {
        named.join("; ")
    };
    tracing::info!(
        frame_width = width,
        frame_height = height,
        written = since.written,
        cleared = since.cleared,
        targets = %names,
        "the window a present closes"
    );
}

/// Once: a present whose frame nothing wrote, and no target of its shape to
/// stand for it. Names what the title did write, so the run says why the window
/// is black and a later one whether this fix saw the frame at all.
fn nothing_written(since: &crate::present::Since) {
    victory_core::logging::warn_once("present.fallback.nothing", || {
        let mut sizes: Vec<String> = Vec::new();
        for target in &since.targets {
            let size = format!("{}x{}", target.width, target.height);
            if !sizes.contains(&size) {
                sizes.push(size);
            }
        }
        let more = sizes.len().saturating_sub(8);
        sizes.truncate(8);
        if more > 0 {
            sizes.push(format!("and {more} more"));
        }
        let written = if sizes.is_empty() {
            "none".to_string()
        } else {
            sizes.join(", ")
        };
        let frame = if since.cleared {
            "presented frame was cleared but never drawn into"
        } else {
            "presented frame was not written"
        };
        format!(
            "{frame}, and no target of its shape was written either (targets written: \
             {written}): showing it as it is"
        )
    });
}

/// Records `texture`'s move from `before` to `after` in `list`, unless it is
/// already there: a barrier that changes nothing is a debug-layer message and
/// no more.
fn move_to(
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) {
    if before.0 == after.0 {
        return;
    }
    // SAFETY: an open list and a live resource.
    unsafe { list.ResourceBarrier(&[transition(texture, before, after)]) };
}

#[cfg(test)]
mod present_tests {
    use super::{Fit, fit};
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
        DXGI_FORMAT_R8G8B8A8_TYPELESS, DXGI_FORMAT_R8G8B8A8_UNORM,
        DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
    };

    /// A view of the frame names a format the frame's own resource allows. The
    /// swap chain's is one only for a typeless frame; a typed frame is read with
    /// its own format, because D3D12 refuses a `_UNORM` view of a typed
    /// `_UNORM_SRGB` resource, and `CreateShaderResourceView` returns nothing to
    /// say so.
    #[test]
    fn an_srgb_frame_is_read_with_its_own_format() {
        // The sRGB frame the audit's black window came from: it presents into
        // the family's linear swap chain format...
        assert_eq!(
            super::swap_format(DXGI_FORMAT_R8G8B8A8_UNORM_SRGB),
            Some(DXGI_FORMAT_R8G8B8A8_UNORM)
        );
        // ...and its view must name the resource's own format, not that one.
        assert_eq!(
            super::frame_view(DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, DXGI_FORMAT_R8G8B8A8_UNORM),
            Some(DXGI_FORMAT_R8G8B8A8_UNORM_SRGB)
        );
        // The plain frame: the two agree, so nothing about today's run changes.
        assert_eq!(
            super::frame_view(DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM),
            Some(DXGI_FORMAT_R8G8B8A8_UNORM)
        );
        // A typeless frame: the swap chain's format views it, and a view may not
        // name the typeless format itself.
        assert_eq!(
            super::frame_view(DXGI_FORMAT_R8G8B8A8_TYPELESS, DXGI_FORMAT_R8G8B8A8_UNORM),
            Some(DXGI_FORMAT_R8G8B8A8_UNORM)
        );
        assert_ne!(
            super::frame_view(DXGI_FORMAT_R8G8B8A8_TYPELESS, DXGI_FORMAT_R8G8B8A8_UNORM),
            Some(DXGI_FORMAT_R8G8B8A8_TYPELESS)
        );
    }

    /// Every frame the pass can present is read with a format its resource
    /// allows: its own when typed, the swap chain's when typeless, never a
    /// typeless one; an integer frame is left to the copy path.
    #[test]
    fn a_view_of_the_frame_names_a_format_the_frame_allows() {
        let integer = [12, 14, 25, 30, 32];
        let frames = [
            9, 10, 11, 12, 13, 14, 23, 24, 25, 27, 28, 29, 30, 31, 32, 87, 90, 91,
        ];
        for format in frames {
            let frame = DXGI_FORMAT(format);
            let Some(swap) = super::swap_format(frame) else {
                panic!("frame {format} has no swap chain format");
            };
            match super::frame_view(frame, swap) {
                Some(view) => {
                    assert!(
                        !super::typeless(view),
                        "frame {format}: a view may not name the typeless format {view:?}"
                    );
                    assert!(
                        !integer.contains(&format),
                        "frame {format} is integer-typed: it cannot be read as float"
                    );
                    let allowed = if super::typeless(frame) { swap } else { frame };
                    assert_eq!(view, allowed, "frame {format}");
                }
                None => assert!(
                    integer.contains(&format),
                    "frame {format} is refused but is not integer-typed"
                ),
            }
        }
        // The family of the other swap chain format, for shape.
        assert_eq!(
            super::frame_view(DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM),
            Some(DXGI_FORMAT_B8G8R8A8_UNORM)
        );
    }

    /// The pass must write every colour channel: a `Default` blend description
    /// writes none, which turns the window black with no error to read.
    #[test]
    fn the_present_pass_writes_every_colour_channel() {
        let (blend, _) = super::present_states();
        for (target, described) in blend.RenderTarget.iter().enumerate() {
            assert_eq!(
                described.RenderTargetWriteMask,
                super::D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
                "render target {target} writes no colour"
            );
        }
    }

    /// The frame keeps its shape in the client area, scaled by the smaller of
    /// the two ratios and centred: bars at the sides, or at the top and bottom.
    #[test]
    fn the_frame_keeps_its_shape_in_the_client_area() {
        // 4K into a 16:9 client area: the whole of it, no bars.
        assert_eq!(
            fit((3840, 2160), (1920, 1080)),
            Some(Fit {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            })
        );
        // 4K into a larger 16:9 one: still the whole of it.
        assert_eq!(
            fit((3840, 2160), (3840, 2160)),
            Some(Fit {
                x: 0,
                y: 0,
                width: 3840,
                height: 2160
            })
        );
        // A 21:9 window (3440x1440): bars at the sides, 440 pixels each, and the
        // frame's height fills the window's.
        assert_eq!(
            fit((3840, 2160), (3440, 1440)),
            Some(Fit {
                x: 440,
                y: 0,
                width: 2560,
                height: 1440
            })
        );
        // A window taller than it is wide (1080x1920): bars at the top and
        // bottom, 656 pixels each, and the frame's width fills the window's.
        assert_eq!(
            fit((3840, 2160), (1080, 1920)),
            Some(Fit {
                x: 0,
                y: 656,
                width: 1080,
                height: 607
            })
        );
        // The measured run: a 3840x2160 frame in the 1978x1083 client area the
        // title's own window was fitted to, 26 pixels of bar each side.
        assert_eq!(
            fit((3840, 2160), (1978, 1083)),
            Some(Fit {
                x: 26,
                y: 0,
                width: 1925,
                height: 1083
            })
        );
    }

    /// A window with no client area (minimised) has nowhere to put the frame, and
    /// a frame with no size is nothing to put: no rectangle, so no swap chain of
    /// a size of zero.
    #[test]
    fn a_size_of_zero_has_no_place() {
        assert_eq!(fit((3840, 2160), (0, 0)), None);
        assert_eq!(fit((3840, 2160), (0, 1080)), None);
        assert_eq!(fit((3840, 2160), (1920, 0)), None);
        assert_eq!(fit((0, 0), (1920, 1080)), None);
        assert_eq!(fit((3840, 0), (1920, 1080)), None);
    }

    /// Whatever the two sizes, the rectangle stays inside the client area and
    /// keeps the frame's shape to within a pixel of rounding.
    #[test]
    fn the_rectangle_fits_the_client_area_at_the_frames_shape() {
        for client in [(640, 480), (1366, 768), (2560, 1080), (800, 1280), (7, 900)] {
            let Some(rect) = fit((3840, 2160), client) else {
                panic!("no place for the frame in {client:?}");
            };
            assert!(rect.width >= 1 && rect.height >= 1, "{client:?}: {rect:?}");
            assert!(rect.x + rect.width <= client.0, "{client:?}: {rect:?}");
            assert!(rect.y + rect.height <= client.1, "{client:?}: {rect:?}");
            // The picture's shape: the two cross products differ by less than a
            // pixel's worth of the longer side.
            let (left, right) = (
                u64::from(rect.width) * 2160,
                u64::from(rect.height) * 3840,
            );
            assert!(
                left.abs_diff(right) <= 3840,
                "{client:?}: {rect:?} is not 16:9"
            );
        }
    }

    /// A frame the swap chain takes as it is still takes the path it takes
    /// today: the same format at the client area's size is a `CopyResource`,
    /// never a stretch through the present pass.
    #[test]
    fn a_frame_of_the_swap_chains_own_format_is_still_copied() {
        assert!(super::copy_compatible(
            DXGI_FORMAT_R8G8B8A8_UNORM,
            DXGI_FORMAT_R8G8B8A8_UNORM
        ));
        assert_eq!(super::draw_or_copy(true, true, true), Some(false));
    }

    /// A copy takes the format as it is: a picture of another format is never
    /// copied into the back buffer, whatever its size. It is drawn when the
    /// pass reads it, and nothing is shown when the pass cannot read it
    /// either — a black window rather than a present the device refuses.
    #[test]
    fn a_picture_of_another_format_is_not_copied() {
        // 64 bpp into a 32 bpp chain: `CopyResource` refuses it.
        assert!(!super::copy_compatible(
            DXGI_FORMAT_R16G16B16A16_FLOAT,
            DXGI_FORMAT_R8G8B8A8_UNORM
        ));
        assert_eq!(super::draw_or_copy(true, false, true), Some(true));
        assert_eq!(super::draw_or_copy(false, false, true), None);
        // sRGB into the family's linear chain: another format, so a draw.
        assert!(!super::copy_compatible(
            DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
            DXGI_FORMAT_R8G8B8A8_UNORM
        ));
        // A typeless picture: a format of its family, the chain's, stands in.
        assert!(super::copy_compatible(
            DXGI_FORMAT_R8G8B8A8_TYPELESS,
            DXGI_FORMAT_R8G8B8A8_UNORM
        ));
    }

    /// A copy is only offered at the client area's size: a picture of the
    /// chain's own format but another size is stretched by the pass, and one
    /// the pass cannot read is not presented at all.
    #[test]
    fn a_copy_is_only_offered_at_the_client_areas_size() {
        assert_eq!(super::draw_or_copy(true, true, false), Some(true));
        assert_eq!(super::draw_or_copy(false, true, false), None);
    }
}

/// A visible top-level window of this process: the one the title made, if
/// it made one.
fn title_window() -> Option<HWND> {
    extern "system" fn visit(hwnd: HWND, found: LPARAM) -> windows::core::BOOL {
        let mut process = 0;
        // SAFETY: a window handle from the enumeration; `found` is ours.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process));
            if process == windows::Win32::System::Threading::GetCurrentProcessId()
                && IsWindowVisible(hwnd).as_bool()
            {
                *(found.0 as *mut Option<HWND>) = Some(hwnd);
                return false.into();
            }
        }
        true.into()
    }
    let mut found: Option<HWND> = None;
    // SAFETY: the callback only writes `found`, which outlives the call.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&raw mut found as isize)) };
    found
}

/// `state` without the bits the host texture's flags forbid: the console
/// lets the title name states its own resources allow (unordered access on
/// a depth target, whose host copy cannot have both).
pub(crate) fn legal_state(texture: &ID3D12Resource, state: u32) -> u32 {
    // SAFETY: a plain query.
    legal_for(unsafe { texture.GetDesc() }.Flags.0 as u32, state)
}

/// [`legal_state`] for a texture with resource flags `flags`.
pub(crate) fn legal_for(flags: u32, state: u32) -> u32 {
    // (flag, set or clear forbids, state bits): render target, unordered
    // access, depth, denied shader resource.
    const FORBIDDEN: [(u32, bool, u32); 4] = [
        (0x1, false, 0x4),
        (0x4, false, 0x8),
        (0x2, false, 0x10 | 0x20),
        (0x8, true, 0x40 | 0x80),
    ];
    FORBIDDEN
        .iter()
        .filter(|&&(flag, deny, _)| (flags & flag != 0) == deny)
        .fold(state, |s, &(_, _, bits)| s & !bits)
}

/// What a state request does to a texture in a list, by the state the list
/// holds for it ([`HostList::transition`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Move {
    /// The list's first use of the texture: its prelude brings the texture
    /// there at submission, so nothing is recorded now.
    FirstUse(u32),
    /// The list holds that state: nothing moves.
    Settled,
    /// From the state held to the one wanted: the barrier the list records.
    Moved(u32, u32),
}

/// [`Move`] for a request of `wanted` (a console state) when the list holds
/// `held` (the last host state it moved the texture to), with `legal` giving
/// the host state the texture's flags allow.
///
/// A request equal to the held state settles it without asking `legal`: the
/// map only ever holds a state the flags allow, so a request equal to one
/// asks for no bit they forbid, and the exact answer is "nothing moves"
/// either way. That is the pre-test that matters: a draw asks for its
/// targets, its depth buffer and every texture it reads on every draw, and
/// they are already there (18.6 of 33 a draw; run 20261125-094023).
fn moving(held: Option<u32>, wanted: u32, legal: impl Fn(u32) -> u32) -> Move {
    let Some(last) = held else {
        let to = legal(wanted);
        return Move::FirstUse(to);
    };
    if last == wanted {
        return Move::Settled;
    }
    let to = legal(wanted);
    if to == last { Move::Settled } else { Move::Moved(last, to) }
}

impl Host {
    /// The card's memory as the system accounts it to this process, in
    /// bytes: the budget and the use of the card's own memory, then of the
    /// PC's memory shared with the card (where the system moves what does
    /// not fit on the card).
    pub(crate) fn video_memory(&self) -> Option<[u64; 4]> {
        let adapter = self.adapter.as_ref()?;
        let mut own = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let mut shared = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: plain queries that fill our structures.
        unsafe {
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut own)
                .ok()?;
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut shared)
                .ok()?;
        }
        Some([own.Budget, own.CurrentUsage, shared.Budget, shared.CurrentUsage])
    }

    /// A buffer in system memory that the CPU maps (write-back, so reading
    /// it is fast too) and shaders read and write: for host copies of title
    /// memory that shaders write, which upload memory cannot be. Starts in
    /// the common state (buffers move out of it implicitly).
    pub(crate) fn shared_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        MADE[2].fetch_add(1, Ordering::Relaxed);
        MADE[3].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of a CPU-visible buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no shared buffer");
                note_removal(&e);
                None
            }
        }
    }

    /// A device-local buffer shaders may write (unordered access) and indirect
    /// execution may read, starting in the common state.
    pub(crate) fn uav_buffer(&self, size: u64) -> Option<ID3D12Resource> {
        MADE[4].fetch_add(1, Ordering::Relaxed);
        MADE[5].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a device-local buffer.
        let made = unsafe {
            self.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut buffer,
            )
        };
        match made {
            Ok(()) => buffer,
            Err(e) => {
                tracing::warn!(error = %e, size, "no unordered-access buffer");
                None
            }
        }
    }
}

/// For adapters without device-removed data (the software one): the last
/// commands each list of the failed submission noted (diagnostics, with
/// `VICTORY_D3D12_DRED`).
fn last_notes(runs: &[&HostList]) -> String {
    #[cfg(feature = "diagnostics")]
    {
        let mut text = String::new();
        for (i, r) in runs.iter().enumerate() {
            let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
            for (n, _) in notes.iter().rev().take(6).rev() {
                text.push_str(&format!("; list {i} of {}, of its {}: {n}", runs.len(), notes.len()));
            }
        }
        text
    }
    #[cfg(not(feature = "diagnostics"))]
    {
        let _ = runs;
        String::new()
    }
}

/// Where the faulting address `va` lies against a range `(start, bytes)` a
/// command reads (diagnostics).
#[cfg(feature = "diagnostics")]
fn fault_in(va: u64, (start, bytes): (u64, u64)) -> String {
    let end = start + bytes;
    if va < start {
        format!(" (fault {:#x} below its source)", start - va)
    } else if va < end {
        format!(" (fault at source +{:#x})", va - start)
    } else {
        format!(" (fault {:#x} past its source's end)", va - end)
    }
}

/// The commands that may have been running where a list stopped at
/// command `done`: those between the barriers around it (work between
/// barriers overlaps on the GPU), by kind, and what the markers just
/// before them say (`HostList::note_op`: diagnostics builds with
/// device-removed data on).
fn in_flight(history: &[D3D12_AUTO_BREADCRUMB_OP], markers: &[(u32, String)], done: usize) -> String {
    let barrier = |op: &D3D12_AUTO_BREADCRUMB_OP| *op == D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER;
    let from = history[..done].iter().rposition(barrier).map_or(0, |b| b + 1);
    let to = history[done..]
        .iter()
        .position(barrier)
        .map_or(history.len(), |b| (done + b).max(done + 1));
    let mut text = format!(", commands {from}..{to} between its barriers:");
    let mut shown = 0;
    // From just before the command the GPU had not finished: the first
    // twelve after the barrier named draws long done when a list of 79
    // mesh draws stopped at its 45th (run 41).
    for i in from.max(done.saturating_sub(4))..to {
        let op = history[i];
        if op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER || shown == 12 {
            continue;
        }
        shown += 1;
        let marker = i
            .checked_sub(1)
            .filter(|&m| history[m] == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
            .and_then(|m| markers.iter().find(|(at, _)| *at as usize == m))
            .map_or("no marker", |(_, s)| s.as_str());
        text.push_str(&format!("; command {i} (op {}): {marker}", op.0));
    }
    text
}

pub(crate) fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: a borrowed pointer; the barrier never releases it.
                pResource: unsafe { std::mem::transmute_copy(resource) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// A window of the given client size, owned by its own message thread.
fn window(width: u32, height: u32) -> windows::core::Result<HWND> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("victory-window".into())
        .spawn(move || {
            let made = make_window(width, height);
            let ok = made.is_ok();
            let _ = sender.send(made.map(|h| h.0 as usize));
            if ok {
                pump();
            }
        })
        .map_err(|_| windows::core::Error::empty())?;
    let hwnd = receiver
        .recv()
        .map_err(|_| windows::core::Error::empty())??;
    Ok(HWND(hwnd as *mut _))
}

extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: the default handling for our window.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn make_window(width: u32, height: u32) -> windows::core::Result<HWND> {
    const CLASS: PCWSTR = w!("VIctory");
    // SAFETY: registers our class once and creates a window of it.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassExW(&class);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false)?;
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("VIctory"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
}

fn pump() {
    let mut message = MSG::default();
    // SAFETY: the standard message loop of this thread's window.
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[cfg(test)]
mod trail {
    /// The trail keeps the newest things, and the report made when the
    /// host device is removed says them and the pipeline library's state
    /// (no card is asked for any of it).
    #[test]
    fn the_removal_report_says_the_newest_things_done() {
        for n in 0..super::TRAIL_MOST + 5 {
            super::trail(|| format!("thing {n} done"));
        }
        let text = super::removal_context();
        assert!(text.contains("the submitting thread was"));
        assert!(text.contains("pipeline library: no call running"));
        assert!(text.contains(&format!("thing {} done", super::TRAIL_MOST + 4)));
        assert!(!text.contains("thing 4 done"), "the oldest entries are dropped");
    }
}

#[cfg(test)]
mod spare {
    use super::Spare;

    /// Kept things come back by size, the last given first, and nothing is
    /// kept past the limit.
    #[test]
    fn kept_things_come_back_by_size_within_the_limit() {
        let mut spare: Spare<u32> = Spare::new();
        assert_eq!(spare.take(4), None);
        assert_eq!(spare.give(1, 4, 10), None);
        assert_eq!(spare.give(2, 1, 10), None);
        assert_eq!(spare.give(3, 4, 10), None);
        assert_eq!(spare.bytes, 9);
        // Past the limit: handed back, nothing changes.
        assert_eq!(spare.give(4, 4, 10), Some(4));
        assert_eq!(spare.bytes, 9);
        assert_eq!(spare.take(2), None, "no thing of that size");
        assert_eq!(spare.take(4), Some(3));
        assert_eq!(spare.take(4), Some(1));
        assert_eq!(spare.take(4), None);
        assert_eq!(spare.take(1), Some(2));
        assert_eq!(spare.bytes, 0);
        assert!(spare.kept.is_empty());
    }
}

#[cfg(test)]
mod batch {   // Batch::texture_due lives here; `mod tests` is already taken at the top of this file
    use super::Batch;
    use std::sync::Arc;

    #[test]
    fn a_texture_is_checked_once_per_submission() {
        let mut batch = Batch::default();
        let (a, b) = (Arc::new([0u8; 16]), Arc::new([0u8; 16]));
        // The first list sampling each texture checks it ...
        assert!(batch.texture_due(&a));
        assert!(batch.texture_due(&b));
        // ... later lists of the same submission do not.
        assert!(!batch.texture_due(&a));
        assert!(!batch.texture_due(&a.clone()));
        assert!(!batch.texture_due(&b));
    }

    #[test]
    fn title_writes_make_textures_due_again() {
        let mut batch = Batch::default();
        let a = Arc::new(1u32);
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        // A copy into title memory or work that may write it may have
        // changed the texture's memory.
        batch.title_written();
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        // A fresh submission starts over.
        let mut next = Batch::default();
        assert!(next.texture_due(&a));
    }

    /// The hashing a submission does for textures in physical-page memory,
    /// with every list checking the textures it samples (before) and with
    /// each checked once (after). Run with
    /// `cargo test --release -p victory-gpu -- --ignored --nocapture texture_checks`.
    #[test]
    #[ignore = "benchmark"]
    fn texture_checks_per_submission() {
        const TEXTURES: usize = 48;
        const SIZE: usize = 4 << 20;
        const LISTS: usize = 12;
        let textures: Vec<Arc<Vec<u8>>> = (0..TEXTURES)
            .map(|t| Arc::new((0..SIZE).map(|i| (i * 31 + t) as u8).collect()))
            .collect();
        let submit = |dedupe: bool| {
            let mut batch = Batch::default();
            let mut hashed = 0usize;
            let started = std::time::Instant::now();
            for _ in 0..LISTS {
                for t in &textures {
                    if !dedupe || batch.texture_due(t) {
                        std::hint::black_box(blake3::hash(t));
                        hashed += 1;
                    }
                }
            }
            (started.elapsed(), hashed)
        };
        submit(false); // warm up
        let (before, n_before) = submit(false);
        let (after, n_after) = submit(true);
        assert_eq!((n_before, n_after), (TEXTURES * LISTS, TEXTURES));
        println!(
            "{LISTS} lists sampling {TEXTURES} textures of {} MiB: \
             before {n_before} hashes in {before:?}, after {n_after} in {after:?} ({:.1}x)",
            SIZE >> 20,
            before.as_secs_f64() / after.as_secs_f64()
        );
    }

    #[test]
    fn vram_idle_ms_scales_with_pressure() {
        use super::vram_idle_ms;
        let gib = 1024 * 1024 * 1024;
        // Normal (< 75%)
        assert_eq!(vram_idle_ms(Some([10 * gib, 0, 0, 0])), 5_000);
        assert_eq!(vram_idle_ms(Some([10 * gib, 7 * gib, 0, 0])), 5_000);
        assert_eq!(vram_idle_ms(Some([100, 74, 0, 0])), 5_000);
        // Moderate (75%..85%)
        assert_eq!(vram_idle_ms(Some([100, 75, 0, 0])), 2_500);
        assert_eq!(vram_idle_ms(Some([100, 84, 0, 0])), 2_500);
        // High (>= 85%, including over-budget)
        assert_eq!(vram_idle_ms(Some([100, 85, 0, 0])), 1_000);
        assert_eq!(vram_idle_ms(Some([10 * gib, 12 * gib, 0, 0])), 1_000);
        // Missing or zero budget falls back to normal
        assert_eq!(vram_idle_ms(None), 5_000);
        assert_eq!(vram_idle_ms(Some([0, 100, 0, 0])), 5_000);
    }

    #[test]
    fn ram_idle_ms_scales_with_pressure() {
        use super::ram_idle_ms;
        let gib = 1024 * 1024 * 1024;
        // Normal (>= 4 GiB)
        assert_eq!(ram_idle_ms(Some(8 * gib)), 10_000);
        assert_eq!(ram_idle_ms(Some(4 * gib)), 10_000);
        // Moderate (2 GiB..4 GiB)
        assert_eq!(ram_idle_ms(Some(4 * gib - 1)), 5_000);
        assert_eq!(ram_idle_ms(Some(2 * gib)), 5_000);
        // High (< 2 GiB)
        assert_eq!(ram_idle_ms(Some(2 * gib - 1)), 2_000);
        assert_eq!(ram_idle_ms(Some(0)), 2_000);
        // Missing falls back to normal
        assert_eq!(ram_idle_ms(None), 10_000);
    }
}

#[cfg(test)]
mod copy_queue {
    use super::{
        COPY_LISTS, CopyOrder, ListFrom, batch_done, copy_queue_window, copied_state, fetch_waits,
        list_from, overlap_slot,
    };
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST,
    };

    /// Batches overlap the direct queue while the pool has room, and only then
    /// does the next batch wait for the oldest: a pool of one waited for every
    /// batch before it sampled the fence, so nothing overlapped and the count
    /// of batches still running was always zero.
    #[test]
    fn a_batch_only_waits_once_the_pool_is_full() {
        assert_eq!(list_from(1, 0), ListFrom::Idle);
        assert_eq!(list_from(0, 0), ListFrom::Fresh);
        assert_eq!(list_from(0, COPY_LISTS - 1), ListFrom::Fresh);
        assert_eq!(list_from(0, COPY_LISTS), ListFrom::Wait);
        assert_eq!(list_from(0, COPY_LISTS + 3), ListFrom::Wait);
        // A list back from the card is used before a new one is made.
        assert_eq!(list_from(2, COPY_LISTS + 3), ListFrom::Idle);
    }

    /// A batch whose signal failed has no value of its own: a later value on
    /// the queue says it has run, and with no later value nothing may be
    /// waited for — a wait for a value never signalled never returns.
    #[test]
    fn a_batch_whose_signal_failed_is_not_waited_for() {
        // Its own value at or under what the card reached: run.
        assert!(batch_done(Some(5), None, 5));
        assert!(!batch_done(Some(6), None, 5));
        // No value of its own: the first value after it names its completion.
        assert!(!batch_done(None, Some(6), 5));
        assert!(batch_done(None, Some(6), 6));
        // No value at all: it is run behind the next batch that has one, and
        // nothing here may be waited for.
        assert!(!batch_done(None, None, u64::MAX));
    }

    /// The run's overlap figure: a batch the card had reached when the next
    /// was ready ran beside the direct queue, one it had not was still running
    /// then. Every batch that a later one arrived behind counts in one of the
    /// two, and in one only.
    #[test]
    fn a_batch_the_card_had_reached_ran_beside_the_direct_queue() {
        assert_eq!(overlap_slot(7, u64::MAX), 2);
        assert_eq!(overlap_slot(7, 7), 2);
        assert_eq!(overlap_slot(7, 6), 3);
    }

    /// Each window reports its own batches and bytes: a total divided by one
    /// window's frames reported the whole run's bytes as if this window sent
    /// them, and that grew with uptime.
    #[test]
    fn a_window_reports_only_what_it_sent() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let stats = [const { AtomicU64::new(0) }; 4];
        for _ in 0..2 {
            stats[0].fetch_add(1, Ordering::Relaxed);
            stats[1].fetch_add(4_000_000, Ordering::Relaxed);
            stats[2].fetch_add(1, Ordering::Relaxed);
        }
        let frames = 2.0;
        // Two batches of 4 MB over two frames: 4 MB a frame.
        assert_eq!(copy_queue_window(&stats, frames), (2, 4.0, 1.0, 0.0));
        // Read, not drained: the batches sent so far are a running total.
        assert_eq!(stats[0].load(Ordering::Relaxed), 2);
        // A second window with one batch of 1 MB and one still running.
        stats[0].fetch_add(1, Ordering::Relaxed);
        stats[1].fetch_add(1_000_000, Ordering::Relaxed);
        stats[3].fetch_add(1, Ordering::Relaxed);
        assert_eq!(copy_queue_window(&stats, frames), (3, 0.5, 0.0, 0.5));
        // Nothing sent since: nothing to report but the total.
        assert_eq!(copy_queue_window(&stats, frames), (3, 0.0, 0.0, 0.0));
    }

    /// The rule the copy queue is built on: a batch of copies is signalled
    /// with a value no list before it has waited for, and every list after
    /// it that reads a copy waits for the newest one. A value waited for is
    /// not waited for again, so a submission that reads no copy the batch
    /// wrote waits for nothing.
    #[test]
    fn a_copy_batch_is_waited_for_before_a_list_reads_it() {
        let mut order = CopyOrder::default();
        // Nothing has been sent: the direct queue waits for nothing.
        assert_eq!(order.wait(), None);
        let first = order.sending();
        order.signalled(first);
        assert_eq!(order.wait(), Some(first));
        order.waited(first);
        assert_eq!(order.wait(), None, "a batch waited for is not waited for again");
        // A second batch is a value behind the first, and a wait of its own.
        let second = order.sending();
        order.signalled(second);
        assert!(second > first);
        assert_eq!(order.wait(), Some(second));
        // Waiting for an older value leaves the newer one to be waited for.
        order.waited(first);
        assert_eq!(order.wait(), Some(second));
    }

    /// A batch whose `Signal` failed is numbered and never named by the fence:
    /// no wait may name its number, or it never returns — the copy queue drains
    /// such a batch on the CPU instead ([`super::CopyLists::drain`]) and names
    /// that. Until then a wait names the last value a signal carried.
    #[test]
    fn a_failed_signal_leaves_no_value_to_wait_for() {
        let mut order = CopyOrder::default();
        // Nothing signalled at all: nothing to wait for, on either queue.
        assert_eq!(fetch_waits(&order), None);
        assert_eq!(order.wait(), None);
        let first = order.sending();
        order.signalled(first);
        assert_eq!(fetch_waits(&order), Some(first));
        order.waited(first);
        // The next batch's signal failed: its number is spent, not on the fence.
        let failed = order.sending();
        assert!(failed > first, "a number is used once, however its signal went");
        assert_ne!(fetch_waits(&order), Some(failed), "the fetch does not wait for it");
        assert_eq!(order.wait(), None, "nor does the direct queue");
        // The drain that follows is signalled above it, and names both batches.
        let drained = order.sending();
        order.signalled(drained);
        assert_eq!(fetch_waits(&order), Some(drained));
        assert_eq!(order.wait(), Some(drained));
        assert!(batch_done(Some(drained), None, drained), "the drained batch is run with it");
    }

    /// The direct queue's operations, in the order they were enqueued.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Direct {
        /// The wait for the batch just sent on the copy queue.
        Copies(u64),
        /// The fetch's own signal: the value a batch waits for.
        Fetch(u64),
    }

    /// A step of a script: a batch, or a fetch publishing the value it waits for
    /// and the barrier the batches after it wait for, its own wait, list and
    /// signal going to the direct queue in the same step. In the code only the
    /// second is one hold of [`super::QUEUE_ORDER`]; [`Queues::held`] is that
    /// hold, and the test's second half is the two steps apart.
    #[derive(Clone, Copy)]
    enum Step {
        Batch,
        Fetch,
    }

    /// The two queues and the waits between them, at the level the deadlock
    /// lives at: a batch waits for the fetch in force when it is recorded, the
    /// direct queue waits for every batch it sends, and the fetch's own list
    /// sits at one place on the direct queue. A deadlock is a batch whose wait
    /// for the fetch is remembered by the direct queue *ahead of* the fetch's
    /// own signal: the direct queue stops at that wait, the copy queue's batch
    /// stops at the fetch, and neither moves again.
    #[derive(Default)]
    struct Queues {
        order: CopyOrder,
        /// The direct queue, in order.
        direct: Vec<Direct>,
        /// Per batch: its copy value, and the fetch value it waits for.
        batches: Vec<(u64, Option<u64>)>,
        /// Per fetch: its own value, the copy value it waits for, and where its
        /// own signal sits on the direct queue ([`usize::MAX`] until enqueued).
        fetches: Vec<(u64, Option<u64>, usize)>,
        /// The fetch in force, by its own value.
        barrier: Option<u64>,
    }

    impl Queues {
        fn batch(&mut self) {
            let number = self.order.sending();
            self.order.signalled(number);
            self.batches.push((number, self.barrier));
            self.direct.push(Direct::Copies(number));
        }

        /// The fetch's wait for the copy queue, and the barrier the batches
        /// recorded from here on wait for.
        fn publishes(&mut self) {
            let own = (self.fetches.len() as u64 + 1) * 2;
            self.fetches.push((own, fetch_waits(&self.order), usize::MAX));
            self.barrier = Some(own);
        }

        /// The fetch's own wait, list and signal on the direct queue.
        fn enqueues(&mut self) {
            let last = self.fetches.len() - 1;
            let (own, waits, _) = self.fetches[last];
            if let Some(waits) = waits {
                self.direct.push(Direct::Copies(waits));
            }
            self.direct.push(Direct::Fetch(own));
            self.fetches[last] = (own, waits, self.direct.len() - 1);
        }

        /// The first batch that waits for a fetch whose own signal is behind the
        /// direct queue's wait for that batch, named with the value it waits
        /// for.
        fn deadlock(&self) -> Option<(u64, u64)> {
            for &(number, fetch) in &self.batches {
                let Some(value) = fetch else { continue };
                let Some(&(_, _, at)) = self.fetches.iter().find(|(own, _, _)| *own == value)
                else {
                    continue;
                };
                let Some(here) = self
                    .direct
                    .iter()
                    .position(|op| *op == Direct::Copies(number))
                else {
                    continue;
                };
                if at > here {
                    return Some((number, value));
                }
            }
            None
        }

        /// Runs a script: the two steps of a fetch are one hold of the order,
        /// so nothing is recorded between them.
        fn held(script: &[Step]) -> Self {
            let mut queues = Self::default();
            for step in script {
                match step {
                    Step::Batch => queues.batch(),
                    Step::Fetch => {
                        queues.publishes();
                        queues.enqueues();
                    }
                }
            }
            queues
        }
    }

    /// What keeps the two queues in a chain and never in a cycle, modelled at
    /// the level the deadlock lives at (`Host::relay_card_fetch`,
    /// `Host::card_before` → `Host::card_copies`): a fetch publishes the value
    /// it waits for and puts its own wait, list and signal on the direct queue
    /// in one hold of [`super::QUEUE_ORDER`]; a batch takes its number, reads
    /// that barrier and records the direct queue's wait for itself in another.
    /// So a batch that reads the barrier is recorded after the fetch's list is
    /// on the direct queue: the fetch's signal is ahead of the direct queue's
    /// wait for that batch, and the chain holds.
    ///
    /// Publishing the barrier before taking the order, as the fetch used to, is
    /// the cycle: the batch waits for the fetch's value, the direct queue waits
    /// for that batch, and the fetch's own signal — the only thing that carries
    /// the value — is behind that wait.
    #[test]
    fn a_fetch_and_the_batches_that_wait_for_it_form_a_chain() {
        use Step::{Batch, Fetch};
        let script = [Batch, Batch, Fetch, Batch, Batch, Fetch, Batch];
        let held = Queues::held(&script);
        assert!(held.fetches.len() == 2 && held.batches.len() == 5, "the script ran both");
        assert_eq!(held.deadlock(), None, "the fetch's signal is ahead of every wait for it");
        // The shape that deadlocked: the publication, a batch recorded against
        // it, and only then the fetch's own list and signal.
        let mut split = Queues::default();
        split.batch();
        split.publishes();
        split.batch();
        split.enqueues();
        assert_eq!(
            split.deadlock(),
            Some((2, 2)),
            "the copy queue's batch and the direct queue wait for each other"
        );
    }

    /// The write of a batch reaches the direct queue only after the copy
    /// queue's run leaves the copy: a copy read after a batch on the copy
    /// queue is taken from `COMMON` (state decay, `copied_state`), a copy
    /// read in the same list from `COPY_DEST`.
    #[test]
    fn a_copy_read_after_the_copy_queue_is_taken_from_common() {
        assert_eq!(copied_state(true), D3D12_RESOURCE_STATE_COMMON);
        assert_eq!(copied_state(false), D3D12_RESOURCE_STATE_COPY_DEST);
    }
}

#[cfg(test)]
mod states {
    use super::{Move, legal_for, moving};

    const COMMON: u32 = 0;
    const RENDER_TARGET: u32 = 0x4;
    const DEPTH_WRITE: u32 = 0x10;
    const PIXEL_SHADER_READ: u32 = 0x40;
    /// A depth target's flags: render target and unordered access are not
    /// allowed on it.
    const DEPTH_FLAGS: u32 = 0x2;

    /// A request the list already holds settles without the texture's flags
    /// being read, and records nothing: most asks of a frame are one.
    #[test]
    fn a_state_the_list_holds_settles_without_reading_the_flags() {
        let read = std::cell::Cell::new(0);
        let decision = moving(Some(PIXEL_SHADER_READ), PIXEL_SHADER_READ, |w| {
            read.set(read.get() + 1);
            w
        });
        assert_eq!(decision, Move::Settled);
        assert_eq!(read.get(), 0, "a settled request reads no flags");
    }

    /// A state that really changes moves: the barrier is recorded, from the
    /// state held to the one wanted.
    #[test]
    fn a_state_that_changes_moves() {
        assert_eq!(
            moving(Some(PIXEL_SHADER_READ), RENDER_TARGET, |w| w),
            Move::Moved(PIXEL_SHADER_READ, RENDER_TARGET)
        );
    }

    /// The console lets the title ask for a state its resource's flags
    /// forbid. A request the flags cut back to the state held moves nothing;
    /// one they cut back to another state still moves.
    #[test]
    fn a_request_the_flags_cut_back_moves_only_what_it_changes() {
        let depth = |w| legal_for(DEPTH_FLAGS, w);
        assert_eq!(
            moving(Some(DEPTH_WRITE), RENDER_TARGET | DEPTH_WRITE, depth),
            Move::Settled,
            "the flags drop the render target bit: the depth state stands"
        );
        assert_eq!(
            moving(Some(DEPTH_WRITE), RENDER_TARGET, depth),
            Move::Moved(DEPTH_WRITE, COMMON),
            "a texture asked into a state its flags forbid moves to none"
        );
    }

    /// The list's first use of a texture records nothing (its prelude moves
    /// it at submission), and keeps the state the flags allow.
    #[test]
    fn a_first_use_records_nothing() {
        assert_eq!(
            moving(None, RENDER_TARGET | PIXEL_SHADER_READ, |w| legal_for(DEPTH_FLAGS, w)),
            Move::FirstUse(PIXEL_SHADER_READ)
        );
    }
}
