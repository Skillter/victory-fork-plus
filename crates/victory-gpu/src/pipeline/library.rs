//! Compiled host pipelines kept on disk, and the title pipelines worth
//! starting early (decision 0007, "Pipeline cache").
//!
//! 1. Every host pipeline goes into the host's pipeline library, named by
//!    a hash of all that its host description holds (the translated
//!    shaders' code, which changes when a translation does, our root
//!    signature, the graphics state). The library is saved under the cache
//!    folder; the host refuses a library of another adapter or driver, and
//!    an entry whose description differs, so neither is ever used.
//! 2. The title pipelines a run drew or dispatched with are listed by a key
//!    known when the title creates them (their shader containers and
//!    graphics state); the next run hands those to the workers at
//!    creation, during loading.
//!
//! The folder keeps more than the live library: a start that could not
//! replace a mapped file leaves the newer one beside it ([`NEXT_FILE`]), and
//! a save killed while it wrote the whole library leaves one as `.tmp`. No
//! run ever reads those back, so opening the folder drops them ([`prune`]):
//! the live file and one other generation stay, and a generation nothing has
//! read for [`MAX_IDLE`] goes, whatever else is there.
//!
//! Library calls overlap: the host documents the library as safe to use
//! from several threads ("The pipeline library is thread-safe to use, and
//! will internally synchronize as necessary", `CreatePipelineLibrary`,
//! Thread Safety), with one exception kept here: two loads of the same
//! pipeline are made one after the other ([`same_name`]). Under one lock of
//! ours for every call, the eight workers made pipelines one at a time:
//! 87 to 96 of 110 looks at each worker found it waiting for that lock
//! (run 49), and an earlier run's 2,300 pipelines, all in the library, were
//! still being loaded 46 s into the opening cutscene.
//! `VICTORY_LIBRARY_LOCK=1`: one call at a time, as before.
//!
//! The library's file is mapped, not read into memory ([`Blob`]): the host
//! keeps pointing into the bytes it was made from for as long as it lives,
//! and read into a buffer they were 1,352 MB of the process's own memory
//! for the whole run (run 60), all of it in physical memory. Mapped, the
//! pages are the file's: only those the host reads are in memory, and the
//! system can drop them without writing them anywhere. A save writes the
//! new library through a mapping of the new file in the same way (a buffer
//! of the library's size was made, filled and written out at each save).
//! `VICTORY_LIBRARY_MAP=0`: read and written through buffers, as before.
//!
//! The load of a 752 MB library took 26 s of the opening load, with nothing
//! logged in between (run 05:19:01: first translated shader 05:19:11.328,
//! `host pipeline library loaded` 05:19:37.473), while the same file loaded
//! from the system's cache took 11 to 15 ms in other runs: the time is the
//! mapping's pages coming off the disk, not the host's parse, which takes
//! 49 ms with the bytes in memory (`VICTORY_LIBRARY_BENCH`, the test below).
//! So the pages are read ahead in one pass before the parse
//! ([`Mapped::prefetch`]: 256 MiB cost 578 ms at 465 MB/s against 3,575 ms
//! faulted at 75 MB/s), the load says where its time went, and past
//! `VICTORY_LIBRARY_MB` the library takes no more pipelines ([`room`]): a
//! smaller file is the only other lever on what a start must read.
//!
//! What the library holds is the host's own form of each pipeline, not
//! copies of the shader containers we hand it: of the 26,616 containers in
//! `cache/shaders` none is in the 753 MB file (no `DXBC` in it at all, and 0
//! bytes matched, `VICTORY_LIBRARY_BENCH_SHADERS`), and 753,134,320 B over
//! the census's 2,454 pipelines is 307,000 B a pipeline of its compiled code
//! and our descriptions together. A start reads all of it; nothing in the
//! file can be left out.
//!
//! The mapped file's handle stays open for as long as the mapping lives
//! ([`holds`]). A save renames the new library over the file this run has
//! mapped, and with the handle closed Windows then gives up the old file's
//! contents: every page of the mapping that is not in memory at that
//! moment, or leaves memory later (the trim below, or Windows taking pages
//! back), reads as zeros from then on. The host's library reads such pages
//! as stored pipelines; the driver, handed a pipeline of zeros, removes the
//! device with DXGI_ERROR_DRIVER_INTERNAL_ERROR (runs 72, 73 and 82, each
//! within 5 ms of the first library call after a save and a trim; and the
//! library run 77 saved from such pages was refused by run 78). Measured
//! with a 64 MB file written past the system's cache, mapped and replaced:
//! 16,352 of 16,384 pages read zeros with the handle closed, none with it
//! open (the test below). `VICTORY_LIBRARY_HANDLE=0`: closed, as before.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{HSTRING, Interface, PCWSTR};

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Keeps the pipeline library and the early list in `dir` from now on, after
/// dropping the library files this run will not read ([`prune`]).
pub fn use_pipeline_cache(dir: PathBuf) {
    prune(&dir, SystemTime::now());
    let _ = DIR.set(dir);
}

/// Host pipelines created, loaded from the library, and title pipelines
/// started at creation, since the start.
pub(crate) static MADE: AtomicU64 = AtomicU64::new(0);
pub(crate) static LOADED: AtomicU64 = AtomicU64::new(0);
pub(crate) static EARLY: AtomicU64 = AtomicU64::new(0);
/// For the memory lines: the bytes of the library's file, kept in memory
/// while the library lives, and the bytes the file holds on the disk (as the
/// last save wrote them, or as this run loaded them): what a save would
/// rewrite, and the base the growth gate in `save` measures against.
pub(crate) static BLOB: AtomicU64 = AtomicU64::new(0);
pub(crate) static SAVED: AtomicU64 = AtomicU64::new(0);
/// Whether the cap (`VICTORY_LIBRARY_MB`) has stopped a store this run: said
/// once, not once a pipeline.
static AT_CAP: AtomicBool = AtomicBool::new(false);

/// Whether the library's file is mapped ([`Blob`]) and saved through a
/// mapping. On by default; `VICTORY_LIBRARY_MAP=0` uses buffers, as before.
fn mapped() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LIBRARY_MAP").is_none_or(|v| v != "0"))
}

/// Whether the mapped library file's handle is kept open while it is
/// mapped (see the module's notes: without it a save in the same run turns
/// the mapping's pages into zeros as they leave memory). On by default;
/// `VICTORY_LIBRARY_HANDLE=0` closes it after mapping, as before.
fn holds() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LIBRARY_HANDLE").is_none_or(|v| v != "0"))
}

/// Whether the mapped library's pages are read into memory in one pass
/// before the host parses them ([`Mapped::prefetch`]). On by default;
/// `VICTORY_LIBRARY_PREFETCH=0` lets the parse fault them in, as before.
fn prefetches() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| prefetches_of(std::env::var("VICTORY_LIBRARY_PREFETCH").ok().as_deref()))
}

/// `VICTORY_LIBRARY_PREFETCH` read as the switch it is: on unless switched
/// off, so a run with no environment is the read-ahead the notes measure.
fn prefetches_of(value: Option<&str>) -> bool {
    value != Some("0")
}

/// Bytes of a mapping read ahead in one call: the whole file is read ahead,
/// a quarter of a gigabyte at a time.
const PREFETCH_CHUNK: usize = 256 << 20;

/// Milliseconds since `since`, for the load's phases.
fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1e3
}

/// A whole file mapped into memory: read-only (the library's file) or
/// writable (the file a save fills). Unmapped when dropped.
struct Mapped {
    at: windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
    /// The file, for a mapping made by [`Mapped::open`] ([`holds`]): open
    /// until after the view is unmapped, so that the file a rename took
    /// the name from keeps its contents for the view.
    _file: Option<std::fs::File>,
}

impl Mapped {
    /// Maps `file` (`len` bytes, not zero) for reading, or for reading and
    /// writing.
    fn map(file: &std::fs::File, len: usize, write: bool) -> Option<Mapped> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, PAGE_READONLY, PAGE_READWRITE,
        };
        if len == 0 {
            return None;
        }
        // SAFETY: a file we hold open; the view keeps the file's pages
        // after the mapping object and the file are closed.
        let at = unsafe {
            let mapping = CreateFileMappingW(
                file.as_raw_handle(),
                std::ptr::null(),
                if write { PAGE_READWRITE } else { PAGE_READONLY },
                0,
                0,
                std::ptr::null(),
            );
            if mapping.is_null() {
                return None;
            }
            let at = MapViewOfFile(mapping, if write { FILE_MAP_WRITE } else { FILE_MAP_READ }, 0, 0, 0);
            CloseHandle(mapping);
            at
        };
        (!at.Value.is_null()).then_some(Mapped { at, len, _file: None })
    }

    /// The file at `path`, mapped for reading; `None` if it is not there,
    /// is empty or cannot be mapped.
    fn open(path: &std::path::Path) -> Option<Mapped> {
        Mapped::open_holding(path, holds())
    }

    /// As [`Mapped::open`]; `hold` says whether the file stays open with
    /// the mapping.
    fn open_holding(path: &std::path::Path, hold: bool) -> Option<Mapped> {
        // Opened as the standard library opens a file to read: others may
        // read, write, rename and delete it meanwhile, so a save still
        // renames the new library over this one.
        let file = std::fs::File::open(path).ok()?;
        let len = usize::try_from(file.metadata().ok()?.len()).ok()?;
        let mut mapped = Mapped::map(&file, len, false)?;
        if hold {
            mapped._file = Some(file);
        }
        Some(mapped)
    }

    /// A new file of `len` bytes at `path` (replacing what is there),
    /// mapped for writing.
    fn create(path: &std::path::Path, len: usize) -> Option<Mapped> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .ok()?;
        file.set_len(len as u64).ok()?;
        Mapped::map(&file, len, true)
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the view holds `len` readable bytes until dropped.
        unsafe { std::slice::from_raw_parts(self.at.Value.cast(), self.len) }
    }

    /// Only for a view made by [`Mapped::create`].
    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: a writable view of `len` bytes, ours alone.
        unsafe { std::slice::from_raw_parts_mut(self.at.Value.cast(), self.len) }
    }

    /// Reads the view's pages into memory now, before the host parses them:
    /// one sequential read of the file instead of a fault per page the
    /// parse reaches, which is what 26 s of a 752 MB load was made of.
    /// Milliseconds spent, `None` when the switch is off ([`prefetches`]).
    fn prefetch(&self) -> Option<f64> {
        if !prefetches() {
            return None;
        }
        let began = Instant::now();
        for (at, bytes) in prefetch_ranges(self.len) {
            // SAFETY: as `prefetch` asks: a range of our own view, readable
            // for `len` bytes.
            unsafe { prefetch(self.at.Value.cast::<u8>().wrapping_add(at), bytes) };
        }
        Some(ms(began))
    }
}

