//! Texture contents the title writes itself (decision 0007, "Texture
//! memory"): streamed pages, layout-library copies, movie frames, glyph
//! atlases. Such a texture lives in console memory in its tiled layout;
//! before a submission samples its host copy, the memory is detiled into a
//! staging buffer when its content changed, and the staging buffer is
//! copied into the host texture.
//!
//! "Changed" means a page of the memory was written since the last detiling
//! ([`crate::watch`]), in write-watched memory; in physical-page views,
//! which cannot be watched, a content hash of the whole memory at every
//! submission that samples it.
//!
//! shortcut: a change detiles the whole texture, not the pages written.
//! Only textures read through descriptor tables are brought up to date
//! (directly indexed views are not known per draw); plane 0 is uploaded
//! (every mip and array slice), depth and stencil planes are not.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::{HostList, host};
use crate::object::This;

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A texture's upload state: the staging buffer laid out as the host
/// texture's subresources, and what was last detiled.
pub(crate) struct Staging {
    buffer: ID3D12Resource,
    /// Its bytes.
    #[cfg(feature = "diagnostics")]
    size: u64,
    data: *mut u8,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    layout: victory_formats::texture::Layout,
    /// Mips per array slice.
    mips: u32,
    /// The staging buffer's bytes (counted in `STAGED` while it lives).
    bytes: u64,
    memory: *const u8,
    last: Mutex<Last>,
    /// Whether the staging buffer was detiled since a submitted list last
    /// uploaded it, and whether one has uploaded it at all: a list records
    /// the upload only then. (Every list that sampled a title texture
    /// uploaded all of it again, changed or not: 5,411 subresource copies
    /// of 1,120 textures in one frame of run 18.)
    dirty: std::sync::atomic::AtomicBool,
    uploaded: std::sync::atomic::AtomicBool,
    /// When a list last recorded a use of it (`crate::host::moment`).
    used: std::sync::atomic::AtomicU64,
}

/// What a texture's staging buffer holds: the hash of the memory detiled
/// (memory not watched), or the write-watch asking it was detiled after.
#[derive(Default)]
struct Last {
    hash: Option<blake3::Hash>,
    asked: u64,
    /// The frame a partly mapped texture was last read in.
    partial: Option<u64>,
}

// SAFETY: the staging mapping is written only at submission, under
// `last`'s lock; the title memory is only read.
unsafe impl Send for Staging {}
// SAFETY: as above.
unsafe impl Sync for Staging {}

static STAGING: Mutex<Option<HashMap<usize, Arc<Staging>>>> = Mutex::new(None);

/// The buffer a texture the host writes is copied through into the texture
/// of a view the title placed in its memory in another format
/// ([`alias`]): every level of every layer as each of the two formats lays
/// it out for a copy (the same bytes: the texels are as wide), the written
/// texture's layout first.
struct Through {
    buffer: ID3D12Resource,
    parts: Vec<(
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    )>,
}

/// The most levels times layers [`alias`] copies (a cube of nine levels).
const ALIAS_PARTS: u32 = 54;

// SAFETY: a device-local buffer only lists record copies with.
unsafe impl Send for Through {}
// SAFETY: as above.
unsafe impl Sync for Through {}

/// The copy buffers of [`alias`], by the placed view's texture.
static THROUGH: Mutex<Option<HashMap<usize, Arc<Through>>>> = Mutex::new(None);

/// Brings `placed`, the texture of a view the title placed in the memory
/// of `source` in another format (`crate::resource::alias_of`), up to date
/// with what the host wrote to `source`: the texels of every level and
/// layer are copied as they are, through a buffer (the host copies between
/// two formats of one texel width in no other way). Recorded the first time
/// the list reads the view
/// and again whenever the list used `source` in another way since (wrote
/// it, or read it itself). Leaves `placed` in the copy destination state,
/// as [`upload`] does. False if the copy cannot be made: the caller fills
/// the texture from title memory, as before.
pub(crate) fn alias(list: &HostList, placed: This, source: This) -> bool {
    const TARGET_OR_UNORDERED: u32 = 0x4 | 0x8;
    const COPY_DEST: u32 = 0x400;
    const COPY_SOURCE: u32 = 0x800;
    let (Some(to), Some(from)) = (
        crate::resource::host_resource(placed),
        crate::resource::host_resource(source),
    ) else {
        return false;
    };
    let Some(host) = host() else { return false };
    let through = {
        let mut map = THROUGH.lock().unwrap_or_else(|e| e.into_inner());
        let map = map.get_or_insert_with(HashMap::new);
        match map.get(&(placed as usize)) {
            Some(through) => through.clone(),
            None => {
                // SAFETY: live textures.
                let (of_from, of_to) = unsafe { (from.GetDesc(), to.GetDesc()) };
                // Levels times layers (a volume's slices are one part).
                let count = |d: &D3D12_RESOURCE_DESC| {
                    let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
                        1
                    } else {
                        u32::from(d.DepthOrArraySize)
                    };
                    u32::from(d.MipLevels) * layers
                };
                let parts = count(&of_from);
                if parts == 0 || parts > ALIAS_PARTS || parts != count(&of_to) {
                    return false;
                }
                let mut rows = [(); 2]
                    .map(|()| vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); parts as usize]);
                let mut bytes = [0u64; 2];
                // SAFETY: plain queries; the out pointers are ours and
                // hold `parts` layouts each.
                unsafe {
                    host.device.GetCopyableFootprints(
                        &of_from,
                        0,
                        parts,
                        0,
                        Some(rows[0].as_mut_ptr()),
                        None,
                        None,
                        Some(&mut bytes[0]),
                    );
                    host.device.GetCopyableFootprints(
                        &of_to,
                        0,
                        parts,
                        0,
                        Some(rows[1].as_mut_ptr()),
                        None,
                        None,
                        Some(&mut bytes[1]),
                    );
                }
                let alike = rows[0].iter().zip(&rows[1]).all(|(a, b)| {
                    let (fa, fb) = (a.Footprint, b.Footprint);
                    a.Offset == b.Offset
                        && (fa.Width, fa.Height, fa.Depth, fa.RowPitch)
                            == (fb.Width, fb.Height, fb.Depth, fb.RowPitch)
                });
                if bytes[0] == 0 || bytes[0] != bytes[1] || !alike {
                    return false;
                }
                let Some(buffer) = host.uav_buffer(bytes[0]) else {
                    return false;
                };
                crate::host::name(&buffer, || {
                    format!(
                        "alias copy {}x{} f{} to f{}",
                        of_from.Width, of_from.Height, of_from.Format.0, of_to.Format.0
                    )
                });
                tracing::info!(
                    view = ?placed,
                    texture = ?source,
                    width = of_from.Width,
                    height = of_from.Height,
                    parts,
                    written_as = of_from.Format.0,
                    read_as = of_to.Format.0,
                    "a view placed in the memory of a texture the host writes, in another format: read from that texture by a copy on the host"
                );
                let [of_written, of_read] = rows;
                let through = Arc::new(Through {
                    buffer,
                    parts: of_written.into_iter().zip(of_read).collect(),
                });
                map.insert(placed as usize, through.clone());
                through
            }
        }
    };
    let first = list.first_use(placed as usize);
    // The source's first use in the list records nothing (its prelude
    // brings it to the state); a use in another way since the last copy
    // shows as a change of state.
    let before = list.transition(source, COPY_SOURCE).map(|(texture, before)| {
        let state = crate::host::legal_state(texture, COPY_SOURCE);
        let barrier = crate::host::transition(
            texture,
            D3D12_RESOURCE_STATES(before as i32),
            D3D12_RESOURCE_STATES(state as i32),
        );
        // SAFETY: an open list and a live texture.
        unsafe { list.list.ResourceBarrier(&[barrier]) };
        before
    });
    if !first && before.is_none() {
        return true;
    }
    crate::list::ensure_state(list, placed, COPY_DEST);
    let part = |texture: &ID3D12Resource, index: usize| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: index as u32,
        },
    };
    let rows = |row: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(&through.buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: *row,
        },
    };
    let (common, read, written) = (
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    #[cfg(feature = "diagnostics")]
    list.note_op(|| format!("alias copy of {source:?} into {placed:?}"));
    // SAFETY: an open list and live resources; the buffer is as big as
    // the footprints and rests in the common state between copies.
    unsafe {
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, common, written)]);
        for (index, (of_written, _)) in through.parts.iter().enumerate() {
            list.list
                .CopyTextureRegion(&rows(of_written), 0, 0, 0, &part(from, index), None);
        }
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, written, read)]);
        for (index, (_, of_read)) in through.parts.iter().enumerate() {
            list.list
                .CopyTextureRegion(&part(to, index), 0, 0, 0, &rows(of_read), None);
        }
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, read, common)]);
    }
    list.keep(&through.buffer);
    // A draw that also writes the source (a view of it handled before
    // this one) needs it in that state again; the next read of the alias
    // then copies once more, as it must after a write.
    if let Some(before) = before
        && before & TARGET_OR_UNORDERED != 0
    {
        crate::list::ensure_state(list, source, before);
    }
    true
}

