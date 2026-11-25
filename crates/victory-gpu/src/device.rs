//! The device titles get from device creation, laid out as their inlined
//! code expects: the object header, then device fields. Its table starts
//! like the PC device interface (IUnknown, the object methods, then the
//! device methods in PC order); console additions follow. Methods we have
//! not written log their table offset and fail, so a run shows what to
//! write next.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;

use crate::object::{
    self, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, HRESULT, Iid, Object, S_FALSE, S_OK, Table, This,
    guid, guid_text,
};
use crate::{
    descriptor, dxgi, fence, frame, list, pipeline, query, queue, resource, root, signature,
};

/// What D3D returns when none of the requested feature levels is supported.
const UNSUPPORTED: HRESULT = 0x887A_0004_u32 as i32;

/// The device interface id (the same as on PC). Titles also import it as
/// data.
pub static IID: Iid = guid(
    0x189819f1,
    0x1db6,
    0x4b57,
    [0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7],
);

/// The levels the device answers to, as on PC: `ID3D12Device1` to
/// `ID3D12Device8`, and `ID3D12Device11`, which the title creates
/// its device as. Each level's table is the one before plus its methods.
const LEVEL_IIDS: [Iid; 9] = [
    guid(
        0x77acce80,
        0x638e,
        0x4e65,
        [0x88, 0x95, 0xc1, 0xf2, 0x33, 0x86, 0x86, 0x3e],
    ),
    guid(
        0x30baa41e,
        0xb15b,
        0x475c,
        [0xa0, 0xbb, 0x1a, 0xf5, 0xc5, 0xb6, 0x43, 0x28],
    ),
    guid(
        0x81dadc15,
        0x2bad,
        0x4392,
        [0x93, 0xc5, 0x10, 0x13, 0x45, 0xc4, 0xaa, 0x98],
    ),
    guid(
        0xe865df17,
        0xa9ee,
        0x46f9,
        [0xa4, 0x63, 0x30, 0x98, 0x31, 0x5a, 0xa2, 0xe5],
    ),
    guid(
        0x8b4f173b,
        0x2fea,
        0x4b80,
        [0x8f, 0x58, 0x43, 0x07, 0x19, 0x1a, 0xb9, 0x5d],
    ),
    guid(
        0xc70b221b,
        0x40e4,
        0x4a17,
        [0x89, 0xaf, 0x02, 0x5a, 0x07, 0x27, 0xa6, 0xdc],
    ),
    guid(
        0x5c014b53,
        0x68a1,
        0x4b9b,
        [0x8b, 0xd1, 0xdd, 0x60, 0x46, 0xb9, 0x35, 0x8b],
    ),
    guid(
        0x728f1fa2,
        0xea11,
        0x4a9f,
        [0x9b, 0x52, 0x9b, 0x42, 0xa4, 0x84, 0xd5, 0xc9],
    ),
    guid(
        0x5405c344,
        0xd457,
        0x444e,
        [0xb4, 0xdd, 0x23, 0x66, 0xe4, 0x5a, 0xee, 0x39],
    ),
];

/// Whether `iid` names the device: its first interface or a newer level.
pub(crate) fn is_device(iid: *const Iid) -> bool {
    object::is(iid, &[IID]) || object::is(iid, &LEVEL_IIDS)
}

/// The interface version these layouts were checked against (2.4). Both
/// Minecraft (March 2022) and the title (April 2026) pass it.
pub const CHECKED_VERSION: u32 = 0x0002_0004;

/// Feature level 12_2, what the console hardware supports.
const HIGHEST_FEATURE_LEVEL: u32 = 0xc200;

#[repr(C)]
pub struct Device {
    object: Object,
    /// GPU address of a null constant buffer; none yet.
    null_constant_buffer: u64,
    /// Byte 0 non-zero: calls the title would otherwise inline come to the
    /// table. The rest is reserved.
    out_of_line: [u32; 16],
}
const _: () = assert!(size_of::<Device>() == 0x58);