/// The ranges of a mapping of `len` bytes read ahead in one pass: whole
/// chunks of [`PREFETCH_CHUNK`], the last one what is left (nothing for an
/// empty file).
fn prefetch_ranges(len: usize) -> Vec<(usize, usize)> {
    (0..len)
        .step_by(PREFETCH_CHUNK)
        .map(|at| (at, PREFETCH_CHUNK.min(len - at)))
        .collect()
}

/// `PrefetchVirtualMemory` of `bytes` at `at`: the system reads those pages
/// in now. False if it declined (the pages then come back a page fault at a
/// time, as they did before).
///
/// # Safety
/// `at` must be the address of `bytes` readable bytes of an accessible view.
unsafe fn prefetch(at: *const u8, bytes: usize) -> bool {
    use windows_sys::Win32::System::Memory::{PrefetchVirtualMemory, WIN32_MEMORY_RANGE_ENTRY};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let entry =
        WIN32_MEMORY_RANGE_ENTRY { VirtualAddress: at.cast_mut().cast(), NumberOfBytes: bytes };
    // SAFETY: a range of an accessible view, read by the call and nothing
    // else touched.
    unsafe { PrefetchVirtualMemory(GetCurrentProcess(), 1, &entry, 0) != 0 }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: our view.
        unsafe { windows_sys::Win32::System::Memory::UnmapViewOfFile(self.at) };
    }
}

/// The bytes a library is made from: the host reads them for as long as
/// the library lives, so they are kept beside it.
enum Blob {
    /// No file (an empty library).
    None,
    /// The file read into memory (`VICTORY_LIBRARY_MAP=0`, tests, or a file
    /// that could not be mapped).
    Held(Vec<u8>),
    /// The file itself, mapped.
    Mapped(Mapped),
}

impl Blob {
    /// The library file in `dir`: a library a save left beside it
    /// ([`NEXT_FILE`]) takes its place first.
    fn of(dir: &std::path::Path) -> Blob {
        let path = dir.join(LIBRARY_FILE);
        let next = dir.join(NEXT_FILE);
        if next.exists() && std::fs::rename(&next, &path).is_err() {
            tracing::warn!("the host pipeline library saved last could not take the old one's place");
        }
        if mapped()
            && let Some(file) = Mapped::open(&path)
        {
            return Blob::Mapped(file);
        }
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => Blob::Held(bytes),
            _ => Blob::None,
        }
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Blob::None => &[],
            Blob::Held(bytes) => bytes,
            Blob::Mapped(file) => file.bytes(),
        }
    }
}

/// The mapped library file's view (address, bytes; 0: not mapped), when a
/// pipeline was last loaded from the library (`crate::host::now_ms`), and
/// whether the view's pages were let go since ([`trim`]).
static VIEW: [std::sync::atomic::AtomicUsize; 2] = [const { std::sync::atomic::AtomicUsize::new(0) }; 2];
static LOADED_AT: AtomicU64 = AtomicU64::new(0);
static TRIMMED: AtomicBool = AtomicBool::new(false);
/// The bytes of the view let go so far, for the memory line.
pub(crate) static TRIMMED_BYTES: AtomicU64 = AtomicU64::new(0);
/// When the view's pages were last let go (`crate::host::now_ms`), and how
/// often and when a save of this run last renamed a new library over the
/// mapped file: for [`removal_note`].
static TRIMMED_AT: AtomicU64 = AtomicU64::new(0);
static REPLACED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The library calls running now: thread, what (load, make, store,
/// serialise), the pipeline, and since when (`crate::host::now_us`).
static CALLS: Mutex<Vec<(u32, &'static str, String, u64)>> = Mutex::new(Vec::new());

thread_local! {
    /// The pipeline this thread is making (its shaders), for [`CALLS`].
    static LABEL: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Names the pipeline this thread makes next.
pub(crate) fn label(text: String) {
    LABEL.with(|l| *l.borrow_mut() = text);
}

/// A library call of this thread, listed in [`CALLS`] until dropped.
struct Call(u32, u64);

fn call(what: &'static str, name: &str) -> Call {
    let (thread, since) = (crate::host::thread_id(), crate::host::now_us());
    let label = LABEL.with(|l| l.borrow().clone());
    CALLS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((thread, what, format!("{name} {label}"), since));
    Call(thread, since)
}

impl Drop for Call {
    fn drop(&mut self) {
        CALLS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|c| (c.0, c.3) != (self.0, self.1));
    }
}

/// For the report when the host device is removed: the library calls
/// running at that moment, whether this run replaced the mapped file and
/// let its pages go, and how many pages of the mapping read as zeros now.
/// A library holds no page of zeros (the 259 MB one of run 83 has no run of
/// 2,048 zero bytes): each such page is one whose contents were lost (see
/// the module's notes), and a pipeline stored there is what the driver was
/// handed.
pub(crate) fn removal_note() -> String {
    use std::fmt::Write;
    let now = crate::host::now_us();
    let mut text = String::from("pipeline library: ");
    {
        let calls = CALLS.lock().unwrap_or_else(|e| e.into_inner());
        if calls.is_empty() {
            text.push_str("no call running");
        }
        for (i, (thread, what, name, since)) in calls.iter().enumerate() {
            let _ = write!(
                text,
                "{}{what} of {name} running for {:.1} ms on thread {thread}",
                if i == 0 { "" } else { ", " },
                now.saturating_sub(*since) as f64 / 1000.0
            );
        }
    }
    let (at, len) = (VIEW[0].load(Ordering::Relaxed), VIEW[1].load(Ordering::Relaxed));
    if at == 0 {
        text.push_str("; its file is not mapped");
        return text;
    }
    let ago = |ms: u64| crate::host::now_ms().saturating_sub(ms) as f64 / 1000.0;
    let _ = write!(
        text,
        "; its file is mapped ({len} bytes), the handle {}",
        if holds() { "kept open" } else { "closed (VICTORY_LIBRARY_HANDLE=0)" }
    );
    match REPLACED[0].load(Ordering::Relaxed) {
        0 => text.push_str("; this run has not replaced the file"),
        n => {
            let _ = write!(
                text,
                "; this run replaced the file under the mapping {n} times, last {:.1} s ago",
                ago(REPLACED[1].load(Ordering::Relaxed))
            );
        }
    }
    if TRIMMED_BYTES.load(Ordering::Relaxed) > 0 {
        let _ = write!(text, "; its pages were last let go {:.1} s ago", ago(TRIMMED_AT.load(Ordering::Relaxed)));
    }
    // Within two seconds: pages not in memory come from the disk.
    let started = std::time::Instant::now();
    // SAFETY: the view lives as long as the library, which is never dropped.
    let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, len) };
    let (mut zero, mut looked, mut first) = (0u64, 0u64, None);
    for (n, page) in bytes.chunks(4096).enumerate() {
        if n % 4096 == 0 && started.elapsed() > std::time::Duration::from_secs(2) {
            break;
        }
        looked += 1;
        if page.iter().all(|&b| b == 0) {
            zero += 1;
            if first.is_none() {
                first = Some(n);
            }
        }
    }
    let _ = write!(text, "; {zero} of the {looked} pages looked at read as zeros now");
    if let Some(first) = first {
        let _ = write!(text, " (the first is page {first}): the library's contents were lost there");
    }
    text
}