/// Whether a texture refresh that finds the runtime remapping title memory
/// goes on without waiting for it (`VICTORY_TEXTURE_NO_WAIT=1`; off by
/// default).
///
/// A texture whose memory is all mapped is read in place, under a lock
/// the runtime takes alone while it changes a mapping
/// (`crate::watch::title_memory_reading`), so that no page goes away under
/// the read. While the world streams in the runtime remaps all the time,
/// and each of the frame's 150-360 refreshes waited for it: runs 99-101
/// have 4, 15 and 16 frame lines with over 300 ms of "textures", up to
/// 3,708 ms, and on every one of them the wait for this lock is the whole
/// of it (1,345 of 1,381 ms; 3,630 of 3,708; the write-watch askings 3-12
/// ms, the texture's own lock 0).
///
/// With the switch the refresh asks for the lock without waiting. If the
/// runtime has it, the texture is read through the kernel instead, which
/// needs no lock: it fails if a page is not there, and then the texture is
/// left as it is and read at its next refresh, a frame later. A read
/// through the kernel costs about 0.6 ms a megabyte against 0.3 in place.
///
/// What changes for the picture: a texture that arrives while the runtime
/// is remapping may show a frame later. It never shows partly read.
fn no_wait() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_NO_WAIT").is_some_and(|v| v == "1"))
}

/// Since the last frame-rate line, with [`no_wait`]: refreshes that read
/// through the kernel because title memory was being remapped, and those
/// left for the next refresh because a page was not there.
pub(crate) static UNLOCKED: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Since the last frame-rate line, of texture refreshes on every thread, in
/// microseconds: waiting for the texture's own lock (another thread is
/// refreshing the same texture), asking the write watch and the guards,
/// waiting for title memory to hold still (the lock the runtime takes
/// alone while it remaps, `crate::watch::title_memory_reading`), and the
/// refreshes in all.
///
/// While the world streams in, the frame line's "textures" is seconds a
/// frame in some runs and tenths in others for the same megabytes (2,566
/// ms for 110 MB in run 93, 314 ms for 132 MB in run 91; before these
/// counters, runs 81, 83, 85 and 94 each had lines of 1.5-2.9 s), and the
/// submitting thread is not running while it lasts (run 93: 240-310 ms of
/// its own time a frame beside 1,100-1,800 ms of "textures"): it waits.
/// These say for what.
pub(crate) static REFRESH_WAITS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// `f`'s result, its time added to [`REFRESH_WAITS`]' place `slot`.
fn waited<T>(slot: usize, f: impl FnOnce() -> T) -> T {
    let started = std::time::Instant::now();
    let out = f();
    REFRESH_WAITS[slot].fetch_add(started.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
    out
}

thread_local! {
    /// Room to read a texture's memory into: one buffer per submitting
    /// thread. Each texture kept its own, a second copy in memory of every
    /// write-watched or partly mapped texture ever sampled, for the run.
    static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Calls `f` with `size` bytes of this thread's read buffer (their
/// contents are whatever the last texture left).
fn with_read<R>(size: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    READ.with(|read| {
        let mut read = read.borrow_mut();
        if read.len() < size {
            read.resize(size, 0);
        }
        f(&mut read[..size])
    })
}

/// Whether a texture all of whose memory is mapped is read through the
/// kernel's copy instead of in place (`VICTORY_TEXTURE_COPY=1`; off by
/// default). Round 3 of the memory work read every such texture that way
/// to be safe from pages taken away under the read; it cost 0.6 ms per MB
/// against 0.3 in place (90-105 ms a frame while the world streams in, runs
/// 72 and 73, against 20-40), and both runs with it lost the graphics
/// device, for a reason not found. The read in place is safe now for
/// another reason (`crate::watch::title_memory_reading`); this switch is
/// left to tell whether the copy was what lost the device.
fn through_copy() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_COPY").is_some_and(|v| v == "1"))
}

/// Calls `f` with the `size` bytes of title memory at `memory`, read
/// through the kernel into this thread's read buffer; pages that are not
/// there read as zeros. The bytes are those a read in place would see.
fn with_title<R>(memory: *const u8, size: usize, f: impl FnOnce(&[u8]) -> R) -> R {
    with_read(size, |read| {
        crate::host::copy_mapped(read.as_mut_ptr(), memory, size, false);
        f(read)
    })
}

/// Staging buffers alive (also those only a list still holds) and their
/// bytes.
static STAGED: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

impl Drop for Staging {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        STAGED[0].fetch_sub(1, Relaxed);
        STAGED[1].fetch_sub(self.bytes, Relaxed);
    }
}

/// For the memory line: staging buffers alive, their bytes, and how many
/// of them their texture still has (the others wait for a list's reset).
#[cfg(feature = "diagnostics")]
pub(crate) fn memory() -> [u64; 3] {
    use std::sync::atomic::Ordering::Relaxed;
    let kept = STAGING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(0, |map| map.len() as u64);
    [STAGED[0].load(Relaxed), STAGED[1].load(Relaxed), kept]
}

/// Staging buffers let go as idle since the start, and their bytes.
static LET_GO: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// For the memory line: staging buffers let go as idle, and their bytes.
#[cfg(feature = "diagnostics")]
pub(crate) fn let_go() -> [u64; 2] {
    use std::sync::atomic::Ordering::Relaxed;
    [LET_GO[0].load(Relaxed), LET_GO[1].load(Relaxed)]
}

/// The most staging buffers may hold in system RAM (`VICTORY_STAGING_MB`,
/// default 6 GiB): past it the coldest no list holds are let go even if
/// recently used, so a re-admit still copies from RAM (a 0.5 MB DMA) rather
/// than decoding and detiling title memory again (measured ~87-120 MB/s).
/// Generous by default: the point is a ceiling, not a small cache.
fn staging_ceiling() -> u64 {
    static MB: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MB.get_or_init(|| {
        std::env::var("VICTORY_STAGING_MB").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(6144)
    }) << 20
}

/// The entries to let go to bring `total` bytes under `ceiling` (each
/// `(used, bytes, held)`): coldest first, never one a list holds (`held`),
/// stopping when the rest fit. Pure, so the ceiling is tested without a
/// device.
fn to_ceiling(entries: &[(u64, u64, bool)], total: u64, ceiling: u64) -> Vec<usize> {
    if total <= ceiling {
        return Vec::new();
    }
    let mut order: Vec<usize> = (0..entries.len()).filter(|&i| !entries[i].2).collect();
    order.sort_unstable_by_key(|&i| entries[i].0);
    let mut drop = Vec::new();
    let mut left = total;
    for i in order {
        if left <= ceiling {
            break;
        }
        left = left.saturating_sub(entries[i].1);
        drop.push(i);
    }
    drop
}

/// Lets go of the staging buffers no list recorded a use of for `IDLE_MS`
/// and `IDLE_FRAMES` frames and no list holds: a staging buffer is system memory the size of its
/// texture, kept so that an unchanged texture is not uploaded again, and
/// those of textures no longer sampled were kept for the run (2,246 of
/// them, 2.1 GB, at the end of run 24). The next list that samples such a
/// texture makes a staging buffer anew, detiles and uploads once more.
/// Called before a submission starts, by the thread that submits.
pub(crate) fn let_go_idle() {
    use std::sync::atomic::Ordering::Relaxed;
    const IDLE_FRAMES: u64 = 4;
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = map.as_mut() else {
        return;
    };
    if map.is_empty() {
        return;
    }
    let idle_ms = crate::host::ram_idle_ms(crate::host::available_physical_ram());
    // A list that recorded an upload or a check holds the staging until it
    // is reset: one only the table holds is named by no list, recorded or
    // running.
    let (mut went, mut bytes) = (0u64, 0u64);
    map.retain(|_, s| {
        let keep = Arc::strong_count(s) > 1
            || !crate::host::long_ago(s.used.load(Relaxed), idle_ms, IDLE_FRAMES);
        if !keep {
            LET_GO[0].fetch_add(1, Relaxed);
            LET_GO[1].fetch_add(s.bytes, Relaxed);
            went += 1;
            bytes += s.bytes;
        }
        keep
    });
    // A ceiling on what staging holds in system RAM: past it the coldest
    // buffers no list holds go, even if recently used, so a re-admit still
    // copies from RAM rather than detiling title memory again.
    let ceiling = staging_ceiling();
    let total = STAGED[1].load(Relaxed);
    if total > ceiling {
        let entries: Vec<(u64, u64, bool, usize)> = map
            .iter()
            .map(|(&key, s)| (s.used.load(Relaxed), s.bytes, Arc::strong_count(s) > 1, key))
            .collect();
        let flat: Vec<(u64, u64, bool)> = entries.iter().map(|&(u, b, h, _)| (u, b, h)).collect();
        for i in to_ceiling(&flat, total, ceiling) {
            if let Some(s) = map.remove(&entries[i].3) {
                LET_GO[0].fetch_add(1, Relaxed);
                LET_GO[1].fetch_add(s.bytes, Relaxed);
                went += 1;
                bytes += s.bytes;
            }
        }
    }
    if went > 0 {
        crate::host::trail(|| {
            format!(
                "sweep: {went} staging buffers of textures let go ({} KiB), idle or past the staging ceiling, held by no list",
                bytes >> 10
            )
        });
    }
}