/// One device per process, as on the console.
pub(crate) static DEVICE: Device = Device {
    object: Object::new(&FUNCTIONS),
    null_constant_buffer: 0,
    out_of_line: [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
};

pub(crate) fn device() -> This {
    (&raw const DEVICE).cast()
}

/// Device creation: `(adapter, parameters, iid, out)`. The parameters start
/// with the interface version; the rest (ring and scratch sizes, features
/// to leave out) does not matter to us yet.
pub extern "system" fn create(
    _adapter: *const c_void,
    parameters: *const u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    if parameters.is_null() || iid.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes its parameters and an id.
    let (version, iid) = unsafe { (parameters.read(), &*iid) };
    tracing::info!(version = format_args!("{version:#x}"), "D3D12.X device");
    if version != CHECKED_VERSION {
        tracing::warn!(
            version = format_args!("{version:#x}"),
            "device layouts were checked against {CHECKED_VERSION:#x} only"
        );
    }
    if !is_device(iid) {
        tracing::warn!(iid = %guid_text(iid), "device created as an unknown interface");
        return E_NOINTERFACE;
    }
    if out.is_null() {
        return S_FALSE;
    }
    if *iid == LEVEL_IIDS[8] {
        NEWER.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    object::answer(object::DEVICE, iid, Some(device()), out)
}

/// Set when the title creates the device as `ID3D12Device11`. Its lists and
/// queues then have the newer layouts (two more methods each; see
/// `list` and `queue`), although the interface version stays 2.4.
/// shortcut: the creation interface is the only sign of the newer layouts
/// we have (the title, April 2026, versus Minecraft, March 2022);
/// look for a better one if a title breaks the rule.
static NEWER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn newer() -> bool {
    NEWER.load(std::sync::atomic::Ordering::Relaxed)
}

/// The device, or its DXGI side.
extern "system" fn query_interface(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = if is_device(iid) {
        Some(this)
    } else if object::is(iid, &dxgi::DEVICE_IIDS) {
        Some(dxgi::device())
    } else {
        None
    };
    object::answer(object::DEVICE, iid, found, out)
}

/// What `D3D12ConfigureVirtualMemoryX` takes and fills in.
#[repr(C)]
pub struct VirtualMemory {
    flags: u32,
    /// GPU page table size in 4 MiB pages.
    page_table_pages: u32,
    /// Memory kept across title processes for the frame buffer.
    preserved_bytes: u32,
    /// Out: the range graphics addresses fall in.
    lowest: u64,
    highest: u64,
}
const _: () = assert!(size_of::<VirtualMemory>() == 0x20);

/// `d3d12_xs!D3D12ConfigureVirtualMemoryX`, called before any graphics
/// allocation. The inputs size console structures we do not have; the
/// range out is all of user space, since GPU addresses are CPU addresses.
pub extern "system" fn configure_virtual_memory(config: *mut VirtualMemory) -> HRESULT {
    // SAFETY: the title passes its configuration (null is tolerated).
    let Some(config) = (unsafe { config.as_mut() }) else {
        return E_INVALIDARG;
    };
    tracing::info!(
        flags = config.flags,
        page_table_pages = config.page_table_pages,
        preserved_bytes = config.preserved_bytes,
        "graphics virtual memory"
    );
    (config.lowest, config.highest) = user_space();
    S_OK
}

fn user_space() -> (u64, u64) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
        // SAFETY: plain data, filled in by the call.
        let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer.
        unsafe { GetSystemInfo(&mut info) };
        (
            info.lpMinimumApplicationAddress as u64,
            info.lpMaximumApplicationAddress as u64,
        )
    }
    #[cfg(not(windows))]
    (0x1_0000, 0x7fff_fffe_ffff)
}

extern "system" fn node_count(_: This) -> u32 {
    1
}

/// Feature query 2: the highest of the requested feature levels we support.
#[repr(C)]
struct FeatureLevels {
    count: u32,
    requested: *const u32,
    highest: u32,
}

/// Feature query 0, what an RDNA 2 GPU reports. The console's layout has
/// one field more than PC's (index 11).
const OPTIONS: [u32; 16] = [
    1,  // double-precision shader operations
    1,  // output-merger logic operations
    2,  // minimum precision: 16-bit
    3,  // tiled resources tier
    3,  // resource binding tier
    1,  // stencil reference from pixel shaders
    1,  // typed UAV loads of more formats
    1,  // rasterizer-ordered views
    3,  // conservative rasterization tier
    40, // GPU address bits per resource
    0,  // 64 KiB standard swizzle
    0,  // console-only field
    0,  // cross-node sharing tier
    0,  // cross-adapter row-major textures
    1,  // viewport and render target index from any stage
    2,  // resource heap tier
];