/// Lets the mapped library file's pages go from the process's memory once
/// no pipeline has been loaded for ten seconds. The host reads nearly the
/// whole file while the early pipelines load (run 65: all 1,490 MB of it
/// were in the process's memory a minute in, and 1,300 MB still at the
/// end, on a PC with 2 GB free); after that it reads the file again only
/// when a new pipeline loads. Pages let go stay in Windows' file cache
/// while nothing else wants the memory, and come back on the next read.
/// Called once a second by the sweep; does its work on a thread of its own
/// (hundreds of thousands of pages), once after each burst of loads.
/// `VICTORY_LIBRARY_TRIM=0`: the pages stay until Windows takes them.
pub(crate) fn trim() {
    static ON: OnceLock<bool> = OnceLock::new();
    let (at, len) = (VIEW[0].load(Ordering::Relaxed), VIEW[1].load(Ordering::Relaxed));
    if at == 0
        || TRIMMED.load(Ordering::Relaxed)
        || !*ON.get_or_init(|| std::env::var_os("VICTORY_LIBRARY_TRIM").is_none_or(|v| v != "0"))
    {
        return;
    }
    let quiet_ms = crate::host::ram_idle_ms(crate::host::available_physical_ram());
    if crate::host::now_ms() < LOADED_AT.load(Ordering::Relaxed) + quiet_ms
        || TRIMMED.swap(true, Ordering::Relaxed)
    {
        return;
    }
    let work = move || {
        // SAFETY: the view lives as long as the library, which is never
        // dropped. Unlocking pages that are not locked fails, as expected,
        // and takes them out of the working set: the documented way to let
        // pages of a mapped file go.
        unsafe { windows_sys::Win32::System::Memory::VirtualUnlock(at as *const _, len) };
        TRIMMED_BYTES.fetch_add(len as u64, Ordering::Relaxed);
        TRIMMED_AT.store(crate::host::now_ms(), Ordering::Relaxed);
        crate::host::trail(|| format!("pipeline library: the mapped file's pages let go from memory ({len} bytes)"));
    };
    if std::thread::Builder::new().name("victory-library-trim".into()).spawn(work).is_err() {
        TRIMMED.store(false, Ordering::Relaxed);
    }
}

struct Library {
    library: ID3D12PipelineLibrary1,
    /// The blob the library was made from: it must outlive the library.
    _blob: Blob,
    /// Pipelines stored since the last save.
    added: AtomicU64,
    /// Stores share it and a save takes it alone: the size the library
    /// asks for must still be its size when it is written out.
    saving: RwLock<()>,
}

// SAFETY: D3D12 objects are free-threaded, and the library synchronises
// its own calls (see the module's notes); what it does not, two loads of
// one name, is serialised by `same_name`.
unsafe impl Send for Library {}
// SAFETY: as above; the other fields are an atomic and a lock.
unsafe impl Sync for Library {}

/// Held by a load of the pipeline named `name`: loads of the same name
/// wait for each other (the host's one exception to the library's thread
/// safety: such a load may change that pipeline's entry). Names share 64
/// locks, so two different names seldom wait.
fn same_name(name: &str) -> MutexGuard<'static, ()> {
    static LOADING: [Mutex<()>; 64] = [const { Mutex::new(()) }; 64];
    let at = name.bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(usize::from(b)));
    LOADING[at % LOADING.len()].lock().unwrap_or_else(|e| e.into_inner())
}

/// With `VICTORY_LIBRARY_LOCK=1`: held by every library call, as before.
fn one_call() -> Option<MutexGuard<'static, ()>> {
    static ON: OnceLock<bool> = OnceLock::new();
    static CALLING: Mutex<()> = Mutex::new(());
    ON.get_or_init(|| std::env::var_os("VICTORY_LIBRARY_LOCK").is_some_and(|v| v == "1"))
        .then(|| CALLING.lock().unwrap_or_else(|e| e.into_inner()))
}

const LIBRARY_FILE: &str = "host-pipelines.bin";
/// What the library's own files begin with: the live one, and beside it the
/// generations [`prune`] drops (`host-pipelines.next`, `.tmp`, and a
/// generation named after an adapter or driver key if one is ever added).
const LIBRARY_STEM: &str = "host-pipelines";
/// A saved library that could not replace [`LIBRARY_FILE`] while this run
/// had it mapped (a system that refuses to replace a mapped file): the next
/// start renames it over the library first ([`Blob::of`]).
const NEXT_FILE: &str = "host-pipelines.next";
const EARLY_FILE: &str = "early-pipelines.txt";
/// Title pipelines listed for an early start, at most. Past this the list
/// stops recording: a name first drawn after it is filled is not listed, and
/// the next run loses that early start. [`note_used`] says so once.
const EARLY_MAX: usize = 8192;

/// Generations of the library's file kept beside the live one: one. A start
/// that could not replace a mapped file leaves the newer library beside it
/// as [`NEXT_FILE`], and the next start promotes it; a save killed while it
/// wrote the whole library leaves one as `.tmp` instead, which no run reads
/// back. One of those is worth keeping, and every further one is hundreds of
/// megabytes nothing will open again.
const OTHERS_KEPT: usize = 1;

/// How long a generation of the library's file may go unread before it goes
/// however few are beside it: a month, the same rule the shader cache keeps
/// (`victory_pipeline::PrunePolicy`). The host refuses a library of another
/// adapter, driver or generator, so such a generation is never read again,
/// and at 187 KB to 1.4 MB a pipeline it is 232 MB to 737 MB each.
const MAX_IDLE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// What one pruning pass removed from the pipelines folder.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pruned {
    pub(crate) files: u64,
    pub(crate) bytes: u64,
}

/// Drops the library's own older generations in `dir`, and says one line when
/// it removed anything (nothing at all when there was nothing to remove).
///
/// Only files named after the library are looked at, so the early list beside
/// them and anything someone else put there are left alone. Of those:
/// - the live file this run opens ([`LIBRARY_FILE`]) is never a candidate;
/// - a `.tmp` is a save that never finished, and is a candidate whatever its
///   age: no run reads one back, and the next save truncates it;
/// - one other generation stays, the newest of them (the [`NEXT_FILE`] the
///   next start promotes), and any generation nothing has read for
///   [`MAX_IDLE`] goes whatever else is beside it.
///
/// A file another run has open for writing, or one this account may not
/// remove, is a warning and nothing more: a cache that cannot be pruned must
/// not stop the run. `now` is given so that a test can run the rule without
/// waiting a month.
pub(crate) fn prune(dir: &Path, now: SystemTime) -> Pruned {
    let mut pruned = Pruned::default();
    let listing = match std::fs::read_dir(dir) {
        Ok(listing) => listing,
        // Nothing was ever written here: the first run.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return pruned,
        Err(error) => {
            tracing::warn!(dir = %dir.display(), %error, "cannot read the pipeline library folder");
            return pruned;
        }
    };
    // Path, last write, bytes, and whether it is a save that never finished.
    let mut candidates: Vec<(PathBuf, SystemTime, u64, bool)> = Vec::new();
    for entry in listing.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // The live library, the early list and everything else here belongs
        // to this run, to a title, or to someone who put it there.
        if !name.starts_with(LIBRARY_STEM) || name == LIBRARY_FILE {
            continue;
        }
        // `file_type` describes the entry itself, never what it points at: a
        // link out of the folder is not ours to remove, and neither is a
        // directory in a generation's place.
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else { continue };
        let Ok(modified) = metadata.modified() else { continue };
        candidates.push((entry.path(), modified, metadata.len(), name.ends_with(".tmp")));
    }
    // Newest first: the one generation worth keeping is the newest one.
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.1));
    let mut kept = 0usize;
    for (path, modified, bytes, unfinished) in candidates {
        // A time in the future (a clock that moved) counts as untouched.
        let idle = now.duration_since(modified).unwrap_or_default();
        if !unfinished && idle <= MAX_IDLE && kept < OTHERS_KEPT {
            kept += 1;
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                pruned.files += 1;
                pruned.bytes += bytes;
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "cannot prune a pipeline library generation");
            }
        }
    }
    if pruned.files > 0 {
        tracing::info!(
            files = pruned.files,
            bytes = pruned.bytes,
            dir = %dir.display(),
            "pruned older pipeline library generations"
        );
    }
    pruned
}

fn library() -> Option<&'static Library> {
    static LIBRARY: OnceLock<Option<Library>> = OnceLock::new();
    LIBRARY
        .get_or_init(|| {
            let began = Instant::now();
            let device = crate::host::host()?.device.cast::<ID3D12Device1>().ok()?;
            let blob = DIR.get().map_or(Blob::None, |d| Blob::of(d));
            // Mapping the file is the open, the size and one call into the
            // system; reading it into a buffer (`VICTORY_LIBRARY_MAP=0`) is
            // the whole file, which is why the two are told apart here.
            let (mapped, bytes, file_ms) = match &blob {
                Blob::None => (false, 0, ms(began)),
                Blob::Held(bytes) => (false, bytes.len(), ms(began)),
                Blob::Mapped(file) => (true, file.len, ms(began)),
            };
            // The host reads the whole file while it parses it (752 MB took
            // 26 s of a run): read ahead first, so that the parse waits for
            // the disk once and not once a page.
            let prefetched = match &blob {
                Blob::Mapped(file) => file.prefetch().map(|ms| (file.len, ms)),
                _ => None,
            };
            let parse = Instant::now();
            let library = open(&device, blob);
            let parse_ms = ms(parse);
            tracing::info!(
                bytes,
                mapped,
                file_ms = file_ms as u64,
                prefetched_bytes = prefetched.map_or(0, |(bytes, _)| bytes as u64),
                prefetch_ms = prefetched.map_or(0, |(_, ms)| ms as u64),
                parse_ms = parse_ms as u64,
                total_ms = ms(began) as u64,
                "pipeline library load: the file, the read-ahead and the host's parse"
            );
            if library.is_none() {
                tracing::warn!("no host pipeline library: pipelines are not kept");
            }
            library
        })
        .as_ref()
}