/// Textures the host GPU wrote by a copy or resolve: their contents are on
/// the host, not in title memory, which stays as it was.
static HOST_WRITTEN: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);

/// A host copy or resolve writes `resource`.
pub(crate) fn host_written(resource: This) {
    let new = {
        let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
        set.get_or_insert_with(Default::default).insert(resource as usize)
    };
    // Diagnostics (run 177): where its title memory is, for placed views
    // the title reads that memory through ([`placed_in_host_written`]).
    if new && let Some((at, desc)) = crate::resource::texture_memory(resource) {
        // SAFETY: 56 bytes in the description's layout.
        let words = unsafe { std::ptr::from_ref(&desc).cast::<[u32; 14]>().read_unaligned() };
        let bytes = crate::resource::placed_bytes(&words).unwrap_or(0);
        if !at.is_null() && bytes > 0 {
            HOST_WRITTEN_AT
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(at as u64, (bytes, words[4], words[6], words[7], words[8], resource as usize));
        }
    }
}

/// The title memory of textures the host copied into, by address: bytes
/// and description words 4, 6, 7 and 8 (width, height, layers and levels,
/// format).
static HOST_WRITTEN_AT: Mutex<std::collections::BTreeMap<u64, (u64, u32, u32, u32, u32, usize)>> =
    Mutex::new(std::collections::BTreeMap::new());

/// A texture the title placed at `address` (description words `desc`) was
/// made: says so once if it lies in the memory of a texture the host
/// copied into ([`host_written`]). Its own host copy is filled from title
/// memory, which the copy never reached: the copied texels are on the
/// card in the other texture, where no shader that reads this view finds
/// them.
pub(crate) fn placed_in_host_written(address: u64, desc: &[u32; 14]) {
    let hit = {
        let map = HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner());
        map.range(..=address)
            .next_back()
            .filter(|(at, d)| address < **at + d.0)
            .map(|(at, d)| (*at, *d))
    };
    if let Some((at, (bytes, width, height, layers_levels, format, _))) = hit {
        static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 200 {
            tracing::warn!(
                view_address = format_args!("{address:#x}"),
                view_offset = address - at,
                view_width = desc[4],
                view_height = desc[6],
                view_layers_levels = format_args!("{:#x}", desc[7]),
                view_format = desc[8],
                copied_address = format_args!("{at:#x}"),
                copied_bytes = bytes,
                copied_width = width,
                copied_height = height,
                copied_layers_levels = format_args!("{layers_levels:#x}"),
                copied_format = format,
                "HERO LEAD: a placed texture view lies in the memory of a texture the host copied into: it is filled from title memory, which the copy never reached"
            );
        }
    }
}

/// Whether the host GPU wrote `resource` by a copy or resolve
/// ([`host_written`]): its host copy cannot be made again from title
/// memory.
pub(crate) fn host_wrote(resource: usize) -> bool {
    HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&resource))
}

/// The title released a resource.
pub(crate) fn forget(resource: usize) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&resource);
    }
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = set.as_mut() {
        set.remove(&resource);
    }
    // Its memory may be given to other textures now (run 177: most of the
    // "HERO LEAD" lines were views in memory a released texture had held).
    HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, d| d.5 != resource);
    let mut through = THROUGH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(through) = through.as_mut() {
        through.remove(&resource);
    }
}

/// Whether `resource`'s host copy takes its contents from title memory:
/// a texture the host GPU does not render to or write.
pub(crate) fn title_written(texture: &ID3D12Resource) -> bool {
    const TARGET_DEPTH_UNORDERED: i32 = 0x7;
    // SAFETY: a live resource.
    unsafe { texture.GetDesc() }.Flags.0 & TARGET_DEPTH_UNORDERED == 0
}

/// Records, once per list, the copy of `resource`'s staging buffer into
/// its host copy, and the detiling that refreshes the staging buffer at
/// submission if the memory changed. Leaves the texture in the copy
/// destination state.
pub(crate) fn upload(list: &HostList, resource: This, texture: &ID3D12Resource) {
    let host_written = HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&(resource as usize)));
    // shortcut: a texture the host copied into keeps the host's contents
    // even if the title later writes its memory too.
    if host_written || !list.first_use(resource as usize) {
        return;
    }
    let Some(staging) = staging(resource, texture) else {
        return;
    };
    use std::sync::atomic::Ordering::Relaxed;
    staging.used.store(crate::host::moment(), Relaxed);
    // The host texture has the staging buffer's contents: only the check
    // at submission is left, and what it finds changed the next list that
    // samples the texture uploads (a frame late where every list of the
    // frame was recorded before that check).
    if staging.uploaded.load(Relaxed) && !staging.dirty.load(Relaxed) {
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexSkipped, 1);
        list.defer_texture(staging);
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::TexUploads, 1);
    const COPY_DEST: u32 = 0x400;
    crate::list::ensure_state(list, resource, COPY_DEST);
    // The staging buffer lives as long as `staging`, which the deferred
    // refresh below holds until the list is reset.
    for (i, footprint) in staging.footprints.iter().enumerate() {
        #[cfg(feature = "diagnostics")]
        {
            let f = footprint.Footprint;
            // SAFETY: live resources.
            let (gpu, d) = unsafe { (staging.buffer.GetGPUVirtualAddress(), texture.GetDesc()) };
            let len = u64::from(f.RowPitch) * u64::from(staging.rows[i]) * u64::from(f.Depth);
            list.note_read(
                || {
                    format!(
                        "texture upload {resource:?} ({}x{}x{} f{} mips {}) sub {i} of {} from staging {gpu:#x}+{:#x}: offset {:#x} {}x{}x{} pitch {}",
                        d.Width,
                        d.Height,
                        d.DepthOrArraySize,
                        d.Format.0,
                        d.MipLevels,
                        staging.footprints.len(),
                        staging.size,
                        footprint.Offset,
                        f.Width,
                        f.Height,
                        f.Depth,
                        f.RowPitch
                    )
                },
                (gpu + footprint.Offset, len),
            );
        }
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: i as u32,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&staging.buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *footprint,
            },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    // When the list is submitted, its copy takes what the staging buffer
    // holds by then: noted before the check below, which sets `dirty`
    // again if it detiles (the next list then uploads once more).
    {
        let staging = staging.clone();
        list.defer_host(move || {
            staging.uploaded.store(true, Relaxed);
            staging.dirty.store(false, Relaxed);
        });
    }
    // Once per submission however many of its lists sample the texture.
    list.defer_texture(staging);
}

impl Staging {
    /// The title memory the texture is read from: address and bytes.
    pub(crate) fn memory(&self) -> (usize, usize) {
        (self.memory as usize, self.layout.size as usize)
    }

    /// Detiles the texture memory into the staging buffer if it changed
    /// since the last time (and is mapped at all).
    pub(crate) fn refresh(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::time::Instant::now();
        let read = self.refresh_now();
        let preparing = &crate::host::PREPARING;
        preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
    }