extern "system" fn check_feature_support(
    _: This,
    feature: u32,
    data: *mut c_void,
    size: u32,
) -> HRESULT {
    const OPTIONS_QUERY: u32 = 0;
    const ARCHITECTURE: u32 = 1;
    const FEATURE_LEVELS: u32 = 2;
    const FORMAT_SUPPORT: u32 = 3;
    const MULTISAMPLE: u32 = 4;
    let expected = match feature {
        OPTIONS_QUERY => size_of_val(&OPTIONS),
        ARCHITECTURE => 16,
        FEATURE_LEVELS => size_of::<FeatureLevels>(),
        FORMAT_SUPPORT => 12,
        MULTISAMPLE => 16,
        _ => {
            tracing::warn!(feature, size, "device feature query not implemented");
            return E_NOTIMPL;
        }
    };
    if data.is_null() || size as usize != expected {
        return E_INVALIDARG;
    }
    match feature {
        OPTIONS_QUERY => {
            // SAFETY: checked size and non-null.
            unsafe { data.cast::<[u32; 16]>().write_unaligned(OPTIONS) };
            S_OK
        }
        ARCHITECTURE => {
            // SAFETY: checked size and non-null.
            let a = unsafe { &mut *data.cast::<[u32; 4]>() };
            if a[0] != 0 {
                return E_INVALIDARG; // one node only
            }
            // Not tile-based; unified, cache-coherent memory.
            a[1..].copy_from_slice(&[0, 1, 1]);
            S_OK
        }
        FORMAT_SUPPORT => {
            // SAFETY: checked size and non-null.
            let q = unsafe { &mut *data.cast::<[u32; 3]>() };
            // shortcut: every use of every format; answer from the host
            // device once there is one.
            q[1] = 0x7fff_ffff;
            q[2] = 0xffff;
            S_OK
        }
        MULTISAMPLE => {
            // SAFETY: checked size and non-null.
            let q = unsafe { &mut *data.cast::<[u32; 4]>() };
            // Format, sample count, flags in; one quality level out for the
            // counts the hardware has.
            q[3] = u32::from(matches!(q[1], 1 | 2 | 4 | 8));
            S_OK
        }
        _ => feature_levels(data),
    }
}

fn feature_levels(data: *mut c_void) -> HRESULT {
    // SAFETY: the caller checked size and non-null.
    let query = unsafe { &mut *data.cast::<FeatureLevels>() };
    if query.requested.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes `count` levels.
    let requested = unsafe { std::slice::from_raw_parts(query.requested, query.count as usize) };
    match requested
        .iter()
        .copied()
        .filter(|&l| l <= HIGHEST_FEATURE_LEVEL)
        .max()
    {
        Some(level) => {
            query.highest = level;
            S_OK
        }
        None => UNSUPPORTED,
    }
}

/// The largest word of a description that could be a size rather than a
/// pointer: shader code and counts are small, and the title's own memory
/// lies far above this.
const SMALL: u64 = 0x1_0000;

/// The size of the PC graphics pipeline description (shaders inline) and of
/// the console compute one: a word of a description equal to either is where
/// that description keeps its own size, so finding neither is what the dump
/// below has to show.
const GRAPHICS_DESCRIPTION: u64 = 0x2b0;
const COMPUTE_DESCRIPTION: u64 = 0x60;

/// `ID3D12PipelineState`, the interface the title asks these two methods for
/// (PC's own GUID): the dump prints the `iid` argument as text, so an
/// argument order we have wrong shows at once.
const PIPELINE_STATE_IID: Iid = guid(
    0x765a30f3,
    0xf624,
    0x4c6f,
    [0xa8, 0x28, 0xac, 0xe9, 0x48, 0x62, 0x24, 0x45],
);

/// Bytes the dump shows behind each argument.
const DUMP_BYTES: usize = 256;

/// Where [`dump_call`]'s text goes: `VICTORY_PIPELINE_DUMP=<file>` in any
/// build (appended, so both slots and later runs stay in one file), the log
/// in a `diagnostics` build, nowhere in a release one.
enum Dump {
    Off,
    Log,
    File(std::path::PathBuf),
}

fn dump_destination() -> &'static Dump {
    static DEST: std::sync::OnceLock<Dump> = std::sync::OnceLock::new();
    DEST.get_or_init(|| {
        if let Some(path) = std::env::var_os("VICTORY_PIPELINE_DUMP").map(std::path::PathBuf::from)
        {
            if outside_repo(&path) {
                return Dump::File(path);
            }
            tracing::warn!(
                path = %path.display(),
                "VICTORY_PIPELINE_DUMP is inside the repository; the dump stays in the log"
            );
        }
        if cfg!(feature = "diagnostics") {
            Dump::Log
        } else {
            Dump::Off
        }
    })
}