/// The library in `blob`, or an empty one if the host rejects the blob (an
/// adapter or driver change, damage).
fn open(device: &ID3D12Device1, blob: Blob) -> Option<Library> {
    let make = |bytes: &[u8]| -> windows::core::Result<ID3D12PipelineLibrary1> {
        // SAFETY: the bytes outlive the library (kept beside it).
        unsafe {
            device.CreatePipelineLibrary::<ID3D12PipelineLibrary>(bytes)?.cast()
        }
    };
    if !blob.bytes().is_empty() {
        match make(blob.bytes()) {
            Ok(library) => {
                let bytes = blob.bytes().len();
                BLOB.store(bytes as u64, Ordering::Relaxed);
                // What a save would rewrite is what is already on the disk: the
                // growth gate in `save` compares against this, so a warm run
                // that adds a handful of pipelines does not rewrite the whole
                // file to gain a few hundred kilobytes.
                SAVED.store(bytes as u64, Ordering::Relaxed);
                tracing::info!(bytes, mapped = matches!(blob, Blob::Mapped(_)), "host pipeline library loaded");
                // A library over the cap loads (it is every pipeline of every
                // earlier run, which a recompile would not give back in one
                // run) but takes no more, so it never grows past it again: the
                // only lever on the parse is a smaller file, and only the
                // reader can say when to spend the compiles of rebuilding.
                if let Some(cap) = cap().filter(|cap| bytes >= *cap) {
                    tracing::warn!(
                        bytes,
                        cap,
                        "the pipeline library is at or over its cap and will take no more pipelines; move the file aside to rebuild a smaller one"
                    );
                }
                if let Blob::Mapped(file) = &blob {
                    VIEW[1].store(file.len, Ordering::Relaxed);
                    VIEW[0].store(file.at.Value as usize, Ordering::Relaxed);
                    LOADED_AT.store(crate::host::now_ms(), Ordering::Relaxed);
                }
                return Some(Library {
                    library,
                    _blob: blob,
                    added: AtomicU64::new(0),
                    saving: RwLock::new(()),
                });
            }
            Err(e) => tracing::info!(error = %e, "host pipeline library rejected; starting an empty one"),
        }
    } else {
        tracing::info!("no host pipeline library yet; starting an empty one");
    }
    // The rejected bytes are let go (a mapped file is unmapped) before the
    // empty library is made: nothing reads them any more.
    drop(blob);
    let library = make(&[]).ok()?;
    Some(Library {
        library,
        _blob: Blob::None,
        added: AtomicU64::new(0),
        saving: RwLock::new(()),
    })
}

/// The most the library's file may grow to, in bytes, from
/// `VICTORY_LIBRARY_MB`; `None` for no cap.
fn cap() -> Option<usize> {
    static CAP: OnceLock<Option<usize>> = OnceLock::new();
    *CAP.get_or_init(|| cap_of(std::env::var("VICTORY_LIBRARY_MB").ok().as_deref()))
}

/// The cap `VICTORY_LIBRARY_MB` names, in bytes. `None` for nothing that
/// limits the library: unset, empty, `0` (off, as before) or not a number.
fn cap_of(mb: Option<&str>) -> Option<usize> {
    let mb: u64 = mb?.trim().parse().ok()?;
    (mb > 0).then(|| (mb as usize).saturating_mul(1 << 20))
}

/// Whether a library of `size` bytes takes one more pipeline: under its cap,
/// or with no cap at all. A library holds one entry per pipeline and the
/// host is given no way to drop one, so the only bound on a file it must
/// parse whole at every start is this one.
fn room(size: usize, cap: Option<usize>) -> bool {
    cap.is_none_or(|cap| size < cap)
}

/// A pipeline's name in the library: the hash of what its host
/// description holds.
pub(crate) fn name(kind: &'static str, root: &[u8], code: &[&[u8]], state: &str) -> String {
    let mut h = blake3::Hasher::new();
    for part in [kind.as_bytes(), root].into_iter().chain(code.iter().copied()).chain([state.as_bytes()]) {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part);
    }
    let name = h.finalize().to_hex()[..32].to_owned();
    // The census below reads this as one pipeline, with this root block.
    CENSUS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Census::default)
        .add(kind, root, &name);
    name
}

/// One session's pipelines: their names, the root signatures they were made
/// with, and their stage classes. One root signature per binding shape
/// (`crate::pipeline::host::root_signature`) is what keeps this small; a
/// library that stores far more per pipeline than the shape it holds is
/// where a per-PSO cached blob would be measured next.
#[derive(Default)]
struct Census {
    /// One name is one pipeline, however often the run names it.
    names: HashSet<String>,
    signatures: HashSet<blake3::Hash>,
    classes: std::collections::BTreeMap<&'static str, u64>,
    /// Names that found their root signature in the set already.
    shared: u64,
}

static CENSUS: Mutex<Option<Census>> = Mutex::new(None);

impl Census {
    fn add(&mut self, kind: &'static str, root: &[u8], name: &str) {
        if !self.names.insert(name.to_owned()) {
            return;
        }
        *self.classes.entry(kind).or_default() += 1;
        if !self.signatures.insert(blake3::hash(root)) {
            self.shared += 1;
        }
    }

    /// Pipeline and signature counts, and the sizes [`Figures`] reports.
    fn figures(&self, library: u64, opened: u64, made: u64) -> Figures {
        let grown = library.saturating_sub(opened);
        Figures {
            pipelines: self.names.len() as u64,
            signatures: self.signatures.len() as u64,
            shared: self.shared,
            library,
            opened,
            grown,
            per_pipeline: (made > 0 && grown > 0).then(|| grown / made),
        }
    }