    /// Returns the bytes of title memory read.
    fn refresh_now(&self) -> usize {
        let whole = std::time::Instant::now();
        let read = self.refresh_timed();
        REFRESH_WAITS[3].fetch_add(whole.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
        read
    }

    fn refresh_timed(&self) -> usize {
        let size = self.layout.size as usize;
        let start = self.memory as usize;
        let mut last = waited(0, || self.last.lock().unwrap_or_else(|e| e.into_inner()));
        let last = &mut *last;
        if let Some((now, written)) = waited(1, || crate::watch::writes(start, start + size, false)) {
            if last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked) {
                return 0;
            }
            last.asked = now;
            // Read through the copy, which stands unmapped pages: memory of
            // big pools is too slow to ask page by page.
            with_read(size, |read| {
                crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false);
                self.detile(read);
            });
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexWatched, size as u64);
            return size;
        }
        // View memory has no write-watch, and a texture there was hashed
        // whole at every check: 229 MB and 51 ms a frame in a gameplay view
        // where two textures change (run 58). It is guarded now, as buffers
        // in view memory are: a write of the title's to it faults once and
        // is noted, a mapping is announced by the runtime. While neither
        // happened the texture is as it was. What the guard cannot see (a
        // write to the same pages through another view) is still found by
        // the hash, which is taken every eighth frame only, by the
        // texture's address, so the textures take turns.
        // `VICTORY_TEXTURE_GUARD=0` hashes at every check, as before.
        static GUARD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *GUARD.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_GUARD").is_none_or(|v| v != "0"))
            && let Some((now, written)) = waited(1, || crate::watch::writes(start, start + size, true))
        {
            const HASH_EVERY: u64 = 8;
            let unwritten = last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked);
            last.asked = now;
            let turn = crate::frame::current().wrapping_add((start >> 16) as u64).is_multiple_of(HASH_EVERY);
            if unwritten && !turn {
                return 0;
            }
        }
        // From here on the texture's memory may be read in place. Held until
        // that read is done: the runtime changes what title memory shows
        // only with this taken alone, so what `committed` finds mapped
        // stays mapped (see `crate::watch::title_memory_reading`; without
        // it a page went between the asking and the read in runs 71 and
        // 74).
        let _mapped = if no_wait() {
            // Not waited for: while the runtime remaps title memory the
            // texture is read through the kernel instead, whole or not at
            // all (`no_wait`).
            let Some(mapped) = crate::watch::title_memory_reading_now() else {
                return self.refresh_unlocked(last, size);
            };
            mapped
        } else {
            waited(2, crate::watch::title_memory_reading)
        };
        if !committed(self.memory, size) {
            // Partly mapped: a streamed texture with only some mips' pages
            // there, the usual state while it streams in. Read through the
            // copy, which stands unmapped pages (zeros), and upload what
            // is there; nothing at all mapped changes nothing.
            // shortcut: read again at most every 30 frames (reading around
            // unmapped pages is slow, and nothing tells us when pages
            // arrive): a mip that streams in shows up to 30 frames late.
            let frame = crate::frame::current();
            if last.partial.is_some_and(|at| frame < at + 30) {
                return 0;
            }
            last.partial = Some(frame);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
            let changed = with_read(size, |read| {
                crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false);
                let hash = blake3::hash(read);
                if last.hash == Some(hash) {
                    return false;
                }
                last.hash = Some(hash);
                if read.iter().all(|&b| b == 0) {
                    return false;
                }
                self.detile(read);
                true
            });
            #[cfg(feature = "diagnostics")]
            if changed {
                crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
            }
            let _ = changed;
            return size;
        }
        if through_copy() {
            return with_title(self.memory, size, |tiled| {
                let hash = blake3::hash(tiled);
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
                if last.hash == Some(hash) {
                    return size;
                }
                last.hash = Some(hash);
                self.detile(tiled);
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
                size
            });
        }
        // SAFETY: committed memory of the texture's layout size, and it
        // stays committed while `_mapped` is held.
        let tiled = unsafe { std::slice::from_raw_parts(self.memory, size) };
        let hash = blake3::hash(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
        if last.hash == Some(hash) {
            return size;
        }
        last.hash = Some(hash);
        self.detile(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
        size
    }

    /// A refresh while the runtime is remapping title memory ([`no_wait`]):
    /// the texture is read through the kernel, which fails if a page of it
    /// is not there at this moment. Then nothing is taken and the texture
    /// is looked at again at its next refresh (what was learnt about writes
    /// to it is forgotten, so that it is not passed over as unwritten).
    /// Returns the bytes read.
    fn refresh_unlocked(&self, last: &mut Last, size: usize) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let read = with_read(size, |read| {
            if !crate::host::read_into(self.memory, read) {
                return false;
            }
            let hash = blake3::hash(read);
            if last.hash != Some(hash) {
                last.hash = Some(hash);
                self.detile(read);
            }
            true
        });
        if read {
            UNLOCKED[0].fetch_add(1, Relaxed);
            size
        } else {
            UNLOCKED[1].fetch_add(1, Relaxed);
            last.asked = 0;
            0
        }
    }

    /// Detiles `tiled` (the texture's memory) into the staging buffer.
    fn detile(&self, tiled: &[u8]) {
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        if tiled.iter().all(|&b| b == 0) {
            tracing::debug!(
                memory = ?self.memory,
                desc = ?self.layout.desc,
                "a sampled texture's memory is all zero (not streamed yet, or a failed BCPack read)"
            );
        }
        for (i, (footprint, &rows)) in self.footprints.iter().zip(&self.rows).enumerate() {
            let (mip, slice) = (i as u32 % self.mips, i as u32 / self.mips);
            let f = footprint.Footprint;
            let pitch = f.RowPitch as usize;
            let len = subresource_len(footprint.Offset, pitch, rows, f.Depth, self.bytes);
            // SAFETY: `subresource_len` clamps `len` to `bytes - footprint.Offset`,
            // keeping the slice inside the mapped staging buffer even when
            // `GetCopyableFootprints` does not pad the final row of the final
            // subresource.
            let linear = unsafe {
                std::slice::from_raw_parts_mut(self.data.add(footprint.Offset as usize), len)
            };
            if let Err(e) =
                self.layout
                    .detile(tiled, mip, slice, linear, pitch, pitch * rows as usize)
            {
                tracing::warn!(?e, mip, slice, "texture not detiled");
                return;
            }
        }
    }
}

/// Lets go of a title texture's staging buffer (lists that recorded an
/// upload from it hold it until they have run); the next upload makes one
/// anew.
pub(crate) fn unstage(resource: This) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&(resource as usize));
    }
}

/// Where a staged title texture's memory is: address and bytes.
pub(crate) fn staged_memory(resource: This) -> Option<(u64, u64)> {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let (at, len) = map.as_ref()?.get(&(resource as usize))?.memory();
    Some((at as u64, len as u64))
}

/// A fingerprint of the title memory `[at, at + len)`, never 0: of all of
/// it up to 48 KiB, else of 16 KiB at its start, its middle and its end
/// (a texture streams in whole mips; the small ones lie first). Read
/// through the kernel, so pages not there count as zeros.
pub(crate) fn fingerprint(at: u64, len: u64) -> u64 {
    const WINDOW: usize = 16 << 10;
    thread_local! {
        static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let len = len as usize;
    let mut hasher = blake3::Hasher::new();
    READ.with(|read| {
        let mut read = read.borrow_mut();
        read.resize(3 * WINDOW, 0);
        let mut take = |from: usize, bytes: usize| {
            read[..bytes].fill(0);
            crate::host::copy_mapped(read.as_mut_ptr(), (at as usize + from) as *const u8, bytes, false);
            hasher.update(&read[..bytes]);
        };
        if len <= 3 * WINDOW {
            take(0, len);
        } else {
            take(0, WINDOW);
            take((len / 2) & !0xfff, WINDOW);
            take(len - WINDOW, WINDOW);
        }
    });
    let hash = hasher.finalize();
    let mut first = [0u8; 8];
    first.copy_from_slice(&hash.as_bytes()[..8]);
    u64::from_le_bytes(first) | 1
}

/// Whether a title texture has a staging buffer: a list recorded its
/// upload, and the lists that sample it check it at submission.
pub(crate) fn is_staged(resource: This) -> bool {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref().is_some_and(|map| map.contains_key(&(resource as usize)))
}

/// The bytes of a title texture's memory, and whether all of it is mapped,
/// if the texture has been staged for upload.
pub(crate) fn staged(resource: This) -> Option<(u64, bool)> {
    let s = {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        map.as_ref()?.get(&(resource as usize))?.clone()
    };
    let size = s.layout.size as usize;
    Some((size as u64, committed(s.memory, size)))
}

/// Runs of title memory the kernel has called committed, `(start, end)`,
/// sorted and disjoint by start. A run holds until the runtime maps or
/// unmaps inside it ([`remapped`]). A map, not a list: every deferred copy
/// asks it ([`committed`]) and the title maps memory under texture views a
/// couple of thousand times a second, and a walk of the table grew every
/// cost with its length — at 3,000 runs held, 0.68 µs a question, 7.3 µs a
/// run noted
/// and 0.96 µs an unmap, against 0.06, 0.19 and 0.03 µs in the map
/// (`what_the_run_table_costs`). The `fault` line of
/// `run-20261125-181859.log` watched one second answer 779,370 pages in
/// 62,681 kernel questions: the walk was asked that many times a second.
///
/// The kernel's answer costs the committed run it walks: 0.3-0.7 µs for one
/// 4 KiB page, 1.1-1.8 µs for a 1.2 GB reservation with nothing in it, and
/// 3.0-4.5 ms inside a 1.2 GB committed block (measured here by
/// `what_a_committed_question_costs`). A question asked of every slot of a
/// title texture is therefore asked once per run and answered from this
/// table after: 170 MB of a committed block is one call of 1.1 ms the first
/// time and 0.12 µs a question after, not the 41.7 ms of 43,520 one-byte
/// reads through the kernel that answered it a page at a time, each
/// faulting the page it asked about back in (`run-20261125-151659.log`
/// asked about 478,920 such pages in one 5 s window).
static RUNS: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());