/// Whether `path`'s folder is outside the workspace: a dump written into the
/// tree fails the asset policy. This crate has no `victory_core` to ask
/// (`ensure_outside_repo`), and a folder that does not exist is unresolvable,
/// hence refused too.
fn outside_repo(path: &std::path::Path) -> bool {
    let Ok(root) = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
    else {
        return false;
    };
    path.parent()
        .unwrap_or(path)
        .canonicalize()
        .is_ok_and(|folder| !folder.starts_with(root))
}

/// One argument's bytes: a header naming it and its value, then sixteen lines
/// of sixteen bytes, hex and as text where printable. A line the kernel
/// cannot read is `??` and counted at the end instead of aborting, so a
/// pointer to a short block, or to nothing, still dumps what is there.
fn dump_argument(
    index: usize,
    name: &str,
    at: usize,
    read: &mut impl FnMut(usize, &mut [u8]) -> bool,
) -> String {
    let mut text = format!("[{index} {name}] {at:#018x}\n");
    let mut faulted = 0;
    for offset in (0..DUMP_BYTES).step_by(16) {
        let mut bytes = [0u8; 16];
        let (hex, shown) = if read(at + offset, &mut bytes) {
            let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
            let shown: String = bytes
                .iter()
                .map(|&b| {
                    if (0x20..0x7f).contains(&b) {
                        char::from(b)
                    } else {
                        '.'
                    }
                })
                .collect();
            (hex.join(" "), shown)
        } else {
            faulted += 1;
            ("?? ".repeat(15) + "??", "unreadable".to_string())
        };
        text += &format!("+{offset:#05x} | {hex} | {shown}\n");
    }
    if faulted > 0 {
        text += &format!(
            "[{index} {name}] {faulted} of {} lines unreadable\n",
            DUMP_BYTES / 16
        );
    }
    text
}

/// Where the two description sizes appear in the first 128 bytes at `at`: a
/// word equal to 0x2b0 says the console's graphics description sits where the
/// PC one keeps its size, 0x60 the compute one.
fn layout_hint(at: usize, read: &mut impl FnMut(usize, &mut [u8]) -> bool) -> String {
    let mut bytes = [0u8; 128];
    let mut whole = true;
    for (line, chunk) in bytes.chunks_mut(16).enumerate() {
        whole &= read(at + line * 16, chunk);
    }
    let words = |size: u64| -> Vec<usize> {
        bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_le_bytes(*c))
            .enumerate()
            .filter(|&(_, word)| word == size)
            .map(|(word, _)| word)
            .collect()
    };
    let graphics = words(GRAPHICS_DESCRIPTION);
    let compute = words(COMPUTE_DESCRIPTION);
    format!(
        "      sizes: 0x2b0 graphics at {graphics:x?}, 0x60 compute at {compute:x?}{}\n",
        if whole { "" } else { " (partial read)" }
    )
}

/// Every argument of one call, [`DUMP_BYTES`] behind each, and where the two
/// documented description sizes appear. The first call of each slot dumps
/// once, so 18,432 calls cost two dumps.
///
/// The mismatch the dump decides: `desc[0]` points at text (a capture decoded
/// the block as `"2026-11-25 01:17:05Z"`), which no description starts with,
/// so either the arguments are in another order than `(this, desc, iid, out)`
/// or the description starts at a later word; the `iid` text and the size
/// words say which.
fn dump_call(slot: usize, this: This, desc: *const u8, iid: *const Iid, out: *mut This) -> String {
    let mut text = format!("slot {slot:#x}: 4 arguments (this, desc, iid, out)\n");
    #[cfg(windows)]
    let mut read =
        |at: usize, bytes: &mut [u8]| at != 0 && crate::host::read_into(at as *const u8, bytes);
    #[cfg(not(windows))]
    let mut read = |_: usize, _: &mut [u8]| false;
    let args = [
        ("this", this as usize),
        ("desc", desc as usize),
        ("iid", iid as usize),
        ("out", out as usize),
    ];
    for (index, &(name, at)) in args.iter().enumerate() {
        text += &dump_argument(index, name, at, &mut read);
    }
    let mut iid_bytes = [0u8; 16];
    if read(iid as usize, &mut iid_bytes) {
        text += &format!(
            "      iid reads {} (ID3D12PipelineState is {})\n",
            guid_text(&iid_bytes),
            guid_text(&PIPELINE_STATE_IID)
        );
    }
    text += &format!("      desc is {:#018x}:\n", desc as usize);
    if !desc.is_null() {
        text += &layout_hint(desc as usize, &mut read);
        let mut first = [0u8; 8];
        if read(desc as usize, &mut first) {
            let at = usize::try_from(u64::from_le_bytes(first)).unwrap_or(0);
            if at != 0 {
                text += &format!("      desc[0] = {at:#018x}:\n");
                text += &layout_hint(at, &mut read);
            }
        }
    }
    text
}