    fn classes(&self) -> String {
        self.classes
            .iter()
            .map(|(class, count)| format!("{class} {count}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// What one census line reports. The library's size and what this run's own
/// pipelines added to it are kept apart on purpose: dividing the whole
/// library (which already holds every pipeline of every earlier run) by the
/// pipelines of this one gave "1,443,536 bytes a pipeline" on a warm library
/// and 187,257 for the same code on a cold one, and neither was a figure
/// anyone could use.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Figures {
    /// Distinct pipelines this run made or loaded.
    pub(crate) pipelines: u64,
    /// Distinct root signatures they were made with.
    pub(crate) signatures: u64,
    /// Those that found their signature in the set already.
    pub(crate) shared: u64,
    /// The library's serialised size now, on disk and in memory alike.
    pub(crate) library: u64,
    /// Its size when this run opened it; 0 when it started empty.
    pub(crate) opened: u64,
    /// What this run's own pipelines added: the library now, less what it
    /// was. The host need not grow a library linearly, so this is measured,
    /// never assumed.
    pub(crate) grown: u64,
    /// [`Figures::grown`] over the pipelines this run made: the bytes one
    /// pipeline of this run added. `None` when it made none (nothing to
    /// divide by) and when the library did not grow.
    pub(crate) per_pipeline: Option<u64>,
}

/// The save a reader will look at: the forced save a run ends with, and every
/// save of a diagnostics build, to watch the figures move. The 60-second
/// saves of a normal run stay quiet: the warm-up's figures are not the run's,
/// and the one shot this replaces printed them once, a minute in, forever.
fn census(now: bool, library: &Library) {
    if !now && !cfg!(feature = "diagnostics") {
        return;
    }
    // SAFETY: a plain query on a live library.
    let size = unsafe { library.library.GetSerializedSize() };
    report_census(size as u64);
}

/// Distinct root signatures against distinct pipelines and stage classes,
/// what the root-signature cache reused and created, the library's size, and
/// what a pipeline this run made added to it.
fn report_census(library: u64) {
    let made = MADE.load(Ordering::Relaxed);
    let (figures, classes) = {
        let census = CENSUS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(census) = census.as_ref() else { return };
        if census.names.is_empty() {
            return;
        }
        (census.figures(library, BLOB.load(Ordering::Relaxed), made), census.classes())
    };
    let (hits, created) = super::host::root_signature_counts();
    tracing::info!(
        pipelines = figures.pipelines,
        signatures = figures.signatures,
        shared = figures.shared,
        created,
        hits,
        library_bytes = figures.library,
        opened_bytes = figures.opened,
        grown_bytes = figures.grown,
        made,
        loaded = LOADED.load(Ordering::Relaxed),
        bytes_per_pipeline_made = ?figures.per_pipeline,
        classes = %classes,
        "pipeline census"
    );
}

/// The pipeline named `name`: loaded from the library by `load` if it is
/// there with the same description, else made by `create` and stored.
pub(crate) fn pipeline(
    name: &str,
    load: impl FnOnce(&ID3D12PipelineLibrary1, PCWSTR) -> windows::core::Result<ID3D12PipelineState>,
    create: impl FnOnce() -> windows::core::Result<ID3D12PipelineState>,
) -> windows::core::Result<ID3D12PipelineState> {
    let wide = HSTRING::from(name);
    let Some(library) = library() else {
        MADE.fetch_add(1, Ordering::Relaxed);
        return create();
    };
    let began = std::time::Instant::now();
    let loaded = {
        let _all = one_call();
        let _one = same_name(name);
        let _call = call("load", name);
        load(&library.library, PCWSTR(wide.as_ptr()))
    };
    // The load read the library's file (see `trim`).
    LOADED_AT.store(crate::host::now_ms(), Ordering::Relaxed);
    TRIMMED.store(false, Ordering::Relaxed);
    let refused = match loaded {
        Ok(pso) => {
            LOADED.fetch_add(1, Ordering::Relaxed);
            crate::host::trail(|| {
                let label = LABEL.with(|l| l.borrow().clone());
                format!("pipeline library: loaded {label} in {:.1} ms", began.elapsed().as_secs_f64() * 1e3)
            });
            return Ok(pso);
        }
        Err(e) => e,
    };
    // Compiled outside any lock: compiles take up to seconds.
    let pso = {
        let _call = call("make", name);
        create()?
    };
    MADE.fetch_add(1, Ordering::Relaxed);
    crate::host::trail(|| {
        let label = LABEL.with(|l| l.borrow().clone());
        format!(
            "pipeline library: made {label} in {:.1} ms (the library did not load it: {refused})",
            began.elapsed().as_secs_f64() * 1e3
        )
    });
    let _all = one_call();
    let _stores = library.saving.read().unwrap_or_else(|e| e.into_inner());
    let _call = call("store", name);
    // `shortcut:` a library past its cap takes no more pipelines, so a run
    // that draws a pipeline the file does not hold compiles it again next
    // time. The alternative is a file the host parses whole at every start
    // (752 MB, 26 s of the opening load), and no host call drops an entry.
    if let Some(cap) = cap() {
        // SAFETY: a plain query on a live library, under the guard that keeps
        // a save from serialising it meanwhile.
        let size = unsafe { library.library.GetSerializedSize() };
        if !room(size, Some(cap)) {
            if !AT_CAP.swap(true, Ordering::Relaxed) {
                tracing::info!(
                    size,
                    cap,
                    "the pipeline library is at its cap: pipelines this run makes are not kept (VICTORY_LIBRARY_MB)"
                );
            }
            return Ok(pso);
        }
    }
    // SAFETY: a live pipeline; a name already stored (a description that
    // differs) is refused, which leaves the library as it was.
    if unsafe { library.library.StorePipeline(PCWSTR(wide.as_ptr()), &pso) }.is_ok() {
        library.added.fetch_add(1, Ordering::Relaxed);
    }
    Ok(pso)
}

/// Writes the library and the early list if pipelines were added since the
/// last save, they grew the library by [`SAVE_GROWTH`], and `SAVE_EVERY` has
/// passed (or `now`).
pub(crate) fn save(now: bool) {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    // A save rewrites the whole library, whichever pipeline was added: the
    // 222 MiB one a 32-minute session left (2,853 pipelines) went out every
    // 20 s through the warm-up, and run 60's second save wrote 1,352 MB. A
    // minute does the same work three times less often, and a crash still
    // costs at most the pipelines of the last minute.
    const SAVE_EVERY: std::time::Duration = std::time::Duration::from_secs(60);
    // What growth is worth rewriting the whole library for: a save writes every
    // byte of it (718 MB measured), and a warm run adds a few hundred kilobytes
    // in all, so saving on `SAVE_EVERY` alone wrote the 718 MB two or three times
    // a run for nothing. Growth waits for the load that adds thousands of
    // pipelines, and the shutdown save is forced and takes the rest.
    const SAVE_GROWTH: usize = 64 << 20;
    let (Some(dir), Some(library)) = (DIR.get(), library()) else { return };
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if !now && last.is_some_and(|t| t.elapsed() < SAVE_EVERY) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!(error = %e, "host pipeline library not saved");
        return;
    }
    let tmp = dir.join(LIBRARY_FILE).with_extension("tmp");
    // The bytes of the library written to `tmp`; 0 when no pipeline was
    // added: the library on disk is then what it was, and only the early
    // list is written (a title pipeline first drawn with wrote all 1,352 MB
    // of the library again: run 60's second save). `None`: there is nothing
    // to write at all.
    let bytes: Option<usize> = {
        let _all = one_call();
        // No store between asking the size and writing; loads go on.
        let _alone = library.saving.write().unwrap_or_else(|e| e.into_inner());
        let added = library.added.load(Ordering::Relaxed);
        if !now && added != 0 {
            // SAFETY: a plain query.
            let size = unsafe { library.library.GetSerializedSize() };
            if size.saturating_sub(SAVED.load(Ordering::Relaxed) as usize) < SAVE_GROWTH {
                // Too little new to be worth the whole file: `added` stays, so
                // the next look in a minute or the shutdown save carries these.
                return;
            }
        }
        let changed = USED_CHANGED.swap(false, Ordering::Relaxed);
        if !changed && added == 0 {
            None
        } else if added == 0 {
            Some(0)
        } else {
            // SAFETY: a plain query.
            let size = unsafe { library.library.GetSerializedSize() };
            let _call = call("serialise", "the whole library");
            if let Err(e) = serialise(&library.library, &tmp, size) {
                tracing::warn!(error = %e, "host pipeline library not serialised");
                let _ = std::fs::remove_file(&tmp);
                return;
            }
            library.added.store(0, Ordering::Relaxed);
            SAVED.store(size as u64, Ordering::Relaxed);
            Some(size)
        }
    };
    let Some(bytes) = bytes else {
        // Nothing to write, but a forced save is still the one a reader looks
        // at: the census of a run that only loaded pipelines is due here and
        // nowhere else.
        census(now, library);
        return;
    };
    let used: Vec<String> = USED.lock().unwrap_or_else(|e| e.into_inner()).iter().flatten().cloned().collect();
    let library_written = if bytes > 0 { replace_library(dir, &tmp) } else { Ok(()) };
    if bytes > 0 && library_written.is_ok() {
        // In place unless the system refused and the new one waits beside it.
        let in_place = !dir.join(NEXT_FILE).exists();
        if in_place && VIEW[0].load(Ordering::Relaxed) != 0 {
            REPLACED[1].store(crate::host::now_ms(), Ordering::Relaxed);
            REPLACED[0].fetch_add(1, Ordering::Relaxed);
        }
        crate::host::trail(|| {
            format!(
                "pipeline library: saved {bytes} bytes, {}",
                if in_place { "renamed over the library's file" } else { "left beside the library's file for the next start" }
            )
        });
    }
    let written = library_written
        .and_then(|()| write_atomically(&dir.join(EARLY_FILE), used.join("\n").as_bytes()));
    match written {
        Ok(()) => {
            // The library's size, not just what this save wrote: a save with
            // nothing new in the library (bytes 0, only the early list
            // written) still shows the blob growing run by run.
            // SAFETY: a plain query on a live library.
            let size = unsafe { library.library.GetSerializedSize() };
            tracing::info!(
                bytes,
                library = size,
                early = used.len(),
                made = MADE.load(Ordering::Relaxed),
                loaded = LOADED.load(Ordering::Relaxed),
                started_early = EARLY.load(Ordering::Relaxed),
                "host pipeline library saved"
            );
            census(now, library);
        }
        Err(e) => tracing::warn!(error = %e, "host pipeline library not saved"),
    }
}

/// Writes the library's `size` serialised bytes to the file `tmp`: through
/// a mapping of that file, so that the bytes are the file's pages and not
/// 1.3 GB more of the process's own memory for the seconds a save takes;
/// through a buffer where the file cannot be mapped (and with
/// `VICTORY_LIBRARY_MAP=0`).
fn serialise(library: &ID3D12PipelineLibrary1, tmp: &std::path::Path, size: usize) -> std::io::Result<()> {
    let failed = |e: windows::core::Error| std::io::Error::other(e.to_string());
    if mapped()
        && let Some(mut file) = Mapped::create(tmp, size)
    {
        // SAFETY: a buffer of the size the library asks for.
        return unsafe { library.Serialize(file.bytes_mut()) }.map_err(failed);
    }
    let mut bytes = vec![0u8; size];
    // SAFETY: a buffer of the size the library asks for.
    unsafe { library.Serialize(&mut bytes) }.map_err(failed)?;
    std::fs::write(tmp, &bytes)
}

/// Renames the written library `tmp` over the library's file in `dir`. This
/// run may have that file mapped: Windows 11 replaces a mapped file, and
/// the mapping goes on reading the old one for as long as a handle on it
/// is open ([`holds`]; with none, only the pages in memory at that moment
/// stay: the rest read as zeros). Where a system refuses, the new library
/// is left beside it as [`NEXT_FILE`] for the next start.
fn replace_library(dir: &std::path::Path, tmp: &std::path::Path) -> std::io::Result<()> {
    std::fs::rename(tmp, dir.join(LIBRARY_FILE)).or_else(|_| std::fs::rename(tmp, dir.join(NEXT_FILE)))
}

/// Writes `bytes` to a temporary file beside `path`, then renames it over
/// `path`: a reader never sees a part.
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Title pipeline keys requested by draws or dispatches (this run and the last).
static USED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
static USED_CHANGED: AtomicBool = AtomicBool::new(false);

fn used() -> std::sync::MutexGuard<'static, Option<HashSet<String>>> {
    let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
    if used.is_none() {
        let listed = DIR
            .get()
            .and_then(|d| std::fs::read_to_string(d.join(EARLY_FILE)).ok())
            .unwrap_or_default();
        let set: HashSet<String> = listed.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect();
        tracing::info!(pipelines = set.len(), "title pipelines to start early");
        *used = Some(set);
    }
    used
}

/// Whether the title pipeline with `key` was requested in this or an earlier run.
pub(crate) fn wanted_early(key: &str) -> bool {
    used().as_ref().is_some_and(|s| s.contains(key))
}

/// A draw or dispatch requested the title pipeline with `key`.
pub(crate) fn note_used(key: &str) {
    // Said once, when the list is full: before this the list stopped
    // recording in silence, and a run past 8,192 pipelines simply started
    // fewer of them early the next time.
    static FULL: AtomicBool = AtomicBool::new(false);
    let mut used = used();
    let set = used.get_or_insert_with(HashSet::new);
    if set.len() >= EARLY_MAX {
        // A listed name still counts as wanted; only a new one is dropped.
        if !set.contains(key) && !FULL.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                cap = EARLY_MAX,
                file = EARLY_FILE,
                "the early pipeline list is full: the pipelines first drawn from now on are not listed for an early start"
            );
        }
        return;
    }
    if set.insert(key.to_owned()) {
        USED_CHANGED.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_change_with_what_goes_into_the_pipeline() {
        let a = name("graphics", b"root", &[b"vs", b"ps"], "state");
        assert_eq!(a, name("graphics", b"root", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps2"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps"], "state2"));
        assert_ne!(a, name("graphics", b"root2", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("mesh", b"root", &[b"vs", b"ps"], "state"));
        // Parts do not run into each other.
        assert_ne!(a, name("graphics", b"root", &[b"vsp", b"s"], "state"));
    }

    /// The census counts one pipeline once, however often it is named, and
    /// the per-pipeline figure is what this run's own pipelines added, never
    /// the whole library over this run's count.
    #[test]
    fn the_census_counts_pipelines_and_what_each_added() {
        let mut census = Census::default();
        census.add("compute", b"root A", "one");
        census.add("compute", b"root A", "two");
        census.add("graphics", b"root B", "three");
        census.add("compute", b"root A", "one"); // the same pipeline again
        assert_eq!(census.classes(), "compute 2, graphics 1");

        // A cold run: a library of 600 bytes, made by three pipelines.
        let cold = census.figures(600, 0, 3);
        assert_eq!((cold.pipelines, cold.signatures, cold.shared), (3, 2, 1));
        assert_eq!((cold.library, cold.opened, cold.grown), (600, 0, 600));
        assert_eq!(cold.per_pipeline, Some(200));

        // A warm run: the library already held 737,846,212 bytes, and the two
        // pipelines this run made added 2,097,152. Dividing the whole library
        // by the session's count gave the 1,443,536-byte "pipeline" of the
        // newest log and 187,257 for the same code on a cold cache; the
        // growth over what was made is 1,048,576 either way.
        let warm = census.figures(737_846_212 + 2_097_152, 737_846_212, 2);
        assert_eq!(warm.grown, 2_097_152);
        assert_eq!(warm.per_pipeline, Some(1_048_576));

        // Nothing made (a run that only loaded pipelines): no figure, and no
        // divide by zero.
        let idle = census.figures(737_846_212, 737_846_212, 0);
        assert_eq!((idle.grown, idle.per_pipeline), (0, None));
        // A library that shrank (the host reorganises its own): no figure
        // rather than a wrapped one.
        let shrank = census.figures(700, 900, 4);
        assert_eq!((shrank.grown, shrank.per_pipeline), (0, None));
    }

    #[test]
    fn a_rejected_blob_gives_an_empty_library() {
        let Some(device) = crate::host::host().and_then(|h| h.device.cast::<ID3D12Device1>().ok()) else {
            return; // no host GPU here
        };
        let library = open(&device, Blob::Held(b"not a pipeline library".repeat(16))).expect("an empty library");
        // SAFETY: a plain query on a live library.
        assert!(unsafe { library.library.GetSerializedSize() } > 0);
        assert!(matches!(library._blob, Blob::None));
    }

    /// A mapped file reads as the file's bytes, a file written through a
    /// mapping holds what was written, and the library's file can be
    /// replaced while it is mapped (or the new one waits as the next).
    #[test]
    fn the_library_file_is_mapped_and_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LIBRARY_FILE);
        assert!(matches!(Blob::of(dir.path()), Blob::None), "no file yet");
        std::fs::write(&path, b"").unwrap();
        assert!(matches!(Blob::of(dir.path()), Blob::None), "an empty file");
        let old: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        std::fs::write(&path, &old).unwrap();
        let blob = Blob::of(dir.path());
        assert!(matches!(blob, Blob::Mapped(_)) || !mapped());
        assert_eq!(blob.bytes(), &old[..]);
        // A save while the old file is mapped.
        let tmp = path.with_extension("tmp");
        let new: Vec<u8> = (0..300_000u32).map(|i| (i * 7) as u8).collect();
        {
            let mut file = Mapped::create(&tmp, new.len()).expect("a new file, mapped");
            file.bytes_mut().copy_from_slice(&new);
        }
        assert_eq!(std::fs::read(&tmp).unwrap(), new);
        replace_library(dir.path(), &tmp).unwrap();
        assert!(!tmp.exists());
        // The mapping still reads the old library.
        assert_eq!(blob.bytes(), &old[..]);
        drop(blob);
        // The next start reads the new one, wherever the save left it.
        assert_eq!(Blob::of(dir.path()).bytes(), &new[..]);
        assert!(!dir.path().join(NEXT_FILE).exists());
    }

    /// The bytes at `at` of the pattern the test below writes.
    fn pattern(at: usize, out: &mut [u8]) {
        for (i, word) in out.chunks_exact_mut(4).enumerate() {
            let v = ((at / 4 + i) as u32).wrapping_mul(2_654_435_761).wrapping_add(1);
            word.copy_from_slice(&v.to_le_bytes());
        }
    }

    /// A file of `len` bytes (whole MiB) of [`pattern`], written past the
    /// system's cache: none of its pages is in memory afterwards. False
    /// where the folder's file system does not take such writes.
    fn write_uncached(path: &std::path::Path, len: usize) -> bool {
        use std::io::Write;
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH
        const UNCACHED: u32 = 0x2000_0000 | 0x8000_0000;
        const CHUNK: usize = 1 << 20;
        let Ok(mut file) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(UNCACHED)
            .open(path)
        else {
            return false;
        };
        // Such writes want a buffer on a sector boundary.
        let mut store = vec![0u8; CHUNK + 4096];
        let start = store.as_ptr().align_offset(4096);
        for at in (0..len).step_by(CHUNK) {
            let chunk = &mut store[start..start + CHUNK];
            pattern(at, chunk);
            if file.write_all(chunk).is_err() {
                return false;
            }
        }
        true
    }

    /// The device loss of runs 72, 73 and 82: the library's file is
    /// replaced while it is mapped, and pages of the mapping that were not
    /// in memory then are read afterwards (in the runs: pages the trim let
    /// go and Windows gave to something else). With the file's handle kept
    /// open they still read as the old file. (With it closed, as before,
    /// all but the first few read as zeros on the PC this was found on.)
    #[test]
    fn a_replaced_library_file_still_reads_from_the_disk() {
        const LEN: usize = 8 << 20;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LIBRARY_FILE);
        if !write_uncached(&path, LEN) {
            return; // no uncached writes here: nothing to show
        }
        let mapped = Mapped::open_holding(&path, true).expect("the file, mapped");
        // A save: the new library, renamed over the mapped one.
        let tmp = path.with_extension("tmp");
        {
            let mut file = Mapped::create(&tmp, LEN).expect("a new file, mapped");
            file.bytes_mut().fill(0xEE);
        }
        replace_library(dir.path(), &tmp).unwrap();
        // Every page of the old mapping, read for the first time now.
        let mut expected = vec![0u8; 4096];
        for (page, bytes) in mapped.bytes().chunks(4096).enumerate() {
            pattern(page * 4096, &mut expected);
            assert!(bytes == &expected[..], "page {page} of the replaced library no longer reads as the old file");
        }
    }

    /// A file of `len` bytes called `name` in `dir`, last written `when`: a
    /// generation of the library's file as a start leaves one.
    fn planted(dir: &Path, name: &str, len: usize, when: SystemTime) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, vec![b'x'; len]).unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(when).unwrap();
        path
    }

    fn ago(now: SystemTime, days: u64) -> SystemTime {
        now - Duration::from_secs(days * 24 * 60 * 60)
    }

    /// A clock a test can reason about: every planted time is relative to it.
    fn a_fixed_now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 * 24 * 60 * 60)
    }

    /// The file this run opens and replaces is never a candidate, however
    /// little anything has read it: a run that dropped its own library would
    /// rebuild thousands of pipelines.
    #[test]
    fn the_live_library_is_never_pruned() {
        // The pruner protects the name the library is opened under: the two
        // must not drift apart.
        assert!(LIBRARY_FILE.starts_with(LIBRARY_STEM));
        let dir = tempfile::tempdir().unwrap();
        let now = a_fixed_now();
        let live = planted(dir.path(), LIBRARY_FILE, 4_096, ago(now, 400));
        // The early list beside it is the title's, not a generation.
        let early = planted(dir.path(), EARLY_FILE, 40, ago(now, 400));
        assert_eq!(prune(dir.path(), now), Pruned::default(), "nothing to prune");
        assert!(live.exists(), "the live library stays whatever its age");
        assert!(early.exists(), "the early list is not the library's");
    }

    /// Beyond the live file one generation stays, the newest: the one a start
    /// left for the next to promote. Older ones are the hundreds of megabytes
    /// a driver or key change leaves behind.
    #[test]
    fn one_generation_beside_the_live_library_stays() {
        let dir = tempfile::tempdir().unwrap();
        let now = a_fixed_now();
        let live = planted(dir.path(), LIBRARY_FILE, 100, ago(now, 4));
        let next = planted(dir.path(), NEXT_FILE, 300, ago(now, 1));
        // A generation of another adapter or driver, and a save of a build
        // that named the library differently: neither is ever read again.
        let older = planted(dir.path(), "host-pipelines-0f3a.bin", 200, ago(now, 2));
        let oldest = planted(dir.path(), "host-pipelines-9c11.bin", 500, ago(now, 3));
        let pruned = prune(dir.path(), now);
        assert_eq!((pruned.files, pruned.bytes), (2, 700));
        assert!(live.exists() && next.exists(), "the live file and the newest other stay");
        assert!(!older.exists() && !oldest.exists(), "the rest go");
    }

    /// The age rule holds under the cap: one generation beside the live one,
    /// and still gone when nothing has read it for a month.
    #[test]
    fn a_generation_nobody_read_for_a_month_goes_under_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let now = a_fixed_now();
        let live = planted(dir.path(), LIBRARY_FILE, 100, ago(now, 1));
        let stale = planted(dir.path(), NEXT_FILE, 400, ago(now, 31));
        let pruned = prune(dir.path(), now);
        assert_eq!((pruned.files, pruned.bytes), (1, 400));
        assert!(live.exists());
        assert!(!stale.exists(), "a month unread is dead whatever the count");
        // A time in the future (a clock that moved) counts as untouched.
        let ahead = planted(dir.path(), "host-pipelines-0f3a.bin", 64, now + Duration::from_secs(86_400));
        assert_eq!(prune(dir.path(), now), Pruned::default());
        assert!(ahead.exists(), "a stamp after the clock is not an old file");
    }

    /// A save killed while it wrote the whole library leaves a full-size
    /// `.tmp` no run reads back: it goes whatever its age, not one of the
    /// generations the cap counts.
    #[test]
    fn a_save_that_never_finished_goes_whatever_its_age() {
        let dir = tempfile::tempdir().unwrap();
        let now = a_fixed_now();
        let live = planted(dir.path(), LIBRARY_FILE, 100, ago(now, 1));
        let next = planted(dir.path(), NEXT_FILE, 300, ago(now, 2));
        let half = planted(dir.path(), "host-pipelines.tmp", 250, now - Duration::from_secs(60));
        let pruned = prune(dir.path(), now);
        assert_eq!((pruned.files, pruned.bytes), (1, 250));
        assert!(live.exists() && next.exists(), "the promotion candidate stays");
        assert!(!half.exists(), "an unfinished save is never read back");
    }

    /// A generation the file system refuses to remove is left where it is and
    /// counted as nothing: a warning, never a panic. A directory in a
    /// generation's place is the one shape of that refusal a test can make
    /// without a second process; a read-only file is not one, because the
    /// standard library deletes it (measured here: 900 bytes taken from under
    /// `FILE_ATTRIBUTE_READONLY`).
    #[test]
    fn a_generation_that_cannot_be_removed_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let now = a_fixed_now();
        let live = planted(dir.path(), LIBRARY_FILE, 100, ago(now, 1));
        let guarded = dir.path().join("host-pipelines-0f3a.bin");
        std::fs::create_dir(&guarded).unwrap();
        std::fs::write(guarded.join("kept.bin"), vec![b'x'; 900]).unwrap();
        assert_eq!(prune(dir.path(), now), Pruned::default(), "nothing was removed");
        assert!(guarded.join("kept.bin").exists(), "a generation that could not be removed stays");
        assert!(live.exists());
        // A read-only file is removed like any other, so the pass reports it.
        let readonly = planted(dir.path(), "host-pipelines-9c11.bin", 300, ago(now, 40));
        let mut permissions = std::fs::metadata(&readonly).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&readonly, permissions).unwrap();
        assert_eq!(prune(dir.path(), now), Pruned { files: 1, bytes: 300 });
        assert!(live.exists() && guarded.exists());
    }

    /// A directory where a generation would be is not ours to remove, and
    /// neither is a file that is not the library's.
    #[test]
    fn only_the_librarys_own_files_are_looked_at() {
        let dir = tempfile::tempdir().unwrap();
        let now = a_fixed_now();
        let live = planted(dir.path(), LIBRARY_FILE, 100, ago(now, 1));
        // A folder in a generation's place, and a neighbour's file.
        let in_the_way = dir.path().join("host-pipelines-0f3a.bin");
        std::fs::create_dir(&in_the_way).unwrap();
        let theirs = planted(dir.path(), "notebook.bin", 64, ago(now, 400));
        assert_eq!(prune(dir.path(), now), Pruned::default());
        assert!(in_the_way.is_dir() && theirs.exists() && live.exists());
        // An empty folder that was never written is the first run: no warning,
        // nothing to do.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(prune(&empty.path().join("pipelines"), now), Pruned::default());
    }

    /// The cap `VICTORY_LIBRARY_MB` names, and that a library at it takes no
    /// more pipelines: the whole point of it is a file the host parses in
    /// a start, and one entry more is a file that grows for ever.
    #[test]
    fn the_cap_bounds_what_the_library_may_grow_to() {
        assert_eq!(cap_of(None), None, "unset: no cap, as before");
        assert_eq!(cap_of(Some("")), None);
        assert_eq!(cap_of(Some("0")), None, "0 is off, as before");
        assert_eq!(cap_of(Some("junk")), None);
        assert_eq!(cap_of(Some("-1")), None);
        assert_eq!(cap_of(Some(" 256 ")), Some(256 << 20));
        assert_eq!(cap_of(Some("4096")), Some(4096 << 20));
        // One byte short of the cap takes a pipeline; the cap itself takes
        // none, and neither does anything past it (a library that shrank is
        // still one the host must parse whole).
        assert!(room((752 << 20) - 1, Some(752 << 20)));
        assert!(!room(752 << 20, Some(752 << 20)));
        assert!(!room((752 << 20) + 1, Some(752 << 20)));
        assert!(room(usize::MAX / 2, None), "no cap, no bound");
        // The read-ahead is on unless a run switches it off, so the shipped
        // behaviour is the one the notes measure.
        assert!(prefetches_of(None));
        assert!(prefetches_of(Some("1")));
        assert!(!prefetches_of(Some("0")));
    }

    /// The read ahead is the whole file, once, and the system takes it: the
    /// pages are the file's, read before the host parses them.
    #[test]
    fn the_read_ahead_covers_the_whole_file_once() {
        assert!(prefetch_ranges(0).is_empty());
        assert_eq!(prefetch_ranges(10), vec![(0, 10)]);
        assert_eq!(prefetch_ranges(PREFETCH_CHUNK * 2 + 7), vec![
            (0, PREFETCH_CHUNK),
            (PREFETCH_CHUNK, PREFETCH_CHUNK),
            (PREFETCH_CHUNK * 2, 7)
        ]);
        // No page skipped, none counted twice, whatever the size.
        for len in [1usize, 4_095, 4_096, PREFETCH_CHUNK - 1, PREFETCH_CHUNK + 1] {
            let mut at = 0;
            for (start, bytes) in prefetch_ranges(len) {
                assert_eq!(start, at, "the ranges run on from each other");
                assert!(bytes > 0);
                at += bytes;
            }
            assert_eq!(at, len, "they cover the file");
        }
        // And the system's call takes one: a mapping of a real file, read
        // ahead the way a load reads it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LIBRARY_FILE);
        std::fs::write(&path, vec![7u8; 1 << 20]).unwrap();
        let mapped = Mapped::open(&path).expect("the file, mapped");
        for (at, bytes) in prefetch_ranges(mapped.len) {
            // SAFETY: as `prefetch` asks: the whole of our own view.
            let read = unsafe { prefetch(mapped.at.Value.cast::<u8>().wrapping_add(at), bytes) };
            assert!(read, "the system read it ahead");
        }
        assert_eq!(&mapped.bytes()[..4], [7; 4]);
    }

    /// Reads `path` (a file of `len` bytes) past the system's cache, into a
    /// buffer of its own: the bytes the disk holds, with no page of them
    /// left in the cache for a mapping to be handed for free.
    fn read_uncached(path: &Path, len: usize) -> Option<Vec<u8>> {
        use std::io::Read;
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_NO_BUFFERING
        const UNCACHED: u32 = 0x2000_0000;
        const CHUNK: usize = 1 << 20;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(UNCACHED)
            .open(path)
            .ok()?;
        let mut store = vec![0u8; CHUNK + 4096];
        let start = store.as_ptr().align_offset(4096);
        let mut bytes = Vec::with_capacity(len);
        while bytes.len() < len {
            let chunk = &mut store[start..start + CHUNK];
            let read = file.read(chunk).ok()?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
        Some(bytes)
    }

    /// Where a real load's time goes, in the order the load spends it:
    /// `VICTORY_LIBRARY_BENCH=<path to a host-pipelines.bin>` and
    /// `cargo test -p victory-gpu --lib the_load_of_a_real_library_is_timed
    /// -- --nocapture`. Skips (passes) without the variable or without a
    /// host GPU, like the GPU tests above.
    ///
    /// The figures it prints, for a 752 MB library:
    /// - the disk, the file read past the cache: the floor under everything;
    /// - a mapping's pages faulted in by the parse itself: what a start does
    ///   with `VICTORY_LIBRARY_PREFETCH=0`;
    /// - the same parse again, the pages in memory: the host's own work;
    /// - the parse of bytes read into a buffer: the same work, no mapping;
    /// - a fresh mapping read ahead first, then parsed: the load as shipped.
    #[test]
    fn the_load_of_a_real_library_is_timed() {
        let Some(path) = std::env::var_os("VICTORY_LIBRARY_BENCH").map(PathBuf::from) else {
            println!("skipped: VICTORY_LIBRARY_BENCH is not set");
            return;
        };
        let Some(device) = crate::host::host().and_then(|h| h.device.cast::<ID3D12Device1>().ok()) else {
            println!("skipped: no host GPU here");
            return;
        };
        let file = std::fs::File::open(&path).expect("the library file, opened");
        let len = usize::try_from(file.metadata().unwrap().len()).unwrap();
        assert!(len > 0, "an empty library");
        println!("{}: {len} bytes", path.display());
        let make = |bytes: &[u8]| -> f64 {
            let began = Instant::now();
            // SAFETY: the bytes outlive the library (`made` is dropped before
            // them, in this call).
            let made = unsafe { device.CreatePipelineLibrary::<ID3D12PipelineLibrary>(bytes) };
            let spent = ms(began);
            match &made {
                // SAFETY: a plain query on a live library.
                Ok(library) => println!(
                    "  parse of {} bytes: {spent:.0} ms (serialised {} bytes)",
                    bytes.len(),
                    unsafe { library.GetSerializedSize() }
                ),
                Err(e) => println!("  parse of {} bytes: {spent:.0} ms, refused: {e}", bytes.len()),
            }
            spent
        };
        // 1. What the disk holds: the file read past the system's cache, so
        //    that no page of it is left for the mappings below.
        let began = Instant::now();
        let uncached = read_uncached(&path, len).expect("an uncached read of the file");
        let disk_ms = ms(began);
        println!(
            "the disk: {disk_ms:.0} ms for {len} bytes ({:.0} MB/s)",
            len as f64 / 1e6 / (disk_ms / 1e3)
        );
        assert_eq!(uncached.len(), len, "the whole file");
        // 2. A mapping as a start makes it, its pages faulted in by the
        //    parse: the load without the read ahead.
        let mapped = Mapped::map(&file, len, false).expect("a mapping of the file");
        let without = {
            let began = Instant::now();
            let spent = make(mapped.bytes());
            println!("  (of which the mapping: {:.0} ms)", ms(began) - spent);
            spent
        };
        // 3. The same parse of the same mapping with its pages in memory.
        let warm = make(mapped.bytes());
        // 4. The bytes as a buffer, as `VICTORY_LIBRARY_MAP=0` holds them.
        let held = make(&uncached);
        drop(mapped);
        // 5. A fresh mapping, read ahead in one pass, then parsed: the load as
        //    shipped. The pages are already the system's here, so the figure
        //    is the parse and the read ahead's own cost, not the disk's.
        let fresh = Mapped::open(&path).expect("the file, mapped");
        let began = Instant::now();
        for (at, bytes) in prefetch_ranges(fresh.len) {
            // SAFETY: as `prefetch` asks: the whole of our own view.
            unsafe { prefetch(fresh.at.Value.cast::<u8>().wrapping_add(at), bytes) };
        }
        let read_ahead = ms(began);
        let read_ahead_parse = make(fresh.bytes());
        println!(
            "summary: disk {disk_ms:.0} ms; faults {without:.0} ms; warm {warm:.0} ms; \
             buffer {held:.0} ms; read ahead {read_ahead:.0} ms + parse {read_ahead_parse:.0} ms"
        );
    }

    /// What a library is made of, and what of it the shader cache already
    /// holds: `VICTORY_LIBRARY_BENCH=<a host-pipelines.bin>` and
    /// `VICTORY_LIBRARY_BENCH_SHADERS=<cache/shaders>` say what to read.
    /// Prints how many bytes of the library are copies of the translated
    /// shader containers in that folder (a lower bound: the host's own
    /// compiled code is not one of them) and the rest.
    #[test]
    fn the_library_holds_copies_of_shader_code_the_cache_already_has() {
        use std::collections::HashSet;
        use std::io::Read;
        let (Some(library), Some(shaders)) = (
            std::env::var_os("VICTORY_LIBRARY_BENCH").map(PathBuf::from),
            std::env::var_os("VICTORY_LIBRARY_BENCH_SHADERS").map(PathBuf::from),
        ) else {
            println!("skipped: VICTORY_LIBRARY_BENCH_SHADERS is not set");
            return;
        };
        // A shader cache file is `VSH1`, a version, a length, then the
        // container the translator made; its first 32 bytes name it.
        const HEAD: usize = 44;
        let mut containers: HashSet<[u8; 32]> = HashSet::new();
        let mut files = 0u64;
        let mut biggest: Vec<(u64, PathBuf)> = Vec::new();
        for at in 0..256u32 {
            let Ok(listing) = std::fs::read_dir(shaders.join(format!("{at:02x}"))) else { continue };
            for entry in listing.flatten() {
                let Ok(mut file) = std::fs::File::open(entry.path()) else { continue };
                let mut head = [0u8; HEAD];
                if file.read_exact(&mut head).is_err() || &head[..4] != b"VSH1" {
                    continue;
                }
                files += 1;
                containers.insert(head[12..].try_into().unwrap());
                if let Ok(metadata) = entry.metadata() {
                    biggest.push((metadata.len(), entry.path()));
                }
            }
        }
        let file = std::fs::File::open(&library).expect("the library file, opened");
        let len = usize::try_from(file.metadata().unwrap().len()).unwrap();
        let blob = Mapped::map(&file, len, false).expect("a mapping of the library");
        let bytes = blob.bytes();
        // Every `DXBC` the library holds, counted with the size the container
        // states at its offset 24.
        let (mut copies, mut made_of, mut shaders_seen) = (0u64, 0u64, 0u64);
        let mut seen: HashSet<[u8; 32]> = HashSet::new();
        let mut at = 0usize;
        while let Some(found) = find(bytes, b"DXBC", at) {
            at = found + 4;
            let Some(head) = bytes.get(found..found + 32) else { break };
            let head: [u8; 32] = head.try_into().unwrap();
            if !containers.contains(&head) {
                continue;
            }
            let size = u32::from_le_bytes(bytes[found + 24..found + 28].try_into().unwrap()) as usize;
            assert!((32..=1 << 26).contains(&size), "a container's size: {size}");
            copies += 1;
            made_of += size as u64;
            if seen.insert(head) {
                shaders_seen += 1;
            }
        }
        println!("the library: {len} bytes; the shader cache: {files} containers");
        println!(
            "the library holds {copies} copies of {shaders_seen} of them: {made_of} bytes of shader \
             code ({:.0} %), the other {} bytes are descriptions and the host's own compiled code",
            made_of as f64 * 100.0 / len as f64,
            len as u64 - made_of
        );
        // ... and whether any container survives at all: three of the largest,
        // matched 4 KiB in, where a rewritten header cannot reach.
        biggest.sort_unstable_by_key(|it| std::cmp::Reverse(it.0));
        for (size, path) in biggest.into_iter().take(3) {
            let Ok(mut file) = std::fs::File::open(&path) else { continue };
            let mut inside = [0u8; 32];
            use std::io::Seek;
            if file.seek(std::io::SeekFrom::Start(12 + 4096)).is_err()
                || file.read_exact(&mut inside).is_err()
            {
                continue;
            }
            println!(
                "a {size} byte container of the shader cache: its bytes 4 KiB in are {} in the library",
                match find(bytes, &inside, 0) {
                    Some(at) => format!("at {at}"),
                    None => "nowhere".to_owned(),
                }
            );
        }
    }

    /// Where `needle` is in `bytes`, from `at` on.
    fn find(bytes: &[u8], needle: &[u8], at: usize) -> Option<usize> {
        let mut from = at;
        while let Some(offset) = bytes.get(from..)?.iter().position(|&b| b == needle[0]) {
            let found = from + offset;
            if bytes.get(found..found + needle.len()) == Some(needle) {
                return Some(found);
            }
            from = found + 1;
        }
        None
    }

    /// What the two ways of getting a file's pages in cost against the disk:
    /// one page fault at a time as a parse reaches them, or one read ahead of
    /// the whole file. `VICTORY_LIBRARY_BENCH_DISK=<MiB>` (256 is a second or
    /// two of a fast disk); skipped without it, because the measurement
    /// writes two files of that size past the system's cache.
    #[test]
    fn a_file_read_ahead_is_read_faster_than_a_file_faulted_in() {
        let Some(mib) = std::env::var("VICTORY_LIBRARY_BENCH_DISK").ok().and_then(|v| v.parse::<usize>().ok()) else {
            println!("skipped: VICTORY_LIBRARY_BENCH_DISK is not set");
            return;
        };
        let len = mib << 20;
        let dir = tempfile::tempdir().unwrap();
        // One byte of every page, as a parse reaching the next page reads
        // one: the pages come in however they come in.
        let touch = |file: &Mapped| -> f64 {
            let began = Instant::now();
            let mut sum = 0u64;
            for at in (0..file.len).step_by(4096) {
                sum = sum.wrapping_add(u64::from(file.bytes()[at]));
            }
            std::hint::black_box(sum);
            ms(began)
        };
        let rate = |ms: f64| len as f64 / 1e6 / (ms / 1e3);
        // 1. The pages faulted in as they are read, as the host's parse of a
        //    mapping does it.
        let faulted = dir.path().join("faulted.bin");
        if !write_uncached(&faulted, len) {
            println!("skipped: this folder takes no writes past the cache");
            return;
        }
        let file = Mapped::open(&faulted).expect("the file, mapped");
        let faults = touch(&file);
        // 2. The pages read ahead in one pass, then read.
        let ahead = dir.path().join("ahead.bin");
        if !write_uncached(&ahead, len) {
            println!("skipped: this folder takes no writes past the cache");
            return;
        }
        let file = Mapped::open(&ahead).expect("the file, mapped");
        let began = Instant::now();
        for (at, bytes) in prefetch_ranges(file.len) {
            // SAFETY: as `prefetch` asks: the whole of our own view.
            unsafe { prefetch(file.at.Value.cast::<u8>().wrapping_add(at), bytes) };
        }
        let ahead_ms = ms(began);
        let after = touch(&file);
        println!(
            "{mib} MiB: faults {faults:.0} ms ({:.0} MB/s); read ahead {ahead_ms:.0} ms ({:.0} MB/s), \
             then a read of every page {after:.0} ms",
            rate(faults),
            rate(ahead_ms)
        );
    }
}