/// The most runs kept: past it the table is emptied rather than grown, and
/// the next questions re-derive it (a run is worth one kernel call).
const MAX_RUNS: usize = 4096;

/// A run shorter than this is not kept: the question costs 0.7 µs a call
/// there, and the title's blocks are megabytes.
///
/// What that cost is worth knowing: one second of `run-20261125-181859.log`
/// asked about 779,370 pages in 62,681 calls, 12.4 pages (50 KiB) a call, so
/// the title's memory is in runs shorter than this and they are asked about
/// again every time.
const SHORT_RUN: usize = 64 << 10;

/// Bytes the staging buffer holds for a subresource at `offset` inside it.
/// `GetCopyableFootprints` does not pad the final row of the final
/// subresource, so `pitch * rows * depth` can exceed `total - offset`; a
/// slice of that length would run past the mapped upload buffer.
fn subresource_len(offset: u64, pitch: usize, rows: u32, depth: u32, total: u64) -> usize {
    let padded = pitch
        .saturating_mul(rows as usize)
        .saturating_mul(depth as usize);
    let remaining = usize::try_from(total.saturating_sub(offset)).unwrap_or(usize::MAX);
    padded.min(remaining)
}

/// Whether `[at, at + len)` is committed memory the process may read
/// (streamed textures may be unmapped until their pages arrive).
pub(crate) fn committed(at: *const u8, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    let start = at as usize;
    committed_bytes(start, start.saturating_add(len)) == len as u64
}

/// How many bytes of `[start, end)` the kernel calls committed and readable.
///
/// One question per run of pages in one state, remembered so the next
/// question about the same run is free. The table is held across the
/// question, so a run the runtime maps or unmaps cannot be entered after
/// the drop that named it ([`remapped`]) — the two take the same lock.
fn committed_bytes(start: usize, end: usize) -> u64 {
    const KERNEL_PAGE: usize = 4096;
    let mut runs = RUNS.lock().unwrap_or_else(|e| e.into_inner());
    let mut there = 0u64;
    let mut there_pages = 0u64;
    let mut at = start;
    let mut calls = 0u64;
    let mut known = 0u64;
    while at < end {
        // The part of the span a run already holds: the one run that could,
        // since the table is disjoint and sorted by start.
        let held = runs.range(..=at).next_back().map(|(_, &end)| end).filter(|&b| at < b);
        if let Some(run_end) = held {
            let stop = run_end.min(end);
            there += (stop - at) as u64;
            there_pages += (stop.saturating_sub(1) / KERNEL_PAGE - at / KERNEL_PAGE + 1) as u64;
            known += (stop - at) as u64;
            at = stop;
            continue;
        }
        // One question: the run of pages in one state that holds `at`.
        // SAFETY: plain data the kernel fills in about our own process.
        let mut info: windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION =
            unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe {
            windows_sys::Win32::System::Memory::VirtualQuery(
                at as *const _,
                &mut info,
                size_of_val(&info),
            )
        };
        calls += 1;
        if n == 0 {
            break;
        }
        let base = info.BaseAddress as usize;
        let stop = base.saturating_add(info.RegionSize);
        if stop <= at {
            break; // the kernel would not advance: no more of the span to ask about
        }
        // Its protection too: a placeholder or a guard is committed memory
        // that may not be read.
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_GUARD, PAGE_NOACCESS};
        let readable = info.State == MEM_COMMIT && info.Protect & (PAGE_NOACCESS | PAGE_GUARD) == 0;
        if readable {
            let stop = stop.min(end);
            there += (stop - at) as u64;
            there_pages += (stop.saturating_sub(1) / KERNEL_PAGE - at / KERNEL_PAGE + 1) as u64;
            note_run(&mut runs, base, base.saturating_add(info.RegionSize));
        }
        at = stop.min(end);
    }
    let pages = (end.saturating_sub(1) / KERNEL_PAGE)
        .saturating_sub(start / KERNEL_PAGE)
        .saturating_add(1) as u64;
    crate::fault::question(pages, pages.saturating_sub(there_pages), calls, known);
    there
}

/// Notes `[base, end)` committed in `runs`, merged with the runs it joins
/// and capped at [`MAX_RUNS`]. A short run is not worth a table entry: its
/// question is 0.7 µs, and one of ours (`crate::fault`'s test buffer, a
/// host copy) is not a title block asked about every frame.
fn note_run(runs: &mut BTreeMap<usize, usize>, base: usize, end: usize) {
    if end.saturating_sub(base) < SHORT_RUN {
        return;
    }
    // A run the table holds already answers it whole.
    if runs.range(..=base).next_back().is_some_and(|(_, &b)| end <= b) {
        return;
    }
    if runs.len() >= MAX_RUNS {
        runs.clear();
    }
    // The runs it joins, touches or swallows are one run: the one holding
    // its first byte, then every one that starts before the merged end.
    let (mut start, mut stop) = (base, end);
    if let Some((&a, &b)) = runs.range(..=base).next_back()
        && b >= base
    {
        start = a;
        stop = stop.max(b);
        runs.remove(&a);
    }
    while let Some((&a, &b)) = runs.range(start..).next() {
        if a > stop {
            break;
        }
        stop = stop.max(b);
        runs.remove(&a);
    }
    runs.insert(start, stop);
}

/// The runtime mapped or unmapped title memory `[at, at + len)`: what the
/// kernel answered about there is no longer true, so no run over it holds.
/// Called from `crate::watch::title_memory_remapped`, which the two take
/// one lock over (see [`committed_bytes`]).
pub(crate) fn remapped(at: usize, len: usize) {
    let end = at.saturating_add(len);
    let mut runs = RUNS.lock().unwrap_or_else(|e| e.into_inner());
    drop_touching(&mut runs, at, end);
}

/// Drops every run `[at, end)` touches: they start before `end` and end
/// after `at`, and the table is disjoint and sorted, so walking back from
/// the last one stops at the first that ends at or before `at`.
fn drop_touching(runs: &mut BTreeMap<usize, usize>, at: usize, end: usize) {
    let mut gone: Vec<usize> = Vec::new();
    for (&a, &b) in runs.range(..end).rev() {
        if b <= at {
            break;
        }
        gone.push(a);
    }
    for a in gone {
        runs.remove(&a);
    }
}

/// How many bytes of `[at, at + len)` are committed memory: all of a
/// texture that is there, some of one streaming in (its smaller mips lie
/// at the start of its memory, and their pages are mapped first), none of
/// one not there. With neither the first nor the last page committed the
/// answer is none without a look at the pages between (two questions for
/// the slots whose texture is away, which are asked again and again).
pub(crate) fn mapped_bytes(at: *const u8, len: usize) -> u64 {
    if len == 0 {
        return 0;
    }
    let start = at as usize;
    let end = start.saturating_add(len);
    if !committed(at, 1) && !committed((end - 1) as *const u8, 1) {
        return 0;
    }
    static BY_PAGE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *BY_PAGE.get_or_init(|| std::env::var_os("VICTORY_MAPPED_BY_PAGE").is_some_and(|v| v == "1")) {
        // By page, cut to the range.
        return committed_bytes(start, end).min(len as u64);
    }
    // By 64 KiB block, one page asked of each: the title's memory is
    // mapped in blocks of that size (the console's page), and the
    // answer is a measure of how much of a texture has arrived, not a
    // promise that every byte can be read (the upload reads through the
    // kernel). By page, a texture half there cost a failed kernel read
    // for each of its missing pages, every frame it stayed due: 1,663
    // textures "uploaded in part" kept the pass over the heap's views at
    // 90-160 ms a frame (run 81). `VICTORY_MAPPED_BY_PAGE=1`: by page.
    const BLOCK: usize = 64 << 10;
    let mut there = 0u64;
    let mut block = start & !(BLOCK - 1);
    while block < end {
        let (lo, hi) = (block.max(start), block.saturating_add(BLOCK).min(end));
        if committed(lo as *const u8, 1) {
            there += (hi - lo) as u64;
        }
        block = block.saturating_add(BLOCK);
    }
    there
}