/// Device methods 0x1c8 and 0x218 (`SLOT`). The title calls both
/// (title+0x2477578 and title+0x24800d5) as `(this, description, interface
/// id, out)`, asks for `ID3D12PipelineState`, and on success keeps the
/// object `out` names and draws with it; on failure it goes without that
/// pipeline, silently.
///
/// The layout of neither description is established: 128 bytes whose first
/// word points at another 128 is neither the PC graphics description
/// (0x2b0, shaders inline) nor the compute one (0x60), and no call so far
/// says which word is the root signature, which is a stage and which a
/// size. So both still fail, with `out` null; the first calls log what
/// writing them needs: the words in hex, every word that reads as a small
/// size, and the block it points at, which tells a byte count from a small
/// pointer (a size beside a pointer is what a shader stage looks like).
/// [`dump_call`] adds the raw bytes of every argument once per slot, and
/// where the two known sizes appear, because 18,432 pipelines are lost here
/// and the argument we assume is a description starts with text.
/// shortcut: not implemented. Answering `S_OK` here without making the
/// pipeline would hand the title whatever its stack held as an object.
extern "system" fn unknown_pipeline<const SLOT: usize>(
    this: This,
    desc: *const u8,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    use std::sync::atomic::{AtomicU32, Ordering};
    static CALLS: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];
    let calls = CALLS[usize::from(SLOT == 0x218)].fetch_add(1, Ordering::Relaxed) + 1;
    if !out.is_null() {
        // SAFETY: the title's out pointer (a local of its own).
        unsafe { *out = std::ptr::null() };
    }
    if calls == 1 {
        let dump = dump_call(SLOT, this, desc, iid, out);
        match dump_destination() {
            Dump::File(path) => {
                use std::io::Write;
                let written = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .and_then(|mut file| file.write_all(dump.as_bytes()));
                if let Err(e) = written {
                    tracing::warn!(
                        path = %path.display(),
                        %e,
                        "VICTORY_PIPELINE_DUMP is not writable"
                    );
                }
            }
            Dump::Log => tracing::warn!(
                slot = format_args!("{:#x}", SLOT),
                "a pipeline creation call's raw arguments:\n{dump}"
            ),
            Dump::Off => {}
        }
    }
    if calls <= 4 || calls.is_power_of_two() {
        // Sixteen 64-bit words at `at`, read through the kernel: neither
        // pointer is known to be mapped.
        #[cfg(windows)]
        let words = |at: usize| -> Option<Vec<u64>> {
            let mut bytes = [0u8; 128];
            (at != 0 && crate::host::read_into(at as *const u8, &mut bytes)).then(|| {
                bytes
                    .as_chunks::<8>().0.iter()
                    .map(|c| c.iter().rev().fold(0u64, |v, &b| v << 8 | u64::from(b)))
                    .collect()
            })
        };
        #[cfg(not(windows))]
        let words = |_: usize| None::<Vec<u64>>;
        let description = words(desc as usize);
        let first = description
            .as_ref()
            .and_then(|w| words(w[0] as usize));
        // A word that reads as a small size — a byte count, a count of
        // words, a stage mask — is the only thing in a block of unknown
        // words that says what the rest is, and the block it points at says
        // whether it is a size of that block or a small pointer. Each one
        // is followed.
        let small: Vec<(usize, u64)> = description
            .iter()
            .flatten()
            .copied()
            .enumerate()
            .filter(|&(_, word)| word != 0 && word <= SMALL)
            .collect();
        let pointed: Vec<String> = small
            .iter()
            .filter_map(|&(at, word)| {
                words(word as usize).map(|block| format!("[{at}] {word:#x} -> {block:x?}"))
            })
            .collect();
        tracing::warn!(
            slot = format_args!("{:#x}", SLOT),
            calls,
            description = format_args!("{description:x?}"),
            first_points_at = format_args!("{first:x?}"),
            small = format_args!("{small:x?}"),
            pointed = format_args!("{pointed:?}"),
            "a pipeline creation method that is not implemented: the title goes without this pipeline"
        );
    }
    E_NOTIMPL
}

/// IUnknown 3, object methods 4, device methods 76 (0x18-0x290), then the
/// methods `ID3D12Device1` to `ID3D12Device8` add, in order: 3, 1, 3, 6, 9,
/// 1, 2, 9 (0x298-0x3a0, the console's own among them), then those of
/// `ID3D12Device9` to `ID3D12Device11`, whose console layout is unknown.
/// Every slot exists so that a call to one we have not written logs it.
static FUNCTIONS: Table<160> = {
    let mut f = object::d3d12_object(object::missing_table::<{ object::DEVICE }, 160>());
    f[0] = query_interface as *const (); // 0x00
    f[7] = node_count as *const (); // 0x38
    f[8] = queue::create as *const (); // 0x40
    f[9] = list::create_allocator as *const (); // 0x48
    f[12] = list::create_list as *const (); // 0x60
    f[10] = pipeline::create_graphics as *const (); // 0x50
    f[11] = pipeline::create_compute as *const (); // 0x58
    f[13] = check_feature_support as *const (); // 0x68
    f[14] = descriptor::create_heap as *const (); // 0x70
    f[15] = descriptor::increment_size as *const (); // 0x78
    f[16] = root::create as *const (); // 0x80
    f[17] = descriptor::create_cbv as *const (); // 0x88
    f[18] = descriptor::create_srv as *const (); // 0x90
    f[19] = descriptor::create_uav as *const (); // 0x98
    f[20] = descriptor::create_rtv as *const (); // 0xa0
    f[21] = descriptor::create_dsv as *const (); // 0xa8
    f[22] = descriptor::create_sampler as *const (); // 0xb0
    f[23] = descriptor::copy as *const (); // 0xb8
    f[24] = descriptor::copy_simple as *const (); // 0xc0
    f[25] = resource::allocation_info as *const (); // 0xc8
    f[26] = resource::custom_heap_properties as *const (); // 0xd0
    f[27] = resource::create_committed as *const (); // 0xd8
    f[28] = resource::create_heap as *const (); // 0xe0
    f[29] = resource::create_placed as *const (); // 0xe8
    f[36] = fence::create as *const (); // 0x120
    f[38] = resource::copyable_footprints as *const (); // 0x130
    f[39] = query::create as *const (); // 0x138
    f[41] = signature::create as *const (); // 0x148
    f[48] = queue::create_x as *const (); // 0x180
    f[57] = unknown_pipeline::<0x1c8> as *const (); // 0x1c8
    f[61] = descriptor::create_sampler_x as *const (); // 0x1e8
    f[62] = pipeline::create_compute_x as *const (); // 0x1f0
    f[67] = unknown_pipeline::<0x218> as *const (); // 0x218
    f[69] = object::ignore as *const (); // 0x228 debug error filter
    f[70] = descriptor::create_placed_srv as *const (); // 0x230
    f[71] = descriptor::create_placed_uav as *const (); // 0x238
    f[73] = list::create_list_x as *const (); // 0x248
    f[74] = resource::register_page_pool as *const (); // 0x250
    f[75] = resource::unregister_page_pool as *const (); // 0x258
    // CreateCommittedResourceX: its description is PC's plus DCC options,
    // which we have no use for (the AMD upscaler's, on Series S).
    f[76] = resource::create_committed as *const (); // 0x260
    f[78] = frame::set_interval as *const (); // 0x270
    f[79] = frame::schedule as *const (); // 0x278
    f[80] = frame::wait as *const (); // 0x280
    f[81] = frame::statistics as *const (); // 0x288
    f[86] = pipeline::create_stream as *const (); // 0x2b0
    f[108] = resource::allocation_info2 as *const (); // 0x360
    f[127] = resource::create_committed3 as *const (); // 0x3f8
    f[133] = resource::create_at_address as *const (); // 0x428
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    fn create_as(iid: &Iid) -> (HRESULT, This) {
        let version = CHECKED_VERSION;
        let mut device = std::ptr::null();
        let hr = create(std::ptr::null(), &version, iid, &mut device);
        (hr, device)
    }

    #[test]
    fn creation_hands_out_the_device() {
        let (hr, device) = create_as(&IID);
        assert_eq!(hr, S_OK);
        assert_eq!(device, super::device());
        // The title tests byte 0x18 before inlining a device call.
        // SAFETY: the device is 0x58 bytes.
        assert_ne!(unsafe { device.cast::<u8>().add(0x18).read() }, 0);
        assert_eq!(create_as(&[0; 16]).0, E_NOINTERFACE);
        assert_eq!(
            slot::<extern "system" fn(This) -> u32>(device, 0x38)(device),
            1
        );
        let missing = slot::<extern "system" fn() -> HRESULT>(device, 0xf0);
        assert_eq!(missing(), E_NOTIMPL);
    }

    #[test]
    fn newer_device_levels_are_the_same_device() {
        // ID3D12Device11, as the title asks for it.
        let (hr, device) = create_as(&LEVEL_IIDS[8]);
        assert_eq!((hr, device), (S_OK, super::device()));
        let mut same = std::ptr::null();
        assert_eq!(query_interface(device, &LEVEL_IIDS[4], &mut same), S_OK);
        assert_eq!(same, device);
        // A Device8 method we have not written fails without crashing.
        let missing = slot::<extern "system" fn() -> HRESULT>(device, 0x3a0);
        assert_eq!(missing(), E_NOTIMPL);
    }

    /// A device child, and the DXGI device, hand out the device as any
    /// interface the device itself answers to.
    #[test]
    fn the_device_is_found_at_every_level_through_others() {
        type Get = extern "system" fn(This, *const Iid, *mut This) -> HRESULT;
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        let mut fence = std::ptr::null();
        assert_eq!(
            crate::fence::create(std::ptr::null(), 0, 0, fence_iid, &mut fence),
            S_OK
        );
        let get_device = slot::<Get>(fence, 0x38);
        let dxgi = dxgi::device();
        let query = slot::<Get>(dxgi, 0);
        for iid in std::iter::once(&IID).chain(&LEVEL_IIDS) {
            let mut found = std::ptr::null();
            assert_eq!(get_device(fence, iid, &mut found), S_OK, "{iid:02x?}");
            assert_eq!(found, device());
            let mut found = std::ptr::null();
            assert_eq!(query(dxgi, iid, &mut found), S_OK, "{iid:02x?}");
            assert_eq!(found, device());
        }
        // What the device is not, it is not through them either.
        let mut found = device();
        assert_eq!(get_device(fence, fence_iid, &mut found), E_NOINTERFACE);
        assert!(found.is_null());
        slot::<extern "system" fn(This) -> u32>(fence, 0x10)(fence);
    }

    /// The host's: a fence's `GetDevice` for `ID3D12Device5`.
    #[cfg(windows)]
    #[test]
    fn the_hosts_children_hand_out_newer_device_levels() {
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_FENCE_FLAG_NONE, ID3D12Device5, ID3D12Fence,
        };
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        // SAFETY: plain calls on the host device and its fence.
        unsafe {
            let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let mut device: Option<ID3D12Device5> = None;
            assert!(fence.GetDevice(&mut device).is_ok());
            assert!(device.is_some());
        }
    }

    #[test]
    fn virtual_memory_configuration_reports_user_space() {
        let mut config = VirtualMemory {
            flags: 0,
            page_table_pages: 7,
            preserved_bytes: 0,
            lowest: 0,
            highest: 0,
        };
        assert_eq!(configure_virtual_memory(&mut config), S_OK);
        assert!(0 < config.lowest && config.lowest < config.highest);
        let heap = Box::new(0u8);
        assert!((config.lowest..=config.highest).contains(&(&raw const *heap as u64)));
        assert_eq!(configure_virtual_memory(std::ptr::null_mut()), E_INVALIDARG);
    }

    #[test]
    fn feature_levels_pick_the_highest_supported() {
        type Check = extern "system" fn(This, u32, *mut c_void, u32) -> HRESULT;
        let device = device();
        let check = slot::<Check>(device, 0x68);
        let query = |levels: &[u32], size| {
            let mut query = FeatureLevels {
                count: levels.len() as u32,
                requested: levels.as_ptr(),
                highest: 0,
            };
            let hr = check(device, 2, (&raw mut query).cast(), size);
            (hr, query.highest)
        };
        let levels = [0xb000, 0xb100, 0xc000, 0xc100, 0xd000];
        assert_eq!(query(&levels, 0x18), (S_OK, 0xc100));
        assert_eq!(query(&levels, 0x10).0, E_INVALIDARG);
        assert_eq!(query(&levels[4..], 0x18).0, UNSUPPORTED);
    }

    /// The dump prints hex, text where printable, and `??` for a line the
    /// kernel could not read, counted rather than aborting the pointer.
    #[test]
    fn the_argument_dump_shows_hex_text_and_marks_unreadable_lines() {
        let block: Vec<u8> = (0..DUMP_BYTES).map(|b| b as u8).collect();
        let mut read = |at: usize, bytes: &mut [u8]| {
            let offset = at - 0x1000;
            if offset == 0x20 {
                return false;
            }
            let len = bytes.len();
            bytes.copy_from_slice(&block[offset..offset + len]);
            true
        };
        let text = dump_argument(2, "iid", 0x1000, &mut read);
        let lines: Vec<&str> = text.lines().collect();
        // A header, sixteen lines of sixteen bytes, and the count of the one
        // that faulted.
        assert_eq!(lines.len(), 18);
        assert_eq!(lines[0], "[2 iid] 0x0000000000001000");
        assert_eq!(
            lines[1],
            "+0x000 | 00 01 02 03 04 05 06 07 08 09 0a 0b 0c 0d 0e 0f | ................"
        );
        assert_eq!(
            lines[3],
            "+0x020 | ?? ?? ?? ?? ?? ?? ?? ?? ?? ?? ?? ?? ?? ?? ?? ?? | unreadable"
        );
        // 0x30 to 0x3f are the digits, 0x40 to 0x4f the capitals.
        assert!(lines[4].ends_with("| 0123456789:;<=>?"));
        assert!(lines[5].ends_with("| @ABCDEFGHIJKLMNO"));
        assert_eq!(lines[17], "[2 iid] 1 of 16 lines unreadable");
        // A null argument dumps every line unreadable and reads nothing.
        let mut none = |_: usize, _: &mut [u8]| false;
        let text = dump_argument(3, "out", 0, &mut none);
        assert_eq!(text.lines().count(), 18);
        assert_eq!(
            text.lines().last(),
            Some("[3 out] 16 of 16 lines unreadable")
        );
    }

    /// The layout line names the offsets of the two documented sizes, in the
    /// description and in the block its first word points at, so a shifted
    /// description or a wrong argument shows up without a hex search.
    #[test]
    fn the_layout_hint_names_the_size_words() {
        let mut block = [0u8; 128];
        block[3 * 8..4 * 8].copy_from_slice(&GRAPHICS_DESCRIPTION.to_le_bytes());
        block[5 * 8..6 * 8].copy_from_slice(&COMPUTE_DESCRIPTION.to_le_bytes());
        let mut read = |at: usize, bytes: &mut [u8]| {
            let offset = at - 0x2000;
            let len = bytes.len();
            bytes.copy_from_slice(&block[offset..offset + len]);
            true
        };
        let hint = layout_hint(0x2000, &mut read);
        assert!(hint.contains("0x2b0 graphics at [3]"), "{hint}");
        assert!(hint.contains("0x60 compute at [5]"), "{hint}");
        assert!(!hint.contains("partial"), "{hint}");
        // A block that cannot be read says so instead of naming offsets.
        let mut none = |_: usize, _: &mut [u8]| false;
        let hint = layout_hint(0x2000, &mut none);
        assert!(hint.contains("graphics at []"), "{hint}");
        assert!(hint.contains("(partial read)"), "{hint}");
    }

    /// A dump inside the tree would fail the asset policy; a folder outside
    /// it is accepted, and one that does not exist is refused.
    #[test]
    fn the_dump_refuses_a_path_inside_the_repository() {
        let inside = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("pipeline.txt");
        assert!(!outside_repo(&inside));
        let temp = tempfile::tempdir().unwrap();
        assert!(outside_repo(&temp.path().join("pipeline.txt")));
        assert!(!outside_repo(
            &temp.path().join("missing").join("pipeline.txt")
        ));
    }

    /// The call dump reads the arguments it is handed: a description whose
    /// word 3 holds the PC size, and the pipeline state's own interface id.
    #[cfg(windows)]
    #[test]
    fn the_call_dump_reads_a_description_and_the_iid() {
        let mut description = [0u64; 16];
        description[3] = GRAPHICS_DESCRIPTION;
        let mut out: This = std::ptr::null_mut();
        let text = dump_call(
            0x1c8,
            std::ptr::null_mut(),
            (&raw const description).cast(),
            std::ptr::from_ref(&PIPELINE_STATE_IID),
            &raw mut out,
        );
        assert!(
            text.starts_with("slot 0x1c8: 4 arguments (this, desc, iid, out)"),
            "{text}"
        );
        assert!(text.contains("sizes: 0x2b0 graphics at [3]"), "{text}");
        assert!(text.contains("ID3D12PipelineState is"), "{text}");
        // The first word is null, so nothing is followed (the bytes behind the
        // description are stack, so only a line of its own counts).
        assert!(
            !text.lines().any(|line| line.starts_with("      desc[0] =")),
            "{text}"
        );
    }
}