/// The staging state of a title texture, made on first use.
fn staging(resource: This, texture: &ID3D12Resource) -> Option<Arc<Staging>> {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if let Some(s) = map.get(&(resource as usize)) {
        return Some(s.clone());
    }
    let (memory, desc) = crate::resource::texture_memory(resource)?;
    let Some(layout) = crate::resource::texture_layout(&desc).map(|t| t.planes[0].layout.clone())
    else {
        once!(
            ?desc,
            "texture contents without a console layout not uploaded"
        );
        return None;
    };
    if layout.desc.swizzle.xor() {
        once!(?desc, "texture contents in an XOR mode not uploaded");
        return None;
    }
    let host = host()?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let count = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        u32::from(d.MipLevels)
    } else {
        u32::from(d.MipLevels) * u32::from(d.DepthOrArraySize)
    };
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device.GetCopyableFootprints(
            &d,
            0,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        );
    }
    let (buffer, data) = host.upload_buffer(total)?;
    crate::host::name(&buffer, || {
        format!(
            "staging of {resource:?} {}x{}x{} f{}",
            d.Width, d.Height, d.DepthOrArraySize, d.Format.0
        )
    });
    tracing::debug!(?desc, total, "texture contents uploaded from title memory");
    STAGED[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    STAGED[1].fetch_add(total, std::sync::atomic::Ordering::Relaxed);
    let s = Arc::new(Staging {
        buffer,
        #[cfg(feature = "diagnostics")]
        size: total,
        data,
        footprints,
        rows,
        layout,
        mips: u32::from(d.MipLevels),
        bytes: total,
        memory,
        last: Mutex::default(),
        dirty: std::sync::atomic::AtomicBool::new(true),
        uploaded: std::sync::atomic::AtomicBool::new(false),
        used: std::sync::atomic::AtomicU64::new(crate::host::moment()),
    });
    map.insert(resource as usize, s.clone());
    Some(s)
}

#[cfg(test)]
mod title_memory_tests {
    use super::with_title;

    /// Title memory whose tail is reserved and not committed reads through
    /// the copy as its bytes and then zeros, without a fault; so does
    /// memory a page of which is taken away between two reads, and the
    /// zeros are zeros whatever the read buffer held before.
    #[test]
    fn an_uncommitted_tail_reads_as_zeros() {
        use windows_sys::Win32::System::Memory::*;
        const HEAD: usize = 0xb_0000;
        const SIZE: usize = 0x20_0000;
        // SAFETY: a private reservation of our own; only committed pages
        // are written in place, and nothing else reads them in place.
        unsafe {
            // The read buffer holds something else first.
            let other = vec![0x77u8; SIZE];
            with_title(other.as_ptr(), SIZE, |bytes| assert!(bytes.iter().all(|&b| b == 0x77)));
            let base = VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(VirtualAlloc(base.cast(), HEAD, MEM_COMMIT, PAGE_READWRITE), base.cast());
            base.write_bytes(0x5a, HEAD);
            with_title(base, SIZE, |bytes| {
                assert!(bytes[..HEAD].iter().all(|&b| b == 0x5a), "what is mapped");
                assert!(bytes[HEAD..].iter().all(|&b| b == 0), "what is not: zeros");
            });
            // The title takes a page in the middle away: zeros from then
            // on, the rest as before.
            assert_ne!(VirtualFree(base.add(0x4_0000).cast(), 0x1_0000, MEM_DECOMMIT), 0);
            with_title(base, SIZE, |bytes| {
                assert!(bytes[..0x4_0000].iter().all(|&b| b == 0x5a));
                assert!(bytes[0x4_0000..0x5_0000].iter().all(|&b| b == 0));
                assert!(bytes[0x5_0000..HEAD].iter().all(|&b| b == 0x5a));
                assert!(bytes[HEAD..].iter().all(|&b| b == 0));
            });
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    /// The texture both device-loss runs named last (16 texels wide, 4 or 8
    /// high, format 10, layout 0x100, console flags 0x300000; its memory
    /// began 0x300 into a page in run 73 and 0x700 in run 72, and here also
    /// 0xf00, so that a page line runs through it): what the detiling is
    /// given and what it writes are the same read in place and read through
    /// the copy, and the rows it writes end where the staging buffer the
    /// host sizes for it ends (896 bytes for 4 rows of 128 bytes 256 apart,
    /// 1,920 for 8).
    #[test]
    fn the_small_texture_of_runs_72_and_73_detiles_the_same_both_ways() {
        use windows_sys::Win32::System::Memory::*;
        for (height, within) in [(4u32, 0x300usize), (8, 0x700), (4, 0xf00), (8, 0xf00)] {
            // The description as the log printed it, word for word.
            let words: [u32; 14] = [3, 0, 0, 0, 16, 0, height, 65537, 10, 1, 0, 256, 3_145_728, 0];
            // SAFETY: fourteen words are a resource description (56 bytes,
            // plain numbers).
            let desc: crate::resource::ResourceDesc = unsafe { std::mem::transmute(words) };
            let layout = crate::resource::texture_layout(&desc)
                .expect("a console layout")
                .planes[0]
                .layout
                .clone();
            let size = layout.size as usize;
            let (row, pitch) = (16 * 8, 256usize);
            assert!(size >= row * height as usize, "the layout holds every texel");
            // Committed pages and a reserved one after them; the texture
            // begins `within` bytes into the first, as the title placed it.
            // SAFETY: our own reservation; the committed part is written
            // and read in place, by this thread alone.
            unsafe {
                let pages = size.div_ceil(4096) + 1;
                let base = VirtualAlloc(std::ptr::null(), (pages + 1) * 4096, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
                assert!(!base.is_null());
                assert_eq!(VirtualAlloc(base.cast(), pages * 4096, MEM_COMMIT, PAGE_READWRITE), base.cast());
                for i in 0..pages * 4096 {
                    base.add(i).write((i * 7 + i / 251) as u8);
                }
                let memory = base.add(within);
                let in_place = std::slice::from_raw_parts(memory, size);
                // The bytes: the same in place and through the copy.
                with_title(memory, size, |copied| assert_eq!(copied, in_place));
                // What the detiling writes: the same, and inside the bytes
                // the host's staging buffer for this texture has.
                let staged = (height as usize - 1) * pitch + row;
                assert_eq!(staged, if height == 4 { 896 } else { 1920 });
                let detile = |tiled: &[u8]| {
                    // The slice `Staging::detile` hands over: whole rows.
                    let mut linear = vec![0xeeu8; pitch * height as usize];
                    layout
                        .detile(tiled, 0, 0, &mut linear, pitch, pitch * height as usize)
                        .expect("detiled");
                    linear
                };
                let direct = detile(in_place);
                let through = with_title(memory, size, |copied| detile(copied));
                assert_eq!(direct, through);
                for y in 0..height as usize {
                    assert!(direct[y * pitch..][..row].iter().any(|&b| b != 0xee), "row {y} written");
                    assert!(direct[y * pitch + row..(y + 1) * pitch].iter().all(|&b| b == 0xee), "nothing past row {y}");
                }
                // A buffer of exactly the staged bytes is enough.
                let mut exact = vec![0u8; staged];
                layout
                    .detile(in_place, 0, 0, &mut exact, pitch, pitch * height as usize)
                    .expect("the staging buffer's bytes are enough");
                assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
            }
        }
    }
}

#[cfg(test)]
mod committed_cost_tests {
    use std::time::Instant;

    /// What a committed question costs, measured here: one `VirtualQuery`
    /// per run of pages in one state, the same question from the runs
    /// already known, and the one-byte read through the kernel that
    /// answered it a page at a time before.
    ///
    /// `cargo test -p victory-gpu --lib --locked what_a_committed_question_costs -- --ignored --nocapture`
    ///
    /// The kernel's answer costs the committed run it walks; the one-byte
    /// read cost the page it asked about, and faulted it back in. The `fault`
    /// line counted up to 478,920 of those in one 5 s window
    /// (`run-20261125-151659.log`).
    #[test]
    #[ignore = "allocates 1.1 GB and empties the working set: run by hand"]
    fn what_a_committed_question_costs() {
        use windows_sys::Win32::System::Memory::*;
        use windows_sys::Win32::System::ProcessStatus::K32EmptyWorkingSet;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        const BIG: usize = 0x4600_0000; // 1.1 GB, the title's block
        const SPAN: usize = 170 << 20;
        // SAFETY: our own reservation, released before the test returns.
        let base = unsafe {
            let base = VirtualAlloc(std::ptr::null(), BIG, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(VirtualAlloc(base.cast(), BIG, MEM_COMMIT, PAGE_READWRITE), base.cast());
            base.write_bytes(1, 4096);
            base
        };
        let query = |at: *const u8| {
            // SAFETY: plain data the call fills in.
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a valid out pointer of the right size.
            let n = unsafe { VirtualQuery(at.cast(), &mut info, size_of_val(&info)) };
            (n, info.RegionSize)
        };
        const N: usize = 10_000;
        let started = Instant::now();
        let mut region = 0;
        for i in 0..N {
            region = query(base.wrapping_add((i * 4096) % BIG)).1;
        }
        let per = started.elapsed().as_nanos() as f64 / N as f64 / 1e3;
        println!("VirtualQuery inside a {:.1} GB committed region: {per:.2} us a call, region {region} bytes", BIG as f64 / 1e9);
        // A span of that region: the question once, then from the table.
        let at = base.wrapping_add(BIG / 2);
        super::remapped(at as usize, SPAN);
        let first = Instant::now();
        let there = super::committed_bytes(at as usize, at as usize + SPAN);
        let first = first.elapsed();
        println!("the committed question over 170 MB of a committed block: {:.2} ms, {there} bytes there", first.as_secs_f64() * 1e3);
        let again = Instant::now();
        for _ in 0..100 {
            assert_eq!(super::committed_bytes(at as usize, at as usize + SPAN), there);
        }
        println!("the same question from the table, 100 times: {:.3} ms", again.elapsed().as_secs_f64() * 1e3);
        // The old page-by-page path over the same 170 MB, both cold and hot.
        // SAFETY: our own process; this reads and writes nothing.
        unsafe { K32EmptyWorkingSet(GetCurrentProcess()) };
        let started = Instant::now();
        let mut pages = 0u64;
        let mut byte = [0u8; 1];
        for page in 0..SPAN / 4096 {
            // SAFETY: our own committed range; the kernel reads one byte of it.
            let page_at = unsafe { at.add(page * 4096) };
            if crate::host::read_into(page_at, &mut byte) {
                pages += 1;
            }
        }
        println!(
            "the same range read a page at a time through the kernel: {:.1} ms for {} pages, {pages} of them not zero",
            started.elapsed().as_secs_f64() * 1e3,
            SPAN / 4096
        );
        // One query a region over the whole span.
        let walked = Instant::now();
        let (mut p, mut end) = (at as usize, at as usize + SPAN);
        let mut asked = 0;
        while p < end {
            let (n, size) = query(p as *const u8);
            if n == 0 || size == 0 {
                break;
            }
            asked += 1;
            end = end.min(p + size);
            p += size;
        }
        println!("VirtualQuery over the same 170 MB span: {asked} calls, {:.2} ms", walked.elapsed().as_secs_f64() * 1e3);
        // SAFETY: our own reservation, released whole.
        assert_ne!(unsafe { VirtualFree(base.cast(), 0, MEM_RELEASE) }, 0);
        // The same question where nothing is committed: a placeholder, as
        // the title's streaming views are until their pages arrive.
        // SAFETY: our own reservation.
        let free = unsafe {
            let free = VirtualAlloc(std::ptr::null(), BIG, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!free.is_null());
            free
        };
        let started = Instant::now();
        for i in 0..N {
            query(free.wrapping_add((i * 4096) % BIG));
        }
        println!(
            "VirtualQuery in a {:.1} GB reservation, nothing committed: {:.2} us a call",
            BIG as f64 / 1e9,
            started.elapsed().as_nanos() as f64 / N as f64 / 1e3
        );
        // SAFETY: our own reservation, released whole.
        assert_ne!(unsafe { VirtualFree(free.cast(), 0, MEM_RELEASE) }, 0);
        // A small committed region: is the cost the region or the call?
        // SAFETY: our own allocation.
        let small = unsafe {
            let small = VirtualAlloc(std::ptr::null(), 4096, MEM_COMMIT, PAGE_READWRITE).cast::<u8>();
            assert!(!small.is_null());
            small
        };
        let started = Instant::now();
        for _ in 0..N {
            query(small);
        }
        println!("VirtualQuery in one committed 4 KiB page: {:.3} us a call", started.elapsed().as_nanos() as f64 / N as f64 / 1e3);
        // SAFETY: our own allocation, released whole.
        assert_ne!(unsafe { VirtualFree(small.cast(), 0, MEM_RELEASE) }, 0);
    }
}

#[cfg(test)]
mod committed_tests {
    use super::{committed, remapped};

    /// A run of committed pages is asked about once and answered from the
    /// table after, and a run the runtime maps or unmaps is asked about
    /// again: the answer the kernel gave about there no longer holds.
    ///
    /// This is the whole cost argument for the table: the question is one
    /// `VirtualQuery` per run, and the kernel's answer costs the committed
    /// run it walks (3,048 µs inside a 1.2 GB committed block, 0.7 µs in one
    /// page: `what_a_committed_question_costs`).
    #[test]
    fn a_run_is_asked_about_once_and_a_mapped_range_again() {
        use windows_sys::Win32::System::Memory::*;
        const HEAD: usize = 16 << 20;
        const TAIL: usize = 16 << 20;
        // SAFETY: our own reservation, released before the test returns; the
        // committed part is ours alone.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), HEAD + TAIL, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(VirtualAlloc(base.cast(), HEAD, MEM_COMMIT, PAGE_READWRITE), base.cast());
            base.write_bytes(7, HEAD);
            // What is there, what is not, and a span over the seam of the two.
            assert!(committed(base, HEAD), "the committed head");
            assert!(!committed(base.add(HEAD), TAIL), "the reserved tail");
            assert!(!committed(base.add(HEAD - 4096), 8192), "a span over the seam");
            // The same questions again, at no cost to the kernel.
            let before = crate::fault::kernel_questions();
            for _ in 0..100 {
                assert!(committed(base, HEAD));
                assert!(committed(base.add(HEAD / 2), 4096));
            }
            assert_eq!(crate::fault::kernel_questions(), before, "a run already answered");
            // The tail becomes committed: the question is asked again, and
            // says so.
            assert_eq!(VirtualAlloc(base.add(HEAD).cast(), TAIL, MEM_COMMIT, PAGE_READWRITE), base.add(HEAD).cast());
            remapped(base.add(HEAD) as usize, TAIL);
            assert!(committed(base.add(HEAD), TAIL), "the tail after it was committed");
            assert!(committed(base, HEAD + TAIL), "and the whole of it");
            // What the runtime unmaps is not there any more, whatever the
            // table held.
            assert_ne!(VirtualFree(base.add(HEAD).cast(), TAIL, MEM_DECOMMIT), 0);
            remapped(base.add(HEAD) as usize, TAIL);
            assert!(!committed(base, HEAD + TAIL), "the span the runtime took away");
            assert!(committed(base, HEAD), "the part of it that stayed");
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }
}

#[cfg(test)]
mod staging_ceiling_tests {
    use super::to_ceiling;

    /// Past the ceiling the coldest buffers no list holds go, coldest
    /// first, until the rest fit; a buffer a list holds is never taken,
    /// however cold, and nothing is taken under the ceiling.
    #[test]
    fn the_ceiling_takes_the_coldest_not_held() {
        // `(used, bytes, held)`: the second is the coldest, but a list holds
        // it, so it is passed over.
        let entries = [(10, 100, false), (1, 100, true), (5, 100, false), (7, 100, false)];
        // 400 bytes against a 250 ceiling: the coldest not held (5) and the
        // next (7) leave 200, under the ceiling.
        assert_eq!(to_ceiling(&entries, 400, 250), vec![2, 3]);
        assert!(to_ceiling(&entries, 400, 400).is_empty(), "under the ceiling");
        // A list holds the only buffer: it goes regardless of the ceiling.
        assert!(to_ceiling(&[(1, 400, true)], 400, 0).is_empty(), "held");
    }
}

#[cfg(all(test, windows))]
mod run_table_cost {
    use std::time::Instant;

    /// What the run table charges a question and an insertion, at the sizes
    /// the table reaches in a run: the answer is a `VirtualQuery` once per
    /// run, and every question after is a lookup in this table.
    ///
    /// `cargo test -p victory-gpu --lib --locked what_the_run_table_costs -- --ignored --nocapture`
    ///
    /// The table charges the copy refresh too: `shadow::read_in_place` asks
    /// it once per copy ([`super::committed`]), and `texture::remapped`
    /// walks it once per range the runtime maps or unmaps — 1,454 to 2,549
    /// of those in a window of `run-20261125-181859.log`.
    #[test]
    #[ignore = "a measurement, not a proof: run by hand"]
    fn what_the_run_table_costs() {
        use windows_sys::Win32::System::Memory::*;
        /// Fills the table with `n - 1` runs of 64 KiB apart from our own,
        /// which stands last: the worst place for a search that walks it.
        fn fill(n: usize, at: usize) {
            let mut runs = super::RUNS.lock().unwrap_or_else(|e| e.into_inner());
            runs.clear();
            for i in 0..n - 1 {
                let base = 0x1_0000_0000u64 as usize + i * (1 << 20);
                super::note_run(&mut runs, base, base + (64 << 10));
            }
            let base = at & !((64 << 10) - 1);
            super::note_run(&mut runs, base, base + (64 << 10));
        }
        // A real committed range, so a question about it is answered from
        // the table and never by the kernel.
        // SAFETY: our own reservation, released before the test returns.
        let base = unsafe {
            let base =
                VirtualAlloc(std::ptr::null(), 4 << 20, MEM_COMMIT, PAGE_READWRITE).cast::<u8>();
            assert!(!base.is_null());
            base.write_bytes(1, 4096);
            base
        };
        let at = base.wrapping_add(1 << 20) as usize;
        for n in [256usize, 1000, 3000] {
            fill(n, at);
            const ASKS: usize = 20_000;
            let started = Instant::now();
            for _ in 0..ASKS {
                assert_eq!(super::committed_bytes(at, at + (64 << 10)), 64 << 10);
            }
            let ask = started.elapsed().as_nanos() as f64 / ASKS as f64 / 1e3;
            // Insertions: a run the table does not hold yet, one after
            // another, into the table `fill` left.
            const PUTS: usize = 500;
            let started = Instant::now();
            {
                let mut runs = super::RUNS.lock().unwrap_or_else(|e| e.into_inner());
                for i in 0..PUTS {
                    let fresh = 0x4000_0000_0000usize + i * (1 << 20);
                    super::note_run(&mut runs, fresh, fresh + (64 << 10));
                }
            }
            let put = started.elapsed().as_nanos() as f64 / PUTS as f64 / 1e3;
            // What one mapping of the runtime costs the table.
            const REMAPS: usize = 20_000;
            let started = Instant::now();
            for i in 0..REMAPS {
                super::remapped(at + (i % 16) * 4096, 4096);
            }
            let remap = started.elapsed().as_nanos() as f64 / REMAPS as f64 / 1e3;
            println!(
                "{n} runs held: a question {ask:.3} us, an insertion {put:.3} us, \
                 an unmap {remap:.3} us"
            );
        }
        // SAFETY: our own reservation.
        unsafe { VirtualFree(base.cast(), 0, MEM_RELEASE) };
    }
}

#[cfg(test)]
mod run_table_tests {
    use super::{MAX_RUNS, SHORT_RUN, drop_touching, note_run};
    use std::collections::BTreeMap;

    /// The runs: `(start, end)` of each, in order.
    fn held(runs: &BTreeMap<usize, usize>) -> Vec<(usize, usize)> {
        runs.iter().map(|(&a, &b)| (a, b)).collect()
    }

    /// A run joins the ones it touches and swallows the ones it covers; a
    /// run the table already holds whole is not noted again, and one
    /// shorter than a short run is not noted at all.
    #[test]
    fn the_runs_a_new_one_joins_are_one() {
        const K: usize = SHORT_RUN;
        let mut runs = BTreeMap::new();
        for (a, b) in [(0, K), (2 * K, 4 * K), (8 * K, 9 * K)] {
            note_run(&mut runs, a, b);
        }
        assert_eq!(held(&runs), vec![(0, K), (2 * K, 4 * K), (8 * K, 9 * K)]);
        // A run the table holds whole changes nothing.
        note_run(&mut runs, 0, K);
        note_run(&mut runs, K / 2, K);
        assert_eq!(held(&runs), vec![(0, K), (2 * K, 4 * K), (8 * K, 9 * K)]);
        // One that touches the first and the second at both ends is one run.
        note_run(&mut runs, K, 2 * K);
        assert_eq!(held(&runs), vec![(0, 4 * K), (8 * K, 9 * K)], "touching joins");
        // One between the two, reaching past the end of the second and the
        // start of the third, swallows both whole.
        note_run(&mut runs, 3 * K, 8 * K + K / 2);
        assert_eq!(held(&runs), vec![(0, 9 * K)], "swallows");
        // A short run is not worth a table entry.
        note_run(&mut runs, 16 * K, 16 * K + K / 2);
        assert_eq!(held(&runs), vec![(0, 9 * K)], "short");
        // Past the cap the table is emptied and the new run stands alone.
        let mut full = BTreeMap::new();
        for i in 0..MAX_RUNS {
            note_run(&mut full, i * 4 * K, i * 4 * K + K);
        }
        assert_eq!(full.len(), MAX_RUNS);
        let last = MAX_RUNS * 8 * K;
        note_run(&mut full, last, last + K);
        assert_eq!(held(&full), vec![(last, last + K)], "the cap empties it");
    }

    /// A mapping or unmapping drops every run it touches, and only those.
    #[test]
    fn an_unmap_drops_the_runs_it_touches() {
        const K: usize = SHORT_RUN;
        let mut runs = BTreeMap::new();
        for (a, b) in [(0, 4 * K), (4 * K, 8 * K), (8 * K, 12 * K)] {
            runs.insert(a, b);
        }
        // Inside the first run: it goes whole, the others stay.
        drop_touching(&mut runs, K, 2 * K);
        assert_eq!(held(&runs), vec![(4 * K, 8 * K), (8 * K, 12 * K)]);
        // Touching the join: the run that ends there goes, the one that
        // starts there stays (a range ends before it).
        drop_touching(&mut runs, 8 * K - 1, 8 * K);
        assert_eq!(held(&runs), vec![(8 * K, 12 * K)]);
        drop_touching(&mut runs, 8 * K + K, 12 * K);
        assert_eq!(held(&runs), Vec::new());
        // Nothing to drop: every run ends before it or starts after it.
        let mut runs = BTreeMap::new();
        runs.insert(0, K);
        runs.insert(4 * K, 5 * K);
        drop_touching(&mut runs, K, 4 * K);
        assert_eq!(held(&runs), vec![(0, K), (4 * K, 5 * K)], "the untouched");
        // A run that only ends where the range starts stays.
        drop_touching(&mut runs, K, K + 1);
        assert_eq!(held(&runs), vec![(0, K), (4 * K, 5 * K)], "just outside");
    }
}

#[cfg(test)]
mod subresource_len_tests {
    use super::subresource_len;

    /// A 4x4 RGBA8 texture is `RowPitch` 256, 4 rows, and the last row of
    /// the last subresource is not padded to the pitch: `total` is 784.
    #[test]
    fn a_subresource_never_runs_past_the_staging_buffer() {
        const ROW_PITCH: usize = 256;
        const TOTAL: u64 = 784;
        // The length the code took before the clamp, which is what the slice
        // used to claim: 1024 bytes of a 784-byte mapping.
        let unclamped = ROW_PITCH * 4;
        assert!(
            unclamped as u64 > TOTAL,
            "the unpadded last row makes pitch * rows * depth overrun"
        );
        assert_eq!(subresource_len(0, ROW_PITCH, 4, 1, TOTAL), 784);
        // A subresource further in gets what is left of the buffer.
        assert_eq!(subresource_len(512, ROW_PITCH, 4, 1, TOTAL), 272);
        // Where the buffer is padded, the whole footprint is used.
        assert_eq!(subresource_len(0, ROW_PITCH, 4, 1, 2048), 1024);
        // Absurd totals never overflow into a huge length.
        assert_eq!(subresource_len(0, usize::MAX, u32::MAX, u32::MAX, u64::MAX), usize::MAX);
    }
}
