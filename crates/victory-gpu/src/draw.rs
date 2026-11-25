//! Draws on the host GPU (decision 0007, step 3). A list keeps the state
//! the title sets for its draws (pipeline, descriptor heaps, root
//! arguments, vertex and index buffers, targets); each draw then brings the
//! host list up to date: the host pipeline and our root signature, the
//! user data the translated shaders read, the host copies of the buffers
//! and descriptors they read, and the states of the textures involved.
//! State the host keeps across draws (viewports, scissors, blend factor,
//! stencil reference, topology, targets) is set on the host list as the
//! title sets it.

use std::sync::atomic::{AtomicU64, Ordering};

use victory_shader::resource::RootIndexedView;
use victory_shader::{ResourceType, RootType, UsedResource};

use crate::descriptor::View;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::Interface;

use crate::host::HostList;
use crate::object::This;
use crate::pipeline::DropReason;
use crate::pipeline::host::HostPipeline;

/// A root argument as the title set it.
#[derive(Debug, Clone, Default)]
enum Root {
    #[default]
    None,
    /// A descriptor table's first descriptor (an address in a heap).
    Table(u64),
    Constants(Vec<u32>),
    /// A root view's GPU address.
    View(u64),
}

/// The kind of root view a draw sets.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RootKind {
    Cbv,
    Srv,
    Uav,
    /// A one-view table (`State::root_srv_table`, `State::root_uav`): the
    /// same slot's handle, set the same way, draw after draw.
    Table,
}

/// A root argument the list has set.
#[derive(Clone, Copy, PartialEq, Eq)]
struct RootArg {
    /// The root signature's address: another signature at it is another host
    /// signature, as another pipeline at a pipeline's address is another host
    /// pipeline ([`State::bind`]).
    signature: usize,
    kind: RootKind,
    value: u64,
}

/// What a list has set at its root parameters ([`State::set_root_arg`]). What
/// a host list has at a parameter survives every other command on it, so a
/// draw that repeats a value leaves the call out. The values are what the
/// title's descriptors resolve to on this draw, so a descriptor the title
/// rewrote resolves to another value and is set again: the cache never stands
/// in for reading the title's descriptor.
#[derive(Default)]
struct RootArgs {
    /// By (compute, parameter), ascending: a parameter holds one argument, and
    /// a root signature has at most 64.
    set: Vec<(bool, u32, RootArg)>,
}

impl RootArgs {
    /// Whether the call must be made: the parameter holds another value, or
    /// another kind of view, or was set under another root signature.
    fn again(
        &mut self,
        signature: usize,
        compute: bool,
        param: u32,
        kind: RootKind,
        value: u64,
    ) -> bool {
        let arg = RootArg {
            signature,
            kind,
            value,
        };
        match self
            .set
            .binary_search_by(|(space, at, _)| (*space, *at).cmp(&(compute, param)))
        {
            Ok(k) if self.set[k].2 == arg => false,
            Ok(k) => {
                self.set[k].2 = arg;
                true
            }
            Err(k) => {
                self.set.insert(k, (compute, param, arg));
                true
            }
        }
    }

    /// The arguments of one space are undefined there (a root signature of
    /// that space was set): the next draw that asks for one sets it again.
    /// The space's entries stay where they are, so a parameter the next draw
    /// sets again needs no allocation (zero is no live signature, as
    /// [`State::host_roots`] holds it for a list that has none).
    fn forget(&mut self, compute: bool) {
        for (space, _, arg) in &mut self.set {
            if *space == compute {
                arg.signature = 0;
            }
        }
    }

    /// A host list with nothing set (a cut, [`State::replay`]).
    fn clear(&mut self) {
        self.set.clear();
    }
}

/// The 32-bit constants a list holds at each (compute, parameter), under the
/// root signature they were set with, so a draw that needs the same values
/// there leaves the call out: a stage's user data is the same dwords for
/// every draw of a pipeline whose tables were served from the same slots.
/// Cleared wherever [`RootArgs`] is ([`State::forget_roots`]).
#[derive(Default)]
struct RootValues {
    /// By (compute, parameter), ascending; values set under another root
    /// signature are set again (the call is undefined after one).
    set: Vec<(bool, u32, usize, Vec<u32>)>,
}

impl RootValues {
    /// Whether the call must be made: the parameter holds other values, or
    /// was set under another root signature.
    fn again(&mut self, signature: usize, compute: bool, param: u32, values: &[u32]) -> bool {
        match self
            .set
            .binary_search_by(|(space, at, _, _)| (*space, *at).cmp(&(compute, param)))
        {
            Ok(k) => {
                let (_, _, set, held) = &mut self.set[k];
                if *set == signature && held.as_slice() == values {
                    return false;
                }
                *set = signature;
                held.clear();
                held.extend_from_slice(values);
                true
            }
            Err(k) => {
                self.set
                    .insert(k, (compute, param, signature, values.to_vec()));
                true
            }
        }
    }

    /// The constants of one space are undefined there (a root signature of
    /// that space was set): the next draw that asks for them sets them again,
    /// into the buffer it already holds (zero is no live signature, as
    /// [`State::host_roots`] holds it for a list that has none).
    fn forget(&mut self, compute: bool) {
        for (space, _, set, _) in &mut self.set {
            if *space == compute {
                *set = 0;
            }
        }
    }

    fn clear(&mut self) {
        self.set.clear();
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct VertexView {
    address: u64,
    size: u32,
    stride: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct IndexView {
    address: u64,
    size: u32,
    format: u32,
}

/// Whether two views ask the list for the same buffer at the same offset: a
/// draw that binds the one the list holds leaves its call out
/// ([`State::prepare_for`]). Not `PartialEq`: the D3D12 types derive none.
fn same_vertex(a: &D3D12_VERTEX_BUFFER_VIEW, b: &D3D12_VERTEX_BUFFER_VIEW) -> bool {
    a.BufferLocation == b.BufferLocation
        && a.SizeInBytes == b.SizeInBytes
        && a.StrideInBytes == b.StrideInBytes
}

fn same_index(a: &D3D12_INDEX_BUFFER_VIEW, b: &D3D12_INDEX_BUFFER_VIEW) -> bool {
    a.BufferLocation == b.BufferLocation
        && a.SizeInBytes == b.SizeInBytes
        && a.Format.0 == b.Format.0
}

/// Host states of a depth target, and of a texture shaders read.
const DEPTH_WRITE: u32 = 0x10;
const DEPTH_READ: u32 = 0x20;
const SHADER_RESOURCE: u32 = 0x40 | 0x80;

/// Host copies a list keeps for its draw buffers before it drops them all
/// ([`State::copy`]): a list drawing from a range per draw would otherwise
/// hold every window it ever saw alive until its reset.
const COPY_CACHE: usize = 512;

/// A per-list memo of answers from [`crate::shadow`]'s registry of copies,
/// each tagged with the registry's revision when it was made
/// ([`crate::shadow::revision`]); an answer is used only while that
/// revision still holds.
///
/// A whole-cache clear at the bump was not taken: the bump happens on the
/// drawing thread inside a frame loop, and clearing would walk up to
/// [`COPY_CACHE`] entries for answers that must be asked again anyway,
/// leaving the next draw to grow the map back. A tag costs one relaxed load
/// per ask on the hit path and nothing when nothing is asked again.
/// Reasoned, not measured: bumps are rare outside loading, because a copy
/// is made only where the registry has none and a window is let go only
/// after several idle frames; a re-ask is the registry lock, a walk of up
/// to 64 range entries and, where the registry had nothing, a copy of a
/// window (4 MiB).
struct Memo<V> {
    /// The answer for a key, and the registry revision it was made under.
    answers: std::collections::HashMap<(u64, u64), (u64, V)>,
}

/// Not derived: an empty memo holds no `V`, whatever `V` is.
impl<V> Default for Memo<V> {
    fn default() -> Memo<V> {
        Memo {
            answers: std::collections::HashMap::new(),
        }
    }
}

impl<V: Clone> Memo<V> {
    /// The answer for `key` if it was made under revision `now`; an answer
    /// of another revision is left where it is (the next ask of its key
    /// replaces it).
    fn get(&self, key: (u64, u64), now: u64) -> Option<V> {
        match self.answers.get(&key) {
            Some((made, answer)) if *made == now => Some(answer.clone()),
            _ => None,
        }
    }

    /// Remembers `value` as the answer for `key`, made under revision
    /// `now`. At [`COPY_CACHE`] answers the memo starts over: a list that
    /// binds a range per draw would otherwise hold a copy of every window it
    /// ever saw.
    fn put(&mut self, key: (u64, u64), now: u64, value: V) {
        if self.answers.len() >= COPY_CACHE {
            self.answers.clear();
        }
        self.answers.insert(key, (now, value));
    }

    fn clear(&mut self) {
        self.answers.clear();
    }
}

thread_local! {
    /// While a draw is prepared (`State::bind`, then its targets' states):
    /// the depth target the draw only tests (0: none, or one it writes),
    /// and whether a view its shaders read is of that same texture.
    static TESTED_DEPTH: std::cell::Cell<(usize, bool)> =
        const { std::cell::Cell::new((0, false)) };
    /// While a stage's tables are walked (`State::tables`): whether that
    /// stage's shader can store to buffers (`Translated::stores`).
    static STORES: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    /// While `State::early_textures` runs the texture pass: the
    /// milliseconds and the bytes that run may take, in place of the
    /// pass's own per frame.
    static PASS_BUDGET: std::cell::Cell<Option<(u64, u64)>> = const { std::cell::Cell::new(None) };
}

/// The milliseconds of every frame the pass that uploads textures picked
/// by index takes before a shader that picks by index has asked for it
/// (`VICTORY_EARLY_TEXTURES_MS`, 12; 0: never, as before), and the
/// milliseconds it may take just before a draw of the title's decoration
/// bake (`VICTORY_BAKE_TEXTURES_MS`, 1500; 0: nothing special).
///
/// The pass ran only from the first world frame on, where it met some
/// 24,000 slots at once and needed 60-80 frames for them. The title bakes
/// its characters' skin and decorations into textures of its own while it
/// loads, 159-1,165 frames before that (runs 156-174), with a pixel shader
/// that picks its sources by index: whatever was not on the card then was
/// baked as nothing, once, for the run. On the console a texture is there
/// when its bytes are.
fn early_budgets() -> (u64, u64) {
    static MS: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
    *MS.get_or_init(|| {
        let ms = |name: &str, default: u64| {
            std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(default)
        };
        (ms("VICTORY_EARLY_TEXTURES_MS", 12), ms("VICTORY_BAKE_TEXTURES_MS", 1500))
    })
}

/// The pixel shaders of the title's decoration bake (the title:
/// `PS_BakeDecorations_*`), by the first 16 hex digits of their hash as the
/// frame listings print it. They draw into a 256x256 corner of 2048x2048
/// targets and name no texture in a table.
const BAKE_PIXEL_SHADERS: [&str; 4] =
    ["c0a7fe77b1862e10", "0d1575a396dfe513", "b7cfd8dda91ebb6b", "280967ad158cee0b"];

/// Since the start: early runs of the texture pass, and those of them
/// made for a bake draw.
pub(crate) static EARLY_TEXTURE_PASSES: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// Whether a buffer range a shader names through an unordered access view
/// is given to shaders as written (snapshot before the submission, compare
/// and write back after) only if that shader's code can store to buffers.
/// The title binds buffers its shaders only read through such views, and
/// every range declared costs a snapshot and a compare of its bytes: 3.97
/// GB a frame in 31 submissions, 0.49 s of snapshots and 0.28 s of
/// comparing (run 28). On by default; `VICTORY_WRITE_SKIP=0` declares
/// every such range, as before.
fn write_skip() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WRITE_SKIP").is_none_or(|v| v != "0"))
}

/// A run of indirect records clamped in one pass ([`windows`]): the host
/// copy they came from (its number), their kind and stride, the offset of
/// the first in that copy, how many, and the list's scratch buffer that
/// holds them clamped, record `i` at `i * step`.
#[derive(Clone)]
struct Window {
    copy: u64,
    kind: u32,
    stride: u32,
    /// How far apart the records are in the copy, and in the scratch
    /// buffer: what the title walks them by ([`windows`]).
    step: u32,
    base: u64,
    records: u32,
    scratch: ID3D12Resource,
}

/// The most records clamped in one pass ([`windows`]).
const WINDOW_MOST: u64 = 64;

/// Whether the pipeline the list runs next can store to buffer memory
/// (`pipeline::Translated::stores`, true when that is not known), so it may
/// write the indirect arguments a [`Window`] holds clamped.
fn pipeline_stores(pipeline: usize) -> bool {
    crate::pipeline::host_pipeline(pipeline as This).is_none_or(|p| {
        p.stages
            .iter()
            .any(|stage| stage.translated().stores)
    })
}

/// The window a draw asking for `offset` of a copy takes, given the last one
/// (its `base` offset, its `records`, their `step`) when the title still walks
/// the same buffer: the step it walks by and how many records to clamp ahead
/// of the draws that will read them. A walk not recognised takes the title's
/// own `stride` and `max`, one record clamped as without a window. `left` is
/// the bytes from `offset` to the copy's end.
fn ahead(
    last: Option<(u64, u32, u32)>,
    offset: u64,
    left: u64,
    stride: u32,
    max: u32,
) -> (u32, u32) {
    let Some((base, records, was)) = last else {
        return (stride, max);
    };
    let last_at = base + u64::from(records - 1) * u64::from(was);
    let step = offset.saturating_sub(last_at);
    let walks = step >= u64::from(stride)
        && step <= 256
        && step % 4 == 0
        && (records == 1 || step == u64::from(was));
    if !walks {
        return (stride, max);
    }
    let fit = (left / step).min(WINDOW_MOST) as u32;
    let next = if records == 1 { 16 } else { records * 2 };
    (step as u32, next.min(fit).max(1))
}

/// Whether indirect draws that read record after record of one argument
/// buffer are clamped a window at a time (off; `VICTORY_INDIRECT_WINDOW=1`
/// asks for it).
///
/// Off because a window records the draws of a run without drawing them:
/// the world with windows on presented a frame nothing had drawn into
/// (`run-20261125-140122.log`, 4.8 fps but black), and with them off the
/// same build showed the scene (`run-20261125-141510.log`, 4.0 fps).
///
/// The title issues its mesh draws one record per call, each from the next
/// slot of the same argument buffer: 3,460 a frame in gameplay, and before
/// each our clamp ran as a compute dispatch of its own between two waits,
/// so the card went from drawing to compute and back for every record. In
/// the timing run of the lit scene (run 79) a shadow draw of next to
/// nothing cost 14-16 microseconds, 4,126 records a frame; the world scene
/// of 2026-11-25 09:40 lists `our indirect clamps 1.03` beside `indirect
/// runs 1.03` per draw, some 4,300 of each a frame where the frame line
/// reads 4,486 draws and 76.5 ms on the GPU (`run-20261125-094023.log`): a
/// dispatch and two barrier pairs for every record drawn.
///
/// With it, when a draw asks for a record a short step after the
/// one drawn before, the clamp takes that record and the ones that step
/// apart after it (16, then twice as many each time, [`WINDOW_MOST`] at
/// most, never past the buffer) into
/// the list's scratch buffer in one pass, and the draws that follow read
/// their record there with no pass of ours, until one of these: a compute
/// dispatch or a draw whose shaders can store to buffers, an indirect call
/// with a count buffer or more than one record, a record outside the
/// window, another host list. Which of them an ended window reached is
/// counted, and printed beside the commands line (`Windowed`,
/// `windowed_line`): a window that the title's own state alone ends is
/// worth a narrower condition, a window that a real write ends is not.
///
/// Its ordering: the window's bytes reach the host copy at the list's
/// submission, never when the window is made (`Shadow::refresh` defers), so
/// a write the title's CPU side makes while the list is recorded is in it;
/// and every command that can write those bytes on the GPU ends the window,
/// so no draw reads a record a later command of the list has rewritten.
///
/// The step is the one the title is seen to take, not the stride its
/// command signature names: it draws one record a call, for which the
/// stride means nothing, and the title names 12 bytes (the three
/// words of a mesh dispatch) for records it keeps 32 bytes apart (2,173 of
/// a frame's 3,349 mesh records in run 87's listed frame; 283 are 16
/// apart, which ends a window). Walked by the named stride the window
/// never opened: run 87, with the switch, still clamped 5,068 records a
/// frame one at a time.
///
/// What it costs the guard against garbage records: nothing against a
/// hang (every record is bounded as before and charged to the list's
/// budget as before), but the budget is charged when a window is clamped,
/// so also for records of the window's tail that are never drawn. Those
/// hold garbage in this title (slots of all ones: run 93 spent lists'
/// budgets on them and emptied the draws after), so in a window a record
/// that asks for more than can run is left empty and charges nothing
/// (`crate::indirect::bound_ahead`); a tail record that looks real is
/// charged what it asks, a few thousand of 2^27. And arguments the title's CPU side changes are seen as
/// before, at the next submission.
fn windows() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| matches!(std::env::var("VICTORY_INDIRECT_WINDOW").as_deref(), Ok("1") | Ok("on")))
}

/// What a fresh table's records last mixed to, the dynamic slot they
/// were made in, and the registry revision they were made under.
#[derive(Clone, Copy, Default)]
struct Fresh {
    /// [`Fingerprint`] of the `n` records at the table.
    words: Fingerprint,
    /// The first dynamic slot they were made in.
    first: u32,
    /// The list's [`State::table_generation`] when they were.
    generation: u32,
    /// [`crate::shadow::revision`] when they were: the slots hold the host
    /// copies the records' descriptors found then.
    revision: u64,
}

/// The record bytes of a fresh table, mixed to 128 bits: two multiply-xor
/// lanes over the three dwords of every record.
///
/// The key of `State::fresh_tables` is the table's address and slot count;
/// this is what tells the same table with other records from itself. Two
/// different sets of records would have to agree in both lanes to be
/// served the same dynamic slots, 128 bits of mixed content.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Fingerprint(u64, u64);

impl Fingerprint {
    /// The `n` records at `table` (three dwords each, in title memory).
    fn of(table: u64, n: u32) -> Fingerprint {
        // Odd constants: a change in any dword carries into both lanes.
        const ODD: u64 = 0x9E37_79B9_7F4A_7C15;
        const OTHER: u64 = 0xC2B2_AE3D_27D4_EB4F;
        let (mut a, mut b) = (
            0x243F_6A88_85A3_08D3 ^ table,
            0x1319_8A2E_0370_7344 ^ u64::from(n),
        );
        for k in 0..n {
            let at = table + u64::from(k) * 32;
            // SAFETY: a descriptor slot in title memory.
            let [x, y, z] = unsafe { (at as *const [u32; 3]).read_unaligned() };
            let (x, y, z) = (u64::from(x), u64::from(y), u64::from(z));
            a = (a.rotate_left(5) ^ x).wrapping_mul(ODD) ^ y.wrapping_mul(OTHER);
            b = (b.rotate_left(7) ^ z).wrapping_mul(OTHER) ^ x.wrapping_add(y);
        }
        Fingerprint(a ^ b.rotate_left(1), b ^ a.rotate_left(1))
    }
}

/// What one title list has set, and what its host list was last given.
#[derive(Default)]
pub(crate) struct State {
    pipeline: usize,
    /// View heap, sampler heap.
    heaps: [usize; 2],
    graphics: Vec<Root>,
    compute: Vec<Root>,
    vertex: [VertexView; 32],
    index: IndexView,
    /// Bound render target and depth resources, for their host states.
    targets: Vec<usize>,
    depth: usize,
    /// The host state the bound depth view needs ([`depth_state`]).
    depth_state: u32,
    /// The title's primitive topology (the host's may differ).
    topology: u32,
    /// What the title set that goes straight onto the host list, kept for
    /// the host list that continues this one after a cut ([`State::replay`]):
    /// whether a topology was set, the render target and depth handles, the
    /// viewports, the scissors, the blend factor and the stencil reference.
    topology_set: bool,
    target_handles: Vec<u64>,
    depth_handle: u64,
    targets_set: bool,
    viewports: Vec<D3D12_VIEWPORT>,
    scissors: Vec<RECT>,
    blend: Option<[f32; 4]>,
    stencil: Option<u32>,
    depth_bounds: Option<[f32; 2]>,
    /// The indirect records clamped last, if later draws may still read
    /// them ([`windows`]).
    window: Option<Window>,
    /// What the host list has: pipeline, root signature, heaps.
    host_pipeline: usize,
    /// Graphics, compute.
    host_roots: [usize; 2],
    host_heaps: [usize; 2],
    /// The vertex-buffer views the list holds from slot 0 on, and how many
    /// (`u32::MAX`: the list holds views [`State::rectangles`] set, which
    /// no draw of the title's states); and the index buffer the list holds.
    /// A draw that binds the same views leaves both calls out: nothing else
    /// in this module sets them, and a host list keeps them across every
    /// command ([`State::prepare_for`]).
    host_vertex: ([D3D12_VERTEX_BUFFER_VIEW; 32], u32),
    host_index: Option<D3D12_INDEX_BUFFER_VIEW>,
    /// Tables made afresh in this list: (table, slot count) → the record
    /// bytes last seen there, the dynamic slots they were made in and the
    /// registry revision they were made under ([`Fresh`]). The records are
    /// held as a [`Fingerprint`] instead of being hashed into the key:
    /// `DefaultHasher` over `3n` dwords read unaligned from title memory ran
    /// for every fresh table of every draw.
    fresh_tables: std::collections::HashMap<(u64, u32), Fresh>,
    /// Bumped each time a table's records are seen to have changed: the
    /// generation of [`Fresh`], so a table rewritten in place is never
    /// served the slots made for its old records.
    table_generation: u32,
    /// Host copies found for this list's draw buffers: (address, bytes) →
    /// the copy `crate::shadow::find_any` named, kept for the list, with the
    /// registry revision it was found under. Every draw asks for every
    /// buffer it binds, and each answer is a registry lock, up to 64 range
    /// entries of a linear walk, and the chance of making a window.
    copies: Memo<std::sync::Arc<crate::shadow::Shadow>>,
    /// Scratch the draw path reuses rather than allocating per draw: the
    /// slots made afresh, the (slot, offset) pairs they became, the used
    /// resources of one slot, the indexed views it selected, and a stage's
    /// user data.
    scratch_slots: Vec<u8>,
    scratch_made: Vec<(u8, u32)>,
    scratch_entries: Vec<u32>,
    scratch_selected: Vec<u32>,
    scratch_values: Vec<u32>,
    /// The tables the last bound stage that has a part in the stand-in for
    /// the indirect light had made afresh, by root parameter
    /// ([`crate::ambient`]): where its program's own tables start in the
    /// view heap, which the title's own table handle no longer says.
    ambient_tables: Vec<(u8, u32)>,
    /// The dynamic slots the list holds, as runs of the view heap's ring;
    /// given back when the list has run, is reset or released.
    ring: crate::descriptor::host::DynamicList,
    /// What the list last set at each root argument ([`RootArgs`]): a draw
    /// that repeats one leaves the call out.
    root_args: RootArgs,
    /// What it last set at each parameter's 32-bit constants
    /// ([`RootValues`]), likewise.
    root_values: RootValues,
    /// Root SRVs bound as one-view tables in this list: (title heap, title
    /// address; 0 for none) → dynamic slot, and the host copy the slot
    /// shows: later draws of the list bring that one up to date, whatever
    /// copy the address would find by then (`shadow.rs` `tails`).
    root_srv_slots: std::collections::HashMap<(usize, u64), RootSlot>,
    /// Root UAVs bound as one-view tables in this list, likewise.
    root_uav_slots: std::collections::HashMap<(usize, u64), RootSlot>,
    /// The list ran and its dynamic slots were given back.
    tables_gone: bool,
}

impl Drop for State {
    fn drop(&mut self) {
        for (heap, first, n) in self.ring.drain() {
            crate::descriptor::release_dynamic(heap, first, n);
        }
    }
}

impl State {
    /// The list's submission has run (submissions are waited for): the
    /// dynamic slots its tables were made in are given back, not kept
    /// until the title resets the list. A frame of the opening cutscene
    /// asks for 12,500 tables; held until the resets they ran the 131,072
    /// slots out, and up to 4,900 draws a frame went without their tables.
    /// shortcut: a list the title runs a second time without a reset then
    /// reads slots others may have taken (logged once).
    pub(crate) fn tables_done(&mut self) {
        if self.tables_gone && self.ring.is_empty() {
            return;
        }
        if self.tables_gone {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!("a list ran again after its descriptor tables were given back"));
        }
        for (heap, first, n) in self.ring.drain() {
            crate::descriptor::release_dynamic(heap, first, n);
        }
        self.fresh_tables.clear();
        self.copies.clear();
        self.root_srv_slots.clear();
        self.root_uav_slots.clear();
        self.tables_gone = true;
    }

    /// The dynamic slots a fresh table was made in before, if its records
    /// still mix to `words` and the registry of host copies still answers as
    /// it did then: the table's address and slot count name the entry, the
    /// records say whether the entry still stands, and the revision says its
    /// slots still name the copies its descriptors found (neither a remap
    /// nor a window let go makes a descriptor's address name another copy).
    fn fresh_table(&self, identity: (u64, u32), words: Fingerprint, now: u64) -> Option<u32> {
        match self.fresh_tables.get(&identity) {
            Some(seen) if seen.words == words && seen.revision == now => Some(seen.first),
            _ => None,
        }
    }

    /// Records a fresh table's records as made in dynamic slot `first`, at a
    /// generation bumped from the one its old records were made under, so
    /// the slots holding those are never served for these (`now` is the
    /// registry revision they were made under, [`crate::shadow::revision`]).
    fn fresh_made(&mut self, identity: (u64, u32), words: Fingerprint, first: u32, now: u64) {
        let generation = match self.fresh_tables.get(&identity) {
            Some(seen) => seen.generation.wrapping_add(1),
            None => self.table_generation.wrapping_add(1),
        };
        self.table_generation = generation;
        self.fresh_tables.insert(
            identity,
            Fresh {
                words,
                first,
                generation,
                revision: now,
            },
        );
    }
}

/// An experiment on the title's scissor rectangles (`VICTORY_SCISSOR_TEST=full`):
/// they are all widened to the whole target. The title's room at
/// the start of the prologue lost its walls in five runs of seven, and in
/// those runs the player's body and the world outside were cut by one
/// rectangle around the window (run 85, frame 2046): one run with this on
/// says whether the walls are drawn and cut away by such a rectangle, or
/// not drawn at all. Off by default: a rectangle the title sets is part of
/// its picture.
fn scissor_test() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_SCISSOR_TEST").as_deref() == Ok("full"))
}

/// An experiment on mesh draws (`VICTORY_MESH_TEST`): 1 for `skip` (none
/// is issued), 2 for `scissor` (each runs its shaders but rasterises one
/// pixel), 0 otherwise. It tells a card hung by a mesh shader from one hung
/// by what the shader's triangles cover.
fn mesh_test() -> u8 {
    static ON: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("VICTORY_MESH_TEST").as_deref() {
        Ok("skip") => 1,
        Ok("scissor") => 2,
        _ => 0,
    })
}

/// `VICTORY_MESH_TEST=keep:<hex digits>[:<hex digits>]`: only the mesh
/// draws whose mesh shader's hash ends in one of the first digits (and
/// whose digit before it is one of the second, if given) are issued: a
/// search for the shaders that stop the card.
fn mesh_kept(pipeline: usize) -> bool {
    static KEEP: std::sync::OnceLock<Option<(String, String)>> = std::sync::OnceLock::new();
    let Some((last, before)) = KEEP.get_or_init(|| {
        let value = std::env::var("VICTORY_MESH_TEST").ok()?;
        let mut parts = value.strip_prefix("keep:")?.split(':');
        Some((parts.next()?.to_owned(), parts.next().unwrap_or("").to_owned()))
    }) else {
        return true;
    };
    let Some(p) = crate::pipeline::host_pipeline(pipeline as This) else {
        return true;
    };
    let Some(at) = p.name.find("mesh ") else {
        return true;
    };
    let hash = p.name[at + 5..].as_bytes();
    if hash.len() < 16 {
        return true;
    }
    last.as_bytes().contains(&hash[15]) && (before.is_empty() || before.as_bytes().contains(&hash[14]))
}

/// Draws recorded on the host GPU since the last frame line.
static DRAWN: AtomicU64 = AtomicU64::new(0);

/// Draws run on the host so far.
pub(crate) fn drawn() -> u64 {
    DRAWN.load(Ordering::Relaxed)
}

/// The draws traced since the last present (diagnostics builds, with
/// debug logging for this module): logged when the frame comes out black.
#[cfg(feature = "diagnostics")]
static FRAME: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// The draws traced since the last call.
#[cfg(feature = "diagnostics")]
pub(crate) fn take_frame() -> Vec<String> {
    std::mem::take(&mut *FRAME.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Draws, mesh dispatches and dispatches recorded but not run on the host
/// (no host pipeline) since the last call.
pub(crate) fn take_dropped() -> u64 {
    DROPPED.swap(0, Ordering::Relaxed)
}

/// A host-list command the recording path emits, or a call left out
/// because the list already holds that state. Counted in every build with
/// one relaxed add per call, and summed per 5 s by [`commands_line`]: the
/// evidence for what a draw of ours costs on the list, and what the caches
/// spare (a list keeps its pipeline, root signature, heaps and root
/// arguments across every other command on it).
///
/// Barriers are counted where the draw path asks for a texture's state
/// (`crate::list::ensure_state`), which records one only when the state
/// changes; the copies and the twin barriers behind a draw are `host`'s and
/// `shadow`'s, and `copystat` counts those.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    /// `DrawInstanced` and `DrawIndexedInstanced`.
    Draw,
    /// `SetPipelineState`.
    Pipeline,
    /// `SetGraphicsRootSignature` and `SetComputeRootSignature`.
    Signature,
    /// `SetDescriptorHeaps` (and the root signature it makes unset).
    Heaps,
    /// `Set*RootDescriptorTable`: a root SRV or UAV bound as a one-view
    /// table ([`State::root_srv_table`], [`State::root_uav`]).
    RootTable,
    /// `Set*RootConstantBufferView`, `...ShaderResourceView`,
    /// `...UnorderedAccessView`.
    RootView,
    /// `Set*Root32BitConstants`: a stage's user data.
    Root32,
    /// `IASetVertexBuffers`.
    Vertex,
    /// `IASetIndexBuffer`.
    Index,
    /// `OMSetRenderTargets`, from the title's own call.
    Targets,
    /// `IASetPrimitiveTopology`, likewise.
    Topology,
    /// `Dispatch` and `DispatchMesh`, from the title's own calls.
    Dispatch,
    /// `ExecuteIndirect`, from the title's own calls.
    Execute,
    /// The clamp dispatch of ours behind an indirect run
    /// (`crate::indirect::bound`); the root signature, pipeline, constants,
    /// root views and waits it also records are counted as theirs.
    Clamp,
    /// `ResourceBarrier`: from a draw's targets and depth buffer
    /// (`crate::list::ensure_state`), and the clamp's two waits (a UAV
    /// barrier and a state change each) around its dispatch. Recorded only
    /// when a texture moves or a clamp runs.
    Barrier,
}

/// One label per [`Command`], in its order.
const LABELS: [(Command, &str); 15] = [
    (Command::Draw, "draw"),
    (Command::Pipeline, "pipeline"),
    (Command::Signature, "root signature"),
    (Command::Heaps, "heaps"),
    (Command::RootTable, "root tables"),
    (Command::RootView, "root views"),
    (Command::Root32, "root constants"),
    (Command::Vertex, "vertex buffers"),
    (Command::Index, "index buffers"),
    (Command::Targets, "targets"),
    (Command::Topology, "topology"),
    (Command::Dispatch, "dispatches"),
    (Command::Execute, "indirect runs"),
    (Command::Clamp, "our indirect clamps"),
    (Command::Barrier, "barriers"),
];
const COMMANDS: usize = LABELS.len();

/// Calls recorded on the host list, and calls left out, by [`Command`].
static EMITTED: [AtomicU64; COMMANDS] = [const { AtomicU64::new(0) }; COMMANDS];
static SPARED: [AtomicU64; COMMANDS] = [const { AtomicU64::new(0) }; COMMANDS];

/// `n` of `command` were recorded on the host list.
pub(crate) fn emitted(command: Command, n: u64) {
    EMITTED[command as usize].fetch_add(n, Ordering::Relaxed);
}

/// One `command` was not recorded: the list already holds that state.
fn spared(command: Command) {
    SPARED[command as usize].fetch_add(1, Ordering::Relaxed);
}

/// [`spared`] for a cache outside this module (`crate::list`).
pub(crate) fn spared_command(command: Command) {
    spared(command);
}

/// A window's counts, taken once: what it emitted and what it spared.
struct Counts {
    emitted: [u64; COMMANDS],
    spared: [u64; COMMANDS],
}

impl Counts {
    fn take() -> Counts {
        Counts {
            emitted: EMITTED.each_ref().map(|c| c.swap(0, Ordering::Relaxed)),
            spared: SPARED.each_ref().map(|c| c.swap(0, Ordering::Relaxed)),
        }
    }

    /// The line for `draws` draws recorded over `presented` frames. Not
    /// `Display`: nothing else wants this shape, and a test asserts it.
    fn text(&self, draws: u64, presented: u64) -> String {
        let sum = |counts: &[u64; COMMANDS]| counts.iter().sum::<u64>() as f64;
        if draws == 0 {
            return format!(
                "commands per draw: none ({} recorded and {} left out as the list already held \
                 them, over {presented} frames)",
                sum(&self.emitted),
                sum(&self.spared),
            );
        }
        let per = |n: f64| n / draws as f64;
        let listed = |counts: &[u64; COMMANDS], zero: bool| {
            LABELS
                .iter()
                .filter(|(command, _)| zero || counts[*command as usize] != 0)
                .map(|(command, label)| {
                    format!("{label} {:.2}", per(counts[*command as usize] as f64))
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "commands per draw: {:.2} on the host list for {draws} draws in {presented} frames \
             ({}); {:.2} left out as the list already held them ({})",
            per(sum(&self.emitted)),
            listed(&self.emitted, true),
            per(sum(&self.spared)),
            listed(&self.spared, false),
        )
    }
}

/// Logs the commands per draw of the window that just ended: one line, every
/// 5 s, beside the frame's lines and in every build (`crate::queue::present`).
pub(crate) fn commands_line(draws: u64, presented: u64) {
    tracing::info!("{}", Counts::take().text(draws, presented));
    if let Some(line) = windowed_line() {
        tracing::info!("{line}");
    }
    if let Some(line) = crate::indirect::clamps_line() {
        tracing::info!("{line}");
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    crate::gpusplit::line(presented as f64);
    #[cfg(all(windows, feature = "diagnostics"))]
    if let Some(line) = crate::list::census::line(draws) {
        tracing::info!("{line}");
    }
}

/// Why a run of indirect records did or did not read a window of records
/// clamped ahead of it ([`windows`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Windowed {
    /// A run read a window an earlier run clamped: no clamp of its own.
    Hit,
    /// A run clamped the records of a new window.
    Clamp,
    /// A direct draw or dispatch whose pipeline can store to buffer memory
    /// (the other three close it the same way, each its own count).
    Stores,
    /// An indirect run with a count buffer, or more than one record: it
    /// clamps what it reads, so nothing is held ahead for it.
    Count,
    /// A record the window does not cover: another buffer, kind or stride,
    /// or a step off the window's.
    Bounds,
    /// A run whose window is stale: another host list, or this one after a
    /// cut, whose scratch buffer is another or holds the old bytes.
    List,
    /// A run that cannot be windowed: its shaders store, so what it would
    /// hold is not sound; or no window at all (the default:
    /// `VICTORY_INDIRECT_WINDOW=1` asks for one).
    Off,
}

impl Windowed {
    const ALL: [Windowed; 7] = [
        Windowed::Hit,
        Windowed::Clamp,
        Windowed::Stores,
        Windowed::Count,
        Windowed::Bounds,
        Windowed::List,
        Windowed::Off,
    ];

    const LABEL: &'static str = "hits, clamps, ended by stores, count, bounds, list, off";
}

/// Runs that read a window, clamped a new one, or ended one, by [`Windowed`].
static WINDOWED: [AtomicU64; Windowed::ALL.len()] =
    [const { AtomicU64::new(0) }; Windowed::ALL.len()];

fn windowed_note(what: Windowed) {
    let index = Windowed::ALL
        .iter()
        .position(|&w| w == what)
        .unwrap_or_default();
    WINDOWED[index].fetch_add(1, Ordering::Relaxed);
}

/// What the run of indirect records did with its window since the last line,
/// when any touched one: the figure that says whether a window is worth
/// keeping, and which of its ends this title reaches.
fn windowed_line() -> Option<String> {
    let counts: [u64; Windowed::ALL.len()] =
        WINDOWED.each_ref().map(|c| c.swap(0, Ordering::Relaxed));
    let any = counts.iter().sum::<u64>();
    if any == 0 {
        return None;
    }
    let listed = counts
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "indirect windows: {listed} ({}), {any} runs",
        Windowed::LABEL
    ))
}

/// A root SRV's dynamic slot in a list, and the host copy it shows.
type RootSlot = (u32, Option<std::sync::Arc<crate::shadow::Shadow>>);

/// Buffer views of the heap whose slot showed another host buffer than
/// the copy the pass brought up to date for them, since the start
/// (diagnostics; for the memory line). It stays 0 while the pass and the
/// slots agree; with `VICTORY_VIEW_EXTENT=0` it counts the views that were
/// read stale.
#[cfg(feature = "diagnostics")]
pub(crate) static HEAP_VIEWS_ELSEWHERE: AtomicU64 = AtomicU64::new(0);
/// Passes over the view heap's buffer views for shaders that pick them by
/// index ([`State::heap_buffers`]), and the microseconds they took.
pub(crate) static HEAP_BUFFER_PASSES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_BUFFER_MICROS: AtomicU64 = AtomicU64::new(0);
/// Textures brought up to date for shaders that pick them by index
/// ([`State::heap_textures`]), the bytes of their memory, and the slots
/// still waiting.
pub(crate) static HEAP_TEXTURES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURE_BYTES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURES_WAITING: AtomicU64 = AtomicU64::new(0);
/// Passes that made no texture because the card had no room
/// (`crate::resource::room`), for the memory line.
pub(crate) static HEAP_TEXTURES_FULL: AtomicU64 = AtomicU64::new(0);

/// Frames after a whole upload of a texture picked by index at which its
/// memory is looked at again, each counted from the look before.
const RECHECK_FRAMES: [u64; 5] = [4, 12, 40, 120, 400];

/// Slots due at which a pass with a backlog triples its bytes and span
/// ([`pass_budget`]).
const CATCHUP_SLOTS: usize = 1024;

/// Bytes and span a pass of [`State::heap_textures`] gets: three times the
/// plain budget when a backlog is due (`catchup`) **and** the card has room
/// (`room`). A full card never triples: with no room the pass only spends
/// the longer span in the refusing loop, three times as long per pass
/// (250 to 750 ms), while the same head of the queue stays due and nothing
/// is uploaded.
fn pass_budget(
    catchup: bool,
    room: bool,
    due: usize,
    per_frame: u64,
    span: std::time::Duration,
) -> (u64, std::time::Duration) {
    if catchup && room && due >= CATCHUP_SLOTS {
        (per_frame * 3, span * 3)
    } else {
        (per_frame, span)
    }
}

/// What a look at a texture it uploaded whole leaves `heap_textures` to do
/// ([`RECHECK_FRAMES`]): `looks` looks have been made, `print` is the
/// fingerprint the upload recorded and `seen` the one the memory gives now.
struct Looked {
    /// The memory is other than the upload left it: upload it again.
    changed: bool,
    /// The look after this one: when it is due, and how many looks will have
    /// been made then (none when the schedule is over).
    again: Option<(u64, u8)>,
}

/// Whether a look's fingerprints differ **and** some of the memory is still
/// there: only then is the difference content the title put in after the
/// upload. `texture::fingerprint` reads through the kernel, which reads what
/// is not mapped as zeros, so a texture the title streamed out differs at
/// every look with nothing written to it: 233 warnings in
/// `run-20261125-055341.log`, 170 slots, for changes this pass could not tell
/// from those (the slot is taken as gone just below, [`Looked`]).
/// `mapped` is asked only when the bytes differ.
fn look_changed(print: u64, seen: u64, mapped: impl FnOnce() -> bool) -> bool {
    seen != print && mapped()
}

/// The step after one look (`heap_textures`): the look that finds a change
/// counts as one and the schedule goes on from there. Restarting it at
/// `RECHECK_FRAMES[0]` uploaded a texture the title rewrites for ever every
/// fourth frame, for the rest of the run.
fn recheck_step(frame: u64, looks: u8, changed: bool) -> Looked {
    let next = looks.saturating_add(1);
    let again = RECHECK_FRAMES
        .get(usize::from(next))
        .map(|&wait| (frame + wait, next));
    Looked { changed, again }
}

/// Looks at uploaded textures picked by index that found the memory as it
/// was, and that found it changed (uploaded again), since the start.
pub(crate) static HEAP_TEXTURES_RECHECKED: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// Whether textures picked by index are looked at again after their upload
/// (`VICTORY_HEAP_RECHECK=0`: no).
fn heap_recheck() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_RECHECK").is_none_or(|v| v != "0"))
}
/// Times a placed texture's slot was passed over because its memory is not
/// mapped.
pub(crate) static HEAP_TEXTURES_AWAY: AtomicU64 = AtomicU64::new(0);
/// Slots the pass found up to date already: their texture is uploaded by
/// the draws that read it through a table (it has a staging buffer).
pub(crate) static HEAP_TEXTURES_TABLED: AtomicU64 = AtomicU64::new(0);
/// The queue of textures picked by index at the last pass
/// (`TextureQueue::census`): slots in it, due, waiting for their memory,
/// uploaded in part, and the frames the oldest due one has waited.
pub(crate) static HEAP_TEXTURES_QUEUE: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
/// Since the start: looks at a slot uploaded in part that found no more of
/// its memory mapped; uploads of textures not all mapped; and of those,
/// the ones whose first page was not there (asked by the first page alone,
/// as before, they counted as not there at all).
pub(crate) static HEAP_TEXTURES_PARTLY: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Marks the last traced draw as not run on the host.
#[cfg(feature = "diagnostics")]
fn dropped(pipeline: usize) {
    crate::pipeline::note_drop(pipeline as This);
    if let Some(last) = FRAME.lock().unwrap_or_else(|e| e.into_inner()).last_mut() {
        last.push_str(" DROPPED");
    }
}

/// Dispatches run on the host so far.
static DISPATCHED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn dispatched() -> u64 {
    DISPATCHED.load(Ordering::Relaxed)
}

/// The host address of a root view: its buffer's host copy, or for a
/// constant buffer not 256-byte aligned there (the console needs 16) a
/// copy in upload memory.
/// shortcut: a root view has no size; constant buffers are taken as 64 KiB
/// (cut at their buffer's end), other views as all that is left of their
/// buffer. (They were taken as 1 MiB: 1,452 of the 2,021 mesh vertex
/// streams of a cutscene frame reach further, and what lay past the first
/// megabyte was never brought up to date - meshes without bodies, exploded
/// triangles. `VICTORY_ROOT_VIEW_MB` sets a limit again.)
fn root_view(list: &HostList, address: u64, srv: bool) -> Option<u64> {
    static LIMIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let limit = *LIMIT.get_or_init(|| {
        std::env::var("VICTORY_ROOT_VIEW_MB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(u64::MAX, |mb| mb << 20)
    });
    let shadow = crate::shadow::find(address, 1)?;
    let len = shadow
        .left(address)
        .min(if srv { limit } else { 1 << 16 });
    let gpu = shadow.gpu(address);
    if srv || gpu % 256 == 0 {
        shadow.refresh(list, address, len);
        return Some(gpu);
    }
    let (gpu, to) = list.scratch(len)?;
    list.defer_fill(to, address as *const u8, len as usize);
    Some(gpu)
}

/// A fresh descriptor table the ring cannot hold: the slot's draws are
/// dropped, and every draw that asks for that table asks again.
///
/// The line names the table and its slot count, and
/// [`victory_core::logging::warn_once`] says which occurrence it is: a scene
/// that keeps overflowing the ring reads as thousands of dropped draws with
/// their running total, not as one that a `Once` reported and then denied.
pub(crate) fn refused_table_span(slot: u8, n: u32) -> bool {
    victory_core::logging::warn_once("draw.table_span", || {
        format!("fresh descriptor table exceeds the available span; table={slot} slots={n}")
    })
}

/// A draw whose root UAV the title never bound: dropped, and drawn again by
/// the same title every frame.
pub(crate) fn refused_uav_unbound() -> bool {
    victory_core::logging::warn_once("draw.uav_unbound", || {
        "a root UAV with nothing to bind: the draw is dropped".to_owned()
    })
}

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

fn set(roots: &mut Vec<Root>, slot: u32, root: Root) {
    let slot = slot as usize;
    if roots.len() <= slot {
        roots.resize(slot + 1, Root::None);
    }
    roots[slot] = root;
}

impl State {
    pub(crate) fn reset(&mut self, pipeline: This) {
        *self = State::default();
        self.pipeline = pipeline as usize;
    }

    /// The title's topology; console-only ones become host lists (see
    /// [`crate::primitive`]).
    pub(crate) fn set_topology(&mut self, list: &HostList, topology: u32) {
        self.topology = topology;
        self.topology_set = true;
        let host = match topology {
            RECTANGLES => 4,
            TRIANGLE_FAN | LINE_LOOP | QUADS | QUAD_STRIP => {
                once!(topology, "console topology drawn as a triangle list");
                4
            }
            t => t,
        };
        // SAFETY: an open list.
        unsafe {
            list.list
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(host as i32))
        };
        emitted(Command::Topology, 1);
    }

    pub(crate) fn set_pipeline(&mut self, pipeline: This) {
        self.pipeline = pipeline as usize;
    }

    pub(crate) fn set_heaps(&mut self, heaps: &[This]) {
        self.heaps = [0; 2];
        for &h in heaps {
            if !h.is_null() {
                let samplers = crate::descriptor::heap(h).samplers();
                self.heaps[usize::from(samplers)] = h as usize;
            }
        }
    }

    fn roots(&mut self, compute: bool) -> &mut Vec<Root> {
        if compute {
            &mut self.compute
        } else {
            &mut self.graphics
        }
    }

    pub(crate) fn set_table(&mut self, compute: bool, slot: u32, handle: u64) {
        set(self.roots(compute), slot, Root::Table(handle));
    }

    pub(crate) fn set_view(&mut self, compute: bool, slot: u32, address: u64) {
        set(self.roots(compute), slot, Root::View(address));
    }

    pub(crate) fn set_constants(&mut self, compute: bool, slot: u32, values: &[u32], offset: u32) {
        let roots = self.roots(compute);
        let s = slot as usize;
        if !matches!(roots.get(s), Some(Root::Constants(_))) {
            set(roots, slot, Root::Constants(Vec::new()));
        }
        if let Root::Constants(c) = &mut roots[s] {
            let end = offset as usize + values.len();
            if c.len() < end {
                c.resize(end, 0);
            }
            c[offset as usize..end].copy_from_slice(values);
        }
    }

    pub(crate) fn set_vertex_buffers(&mut self, start: u32, views: &[VertexView]) {
        for (i, v) in views.iter().enumerate() {
            if let Some(slot) = self.vertex.get_mut(start as usize + i) {
                *slot = *v;
            }
        }
    }

    pub(crate) fn clear_vertex_buffers(&mut self, start: u32, count: u32) {
        for slot in self.vertex.iter_mut().skip(start as usize).take(count as usize) {
            *slot = VertexView::default();
        }
    }

    pub(crate) fn set_index_buffer(&mut self, view: Option<&IndexView>) {
        self.index = view.copied().unwrap_or_default();
    }

    /// Render targets and depth, set on the host list at once.
    pub(crate) fn set_targets(&mut self, list: &HostList, handles: &[u64], depth: u64) {
        let Some(host) = crate::host::host() else {
            return;
        };
        self.target_handles.clear();
        self.target_handles.extend_from_slice(handles);
        self.depth_handle = depth;
        self.targets_set = true;
        self.targets.clear();
        let mut rtvs = Vec::new();
        for &h in handles {
            let Some(record) = crate::descriptor::rtv_record(h) else {
                once!(
                    handle = format_args!("{h:#x}"),
                    "render target without a recorded view"
                );
                continue;
            };
            let Some(texture) = crate::resource::host_resource(record.resource) else {
                // A typed null view (or a target without a host copy) still
                // takes its slot: left out, the next live target would move
                // into it and the pixel shader's outputs reach the wrong
                // targets.
                // shortcut: a view without a description or a format has no
                // null view to stand for it and is left out, as before.
                let null = record
                    .desc()
                    .filter(|d| d.Format.0 != 0 && d.ViewDimension.0 != 0);
                if let Some(desc) = null {
                    rtvs.push(host.rtv(None, Some(&desc)));
                }
                continue;
            };
            self.targets.push(record.resource as usize);
            #[cfg(feature = "diagnostics")]
            crate::capture::note_target(record.resource);
            rtvs.push(host.rtv(Some(texture), record.desc().as_ref()));
        }
        self.depth = 0;
        self.depth_state = DEPTH_WRITE;
        let dsv = crate::descriptor::dsv_record(depth).and_then(|record| {
            let texture = crate::resource::host_resource(record.resource)?;
            self.depth = record.resource as usize;
            self.depth_state = depth_state(texture, record.depth_desc().as_ref());
            if crate::resource::depth_and_unordered(record.resource) {
                crate::resource::DEPTH_UNORDERED[1].fetch_add(1, Ordering::Relaxed);
            }
            #[cfg(feature = "diagnostics")]
            crate::capture::note_target(record.resource);
            Some(host.dsv(texture, record.depth_desc().as_ref()))
        });
        // SAFETY: an open list; the views were just made.
        unsafe {
            list.list.OMSetRenderTargets(
                rtvs.len() as u32,
                (!rtvs.is_empty()).then_some(rtvs.as_ptr()),
                false,
                dsv.as_ref().map(std::ptr::from_ref),
            )
        };
        emitted(Command::Targets, 1);
    }

    /// Viewports, scissors, the blend factor and the stencil reference: set
    /// on the host list at once, and kept for [`State::replay`].
    pub(crate) fn set_viewports(&mut self, list: &HostList, viewports: &[D3D12_VIEWPORT]) {
        self.viewports.clear();
        self.viewports.extend_from_slice(viewports);
        crate::draw::viewports(list, viewports);
    }

    pub(crate) fn set_scissors(&mut self, list: &HostList, rects: &[RECT]) {
        self.scissors.clear();
        self.scissors.extend_from_slice(rects);
        if scissor_test() {
            // The experiment: every rectangle the title sets is widened to
            // the largest target there is, so nothing is cut by one. What
            // the listing shows (`op_text`) stays the title's own.
            let full = RECT {
                left: 0,
                top: 0,
                right: 16384,
                bottom: 16384,
            };
            crate::draw::scissors(list, &vec![full; rects.len()]);
            return;
        }
        crate::draw::scissors(list, rects);
    }

    pub(crate) fn set_blend_factor(&mut self, list: &HostList, factor: Option<&[f32; 4]>) {
        self.blend = factor.copied();
        crate::draw::blend_factor(list, factor);
    }

    pub(crate) fn set_stencil_ref(&mut self, list: &HostList, reference: u32) {
        self.stencil = Some(reference);
        crate::draw::stencil_ref(list, reference);
    }

    pub(crate) fn set_depth_bounds(&mut self, list: &HostList, min: f32, max: f32) {
        self.depth_bounds = Some([min, max]);
        crate::draw::depth_bounds(list, min, max);
    }

    /// `list` is a new host list that continues the title's list (it was
    /// cut at a wait for a memory value, `crate::list::cut`): a new host
    /// list has nothing set, so what the title set so far is set again.
    /// The pipeline, the root signatures and the heaps are set by the next
    /// draw or dispatch (`bind`, which is told here that the host list has
    /// none); root arguments, root constants, vertex and index buffers are
    /// set by every draw until the list has theirs. The dynamic descriptor
    /// slots and the tables made in them stay: the parts before the cut have
    /// not run yet, and they are given back when the title's list is reset.
    /// shortcut: the render target views are made again from the handles'
    /// descriptors as they are now, not as they were when the title set
    /// the targets (a descriptor rewritten in between names another view).
    pub(crate) fn replay(&mut self, list: &HostList) {
        self.host_pipeline = 0;
        self.host_roots = [0; 2];
        self.host_heaps = [0; 2];
        // The new host list has no root argument, no 32-bit constant and no
        // draw buffer set.
        self.forget_roots();
        self.host_vertex.1 = u32::MAX;
        self.host_index = None;
        if self.topology_set {
            self.set_topology(list, self.topology);
        }
        if self.targets_set {
            let handles = std::mem::take(&mut self.target_handles);
            self.set_targets(list, &handles, self.depth_handle);
        }
        if !self.viewports.is_empty() {
            crate::draw::viewports(list, &self.viewports);
        }
        if !self.scissors.is_empty() {
            let rects = std::mem::take(&mut self.scissors);
            self.set_scissors(list, &rects);
        }
        if let Some(factor) = self.blend {
            crate::draw::blend_factor(list, Some(&factor));
        }
        if let Some(reference) = self.stencil {
            crate::draw::stencil_ref(list, reference);
        }
        if let Some([min, max]) = self.depth_bounds {
            crate::draw::depth_bounds(list, min, max);
        }
    }

    /// The user-data value of a root argument, as translated shaders read
    /// it: a table's byte offset in its heap, a constant's value.
    fn user_value(&self, compute: bool, slot: u8, root: RootType, dword: u32) -> u32 {
        match (self.roots_ref(compute).get(usize::from(slot)), root) {
            (Some(Root::Table(handle)), RootType::Table) => match self.heap_of(*handle) {
                Some(heap) => (handle - heap.start) as u32,
                None => {
                    once!(
                        handle = format_args!("{handle:#x}"),
                        "descriptor table outside the bound heaps"
                    );
                    0
                }
            },
            (Some(Root::Constants(c)), RootType::Constants) => {
                c.get(dword as usize).copied().unwrap_or(0)
            }
            // The host's own ([`Self::host_word`]): no root argument.
            (_, RootType::Other(_)) => 0,
            _ => {
                once!(
                    slot,
                    ?root,
                    "a shader reads a root argument the title did not set"
                );
                0
            }
        }
    }

    /// A user-data dword the host computes for a translated shader
    /// (`victory_shader::emit::USER_MEMORY_BASE`, `USER_VIEW_ADDRESS`).
    fn host_word(
        &self,
        compute: bool,
        kind: u32,
        slot: u8,
        dword: u32,
        memory: Option<&crate::memory::Ready>,
    ) -> u32 {
        use victory_shader::emit::{USER_MEMORY_BASE, USER_VIEW_ADDRESS, USER_VIEW_BIAS};
        if kind == USER_MEMORY_BASE {
            return memory.map_or(0, |m| m.base);
        }
        if kind == USER_VIEW_BIAS {
            // The view begins on 16 bytes of the copy the address finds
            // (`root_uav`, `root_srv_table`).
            // shortcut: found again here, not taken from the list's slot.
            return match self.roots_ref(compute).get(usize::from(slot)) {
                Some(&Root::View(address)) => crate::shadow::find(address, 1).map_or(0, |s| {
                    // SAFETY: a live buffer.
                    let base = unsafe { s.buffer.GetGPUVirtualAddress() };
                    ((s.gpu(address) - base) & 15) as u32
                }),
                _ => 0,
            };
        }
        if kind != USER_VIEW_ADDRESS && kind != USER_VIEW_ADDRESS + 1 {
            once!(kind, "a shader reads a host value this build does not know");
            return 0;
        }
        // The title address of the buffer view at that dword of the table.
        let address = match self.roots_ref(compute).get(usize::from(slot)) {
            Some(&Root::Table(table)) => crate::descriptor::view(table + u64::from(dword) * 4)
                .and_then(|view| crate::descriptor::host::buffer_range(&view))
                .map_or(0, |range| range.0),
            _ => 0,
        };
        if address == 0 {
            once!(slot, dword, "a shader takes the address of a buffer view that is not there: it reads 0");
        }
        if kind == USER_VIEW_ADDRESS {
            address as u32
        } else {
            (address >> 32) as u32
        }
    }

    /// For a shader that reads title memory by address
    /// ([`crate::memory`]): its directory, fault notes and view slots,
    /// with the pages of the root views the list has set made ready.
    fn memory_ready(&self, list: &HostList, compute: bool, writes: bool) -> Option<crate::memory::Ready> {
        let heap = match self.heaps[0] {
            0 => None,
            h => crate::descriptor::heap(h as This).host(),
        }?;
        let (mut addresses, mut constants) = (Vec::new(), Vec::new());
        for root in self.roots_ref(compute) {
            match root {
                Root::View(address) => addresses.push(*address),
                Root::Constants(values) => constants.extend_from_slice(values),
                _ => {}
            }
        }
        crate::memory::ready(list, heap, &addresses, &constants, writes)
    }

    /// The bound heap holding `handle`.
    fn heap_of(&self, handle: u64) -> Option<&'static crate::descriptor::Heap> {
        self.heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| h.index(handle).is_some())
    }

    /// Brings the host list up to date for a draw; false if the draw cannot
    /// run on the host.
    fn prepare(&mut self, list: &HostList, indexed: bool) -> bool {
        self.prepare_for(list, indexed, false)
    }

    /// The host copy of a draw's vertex or index buffer: what
    /// [`crate::shadow::find_any`] names, kept for the list. Every draw
    /// asks for every buffer it binds, and each answer costs the registry's
    /// lock and a walk back over up to 64 range entries; a list that draws
    /// the same buffer five thousand times asked as often.
    ///
    /// The answer is only about which host copy covers
    /// `[address, address + len)`, and the copy is brought up to date from
    /// title memory at submission either way, so a title rewrite of the
    /// buffer needs no invalidation. What does is the registry answering
    /// otherwise: title memory remapped while the list records
    /// (`crate::watch::title_memory_remapped`), a buffer registered or
    /// released, a window made or let go. Each answer carries the registry
    /// revision it was made under ([`crate::shadow::revision`]) and is used
    /// only while that holds, so the same address never names the copy made
    /// for what was there before ([`Memo`]).
    fn copy(&mut self, address: u64, len: u64) -> Option<std::sync::Arc<crate::shadow::Shadow>> {
        if address == 0 || len == 0 {
            return None;
        }
        let key = (address, len);
        if let Some(found) = self.copies.get(key, crate::shadow::revision()) {
            return Some(found);
        }
        let shadow = crate::shadow::find_any(address, len)?;
        // Under the revision the ask left the registry in, not the one
        // before it: an ask that made a copy or a window bumped it, and an
        // answer tagged with the old number would be stale at once and
        // asked again by every draw of the list.
        self.copies.put(key, crate::shadow::revision(), shadow.clone());
        Some(shadow)
    }

    /// As [`State::prepare`], for draws (`mesh` false) or mesh dispatches.
    fn prepare_for(&mut self, list: &HostList, indexed: bool, mesh: bool) -> bool {
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            return false;
        }
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return false;
        };
        if p.compute || p.mesh != mesh {
            once!(mesh, "a draw or mesh dispatch with another kind of pipeline");
            return false;
        }
        if p.rects && self.topology != RECTANGLES {
            // Its geometry stage would turn each triangle into a rectangle.
            once!("a rectangle pipeline drawn with another topology");
        }
        if !self.bind(list, p, false) {
            return false;
        }
        let l = &list.list;
        // Vertex buffers the input layout reads.
        let mut views = [D3D12_VERTEX_BUFFER_VIEW::default(); 32];
        let mut count = 0;
        // Copied out: the copies below borrow the list's state.
        let vertex = self.vertex;
        for (slot, (view, &v)) in views.iter_mut().zip(&vertex).enumerate() {
            if p.slots & 1 << slot == 0 {
                continue;
            }
            count = slot + 1;
            if v.address == 0 || v.size == 0 {
                continue;
            }
            let Some(shadow) = self.copy(v.address, u64::from(v.size)) else {
                continue;
            };
            shadow.refresh(list, v.address, u64::from(v.size));
            *view = D3D12_VERTEX_BUFFER_VIEW {
                BufferLocation: shadow.gpu(v.address),
                SizeInBytes: v.size,
                StrideInBytes: v.stride,
            };
        }
        // The views the list holds: a draw that binds the same ones leaves
        // both calls out (nothing else in this module sets them, and they
        // survive every other command on a list).
        if count > 0 && (self.host_vertex.1 != count as u32 || {
            let held = &self.host_vertex.0[..count];
            !views[..count]
                .iter()
                .zip(held)
                .all(|(a, b)| same_vertex(a, b))
        }) {
            // SAFETY: an open list; the views name live host buffers.
            unsafe { l.IASetVertexBuffers(0, Some(&views[..count])) };
            emitted(Command::Vertex, 1);
            self.host_vertex = (views, count as u32);
        } else if count > 0 {
            spared(Command::Vertex);
        }
        if indexed {
            let v = self.index;
            let Some(shadow) = self.copy(v.address, u64::from(v.size)) else {
                return false;
            };
            shadow.refresh(list, v.address, u64::from(v.size));
            let view = D3D12_INDEX_BUFFER_VIEW {
                BufferLocation: shadow.gpu(v.address),
                SizeInBytes: v.size,
                Format: DXGI_FORMAT(v.format as i32),
            };
            if !self.host_index.is_some_and(|held| same_index(&held, &view)) {
                // SAFETY: an open list; the view names a live host buffer.
                unsafe { l.IASetIndexBuffer(Some(&view)) };
                emitted(Command::Index, 1);
                self.host_index = Some(view);
            } else {
                spared(Command::Index);
            }
        }
        const RENDER_TARGET: u32 = 0x4;
        for &t in &self.targets {
            crate::list::ensure_state(list, t as This, RENDER_TARGET);
        }
        // A draw that runs renders into these, which is what a present reads as
        // "the frame was written" (`crate::present`): binding a target this
        // draw never reaches is not a write.
        for &t in &self.targets {
            crate::present::note_target(t as This);
        }
        if self.depth != 0 {
            crate::present::note_target(self.depth as This);
            // A depth texture shaders also write: what they wrote, first.
            list.twin_use(self.depth as This, false, true);
            // A view that writes neither plane keeps the texture readable:
            // the depth read state, together with the shader read states
            // if this draw's shaders read the same texture (the write state
            // put it out of the state those views need, and a move to it
            // and back for every such draw).
            let state = if self.depth_state == DEPTH_READ {
                let (tested, read) = TESTED_DEPTH.get();
                if read && tested == self.depth {
                    DEPTH_READ | SHADER_RESOURCE
                } else {
                    DEPTH_READ
                }
            } else {
                DEPTH_WRITE
            };
            crate::list::ensure_state(list, self.depth as This, state);
        }
        true
    }

    /// A descriptor a shader reads through a table: its host slot made if
    /// it changed, constant data copied, textures made readable.
    /// What draws and dispatches share: heaps, our root signature, the
    /// pipeline, and per stage the user data, root views and the
    /// descriptors its tables hold.
    /// False if the draw or dispatch must not run: a root SRV it reads
    /// could not be given a view.
    pub(crate) fn bind(&mut self, list: &HostList, p: &HostPipeline, compute: bool) -> bool {
        let l = &list.list;
        // Textures picked by index, before anything asks for them.
        self.early_textures(list, p, compute);
        // Heaps first: a root signature with directly indexed heaps must be
        // set after them.
        if self.host_heaps != self.heaps {
            let heaps: Vec<Option<ID3D12DescriptorHeap>> = self
                .heaps
                .iter()
                .filter(|&&h| h != 0)
                .filter_map(|&h| crate::descriptor::heap(h as This).host())
                .map(|h| Some(h.heap.clone()))
                .collect();
            heaps.iter().flatten().for_each(|h| list.keep(h));
            // SAFETY: an open list and shader-visible heaps.
            unsafe { l.SetDescriptorHeaps(&heaps) };
            emitted(Command::Heaps, 1);
            self.host_heaps = self.heaps;
            self.host_roots = [0; 2];
        } else {
            spared(Command::Heaps);
        }
        // The depth target a draw only tests, for the views of it that the
        // draw's shaders read (`readable`).
        let tested = if !compute && self.depth_state == DEPTH_READ {
            self.depth
        } else {
            0
        };
        TESTED_DEPTH.set((tested, false));
        let root = p.root.as_raw() as usize;
        if self.host_roots[usize::from(compute)] != root {
            // SAFETY: an open list and a live root signature.
            unsafe {
                if compute {
                    l.SetComputeRootSignature(&p.root);
                } else {
                    l.SetGraphicsRootSignature(&p.root);
                }
            }
            emitted(Command::Signature, 1);
            self.host_roots[usize::from(compute)] = root;
            // A root signature of this space leaves its own root arguments
            // undefined: every one this draw sets is set again. The other
            // space's stand.
            self.forget_space(compute);
        } else {
            spared(Command::Signature);
        }
        // By host object: a title pipeline released and another made at its
        // address is another host pipeline (the list keeps the old one).
        if self.host_pipeline != p.pso.as_raw() as usize {
            // SAFETY: an open list and a live pipeline.
            unsafe { l.SetPipelineState(&p.pso) };
            emitted(Command::Pipeline, 1);
            self.host_pipeline = p.pso.as_raw() as usize;
            list.used_pipeline(&p.name);
            // The title may release the pipeline before it submits the list.
            list.keep_pair(&p.pso, &p.root);
        } else {
            spared(Command::Pipeline);
        }
        let mut made = std::mem::take(&mut self.scratch_made);
        for stage in &p.stages {
            let t = stage.translated();
            STORES.set(t.stores);
            self.tables(list, &t.used, &t.host.root_indexed_views, compute, &mut made);
            STORES.set(true);
            if stage.shader.ambient.is_some() {
                self.ambient_tables.clear();
                self.ambient_tables.extend(made.iter().copied());
            }
            if made.iter().any(|m| m.1 == u32::MAX) {
                self.scratch_made = made;
                // A table that found no slot: counted, and named with the
                // ring's room by `descriptor::host`, at most once a second.
                crate::descriptor::host::note_dropped_draw();
                return false;
            }
            // What the shader stores through root views and raw addresses is
            // copied back into title memory afterwards.
            let writes = (t.stores || !write_skip())
                && !crate::pipeline::builder_quarantined(self.pipeline as This);
            // Title memory by address: what the shader finds it by.
            let memory = if t.host.root_srvs.contains(&victory_shader::emit::SLOT_MEMORY) {
                let Some(ready) = self.memory_ready(list, compute, writes) else {
                    once!("a shader that reads title memory by address without a view heap to hold its views: the draw is dropped");
                    return false;
                };
                Some(ready)
            } else {
                None
            };
            if let Some((param, dwords)) = stage.user_data {
                // A buffer the list keeps: `vec![0; dwords]` was a heap
                // allocation per stage per draw.
                let mut values = std::mem::take(&mut self.scratch_values);
                values.clear();
                values.resize(dwords as usize, 0);
                for u in &t.host.user_data {
                    if let RootType::Other(kind) = u.root {
                        values[u.index as usize] =
                            self.host_word(compute, kind, u.slot, u.dword, memory.as_ref());
                        continue;
                    }
                    // `made` is ordered by root slot (the slots it was made
                    // from were sorted and deduplicated), so this is a walk
                    // of log n instead of a scan per dword.
                    let fresh = if u.root == RootType::Table {
                        made.binary_search_by_key(&u.slot, |m| m.0)
                            .ok()
                            .map(|at| made[at].1)
                    } else {
                        None
                    };
                    values[u.index as usize] = match fresh {
                        Some(offset) => offset,
                        None => self.user_value(compute, u.slot, u.root, u.dword),
                    };
                }
                // SAFETY: an open list; `values` covers the parameter.
                let n = dwords as usize;
                let signature = self.host_roots[usize::from(compute)];
                if self
                    .root_values
                    .again(signature, compute, param, &values[..n])
                {
                    unsafe {
                        if compute {
                            l.SetComputeRoot32BitConstants(
                                param,
                                dwords,
                                values.as_ptr().cast(),
                                0,
                            );
                        } else {
                            l.SetGraphicsRoot32BitConstants(
                                param,
                                dwords,
                                values.as_ptr().cast(),
                                0,
                            );
                        }
                    }
                    emitted(Command::Root32, 1);
                } else {
                    spared(Command::Root32);
                }
                self.scratch_values = values;
            }
            for (views, srv) in [(&stage.root_cbvs, false), (&stage.root_srvs, true)] {
                for &(slot, param) in views {
                    if srv && slot == victory_shader::emit::SLOT_MEMORY {
                        // Ours: the directory of title memory by address. It
                        // lives as long as the process and the list holds it.
                        if let Some(m) = &memory {
                            self.set_root_arg(list, compute, param, RootKind::Srv, m.directory);
                        }
                        continue;
                    }
                    if srv && slot == victory_shader::emit::SLOT_STRIDES {
                        // Ours, not the title's: the shader picks buffers
                        // from the view heap by an index it computes.
                        match self.heap_buffers(list) {
                            // The table lives as long as the heap the list
                            // holds.
                            Some(gpu) => {
                                self.set_root_arg(list, compute, param, RootKind::Srv, gpu);
                            }
                            None => once!("a shader picks buffers from a view heap that is not set"),
                        }
                        continue;
                    }
                    if srv && slot == victory_shader::emit::SLOT_VIEW_ADDRESSES {
                        // Ours, not the title's: the title address of the
                        // buffer view at each heap slot, which a shader that
                        // picks one by an index it computes reads as data
                        // (`victory_shader::emit::SLOT_VIEW_ADDRESSES`).
                        match self.heap_addresses(list) {
                            Some(gpu) => {
                                self.set_root_arg(list, compute, param, RootKind::Srv, gpu);
                            }
                            None => {
                                once!("a shader reads view addresses from a view heap that is not set");
                            }
                        }
                        continue;
                    }
                    let tabled = srv && crate::pipeline::host::bounded_root_srvs();
                    let Some(&Root::View(address)) = self.roots_ref(compute).get(usize::from(slot))
                    else {
                        once!(slot, "a shader reads a root view the title did not set");
                        // A table left unset is undefined: a null view.
                        if tabled && !self.root_srv_table(list, None, param, compute, false) {
                            return false;
                        }
                        continue;
                    };
                    // The shader adds the bytes a raw view begins early by.
                    let biased = srv
                        && t.host.user_data.iter().any(|u| {
                            u.slot == slot
                                && u.root == RootType::Other(victory_shader::emit::USER_VIEW_BIAS)
                        });
                    if tabled {
                        if !self.root_srv_table(list, Some(address), param, compute, biased) {
                            return false;
                        }
                        continue;
                    }
                    let Some(mut gpu) = root_view(list, address, srv) else {
                        continue;
                    };
                    if biased {
                        gpu -= u64::from(self.host_word(
                            compute,
                            victory_shader::emit::USER_VIEW_BIAS,
                            slot,
                            0,
                            None,
                        ));
                    }
                    // The address is in live memory the list holds.
                    let kind = if srv { RootKind::Srv } else { RootKind::Cbv };
                    self.set_root_arg(list, compute, param, kind, gpu);
                }
            }
            for &(slot, param) in &stage.root_uavs {
                if slot == victory_shader::emit::SLOT_FAULTS {
                    // Ours: where shaders note the pages they found empty;
                    // the buffer lives as long as the process.
                    if let Some(m) = &memory {
                        self.set_root_arg(list, compute, param, RootKind::Uav, m.faults);
                    }
                    continue;
                }
                let address = match self.roots_ref(compute).get(usize::from(slot)) {
                    Some(&Root::View(address)) => Some(address),
                    _ => {
                        once!(slot, "a shader writes through a root view the title did not set");
                        None
                    }
                };
                if !self.root_uav(list, address, param, compute, writes) {
                    return false;
                }
            }
        }
        // Depth textures it writes through unordered access: their colour
        // twins hold what they lack from here on.
        list.twins_written();
        self.scratch_made = made;
        true
    }

    /// Sets a root CBV, SRV or UAV at `param`, or a one-view table: a draw
    /// that repeats the value the list has there leaves the call out, as the
    /// pipeline, the root signature and the heaps are left out. A cut's new
    /// host list has none of them ([`State::replay`]), and neither has one
    /// under another root signature.
    fn set_root_arg(
        &mut self,
        list: &HostList,
        compute: bool,
        param: u32,
        kind: RootKind,
        value: u64,
    ) {
        let signature = self.host_roots[usize::from(compute)];
        if !self.root_args.again(signature, compute, param, kind, value) {
            spared(if kind == RootKind::Table {
                Command::RootTable
            } else {
                Command::RootView
            });
            return;
        }
        // SAFETY: an open list; the value is a GPU address in live memory the
        // list holds, or a handle in the bound view heap.
        unsafe {
            match (compute, kind) {
                (false, RootKind::Cbv) => list.list.SetGraphicsRootConstantBufferView(param, value),
                (false, RootKind::Srv) => list.list.SetGraphicsRootShaderResourceView(param, value),
                (false, RootKind::Uav) => list.list.SetGraphicsRootUnorderedAccessView(param, value),
                (false, RootKind::Table) => {
                    list.list.SetGraphicsRootDescriptorTable(
                        param,
                        D3D12_GPU_DESCRIPTOR_HANDLE { ptr: value },
                    )
                }
                (true, RootKind::Cbv) => list.list.SetComputeRootConstantBufferView(param, value),
                (true, RootKind::Srv) => list.list.SetComputeRootShaderResourceView(param, value),
                (true, RootKind::Uav) => list.list.SetComputeRootUnorderedAccessView(param, value),
                (true, RootKind::Table) => {
                    list.list.SetComputeRootDescriptorTable(
                        param,
                        D3D12_GPU_DESCRIPTOR_HANDLE { ptr: value },
                    )
                }
            }
        }
        emitted(
            if kind == RootKind::Table {
                Command::RootTable
            } else {
                Command::RootView
            },
            1,
        );
    }

    /// A new host list: every root argument and every set of 32-bit
    /// constants is undefined there, so the next draw sets all of them
    /// again. A root signature of one space is [`State::forget_space`].
    fn forget_roots(&mut self) {
        self.root_args.clear();
        self.root_values.clear();
    }

    /// A root signature of one space (graphics or compute) replaced the
    /// list's: that space's root arguments and 32-bit constants are
    /// undefined, and the next draw or dispatch sets its own again. The
    /// other space keeps its own: D3D12 keeps the graphics root arguments
    /// across a `SetComputeRootSignature` (and the compute ones across a
    /// graphics signature), so a title clamp's compute signature must not
    /// make every draw after it set every graphics constant and view again
    /// (~2.4 root-constant calls a draw).
    fn forget_space(&mut self, compute: bool) {
        self.root_args.forget(compute);
        self.root_values.forget(compute);
    }

    /// The clamp `crate::indirect` recorded for an indirect run of `records`
    /// records replaced what the list held of the pipeline and the compute
    /// root signature: it sets its compute pipeline through
    /// `SetPipelineState` (which a dispatch reads) and a root signature of
    /// its own, so `bind` sets both again. `held` is what it left (its own
    /// pipeline and signature, or the ones the list already held when it
    /// spared them), and a root signature it changed makes the compute root
    /// arguments undefined — the graphics ones are the list's still, and
    /// every draw after a clamp reads them without setting them again. A
    /// clamp of no record records no command at all, and a record whose
    /// window another run already clamped (`Window`) records no clamp, so in
    /// both cases the list keeps what it has and `bind` spares the pipeline
    /// it would otherwise set again.
    fn hold_clamp(&mut self, records: u32, held: &crate::indirect::Held) {
        if records == 0 {
            return;
        }
        if self.host_roots[1] != held.root {
            self.host_roots[1] = held.root;
            self.forget_space(true);
        }
        self.host_pipeline = held.pipeline;
    }

    /// Ends the window of records clamped ahead (`windows`), which the command
    /// about to run can write: [`pipeline_stores`] says its shaders may store
    /// to buffer memory, so the run that reads those records must clamp them
    /// itself. A pipeline that cannot store leaves the window standing: nothing
    /// on the list writes those bytes in between, since a title buffer copy is
    /// deferred to the list's submission (`host::copy_run`) and another
    /// indirect run works its own records.
    fn close_window(&mut self) {
        if self.window.is_some() && pipeline_stores(self.pipeline) {
            self.window = None;
            windowed_note(Windowed::Stores);
        }
    }

    /// Binds a root UAV: a raw unordered access view of the title buffer's
    /// host copy from `address` on, as a one-view table (a null view
    /// without an address, without a host copy shaders can write, or at an
    /// address not on 16 bytes), or as a root descriptor with
    /// `VICTORY_ROOT_SRV_DIRECT=1` (the dispatch is dropped then if there
    /// is nothing to bind). With `writes`, what the shader stored is copied
    /// back into title memory when the list has run. False if the draw
    /// must be dropped.
    /// shortcut: a root view has no size: the view, the refresh and the
    /// copy back cover all that is left of the title's buffer
    /// (`VICTORY_ROOT_UAV_MB` sets a limit for all three).
    fn root_uav(
        &mut self,
        list: &HostList,
        address: Option<u64>,
        param: u32,
        compute: bool,
        writes: bool,
    ) -> bool {
        const UNORDERED: i32 = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0;
        static LIMIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let limit = *LIMIT.get_or_init(|| {
            std::env::var("VICTORY_ROOT_UAV_MB")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map_or(u64::MAX, |mb| mb << 20)
        });
        let find = |a: u64| {
            let s = crate::shadow::find(a, 1)?;
            // SAFETY: a live buffer.
            let flags = unsafe { s.buffer.GetDesc() }.Flags;
            if flags.0 & UNORDERED == 0 {
                once!("a root UAV of a buffer whose host copy shaders cannot write");
                return None;
            }
            Some(s)
        };
        let ready = |a: u64, s: &std::sync::Arc<crate::shadow::Shadow>| {
            let len = s.left(a).min(limit);
            s.refresh(list, a, len);
            if writes {
                s.gpu_writes(list, a, len);
            }
        };
        if !crate::pipeline::host::bounded_root_srvs() {
            let Some((a, s)) = address.and_then(|a| Some((a, find(a)?))) else {
                refused_uav_unbound();
                return false;
            };
            ready(a, &s);
            // From the 16 bytes below: the shader adds the rest.
            // SAFETY: a live buffer.
            let base = unsafe { s.buffer.GetGPUVirtualAddress() };
            let gpu = base + ((s.gpu(a) - base) & !15);
            // The address is in a buffer the list holds.
            self.set_root_arg(list, compute, param, RootKind::Uav, gpu);
            return true;
        }
        let heap = match self.heaps[0] {
            0 => None,
            h => Some(crate::descriptor::heap(h as This)),
        };
        let Some((heap, host)) = heap.and_then(|h| Some((h, h.host()?))) else {
            once!("a root UAV without a bound view heap: the draw is dropped");
            return false;
        };
        let key = (std::ptr::from_ref(heap) as usize, address.unwrap_or(0));
        let (index, shadow) = match self.root_uav_slots.get(&key) {
            Some((index, shadow)) => (*index, address.zip(shadow.clone())),
            None => {
                let shadow = address.and_then(|a| find(a).map(|s| (a, s)));
                let Some(index) = host.dynamic_in(&mut self.ring, 1) else {
                    crate::descriptor::host::note_dropped_draw();
                    return false;
                };
                if let Some((a, s)) = &shadow {
                    // SAFETY: a live buffer.
                    let base = unsafe { s.buffer.GetGPUVirtualAddress() };
                    let offset = s.gpu(*a) - base;
                    // Raw views start on 16 bytes: from the 16 bytes
                    // below, and the shader adds the rest to its offsets
                    // (`victory_shader::emit::USER_VIEW_BIAS`). (An address
                    // not on 16 bytes got a null view at first: the
                    // builder's counters are at such addresses.)
                    let bias = offset & 15;
                    host.raw_uav_at(index, &s.buffer, offset - bias, s.left(*a).min(limit) + bias);
                }
                if shadow.is_none() {
                    host.null_raw_uav_at(index);
                }
                self.root_uav_slots
                    .insert(key, (index, shadow.as_ref().map(|(_, s)| s.clone())));
                (index, shadow)
            }
        };
        if let Some((a, s)) = &shadow {
            ready(*a, s);
        }
        let handle = host.gpu_handle(index);
        // A draw of the list that binds the same slot at the same parameter
        // leaves this call out (the slot is cached per list, so it repeats).
        self.set_root_arg(list, compute, param, RootKind::Table, handle.ptr);
        true
    }

    /// Binds a root SRV as a one-view table
    /// ([`crate::pipeline::host::bounded_root_srvs`]): a raw view of the
    /// title buffer's host copy from `address` to the copy's end, or a null
    /// view (no address, no host copy, or an address not on 16 bytes). One
    /// dynamic slot per address per list, given back with the list's
    /// others. False if no slot could be had or no view heap is bound.
    fn root_srv_table(
        &mut self,
        list: &HostList,
        address: Option<u64>,
        param: u32,
        compute: bool,
        biased: bool,
    ) -> bool {
        let heap = match self.heaps[0] {
            0 => None,
            h => Some(crate::descriptor::heap(h as This)),
        };
        let Some((heap, host)) = heap.and_then(|h| Some((h, h.host()?))) else {
            once!("a root SRV without a bound view heap: the draw is dropped");
            return false;
        };
        // A view that begins early is another view than one that does not
        // (title addresses stay below bit 63).
        let key = (
            std::ptr::from_ref(heap) as usize,
            address.unwrap_or(0) | u64::from(biased) << 63,
        );
        let (index, shadow) = match self.root_srv_slots.get(&key) {
            Some((index, shadow)) => (*index, address.zip(shadow.clone())),
            None => {
                let shadow = address.and_then(|a| crate::shadow::find(a, 1).map(|s| (a, s)));
                let Some(index) = host.dynamic_in(&mut self.ring, 1) else {
                    crate::descriptor::host::note_dropped_draw();
                    return false;
                };
                self.root_srv_slots
                    .insert(key, (index, shadow.as_ref().map(|(_, s)| s.clone())));
                match &shadow {
                    Some((a, s)) => {
                        // SAFETY: a live buffer.
                        let base = unsafe { s.buffer.GetGPUVirtualAddress() };
                        let offset = s.gpu(*a) - base;
                        if biased {
                            // From the 16 bytes below; the shader adds the
                            // rest (`victory_shader::emit::USER_VIEW_BIAS`).
                            let bias = offset & 15;
                            host.raw_at(index, &s.buffer, offset - bias, s.left(*a) + bias);
                        } else if offset.is_multiple_of(16) {
                            host.raw_at(index, &s.buffer, offset, s.left(*a));
                        } else {
                            // Raw views start on 16 bytes; one from the
                            // aligned start would shift every read.
                            once!(
                                address = format_args!("{a:#x}"),
                                "a root SRV not on 16 bytes reads zeros"
                            );
                            host.null_raw_at(index);
                        }
                    }
                    None => host.null_raw_at(index),
                }
                (index, shadow)
            }
        };
        // As for root descriptors (`root_view`): all that is left of the
        // buffer is brought up to date. The first 1 MiB only left what lay
        // past it stale (most mesh vertex streams reach further).
        if let Some((a, s)) = &shadow {
            s.refresh(list, *a, s.left(*a));
        }
        let handle = host.gpu_handle(index);
        // As above: the slot is per address per list, so the same draw of the
        // list binds the same handle at the same parameter.
        self.set_root_arg(list, compute, param, RootKind::Table, handle.ptr);
        true
    }

    /// For a shader that picks buffers from the view heap by an index it
    /// computes (SM 6.6 dynamic resources): the GPU address of the heap's
    /// table of element strides, with every buffer view of the heap given
    /// its host slot and its memory brought up to date when `list` is
    /// submitted. Tables tell us which views a draw reads; nothing tells
    /// us which of the heap's a computed index names, so all of them are
    /// made ready, once per list.
    /// shortcut: the buffers are read, not written: what such a shader
    /// writes through a picked unordered access view stays in the host
    /// copy. A list refreshes them where it first runs such a shader;
    /// title writes recorded later in the same list are not seen.
    /// For a shader that reads the title address out of a buffer view it
    /// picks from the view heap by an index it computes: the GPU address of
    /// the heap's table of view addresses, kept on `list`.
    /// The host writes that table as the title writes descriptors
    /// ([`HostHeap::note_buffer`]), so unlike [`State::heap_buffers`] there
    /// is nothing here to make ready.
    fn heap_addresses(&self, list: &HostList) -> Option<u64> {
        let heap = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())?;
        let host = heap.host()?;
        let gpu = host.addresses_gpu()?;
        // One key per list, as textures and the strides table have: no title
        // resource is there.
        const ONCE: usize = usize::MAX - 0x5d;
        if list.first_use(ONCE)
            && let Some(buffer) = host.addresses_buffer()
        {
            list.keep(buffer);
        }
        Some(gpu)
    }

    fn heap_buffers(&self, list: &HostList) -> Option<u64> {
        let heap = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())?;
        let host = heap.host()?;
        let gpu = host.strides_gpu()?;
        // One key per list, as textures have: no title resource is there.
        const ONCE: usize = usize::MAX - 0x5e;
        if !list.first_use(ONCE) {
            return Some(gpu);
        }
        // The list may outlive the title's heap object, which owns the table.
        if let Some(buffer) = host.strides_buffer() {
            list.keep(buffer);
        }
        // The pass itself once per frame: per list it ran up to 51 times a
        // frame over 81,000 views and 4,100 host copies, 18 s of every
        // frame (2026-11-24, run 11).
        // shortcut: what the title writes into such buffers between lists
        // of one frame is seen a frame late.
        static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
        let frame = crate::frame::current();
        if FRAME.swap(frame, Ordering::Relaxed) == frame {
            return Some(gpu);
        }
        let started = std::time::Instant::now();
        // Per host heap: the host copy and range each slot's buffer view
        // reads, kept from pass to pass. Only the slots written since the
        // last pass are looked at again (83,000 slots a pass otherwise, 0.1 s
        // at best); all of them every 16 passes, because a view of a
        // released resource goes stale without a write, and a slot written
        // before its memory had a host copy is only made then.
        type Ranges = std::collections::HashMap<u32, (std::sync::Arc<crate::shadow::Shadow>, u64, u64)>;
        static RANGES: std::sync::Mutex<Option<std::collections::HashMap<usize, (u64, Ranges)>>> =
            std::sync::Mutex::new(None);
        let mut kept = RANGES.lock().unwrap_or_else(|e| e.into_inner());
        let (passes, ranges) = kept
            .get_or_insert_with(Default::default)
            .entry(std::ptr::from_ref(host) as usize)
            .or_insert_with(|| (0, Ranges::new()));
        let changed = host.buffer_changes();
        let slots = if *passes % 16 == 0 {
            ranges.clear();
            host.buffer_slots()
        } else {
            changed
        };
        *passes += 1;
        // Whether the device was there before the pass: a view that removes
        // it is named below.
        static GONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let mut alive = !GONE.load(Ordering::Relaxed) && crate::descriptor::host::device_removed().is_none();
        for (n, &index) in slots.iter().enumerate() {
            ranges.remove(&index);
            let Some(handle) = heap.handle(index) else {
                continue;
            };
            let Some(view) = host.mirror(index, handle, true) else {
                continue;
            };
            // Every 64 views while the device lives (a cheap query).
            if alive
                && n % 64 == 63
                && let Some(why) = crate::descriptor::host::device_removed()
            {
                alive = false;
                GONE.store(true, Ordering::Relaxed);
                let last: Vec<String> = slots[n - 63..=n]
                    .iter()
                    .filter_map(|&i| crate::descriptor::view(heap.handle(i)?).map(|v| format!("{i}: {v:?}")))
                    .map(|s| s.chars().take(200).collect())
                    .collect();
                tracing::error!(why, slot = index, "the host device went while buffer views of the heap were made; one of: {last:#?}");
                crate::host::log_removal_context();
            }
            // The copy the slot shows (`buffer_copy`), not the one the
            // view's range alone would find.
            let Some((shadow, address, len)) = crate::descriptor::host::buffer_copy(&view) else {
                continue;
            };
            #[cfg(feature = "diagnostics")]
            if host.shows(index, &shadow.buffer) == Some(false) {
                HEAP_VIEWS_ELSEWHERE.fetch_add(1, Ordering::Relaxed);
            }
            ranges.insert(index, (shadow, address, address + len));
        }
        let views = ranges.len() as u32;
        // Per host copy, the span its picked views cover.
        let mut spans: std::collections::HashMap<usize, (std::sync::Arc<crate::shadow::Shadow>, u64, u64)> =
            std::collections::HashMap::new();
        for (shadow, from, to) in ranges.values() {
            let key = std::sync::Arc::as_ptr(shadow) as usize;
            let span = spans.entry(key).or_insert_with(|| (shadow.clone(), *from, *to));
            span.1 = span.1.min(*from);
            span.2 = span.2.max(*to);
        }
        drop(kept);
        let bytes: u64 = spans.values().map(|s| s.2 - s.1).sum();
        // Memory that is not write-watched is copied whole by every refresh.
        // It was once refreshed only every 120 frames (unmapped pages read
        // one by one cost 14 GB a frame in run 11, before they were
        // remembered): streamed geometry then drew from stale buffers, and
        // the garbage ended runs (the card stopped answering 14-90 frames
        // into the opening cutscene; with every frame it ran on). The pass
        // costs some 30 ms a frame this way.
        static WHOLE: std::sync::Mutex<Option<std::collections::HashMap<usize, u64>>> = std::sync::Mutex::new(None);
        let mut whole = WHOLE.lock().unwrap_or_else(|e| e.into_inner());
        let whole = whole.get_or_insert_with(Default::default);
        for (shadow, from, to) in spans.values() {
            if shadow.unwatched() {
                // By the copy's number: a copy made where one let go was
                // (`shadow::let_go_idle`) has never been refreshed.
                let key = shadow.id() as usize;
                // `VICTORY_UNWATCHED_FRAMES` sets the interval (1: every frame).
                static EVERY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
                let every = *EVERY.get_or_init(|| {
                    std::env::var("VICTORY_UNWATCHED_FRAMES")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(1)
                });
                match whole.get(&key) {
                    Some(&last) if frame < last + every => continue,
                    _ => {
                        whole.insert(key, frame);
                    }
                }
            }
            shadow.refresh(list, *from, to - from);
        }
        self.heap_textures(list, heap, host);
        HEAP_BUFFER_PASSES.fetch_add(1, Ordering::Relaxed);
        HEAP_BUFFER_MICROS.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        static LOGGED: AtomicU64 = AtomicU64::new(0);
        // The first pass and every growth of the set by half.
        let seen = LOGGED.load(Ordering::Relaxed);
        if u64::from(views) > seen + seen / 2 {
            LOGGED.store(u64::from(views), Ordering::Relaxed);
            tracing::info!(
                slots = slots.len(),
                views,
                copies = spans.len(),
                mb = bytes >> 20,
                ms = started.elapsed().as_millis(),
                "buffer views of the heap made ready for shaders that pick them by index"
            );
        }
        Some(gpu)
    }

    /// Runs the pass that uploads textures picked by index
    /// ([`State::heap_textures`]) where nothing has asked for it yet
    /// ([`early_budgets`]): once a frame for a few milliseconds, at the
    /// frame's first draw with a view heap set, and with a larger budget
    /// of its own before every draw whose pixel shader is one of the
    /// title's bake programs. The uploads are recorded into `list` ahead
    /// of the draw being bound, as the world's pass records them.
    fn early_textures(&self, list: &HostList, p: &HostPipeline, compute: bool) {
        let (early_ms, bake_ms) = early_budgets();
        if compute || (early_ms == 0 && bake_ms == 0) {
            return;
        }
        let bake = bake_ms > 0
            && p.stages.iter().any(|s| BAKE_PIXEL_SHADERS.contains(&s.shader.hash.as_str()));
        static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
        // The milliseconds this frame's bake draws have taken so far: a
        // frame has several, and together they get `bake_ms`.
        static BAKED_MS: AtomicU64 = AtomicU64::new(0);
        let frame = crate::frame::current();
        let first = FRAME.swap(frame, Ordering::Relaxed) != frame;
        if first {
            BAKED_MS.store(0, Ordering::Relaxed);
        }
        let left = bake_ms.saturating_sub(BAKED_MS.load(Ordering::Relaxed));
        let budget = if bake && left > 0 {
            // What a bake may read must be there now: up to 2 GiB.
            (left, 2 << 30)
        } else if first && early_ms > 0 {
            (early_ms, 64 << 20)
        } else {
            return;
        };
        let Some(heap) = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())
        else {
            return;
        };
        let Some(host) = heap.host() else {
            return;
        };
        let bake = bake && left > 0;
        let n = EARLY_TEXTURE_PASSES[usize::from(bake)].fetch_add(1, Ordering::Relaxed) + 1;
        if bake && n.is_power_of_two() {
            let [slots, due, ..] = HEAP_TEXTURES_QUEUE.each_ref().map(|c| c.load(Ordering::Relaxed));
            tracing::info!(
                frame,
                bakes = n,
                queued_at_the_last_pass = slots,
                due_at_the_last_pass = due,
                uploaded_so_far = HEAP_TEXTURES.load(Ordering::Relaxed),
                "a draw of the title's decoration bake: textures picked by index are uploaded first (VICTORY_BAKE_TEXTURES_MS=0: not)"
            );
        }
        let began = std::time::Instant::now();
        PASS_BUDGET.set(Some(budget));
        self.heap_textures(list, heap, host);
        PASS_BUDGET.set(None);
        if bake {
            BAKED_MS.fetch_add(began.elapsed().as_millis() as u64, Ordering::Relaxed);
        }
    }

    /// The texture side of [`State::heap_buffers`]: textures the title fills
    /// itself (streamed pages, layout-library copies) whose views sit in the
    /// view heap are created (placed textures have no host texture until
    /// something reads them) and uploaded, as a table read would do, for
    /// shaders that pick them by a computed index. Within a budget of bytes
    /// per frame and in all; the rest waits for later lists and frames.
    /// The most recently written slots first (`TextureQueue::due`); slots
    /// draws read through tables are not in the queue.
    /// shortcut: uploaded once, and again only when the title rewrites the
    /// slot. A placed texture not all mapped yet is uploaded with what is
    /// there and looked at again (1, 2, 4, ... frames later, at most about
    /// a second): uploaded again when more of it is mapped. Pages mapped
    /// in place of others, and contents changed with no descriptor write,
    /// are missed (hook the page-mapping events for those).
    /// `VICTORY_HEAP_TEXTURES=0` turns it off.
    fn heap_textures(
        &self,
        list: &HostList,
        heap: &crate::descriptor::Heap,
        host: &crate::descriptor::host::HostHeap,
    ) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_TEXTURES").is_none_or(|v| v != "0")) {
            return;
        }
        /// Bytes of texture memory staged per frame.
        // `VICTORY_TEXTURE_STAGE_MB` raises it: a backlog of tens of thousands of
        // textures (a scene's first minutes) then drains in fewer frames.
        static PER_FRAME_MB: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let per_frame: u64 = *PER_FRAME_MB.get_or_init(|| {
            std::env::var("VICTORY_TEXTURE_STAGE_MB").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(192)
        }) << 20;
        static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
        static SPENT: AtomicU64 = AtomicU64::new(0);
        let frame = crate::frame::current();
        if FRAME.swap(frame, Ordering::Relaxed) != frame {
            SPENT.store(0, Ordering::Relaxed);
        }
        // No more of them are made while the card has no room
        // (`resource::room`): by what is alive now, not by what was ever
        // staged (4 GB in all, after which later textures stayed empty for
        // the run). The ones nothing names any more are let go, which
        // makes room again: so the slots are still looked at (the pass
        // returned here, and a slot whose texture had been streamed out
        // kept its host copy for as long as the card was full).
        let no_room = crate::resource::room().err();
        if no_room.is_some() {
            HEAP_TEXTURES_FULL.fetch_add(1, Ordering::Relaxed);
        }
        let mut refused = 0u64;
        let mut reclaimed = 0u64;
        let mut reclaimed_bytes = 0u64;
        // Slots whose texture's memory the runtime mapped, unmapped or
        // wrote since the last pass are due again.
        host.texture_slots_remapped(frame);
        // Ranges of that memory the queue could not hold are thrown away
        // whole (`descriptor::host::remapped`): once a window, with the
        // count and the cap, so a texture written and never re-read says so.
        {
            static LAST: AtomicU64 = AtomicU64::new(0);
            static SINCE: AtomicU64 = AtomicU64::new(0);
            // Taken every frame: the queue hands them over once, so a count
            // read only on the printing frame would lose the others.
            SINCE.fetch_add(
                crate::descriptor::host::RANGES_DROPPED.swap(0, Ordering::Relaxed),
                Ordering::Relaxed,
            );
            let now = crate::host::now_ms();
            let last = LAST.load(Ordering::Relaxed);
            if now >= last + 5_000
                && LAST
                    .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                let dropped = SINCE.swap(0, Ordering::Relaxed);
                if dropped > 0 {
                    let window = match last {
                        0 => "since the start".to_string(),
                        last => format!("in the last {} s", now.saturating_sub(last).max(1) / 1000),
                    };
                    tracing::info!(
                        "textures picked by index: {dropped} ranges of memory mapped, unmapped or written under texture \
                         views were dropped {window}: the queue holds {} of them ({} KiB), and they were not taken \
                         before it filled; the slots there are looked at when they are due",
                        crate::descriptor::host::RANGE_CAP,
                        (crate::descriptor::host::RANGE_CAP * std::mem::size_of::<(u64, u64)>()) >> 10
                    );
                }
            }
        }
        let (due, census) = host.texture_slots_due(frame, 4096);
        HEAP_TEXTURES_WAITING.store(census[0], Ordering::Relaxed);
        for (count, now) in HEAP_TEXTURES_QUEUE.iter().zip(census) {
            count.store(now, Ordering::Relaxed);
        }
        // The frames in about four seconds, between 16 and 256: how long a
        // slot with nothing to upload waits at most for its next look. In
        // frames alone it was 120: four minutes at the opening cutscene's
        // half a frame per second, while the title streams by the clock.
        // (About a second before the runtime told of the memory it maps:
        // the looks are now only for what that does not tell.)
        static PASSED: AtomicU64 = AtomicU64::new(0);
        let now = crate::host::now_ms();
        let took = now.saturating_sub(PASSED.swap(now, Ordering::Relaxed)).max(1);
        let most = (4000 / took).clamp(16, 256);
        // What a table entry of a texture a shader samples says.
        let sampled = UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 8,
            user_data: 0,
            slot: 0,
        };
        // Also within a time per pass: the pass runs on a thread that
        // records the frame, and making and staging 192 MB of textures took
        // 1.6-2.2 s of each of the world's first twenty frames (run 20).
        // The rest waits for the next frames, as it does past the bytes.
        static SPAN_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let span = std::time::Duration::from_millis(*SPAN_MS.get_or_init(|| {
            std::env::var("VICTORY_TEXTURE_STAGE_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(250)
        }));
        // A backlog (a scene's first minutes: 22,000 slots due, 2-3 minutes of black
        // surfaces at 192 MB a frame) gets three times the bytes and the time until it
        // drains; `VICTORY_TEXTURE_CATCHUP=0` keeps the plain budget.
        static CATCHUP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let catchup = *CATCHUP.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_CATCHUP").is_none_or(|v| v != "0"));
        let (per_frame, span) = pass_budget(catchup, no_room.is_none(), due.len(), per_frame, span);
        // A run made early (`State::early_textures`) has a budget of its
        // own: its bytes on top of what the frame has spent so far.
        // (Not `most`: that is the longest wait between looks, used below.)
        let (span, bytes_at_most) = match PASS_BUDGET.get() {
            Some((ms, bytes)) => {
                (std::time::Duration::from_millis(ms), SPENT.load(Ordering::Relaxed).saturating_add(bytes))
            }
            None => (span, per_frame),
        };
        let began = std::time::Instant::now();
        for (index, write) in due {
            if SPENT.load(Ordering::Relaxed) >= bytes_at_most || began.elapsed() > span {
                break;
            }
            // A placed texture whose memory is not mapped (not streamed in
            // yet, or streamed out again: the heap keeps tens of thousands
            // of such views) gets no host texture; looked at again soon (a
            // title writes a texture's view before its pages are mapped,
            // and the pass runs early in a frame). One of which some is
            // mapped (a texture streaming in: its smaller mips lie at the
            // start of its memory and arrive first) is uploaded with what
            // is there, and again when more is. (It was uploaded again
            // every 120 frames, changed or not; and asked by its first
            // page alone, one whose first page is the one missing counted
            // as not there.)
            // A slot uploaded whole is looked at again a few times
            // (`RECHECK_FRAMES`): the title writes a texture's view, and
            // may fill the texture's memory itself only afterwards, with
            // no descriptor write and nothing from the runtime to tell of
            // it. The host copy then kept what the memory held at the
            // first look for the rest of the run. The look reads 48 KiB;
            // only a changed texture is uploaded again.
            // `VICTORY_HEAP_RECHECK=0`: uploaded once, as before.
            //
            // A change resumes the schedule instead of restarting it: both
            // it and a look that found the memory as it was go through
            // `recheck_step` ([`Looked`]), so a texture the title rewrites
            // for ever is uploaded at most as often as the schedule says and
            // then left alone, rather than every fourth frame for the rest
            // of the run.
            // The first upload looks again at `RECHECK_FRAMES[0]` with no
            // look made yet; a look that finds a change replaces it with its
            // own step.
            let mut relook = Some((frame + RECHECK_FRAMES[0], 0u8));
            if let Some((at, len, print, looks, uploaded)) = host.texture_slot_checked(index) {
                let changed = look_changed(print, crate::texture::fingerprint(at, len), || {
                    crate::texture::mapped_bytes(at as *const u8, len as usize) > 0
                });
                let looked = recheck_step(frame, looks, changed);
                if !looked.changed {
                    HEAP_TEXTURES_RECHECKED[0].fetch_add(1, Ordering::Relaxed);
                    match looked.again {
                        Some((due, next)) => {
                            host.texture_slot_recheck(index, write, due, (at, len), print, next, uploaded)
                        }
                        None => host.texture_slot_done(index, write, None),
                    }
                    continue;
                }
                relook = looked.again;
                let n = HEAP_TEXTURES_RECHECKED[1].fetch_add(1, Ordering::Relaxed);
                if n < 200 || n.is_multiple_of(1000) {
                    tracing::warn!(
                        slot = index,
                        address = format_args!("{at:#x}"),
                        bytes = len,
                        frames_after_upload = frame.saturating_sub(uploaded),
                        looks,
                        changed = n + 1,
                        "a texture picked by index changed after its upload with nothing to tell of it: uploaded again"
                    );
                }
            }
            let mut partly = None;
            if let Some(View::Placed(false, resource, desc)) = heap.handle(index).and_then(crate::descriptor::view) {
                let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                let len = crate::resource::placed_bytes(&resource).unwrap_or(1);
                let mapped = crate::texture::mapped_bytes(address as *const u8, len as usize);
                if mapped == 0 {
                    HEAP_TEXTURES_AWAY.fetch_add(1, Ordering::Relaxed);
                    // Streamed out: what the slot showed is let go.
                    host.texture_slot_gone(index);
                    host.texture_slot_later(index, write, frame, most);
                    continue;
                }
                if mapped < len {
                    if mapped <= host.texture_slot_mapped(index) {
                        HEAP_TEXTURES_PARTLY[0].fetch_add(1, Ordering::Relaxed);
                        host.texture_slot_later(index, write, frame, most);
                        continue;
                    }
                    if !crate::texture::committed(address as *const u8, 1) {
                        HEAP_TEXTURES_PARTLY[2].fetch_add(1, Ordering::Relaxed);
                    }
                    partly = Some(mapped);
                }
            }
            // A texture object the host renders to or writes holds the
            // host's own contents: nothing to bring up to date, and this
            // pass is no use of it (its host copy is made when something
            // uses it, and its slots then: `descriptor::make_waiting`).
            if let Some(View::Shader(object, _)) = heap.handle(index).and_then(crate::descriptor::view)
                && crate::resource::buffer_bytes(object as This).is_none()
                && !crate::resource::title_fills(object as This)
            {
                host.texture_slot_done(index, write, None);
                continue;
            }
            // From here on a host texture is made: not without room. Before
            // the slot is left due, a host copy only this slot's idle
            // descriptor holds is reclaimed: its slot is marked gone, as
            // when its memory was streamed out (`texture_slot_gone` above),
            // and the slot waits to be looked at again, so the head of the
            // queue moves instead of the same slots being refused every
            // pass. `placed_made` never makes a resource; `slot_only_idle`
            // asks before anything is unmade.
            if no_room.is_some() {
                if let Some(View::Placed(false, d14, v16)) = heap.handle(index).and_then(crate::descriptor::view)
                    && let Some(at) = crate::resource::placed_made(
                        u64::from(v16[10]) | u64::from(v16[11]) << 32,
                        &d14,
                    )
                    && crate::resource::slot_only_idle(at)
                    && host.texture_slot_gone(index)
                {
                    // The slot is gone from here whether or not the copy is
                    // taken: `let_go_for_room` asks again itself (a list may
                    // have taken a reference, or a use been recorded, since
                    // `slot_only_idle`), and it needs the slot un-made first,
                    // so the order cannot be swapped. So the slot is looked
                    // at again either way: with the copy let go it is re-made
                    // once there is room, with the copy kept it is still the
                    // slot to make. Left due as it was, the queue entry
                    // outlived the host slot it named -- the next look found
                    // the slot `done` (or `recheck`ed) from what the queue
                    // still held -- and the texture was never made again for
                    // the run.
                    match crate::resource::let_go_for_room(at) {
                        Some(bytes) => {
                            reclaimed += 1;
                            reclaimed_bytes += bytes;
                        }
                        None => refused += 1,
                    }
                    host.texture_slot_later(index, write, frame, most);
                } else {
                    refused += 1;
                }
                if refused > 256 {
                    break;
                }
                continue;
            }
            let view = heap.handle(index).and_then(|handle| host.mirror(index, handle, true));
            let placed = view.as_ref().and_then(crate::descriptor::host::placed_texture);
            let resource = match placed.as_ref().or(view.as_ref()) {
                Some(&View::Shader(resource, _)) => resource as This,
                _ => {
                    host.texture_slot_done(index, write, None);
                    continue;
                }
            };
            let title_fills = crate::resource::host_resource(resource).is_some_and(crate::texture::title_written);
            if !title_fills {
                // a render target, a depth or unordered access texture: the host's own
                host.texture_slot_done(index, write, None);
                continue;
            }
            // A texture that has a staging buffer now is one draws read
            // through a table: they upload it, and check it at every
            // submission that samples it.
            let tabled = crate::texture::is_staged(resource);
            if let Some(view) = &view {
                readable(list, sampled.kind, view);
            }
            // A texture view the host refuses removes the device: name it.
            static GONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !GONE.load(Ordering::Relaxed)
                && let Some(why) = crate::descriptor::host::device_removed()
            {
                GONE.store(true, Ordering::Relaxed);
                let text: String = format!("{view:?}").chars().take(300).collect();
                tracing::error!(why, slot = index, "the host device went while textures picked by index were made ready; last: {text}");
                crate::host::log_removal_context();
                return;
            }
            let (bytes, whole) = crate::texture::staged(resource).unwrap_or((0, true));
            // Not all of it there: looked at again next frame, and from
            // then on as a slot with nothing new (a placed texture: until
            // more of it is mapped; a texture object at an address the
            // title mapped: again in 120 frames, as before).
            let again = match partly {
                Some(mapped) => Some((frame + 1, mapped)),
                None => (!whole).then_some((frame + 120, 0)),
            };
            if again.is_some() {
                HEAP_TEXTURES_PARTLY[1].fetch_add(1, Ordering::Relaxed);
            }
            if tabled {
                // Nothing was staged here, so nothing is counted or spent,
                // and the staging buffer stays: taking it away made the
                // next draw that reads the texture through a table stage,
                // detile and upload all of it once more.
                HEAP_TEXTURES_TABLED.fetch_add(1, Ordering::Relaxed);
                host.texture_slot_done(index, write, again);
                continue;
            }
            // Uploaded: the list holds the staging buffer until it has run;
            // a table read, or the next look at a texture not all mapped
            // yet, makes it anew. (Kept for those until now: every partly
            // streamed texture of the heap held a staging buffer of its full
            // size for good.)
            let memory = if again.is_none() && heap_recheck() {
                crate::texture::staged_memory(resource)
            } else {
                None
            };
            crate::texture::unstage(resource);
            SPENT.fetch_add(bytes, Ordering::Relaxed);
            HEAP_TEXTURE_BYTES.fetch_add(bytes, Ordering::Relaxed);
            HEAP_TEXTURES.fetch_add(1, Ordering::Relaxed);
            match memory {
                // Whole and uploaded: looked at again (see above).
                Some((at, len)) => {
                    let print = crate::texture::fingerprint(at, len);
                    match relook {
                        Some((due, looks)) => {
                            host.texture_slot_recheck(index, write, due, (at, len), print, looks, frame)
                        }
                        None => host.texture_slot_done(index, write, again),
                    }
                }
                None => host.texture_slot_done(index, write, again),
            }
        }
        // Why the card has no room, once a window (the stats line's 5 s),
        // with how many slots were refused since the last line and how many
        // are still due: the rule that refused and its numbers, not a line
        // per texture (a 7.5-minute run said only "they wait").
        if let Some(why) = no_room {
            static LAST: AtomicU64 = AtomicU64::new(0);
            static REFUSED: AtomicU64 = AtomicU64::new(0);
            static RECLAIMED: AtomicU64 = AtomicU64::new(0);
            static RECLAIMED_BYTES: AtomicU64 = AtomicU64::new(0);
            let refused = REFUSED.fetch_add(refused, Ordering::Relaxed) + refused;
            let reclaimed = RECLAIMED.fetch_add(reclaimed, Ordering::Relaxed) + reclaimed;
            let reclaimed_bytes =
                RECLAIMED_BYTES.fetch_add(reclaimed_bytes, Ordering::Relaxed) + reclaimed_bytes;
            let now = crate::host::now_ms();
            // The first refusal starts the window: without this the first line
            // says "in the last 188 s" and the rate it prints is a lifetime
            // average.
            let _ = LAST.compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed);
            let last = LAST.load(Ordering::Relaxed);
            if now >= last + 5_000
                && LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok()
            {
                REFUSED.store(0, Ordering::Relaxed);
                tracing::info!(
                    "textures picked by index: no room for more in the last {} s: {refused} slots refused since the last line, \
                     {reclaimed} reclaimed ({} KiB), {} still due; {}",
                    now.saturating_sub(last).max(1) / 1000,
                    reclaimed_bytes >> 10,
                    HEAP_TEXTURES_QUEUE[1].load(Ordering::Relaxed),
                    why.why()
                );
            }
        }
        // Protected and probation host copies, once a window: frequency
        // admission either keeps the world set (protected) or leaves
        // streaming pages evictable (probation), and the bytes say which
        // the card is full of.
        {
            static LAST: AtomicU64 = AtomicU64::new(0);
            let now = crate::host::now_ms();
            let last = LAST.load(Ordering::Relaxed);
            if now >= last + 5_000
                && LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok()
            {
                let [protected, protected_bytes, probation, probation_bytes] =
                    crate::resource::admission();
                if protected + probation > 0 {
                    tracing::info!(
                        "textures picked by index: {protected} protected from the sweep ({} MB, sampled again and \
                         again), {probation} on probation ({} MB, sampled once); {} slots due, {} waiting for \
                         their memory",
                        protected_bytes >> 20,
                        probation_bytes >> 20,
                        HEAP_TEXTURES_QUEUE[1].load(Ordering::Relaxed),
                        HEAP_TEXTURES_QUEUE[2].load(Ordering::Relaxed),
                    );
                }
            }
        }
    }

    /// For the frame listing, after a dropped dispatch's line: its group
    /// counts and the compute root arguments it was recorded with (root
    /// constants as hex dwords, root views with the title buffer they
    /// point into). A program that does not translate has no bindings to
    /// list; these say which memory it would have read and written (the
    /// ray-tracing structure builder passes its buffers this way).
    #[cfg(feature = "diagnostics")]
    fn list_dropped_roots(&self, groups: [u32; 3]) {
        crate::capture::op(0, || {
            use std::fmt::Write;
            let [x, y, z] = groups;
            let mut text = format!("  its roots ({x}x{y}x{z}):");
            for (slot, root) in self.compute.iter().enumerate() {
                match root {
                    Root::None => {}
                    Root::Table(a) => {
                        let _ = write!(text, " {slot}=table {a:#x}");
                    }
                    Root::Constants(v) => {
                        let _ = write!(text, " {slot}=constants {:x?}", &v[..v.len().min(48)]);
                    }
                    Root::View(a) => {
                        let _ = write!(text, " {slot}=view {a:#x}");
                        match crate::shadow::extent(*a) {
                            Some((start, len)) => {
                                let _ = write!(text, " in buffer {start:#x}+{len:#x}");
                            }
                            None => text.push_str(" in no buffer"),
                        }
                    }
                }
            }
            text
        });
    }

    /// Runs a dispatch on the host.
    pub(crate) fn dispatch(&mut self, list: &HostList, x: u32, y: u32, z: u32) {
        // Its shaders may write the arguments a window holds clamped.
        self.close_window();
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::pipeline::note_drop(self.pipeline as This);
            return;
        }
        // The ray-tracing structure builder's stages run all or none.
        if crate::pipeline::is_builder(self.pipeline as This) && !crate::pipeline::builder_runs() {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::pipeline::note_drop_reason(DropReason::BuilderOff);
            #[cfg(feature = "diagnostics")]
            {
                crate::pipeline::note_why("a stage of the ray-tracing structure builder, which is off".to_owned());
                self.list_dropped_roots([x, y, z]);
            }
            return;
        }
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            once!(
                pipeline = format_args!("{:#x}", self.pipeline),
                "a dispatch without a host pipeline dropped"
            );
            DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::pipeline::note_drop_reason(DropReason::NoPipeline);
            #[cfg(feature = "diagnostics")]
            {
                crate::pipeline::note_drop(self.pipeline as This);
                self.list_dropped_roots([x, y, z]);
            }
            return;
        };
        if !p.compute {
            once!("a dispatch with a graphics pipeline");
            return;
        }
        if !self.bind(list, p, true) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::pipeline::note_drop_reason(DropReason::Unbindable);
            // A stage of the structure builder that cannot be bound is a
            // dropped stage: the others stop too.
            if crate::pipeline::is_builder(self.pipeline as This) {
                crate::pipeline::builder_off("one of its dispatches could not be bound");
            }
            return;
        }
        #[cfg(feature = "diagnostics")]
        list.note_op(|| self.op_text(format!("dispatch {x}x{y}x{z}"), true));
        #[cfg(feature = "diagnostics")]
        crate::gpusplit::mark(list, crate::gpusplit::Group::Compute, self.pipeline);
        // SAFETY: an open list with everything set.
        unsafe { list.list.Dispatch(x, y, z) };
        emitted(Command::Dispatch, 1);
        DISPATCHED.fetch_add(1, Ordering::Relaxed);
        if let Some(role) = p.stages.first().and_then(|s| s.shader.ambient)
            && crate::ambient::on()
            && self.ambient(list, p, role)
        {
            // The stand-in set a compute pipeline and a root signature of its
            // own, which makes the compute root arguments undefined: `bind`
            // sets the pipeline, the signature and everything after it again
            // (`hold_clamp` restores the same for `crate::indirect`'s clamp).
            self.host_pipeline = 0;
            self.forget_space(true);
            self.host_roots[1] = 0;
        }
    }

    /// The stand-in for the indirect light after a dispatch of `p`
    /// ([`crate::ambient`]): the views the title's program just wrote and
    /// read, from its own tables, as the program's heap holds them now.
    /// Whether it recorded anything.
    fn ambient(&self, list: &HostList, p: &HostPipeline, role: crate::ambient::Role) -> bool {
        use crate::ambient::{AIR, DEPTH, LIGHT, RADIANCE, Role, Views};
        let Some(t) = p.stages.first().map(|s| s.translated()) else {
            return false;
        };
        let Some(heap) = (self.heaps[0] != 0)
            .then(|| crate::descriptor::heap(self.heaps[0] as This).host())
            .flatten()
        else {
            return false;
        };
        // The entry the program's table has at `(slot, offset)`, as the host
        // heap holds it: the descriptor the title wrote, its resource, and
        // the handle of the slot itself.
        let entry = |kind: ResourceType, (slot, offset): (u8, u32)| {
            t.used.iter().find(|u| {
                u.kind == kind
                    && u.root == RootType::Table
                    && (u.slot, u.table_offset_dwords) == (slot, offset)
            })?;
            let Some(Root::Table(table)) = self.roots_ref(true).get(usize::from(slot)) else {
                return None;
            };
            let resource = match crate::descriptor::view(table + u64::from(offset) * 4)? {
                View::Shader(r, _) | View::Unordered(r, ..) => r,
                _ => return None,
            };
            // A table this draw made afresh lives in the ring's slots, and
            // what the program's user data names is where those start.
            let start = match self.ambient_tables.iter().find(|m| m.0 == slot) {
                Some(m) => m.1,
                None => self.user_value(true, slot, RootType::Table, 0),
            };
            let host = crate::resource::host_resource(resource as This)?;
            // SAFETY: a live resource.
            let desc = unsafe { host.GetDesc() };
            Some((desc, heap.gpu_handle((start + offset * 4) / 32)))
        };
        let at = match role {
            Role::Resolve => LIGHT,
            Role::Radiance => RADIANCE,
            Role::Air => AIR,
        };
        let image = entry(ResourceType::Uav, at);
        let depth = match role {
            Role::Resolve => entry(ResourceType::Srv, DEPTH),
            Role::Radiance | Role::Air => image,
        };
        let (Some((of, image)), Some((seen, depth))) = (image, depth) else {
            return crate::ambient::other_tables(role, || {
                t.used
                    .iter()
                    .filter(|u| {
                        u.root == RootType::Table
                            && matches!(u.kind, ResourceType::Srv | ResourceType::Uav)
                    })
                    .map(|u| {
                        let kind = if u.kind == ResourceType::Srv { 's' } else { 'u' };
                        format!("{kind}{}@{},", u.slot, u.table_offset_dwords)
                    })
                    .collect()
            });
        };
        // (R32_UINT holding R9G9B9E5; R11G11B10_FLOAT.)
        let format = if role == Role::Resolve { 42 } else { 26 };
        let volume = of.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D;
        if of.Format.0 != format
            || (seen.Width, seen.Height) != (of.Width, of.Height)
            || volume != (role == Role::Air)
        {
            return crate::ambient::other_image(role, of.Format.0, of.Width, of.Height);
        }
        crate::ambient::after(
            list,
            role,
            &Views {
                image,
                depth,
                size: [of.Width as u32, of.Height],
                slices: u32::from(of.DepthOrArraySize),
            },
        )
    }

    /// What a draw or dispatch runs, for the device-removed report
    /// (diagnostics): the pipeline, then per stage the user data it gets
    /// (with the view at that heap index, if one is), the host addresses
    /// of its root views and the views its tables name.
    #[cfg(feature = "diagnostics")]
    fn op_text(&self, kind: String, compute: bool) -> String {
        use std::fmt::Write;
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return kind;
        };
        let mut text = format!("{kind} {}", p.name);
        // What it draws into (a dispatch writes through its views below).
        if !compute {
            let described = |t: usize| {
                crate::resource::host_made(t as This).map(|r| {
                    // SAFETY: a live resource.
                    let d = unsafe { r.GetDesc() };
                    format!("{t:#x} {}x{} f{}", d.Width, d.Height, d.Format.0)
                })
            };
            let targets: Vec<String> = self.targets.iter().filter_map(|&t| described(t)).collect();
            let _ = write!(text, " targets {targets:?} depth {:?} state [{}]", described(self.depth), p.state);
            // The title's scissor and viewport at this draw (the first of
            // each): the title draws what it sees through a
            // portal (a window, from inside a room) under the portal's
            // rectangle, and the listing did not say which draws those are.
            if let Some(r) = self.scissors.first() {
                let _ = write!(text, " scissor [{} {} {} {}]", r.left, r.top, r.right, r.bottom);
            }
            if let Some(v) = self.viewports.first() {
                let _ = write!(
                    text,
                    " viewport [{} {} {} {} depth {}..{}]",
                    v.TopLeftX, v.TopLeftY, v.Width, v.Height, v.MinDepth, v.MaxDepth
                );
            }
        }
        let views = (self.heaps[0] != 0)
            .then(|| crate::descriptor::heap(self.heaps[0] as This).host())
            .flatten();
        for stage in &p.stages {
            let t = stage.translated();
            let _ = write!(text, " | {}:", stage.shader.hash);
            for u in &t.host.user_data {
                let v = match u.root {
                    RootType::Other(kind) => self.host_word(compute, kind, u.slot, u.dword, None),
                    _ => self.user_value(compute, u.slot, u.root, u.dword),
                };
                let _ = write!(text, " u{}={v:#x}", u.index);
                let index = if u.root == RootType::Table { v / 32 } else { v };
                if let Some(slot) = views.and_then(|h| h.describe_slot(index)) {
                    let _ = write!(text, " [heap {index}: {slot}]");
                }
            }
            for &(slot, _) in stage.root_cbvs.iter().chain(&stage.root_srvs).chain(&stage.root_uavs) {
                if let Some(Root::View(address)) = self.roots_ref(compute).get(usize::from(slot)) {
                    let host = crate::shadow::find(*address, 1)
                        .map(|s| (s.gpu(*address), s.left(*address)));
                    let _ = write!(text, " root{slot}={address:#x} host {host:#x?}");
                }
            }
        }
        let _ = write!(text, " tables {:?}", self.reads(compute));
        text
    }

    /// A mesh draw under `VICTORY_MESH_TEST=scissor` rasterises one pixel:
    /// the scissors before it (`on`), the title's after it.
    fn mesh_scissor(&self, list: &HostList, on: bool) {
        if mesh_test() != 2 {
            return;
        }
        if on {
            let one = RECT { left: 0, top: 0, right: 1, bottom: 1 };
            crate::draw::scissors(list, &vec![one; self.scissors.len().max(1)]);
        } else if !self.scissors.is_empty() {
            crate::draw::scissors(list, &self.scissors);
        }
    }

    /// Runs a mesh dispatch of `x` groups on the host (the title's mesh
    /// programs are one-dimensional).
    pub(crate) fn dispatch_mesh(&mut self, list: &HostList, x: u32) {
        self.close_window();
        #[cfg(feature = "diagnostics")]
        self.trace("mesh", x, 0);
        let ran = mesh_test() != 1
            && mesh_kept(self.pipeline)
            && self.prepare_for(list, false, true)
            && match list.list.cast::<ID3D12GraphicsCommandList6>() {
                Ok(l6) => {
                    #[cfg(feature = "diagnostics")]
                    list.note_op(|| self.op_text(format!("mesh {x}"), false));
                    self.mesh_scissor(list, true);
                    #[cfg(feature = "diagnostics")]
                    crate::gpusplit::mark(list, crate::gpusplit::Group::Mesh, self.pipeline);
                    // SAFETY: an open list with everything set.
                    unsafe { l6.DispatchMesh(x, 1, 1) };
                    self.mesh_scissor(list, false);
                    emitted(Command::Dispatch, 1);
                    DRAWN.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Err(_) => false,
            };
        if !ran {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::pipeline::note_drop_reason(if mesh_test() == 1 {
                DropReason::MeshOff
            } else {
                DropReason::NoPipeline
            });
            #[cfg(feature = "diagnostics")]
            dropped(self.pipeline);
        }
    }

    fn roots_ref(&self, compute: bool) -> &Vec<Root> {
        if compute {
            &self.compute
        } else {
            &self.graphics
        }
    }

    fn table_entry(&self, list: &HostList, u: &UsedResource, compute: bool) {
        let Some(Root::Table(table)) = self.roots_ref(compute).get(usize::from(u.slot)) else {
            return;
        };
        let handle = table + u64::from(u.table_offset_dwords) * 4;
        let Some(heap) = self.heap_of(handle) else {
            return;
        };
        let (Some(host), Some(index)) = (heap.host(), heap.index(handle)) else {
            return;
        };
        if let Some(view) = host.mirror(index, handle, true) {
            readable(list, u.kind, &view);
            // Up to date by this draw: not the texture pass's to make.
            if crate::descriptor::host::is_texture_view(&view) {
                host.texture_slot_tabled(index, true);
            }
        }
    }

    /// The descriptors a shader reads through tables. Tables holding a
    /// constant buffer are made afresh for the draw in dynamic slots, with
    /// the constant data copied to upload memory when the list is
    /// submitted: the host wants constant buffers 256-byte aligned, the
    /// console only 16. Fills `made` with those tables' new byte offsets by
    /// root parameter; the other tables' slots are made from the title's.
    ///
    /// Within a list, the same table holding the same descriptors is made
    /// once: its constant data is copied at submission either way, and the
    /// big tables the world binds would otherwise take hundreds of slots per
    /// draw, wrapping the dynamic slots onto lists not yet submitted.
    pub(crate) fn tables(
        &mut self,
        list: &HostList,
        used: &[UsedResource],
        indexed: &[RootIndexedView],
        compute: bool,
        made: &mut Vec<(u8, u32)>,
    ) {
        made.clear();
        let tables = |u: &&UsedResource| u.root == RootType::Table;
        let mut fresh = std::mem::take(&mut self.scratch_slots);
        fresh.clear();
        fresh.extend(
            used.iter()
                .filter(tables)
                .filter(|u| u.kind == ResourceType::Cbv)
                .map(|u| u.slot),
        );
        // Tables reaching past a host heap smaller than the title's (the
        // host's limit, 1,000,000 views on NVIDIA) are made afresh too: the
        // title's slots there have no host slot of their own.
        for u in used.iter().filter(tables) {
            if fresh.contains(&u.slot) {
                continue;
            }
            let Some(&Root::Table(table)) = self.roots_ref(compute).get(usize::from(u.slot)) else {
                continue;
            };
            let end = table + u64::from(u.table_offset_dwords) * 4;
            let Some(heap) = self.heap_of(table) else {
                continue;
            };
            let (Some(host), Some(index)) = (heap.host(), heap.index(end)) else {
                continue;
            };
            if index >= host.count() {
                fresh.push(u.slot);
            }
        }
        fresh.sort_unstable();
        fresh.dedup();
        for u in used.iter().filter(tables) {
            if !fresh.contains(&u.slot) {
                self.table_entry(list, u, compute);
            }
        }
        let mut entries = std::mem::take(&mut self.scratch_entries);
        let mut selected = std::mem::take(&mut self.scratch_selected);
        for &slot in &fresh {
            let Some(&Root::Table(table)) = self.roots_ref(compute).get(usize::from(slot)) else {
                continue;
            };
            // This table's used resources, as indices into `used`.
            entries.clear();
            entries.extend(
                used.iter()
                    .enumerate()
                    .filter(|(_, u)| tables(u) && u.slot == slot)
                    .map(|(i, _)| i as u32),
            );
            let Some(heap) = self.heap_of(table) else {
                continue;
            };
            if heap.samplers() || (table - heap.start) % 32 != 0 {
                continue;
            }
            let Some(host) = heap.host() else {
                continue;
            };
            selected.clear();
            selected.extend(indexed.iter().filter(|index| index.table_slot == slot).map(
                |index| {
                    let constant = self.user_value(
                        compute,
                        index.constant_slot,
                        RootType::Constants,
                        index.constant_dword,
                    );
                    // The shader adds i32 byte offsets and shifts by 5.
                    index.byte_offset.wrapping_add(constant << index.shift) / 32
                },
            ));
            selected.sort_unstable();
            selected.dedup();
            // The entries' slot of the table, as the record index it names.
            let at = |i: u32| used[i as usize].table_offset_dwords / 8;
            let n = entries
                .iter()
                .map(|&i| at(i))
                .chain(selected.last().copied())
                .max()
                .unwrap_or(0)
                + 1;
            let last = table.checked_add(u64::from(n - 1) * 32);
            if n > crate::descriptor::host::DYNAMIC
                || last.and_then(|handle| heap.index(handle)).is_none()
            {
                refused_table_span(slot, n);
                continue;
            }
            // The records at the table, mixed. The key is the table and its
            // slot count, which says nothing about what the title wrote
            // there since: the fingerprint does, so a table rewritten in
            // place is never served the slots made for its old records. The
            // slots also hold the host copies the records' descriptors found
            // then, so the registry revision they were made under is part of
            // the answer too ([`Memo`], [`crate::shadow::revision`]).
            let words = Fingerprint::of(table, n);
            let identity = (table, n);
            // Read for the entry this may replace: its slots hold the host
            // copies these records' descriptors found when it was made.
            let seen = self.fresh_table(identity, words, crate::shadow::revision());
            if let Some(seen) = seen
            {
                for &i in &entries {
                    let u = &used[i as usize];
                    if u.kind == ResourceType::Cbv {
                        continue;
                    }
                    let handle = table + u64::from(u.table_offset_dwords) * 4;
                    if let Some(view) = crate::descriptor::view(handle) {
                        readable(list, u.kind, &view);
                    }
                }
                for &k in &selected {
                    if entries.iter().any(|&i| at(i) == k) {
                        continue;
                    }
                    let handle = table + u64::from(k) * 32;
                    if let Some(view) = crate::descriptor::view(handle)
                        && let Some(kind) = view_kind(&view)
                    {
                        readable(list, kind, &view);
                    }
                }
                made.push((slot, seen * 32));
                continue;
            }
            let Some(first) = host.dynamic_in(&mut self.ring, n) else {
                // No room: the draw must not run with the title's own
                // offsets (slots never made for it); `bind` drops it.
                made.push((slot, u32::MAX));
                continue;
            };
            // Records other than those these slots were made for: the
            // identity of this table's contents is a new generation, taken
            // from the one it replaced. The run is the list's ring, given
            // back whole by `State::tables_done`. Recorded after the slots
            // are made below: the makes find host copies and may make some,
            // which bumps the registry's revision, and the entry says which
            // revision its slots name — tagged with the number read before
            // them it would be stale at once and every later draw would make
            // the table again.
            // The title's own slots of this table: what the draw reads of
            // them it brings up to date here, in the dynamic slots.
            let title_first = heap.index(table).filter(|first| first + n <= host.count());
            // The whole range: a shader may index past the entries its
            // used-resource list names (descriptor arrays).
            for k in 0..n {
                let index = first + k;
                let entry = entries
                    .iter()
                    .map(|&i| &used[i as usize])
                    .find(|u| u.table_offset_dwords / 8 == k);
                match crate::descriptor::view(table + u64::from(k) * 32) {
                    Some(View::Constant(address, size)) => {
                        let Some((gpu, to)) = list.scratch(u64::from(size)) else {
                            host.make_null(index);
                            continue;
                        };
                        list.defer_fill(to, address as *const u8, size as usize);
                        host.constant_at(index, gpu, size);
                    }
                    Some(view) => {
                        host.make_at(index, &view);
                        let kind = entry.map(|u| u.kind).or_else(|| {
                            selected.binary_search(&k).ok().and_then(|_| view_kind(&view))
                        });
                        if let Some(kind) = kind {
                            readable(list, kind, &view);
                        }
                        // The title's slot is a table's: out of the queue
                        // of textures picked by index if the draw read it
                        // (its texture is up to date now), and made in
                        // place as the texture pass would have made it,
                        // for a shader that picks it by index all the same.
                        if let Some(title) = title_first
                            && crate::descriptor::host::is_texture_view(&view)
                        {
                            host.texture_slot_tabled(title + k, kind.is_some());
                            if kind.is_some() {
                                host.mirror(title + k, table + u64::from(k) * 32, true);
                            }
                        }
                    }
                    None => {
                        host.make_null(index);
                        if entry.is_some() {
                            once!(slot, "a table entry without a view");
                        }
                    }
                }
            }
            self.fresh_made(identity, words, first, crate::shadow::revision());
            made.push((slot, first * 32));
        }
        self.scratch_slots = fresh;
        self.scratch_entries = entries;
        self.scratch_selected = selected;
    }

    /// The first draws, at debug level: what they bind and read
    /// (diagnostics builds).
    #[cfg(feature = "diagnostics")]
    fn trace(&self, kind: &str, count: u32, first: u32) {
        static TRACED: AtomicU64 = AtomicU64::new(0);
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        let listed = crate::capture::listed(crate::queue::presented() + 1);
        let logged = listed || TRACED.fetch_add(1, Ordering::Relaxed) < 64;
        let targets: Vec<String> = self
            .targets
            .iter()
            .filter_map(|&t| crate::resource::host_made(t as This))
            .map(|t| {
                // SAFETY: a live resource.
                let d = unsafe { t.GetDesc() };
                format!("{}x{} f{}", d.Width, d.Height, d.Format.0)
            })
            .collect();
        let v = self.vertex[0];
        let line = format!(
            "{kind} {count} from {first} topology {} pipeline {:#x} targets {targets:?} vb {:#x}+{} stride {} reads {:?}",
            self.topology,
            self.pipeline,
            v.address,
            v.size,
            v.stride,
            self.reads(false)
        );
        if logged {
            tracing::debug!("draw {line}");
        }
        let mut frame = FRAME.lock().unwrap_or_else(|e| e.into_inner());
        if frame.len() < 256 {
            frame.push(line);
        }
    }

    /// What a draw's shaders read, one line per used resource (diagnostics
    /// builds).
    #[cfg(feature = "diagnostics")]
    fn reads(&self, compute: bool) -> Vec<String> {
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return vec!["no host pipeline".into()];
        };
        let mut out = Vec::new();
        for stage in &p.stages {
            let t = stage.translated();
            for u in &t.used {
                let what = match (u.root, self.roots_ref(compute).get(usize::from(u.slot))) {
                    (RootType::Table, Some(Root::Table(table))) => {
                        let handle = table + u64::from(u.table_offset_dwords) * 4;
                        describe(crate::descriptor::view(handle).as_ref())
                    }
                    (root, _) => format!("{root:?}"),
                };
                out.push(format!(
                    "{} {:?} s{}+{}: {what}",
                    stage.shader.hash, u.kind, u.slot, u.table_offset_dwords
                ));
            }
        }
        out
    }

    pub(crate) fn draw(
        &mut self,
        list: &HostList,
        vertices: u32,
        instances: u32,
        first: u32,
        first_instance: u32,
    ) {
        self.close_window();
        #[cfg(feature = "diagnostics")]
        self.trace("draw", vertices, first);
        let ready = self.prepare(list, false);
        // A pipeline with the rectangle geometry stage draws the title's
        // vertices as they are; any other gets its rectangles expanded.
        let rects = self.topology == RECTANGLES && !self.completes_rects();
        let ran = ready && (!rects || self.rectangles(list, vertices, first));
        if ran {
            // Rectangles were expanded into their own vertex buffers.
            let (count, from) = if rects {
                (vertices / 3 * 6, 0)
            } else {
                (vertices, first)
            };
            #[cfg(feature = "diagnostics")]
            list.note_op(|| self.op_text(format!("draw {count}x{instances}"), false));
            #[cfg(feature = "diagnostics")]
            crate::gpusplit::mark(list, crate::gpusplit::Group::Draw, self.pipeline);
            // SAFETY: an open list with everything set.
            unsafe {
                list.list
                    .DrawInstanced(count, instances, from, first_instance)
            };
            emitted(Command::Draw, 1);
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        if !ran {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::pipeline::note_drop_reason(DropReason::NoPipeline);
            #[cfg(feature = "diagnostics")]
            dropped(self.pipeline);
        }
    }

    /// Whether the bound pipeline completes rectangles itself, in the
    /// geometry stage rectangle pipelines get (`HostPipeline::rects`).
    fn completes_rects(&self) -> bool {
        crate::pipeline::host_pipeline(self.pipeline as This).is_some_and(|p| p.rects)
    }

    /// Binds vertex buffers holding the rectangles of vertices `first..first
    /// + count` as triangles, made when the list is submitted.
    fn rectangles(&mut self, list: &HostList, count: u32, first: u32) -> bool {
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return false;
        };
        let rects = u64::from(count / 3);
        for slot in 0..32 {
            let elements: Vec<(u32, u32)> = p
                .inputs
                .iter()
                .filter(|e| e.slot == slot && !e.per_instance)
                .map(|e| (e.offset, e.format))
                .collect();
            if elements.is_empty() {
                continue;
            }
            let v = self.vertex[slot as usize];
            let stride = u64::from(v.stride);
            let from = v.address + u64::from(first) * stride;
            if v.address == 0
                || stride == 0
                || u64::from(first + count) * stride > u64::from(v.size)
                || crate::shadow::find(from, rects * 3 * stride).is_none()
            {
                once!(slot, "rectangle vertices outside their buffer");
                return false;
            }
            let Some((gpu, to)) = list.scratch(rects * 6 * stride) else {
                return false;
            };
            let (from, to, stride) = (from as usize, to as usize, stride as usize);
            let n = rects as usize;
            // Data, not a boxed closure: a rectangle draw made one closure
            // per vertex stream.
            let work = crate::host::Rectangles::new(from, to, stride, n, &elements);
            match work {
                Some(work) => list.defer_host_rects(work),
                None => list.defer_host(move || {
                    crate::host::expand_rects_now(from, to, stride, n, &elements)
                }),
            }
            let view = D3D12_VERTEX_BUFFER_VIEW {
                BufferLocation: gpu,
                SizeInBytes: (rects * 6 * stride as u64) as u32,
                StrideInBytes: v.stride,
            };
            // SAFETY: an open list; the view names live scratch memory.
            unsafe { list.list.IASetVertexBuffers(slot, Some(&[view])) };
            emitted(Command::Vertex, 1);
            // The list no longer holds the views a draw of the title's set
            // (`State::prepare_for`): the next one sets its own again.
            self.host_vertex.1 = u32::MAX;
        }
        true
    }

    pub(crate) fn draw_indexed(
        &mut self,
        list: &HostList,
        indices: u32,
        instances: u32,
        first: u32,
        base: i32,
        first_instance: u32,
    ) {
        self.close_window();
        #[cfg(feature = "diagnostics")]
        self.trace("indexed", indices, first);
        // Indexed rectangles are drawn by the rectangle geometry stage
        // alone (nothing expands them on the CPU).
        let lost = self.topology == RECTANGLES && !self.completes_rects();
        if lost {
            once!("indexed rectangles not drawn");
        }
        let ran = !lost && self.prepare(list, true);
        if ran {
            #[cfg(feature = "diagnostics")]
            list.note_op(|| self.op_text(format!("indexed {indices}x{instances}"), false));
            #[cfg(feature = "diagnostics")]
            crate::gpusplit::mark(list, crate::gpusplit::Group::Draw, self.pipeline);
            // SAFETY: an open list with everything set.
            unsafe {
                list.list
                    .DrawIndexedInstanced(indices, instances, first, base, first_instance)
            };
            emitted(Command::Draw, 1);
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        if !ran {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::pipeline::note_drop_reason(DropReason::NoPipeline);
            #[cfg(feature = "diagnostics")]
            dropped(self.pipeline);
        }
    }
}

impl State {
    /// Indirect execution of a one-argument signature of `kind` (PC
    /// numbering: 0 draw, 1 indexed draw, 2 dispatch, 10 mesh dispatch) with
    /// records `stride` bytes apart at title address `arguments`, and the
    /// record count at `count` if the title gives one: the GPU reads them
    /// from the buffer's host copy, where earlier dispatches may have
    /// written them. False if it cannot run on the host.
    pub(crate) fn execute_indirect(
        &mut self,
        list: &HostList,
        (kind, stride, max): (u32, u32, u32),
        arguments: u64,
        count: Option<u64>,
    ) -> bool {
        // Skip before the argument clamp can mutate the indirect buffer.
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            crate::pipeline::note_drop(self.pipeline as This);
            return false;
        }
        if crate::pipeline::is_builder(self.pipeline as This) && !crate::pipeline::builder_runs() {
            return false;
        }
        const DRAW: u32 = 0;
        const INDEXED: u32 = 1;
        const DISPATCH: u32 = 2;
        const MESH: u32 = 10;
        // Why an indirect execution was not run: counted in every build (one
        // relaxed add, nothing built), and spelled out for the frame
        // listing in diagnostics builds, which is where the formatted
        // reason is worth its allocation.
        #[cfg(feature = "diagnostics")]
        let why = |reason: DropReason, what: &'static str| {
            crate::pipeline::note_drop_reason(reason);
            crate::pipeline::note_why(format!("indirect kind {kind}: {what}"));
        };
        #[cfg(not(feature = "diagnostics"))]
        let why =
            |reason: DropReason, _what: &'static str| crate::pipeline::note_drop_reason(reason);
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            crate::pipeline::note_drop_reason(DropReason::NoPipeline);
            #[cfg(feature = "diagnostics")]
            crate::pipeline::note_drop(self.pipeline as This);
            return false;
        };
        // Indirect rectangles (the title's tiled lighting, 256
        // indirect draws a frame, makes their corners from the vertex
        // number) are completed by the pipeline's geometry stage: the
        // arguments stay the title's, three vertices a rectangle. They
        // cannot be expanded on the CPU, which does not know their count.
        // shortcut: in a pipeline without that stage, a rectangle with no
        // per-vertex inputs is drawn as its first triangle (three vertices,
        // half of it), and one with vertex buffers is not drawn.
        let made_rects = !p.rects
            && kind == DRAW
            && self.topology == RECTANGLES
            && !p.inputs.iter().any(|e| !e.per_instance);
        if made_rects {
            once!("indirect rectangles without vertex inputs drawn as their first triangle");
        }
        let matches_kind = match kind {
            DRAW | INDEXED => {
                (self.topology != RECTANGLES || made_rects || p.rects) && !p.compute && !p.mesh
            }
            MESH => !p.compute && p.mesh,
            DISPATCH => p.compute,
            _ => false,
        };
        if !matches_kind {
            // The only reason whose text needs the pipeline's own kinds:
            // counted here, and spelled out for the frame listing that
            // diagnostics builds print.
            crate::pipeline::note_drop_reason(DropReason::Mismatch);
            #[cfg(feature = "diagnostics")]
            crate::pipeline::note_why(format!(
                "indirect kind {kind}: pipeline mismatch (compute {}, mesh {}, topology {})",
                p.compute, p.mesh, self.topology
            ));
            return false;
        }
        let len = u64::from(stride) * u64::from(max.max(1));
        let Some(args) = crate::shadow::find_any(arguments, len) else {
            why(DropReason::Arguments, "arguments outside title buffers");
            return false;
        };
        let counter = match count {
            Some(at) => match crate::shadow::find_any(at, 4) {
                Some(s) => Some((s, at)),
                None => {
                    why(DropReason::Count, "count outside title buffers");
                    return false;
                }
            },
            None => None,
        };
        let Some(signature) = matches!(kind, DRAW | INDEXED | DISPATCH | MESH)
            .then(|| host_signature(kind, stride))
            .flatten()
        else {
            why(DropReason::Signature, "no host signature");
            return false;
        };
        if kind == MESH && (mesh_test() == 1 || !mesh_kept(self.pipeline)) {
            why(
                DropReason::MeshOff,
                "mesh draws are switched off (VICTORY_MESH_TEST=skip)",
            );
            return false;
        }
        let offset = args.offset(arguments);
        // `VICTORY_CLAMP=off` (`crate::indirect::clamp_on`): the title's own
        // arguments at their own offset, and nothing of ours recorded. A
        // malformed record then reaches the card unchecked, so this arm is for
        // measuring what a clamp costs, not for playing.
        // The third: whether a clamp of this run moved a scratch buffer to
        // `INDIRECT_ARGUMENT`, which is the only case `done` has to undo. A
        // record a window holds is read from a buffer `done` left in `COMMON`
        // (`Window`), and the title's own buffer is in `COMMON` throughout.
        let (bounded, at, moved) = if !crate::indirect::clamp_on() {
            args.refresh(list, arguments, len);
            if let Some((s, at)) = &counter {
                s.refresh(list, *at, 4);
            }
            if self.window.take().is_some() {
                windowed_note(Windowed::Off);
            }
            crate::indirect::skipped();
            (args.buffer.clone(), offset, false)
        } else {
            // A window of records clamped in one pass (`windows`): for single
            // records without a count buffer, drawn by shaders that store to
            // no buffer. Anything else ends the window: it uses the scratch
            // buffer or may write the arguments.
            const WINDOW_USE: usize = usize::MAX - 0x77;
            let stores = p.stages.iter().any(|stage| stage.translated().stores);
            let windowed = windows() && max == 1 && count.is_none() && !stores;
            if !windowed {
                if self.window.is_some() {
                    windowed_note(if windows() && !stores {
                        Windowed::Count
                    } else {
                        Windowed::Off
                    });
                    self.window = None;
                }
            } else if list.first_use(WINDOW_USE) {
                // Another host list (a new one after a cut, or this one after
                // its reset) has another scratch buffer, or this one's with
                // old bytes.
                if self.window.is_some() {
                    windowed_note(Windowed::List);
                    self.window = None;
                }
            }
            let same = |w: &Window| w.copy == args.id() && w.kind == kind && w.stride == stride;
            let hit = self
                .window
                .as_ref()
                .filter(|w| same(w) && offset >= w.base)
                .filter(|w| {
                    (offset - w.base) % u64::from(w.step) == 0
                        && (offset - w.base) / u64::from(w.step) < u64::from(w.records)
                })
                .map(|w| (w.scratch.clone(), offset - w.base));
            if hit.is_none() && windowed && self.window.is_some() {
                windowed_note(Windowed::Bounds);
            }
            let (bounded, at, moved) = match hit {
                Some(hit) => {
                    windowed_note(Windowed::Hit);
                    // Nothing was clamped and nothing moved: the record is one
                    // an earlier clamp wrote, and `done` below returned the
                    // scratch buffer to `COMMON` then, so this run reads it
                    // from there.
                    (hit.0, hit.1, false)
                }
                None => {
                    // A record a short step after the last window's last one
                    // (the same step as that window's, if it had one): the
                    // title walks the buffer, so the ones that step apart
                    // after it are clamped with it.
                    let last = self
                        .window
                        .as_ref()
                        .filter(|w| windowed && same(w))
                        .map(|w| (w.base, w.records, w.step));
                    let (step, records) = ahead(last, offset, args.left(arguments), stride, max);
                    args.refresh(list, arguments, len.max(u64::from(step) * u64::from(records)));
                    if let Some((s, at)) = &counter {
                        s.refresh(list, *at, 4);
                    }
                    // Bounded first: the clamp binds a compute pipeline of its
                    // own and copies clamped active records into a scratch
                    // buffer at offset 0. `held` is what the list holds of that
                    // state, so a clamp right after another clamps it with the
                    // signature and pipeline the list already has.
                    let mut held = crate::indirect::Held {
                        pipeline: self.host_pipeline,
                        root: self.host_roots[1],
                    };
                    let count_arg = counter.as_ref().map(|(s, at)| (&s.buffer, s.offset(*at)));
                    // The clamp's dispatch (and its two waits) is the group
                    // `VICTORY_GPU_SPLIT` charges for it, so the timestamp
                    // goes in before it records: a clamp that refuses leaves
                    // a segment of no time, not a wrong one.
                    #[cfg(feature = "diagnostics")]
                    crate::gpusplit::mark(list, crate::gpusplit::Group::Clamp, 0);
                    // More than the record asked for: a window clamped ahead,
                    // whose unfilled slots must not spend the list's budget.
                    let bounded = if windowed && records > 1 {
                        crate::indirect::bound_ahead(
                            list,
                            (&args.buffer, offset),
                            (kind, step, records),
                            &mut held,
                        )
                    } else {
                        crate::indirect::bound(
                            list,
                            (&args.buffer, offset),
                            count_arg,
                            (kind, step, records),
                            &mut held,
                        )
                    };
                    let Some(bounded) = bounded else {
                        self.window = None;
                        why(
                            DropReason::Unbounded,
                            "not bounded (no clamp, or arguments not 4-byte aligned)",
                        );
                        return false;
                    };
                    // The clamp set its own compute pipeline and root
                    // signature; `bind` sets both again unless the list
                    // already held them. A run whose record a window already
                    // holds records no clamp, and takes nothing from the list.
                    self.hold_clamp(records, &held);
                    if records > 0 {
                        windowed_note(Windowed::Clamp);
                    }
                    self.window = windowed.then(|| Window {
                        copy: args.id(),
                        kind,
                        stride,
                        step,
                        base: offset,
                        records,
                        scratch: bounded.clone(),
                    });
                    // The clamp transitions the scratch buffer
                    // `COMMON -> UNORDERED_ACCESS -> INDIRECT_ARGUMENT`
                    // (`crate::indirect`), whether or not it clamped a record.
                    (bounded, 0, true)
                }
            };
            (bounded, at, moved)
        };
        let ran = match kind {
            DRAW | INDEXED => self.prepare(list, kind == INDEXED),
            MESH => self.prepare_for(list, false, true),
            DISPATCH => self.bind(list, p, true),
            _ => false,
        };
        if !ran {
            if moved {
                crate::indirect::done(list, &bounded);
            }
            why(DropReason::Unbindable, "prepare failed");
            return false;
        }
        #[cfg(feature = "diagnostics")]
        list.note_op(|| {
            self.op_text(format!("indirect kind {kind} max {max} at {arguments:#x}"), kind == DISPATCH)
        });
        if kind == MESH {
            self.mesh_scissor(list, true);
        }
        #[cfg(feature = "diagnostics")]
        crate::gpusplit::mark(list, crate::gpusplit::Group::Indirect, self.pipeline);
        // SAFETY: an open list with everything set; the scratch buffer and
        // host copies are kept alive by the list, and the signature is cached.
        unsafe {
            list.list.ExecuteIndirect(
                &signature,
                max,
                &bounded,
                at,
                counter.as_ref().map(|(s, _)| &s.buffer),
                counter.as_ref().map_or(0, |(s, at)| s.offset(*at)),
            )
        };
        if kind == MESH {
            self.mesh_scissor(list, false);
        }
        if moved {
            crate::indirect::done(list, &bounded);
        }
        emitted(Command::Execute, 1);
        if kind == DISPATCH {
            emitted(Command::Dispatch, 1);
            DISPATCHED.fetch_add(1, Ordering::Relaxed);
        } else {
            emitted(Command::Draw, 1);
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        true
    }
}

/// The host command signature for one argument of `kind` with records
/// `stride` bytes apart (no root arguments, so no root signature), made
/// once.
fn host_signature(kind: u32, stride: u32) -> Option<ID3D12CommandSignature> {
    static MADE: std::sync::Mutex<Vec<((u32, u32), ID3D12CommandSignature)>> =
        std::sync::Mutex::new(Vec::new());
    let mut made = MADE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, s)) = made.iter().find(|(k, _)| *k == (kind, stride)) {
        return Some(s.clone());
    }
    let argument = D3D12_INDIRECT_ARGUMENT_DESC {
        Type: D3D12_INDIRECT_ARGUMENT_TYPE(kind as i32),
        ..Default::default()
    };
    let desc = D3D12_COMMAND_SIGNATURE_DESC {
        ByteStride: stride,
        NumArgumentDescs: 1,
        pArgumentDescs: &argument,
        NodeMask: 0,
    };
    let mut signature: Option<ID3D12CommandSignature> = None;
    // SAFETY: a description of one argument; no root arguments.
    let result = unsafe {
        crate::host::host()?.device.CreateCommandSignature(
            &desc,
            None::<&ID3D12RootSignature>,
            &mut signature,
        )
    };
    if let Err(e) = result {
        once!(error = %e, kind, stride, "host command signature refused");
        return None;
    }
    let signature = signature?;
    made.push(((kind, stride), signature.clone()));
    Some(signature)
}

/// Gets what a shader reads or writes through a view ready: buffer data
/// copied, textures moved to the state they need.
fn view_kind(view: &View) -> Option<ResourceType> {
    match view {
        View::Shader(..) | View::Placed(false, ..) => Some(ResourceType::Srv),
        View::Unordered(..) | View::Placed(true, ..) => Some(ResourceType::Uav),
        _ => None,
    }
}

/// The host state a depth view needs of its texture: the read state if the
/// view writes no plane the format has (the title's read-only flags), else
/// the write state.
/// shortcut: whole textures have one state, so a view that writes one of
/// two planes counts as writing.
fn depth_state(texture: &ID3D12Resource, desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>) -> u32 {
    // D16_UNORM and D32_FLOAT: depth formats without a stencil plane.
    const ONE_PLANE: [i32; 2] = [55, 40];
    let Some(desc) = desc else {
        return DEPTH_WRITE;
    };
    let format = if desc.Format.0 == 0 {
        // SAFETY: a live texture; a view without a format has its own.
        unsafe { texture.GetDesc() }.Format.0
    } else {
        desc.Format.0
    };
    let mut needed = D3D12_DSV_FLAG_READ_ONLY_DEPTH.0;
    if !ONE_PLANE.contains(&format) {
        needed |= D3D12_DSV_FLAG_READ_ONLY_STENCIL.0;
    }
    if desc.Flags.0 & needed == needed {
        DEPTH_READ
    } else {
        DEPTH_WRITE
    }
}

fn readable(list: &HostList, kind: ResourceType, view: &View) {
    const UNORDERED_ACCESS: u32 = 0x8;
    let placed = crate::descriptor::host::placed_texture(view);
    let view = placed.as_ref().unwrap_or(view);
    // Buffer views read title memory: their range is copied at submission;
    // what shaders write through unordered access goes back to it after.
    if let Some((shadow, address, len)) = crate::descriptor::host::buffer_copy(view) {
        shadow.refresh(list, address, len);
        if kind == ResourceType::Uav {
            // A shader that cannot store to buffers leaves the range as
            // the title has it: nothing to find afterwards.
            if STORES.get() || !write_skip() {
                shadow.gpu_writes(list, address, len);
            } else {
                #[cfg(feature = "diagnostics")]
                {
                    crate::copystat::add(crate::copystat::C::GpuWritesLeftOut, 1);
                    crate::copystat::add(crate::copystat::C::GpuWritesLeftOutBytes, len);
                }
            }
        }
    }
    match (view, kind) {
        (View::Shader(resource, _), ResourceType::Srv) => {
            // Sampled: counts the hit that admits a texture seen again and
            // again to the protected set (`crate::resource::note_sampled`).
            crate::resource::note_sampled(*resource as This);
            crate::present::note_sampled(*resource as This);
            // A view the title placed, in another format, in the memory of
            // a texture the host writes: that texture's texels, copied on
            // the host (title memory does not have them).
            let aliased = crate::resource::alias_of(*resource as This)
                .is_some_and(|source| crate::texture::alias(list, *resource as This, source));
            if !aliased
                && let Some(t) = crate::resource::host_resource(*resource as This)
                && crate::texture::title_written(t)
            {
                crate::texture::upload(list, *resource as This, t);
            }
            // A depth texture shaders also write: what they wrote, first.
            list.twin_use(*resource as This, false, false);
            // The depth target the draw only tests is read in both ways at
            // once: both read states, which its preparation asks for too.
            let (tested, _) = TESTED_DEPTH.get();
            let state = if tested != 0 && tested == *resource {
                TESTED_DEPTH.set((tested, true));
                DEPTH_READ | SHADER_RESOURCE
            } else {
                SHADER_RESOURCE
            };
            crate::list::ensure_state(list, *resource as This, state);
            #[cfg(feature = "diagnostics")]
            if let Some(t) = crate::resource::host_resource(*resource as This) {
                // SAFETY: a live resource.
                let d = unsafe { t.GetDesc() };
                if d.Flags.0 & 7 == 0 {
                    tracing::debug!(
                        resource = format_args!("{resource:#x}"),
                        ?d,
                        "a shader reads a texture the title fills"
                    );
                }
            }
        }
        (View::Unordered(resource, _, _), ResourceType::Uav) => {
            // A shader writes it: a target, as a render target is
            // (`crate::present`).
            crate::present::note_target(*resource as This);
            #[cfg(feature = "diagnostics")]
            crate::capture::note_target(*resource as This);
            // Unordered access to a depth texture is its colour twin's.
            if !list.twin_use(*resource as This, true, true) {
                crate::list::ensure_state(list, *resource as This, UNORDERED_ACCESS);
            }
        }
        _ => {}
    }
}

/// A view in a few words (diagnostics builds).
#[cfg(feature = "diagnostics")]
fn describe(view: Option<&View>) -> String {
    let texture = |r: This| {
        crate::resource::host_made(r).map(|t| {
            // SAFETY: a live resource.
            let d = unsafe { t.GetDesc() };
            format!("{}x{} f{} flags {:#x} of {r:?}", d.Width, d.Height, d.Format.0, d.Flags.0)
        })
    };
    match view {
        None => "no view".into(),
        Some(v @ View::Shader(r, _)) => texture(*r as This)
            .unwrap_or_else(|| format!("buffer {:?}", crate::descriptor::host::buffer_range(v))),
        Some(v @ View::Unordered(r, ..)) => match texture(*r as This) {
            Some(t) => format!("uav texture {t}"),
            None => format!(
                "uav resource {r:#x} buffer {:?}",
                crate::descriptor::host::buffer_range(v)
            ),
        },
        Some(View::Placed(u, r, d)) => format!(
            "placed uav={u} dim {} {}x{} f{} layout {:#x} at {:#x}",
            r[0],
            u64::from(r[4]) | u64::from(r[5]) << 32,
            r[6],
            r[8],
            r[11],
            u64::from(d[10]) | u64::from(d[11]) << 32
        ),
        // The first four values, as a colour rectangle's constants are.
        Some(View::Constant(address, size))
            if *size >= 16 && crate::texture::committed(*address as *const u8, 16) =>
        {
            // SAFETY: 16 committed bytes of title memory.
            let v = unsafe { (*address as *const [f32; 4]).read_unaligned() };
            format!("Constant({address:#x}, {size}) {v:?}")
        }
        Some(v) => format!("{v:?}").chars().take(60).collect(),
    }
}

/// The console's own primitive topologies.
const TRIANGLE_FAN: u32 = 6;
const RECTANGLES: u32 = 17;
const LINE_LOOP: u32 = 18;
const QUADS: u32 = 19;
const QUAD_STRIP: u32 = 20;

/// Fixed-function state, straight onto the host list.
pub(crate) fn viewports(list: &HostList, viewports: &[D3D12_VIEWPORT]) {
    // SAFETY: an open list.
    unsafe { list.list.RSSetViewports(viewports) };
}

pub(crate) fn scissors(list: &HostList, rects: &[RECT]) {
    // SAFETY: an open list.
    unsafe { list.list.RSSetScissorRects(rects) };
}

pub(crate) fn blend_factor(list: &HostList, factor: Option<&[f32; 4]>) {
    // SAFETY: an open list.
    unsafe { list.list.OMSetBlendFactor(factor) };
}

pub(crate) fn stencil_ref(list: &HostList, reference: u32) {
    #[cfg(feature = "diagnostics")]
    crate::capture::op(std::ptr::from_ref(list) as usize, || format!("STENCIL REF {reference:#x}"));
    // SAFETY: an open list.
    unsafe { list.list.OMSetStencilRef(reference) };
}

pub(crate) fn depth_bounds(list: &HostList, min: f32, max: f32) {
    #[cfg(feature = "diagnostics")]
    crate::capture::op(std::ptr::from_ref(list) as usize, || {
        format!("DEPTH BOUNDS {min}..{max}")
    });
    if crate::host::host().is_some_and(|h| h.depth_bounds())
        && let Ok(l1) = list.list.cast::<ID3D12GraphicsCommandList1>()
    {
        // SAFETY: an open list on a device that supports depth bounds.
        unsafe { l1.OMSetDepthBounds(min, max) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window of records clamped ahead of their draws: a title walking
    /// one argument buffer a record a call opens with 16, doubles to
    /// [`WINDOW_MOST`] at most, and ends the moment the walk stops being the
    /// one seen (a step that differs from the window's, one shorter than the
    /// named stride, past 256 bytes, or not a multiple of four), where one
    /// record is clamped as without a window.
    #[test]
    fn an_indirect_walk_widens_by_doubling_and_ends_when_the_step_changes() {
        const LEFT: u64 = 1 << 20;
        // No window yet: the title's own stride and count.
        assert_eq!(ahead(None, 0x1000, LEFT, 12, 1), (12, 1));
        // A record 32 bytes after a one-record window's record: 16 ahead.
        let one = Some((0x1000, 1, 12));
        assert_eq!(ahead(one, 0x1000 + 32, LEFT, 12, 1), (32, 16));
        // The 16 records drawn, the next 32 after the last of them: 32.
        let window = Some((0x1000, 16, 32));
        assert_eq!(ahead(window, 0x1000 + 16 * 32, LEFT, 12, 1), (32, 32));
        // Then 64, and no further.
        let window = Some((0x1000, 32, 32));
        assert_eq!(ahead(window, 0x1000 + 32 * 32, LEFT, 12, 1), (32, 64));
        let window = Some((0x1000, 64, 32));
        assert_eq!(ahead(window, 0x1000 + 64 * 32, LEFT, 12, 1), (32, 64));
        // A window never reaches past the copy: 100 bytes left, 32 apart, 3.
        assert_eq!(ahead(one, 0x1000 + 32, 100, 12, 1), (32, 3));
        // Nothing left after the record: a window of the one record.
        assert_eq!(ahead(one, 0x1000 + 32, 16, 12, 1), (32, 1));
        // A step other than the window's ends it (16 where it walked 32).
        let window = Some((0x1000, 16, 32));
        assert_eq!(ahead(window, 0x1000 + 16 * 32 - 16, LEFT, 12, 1), (12, 1));
        // As do one shorter than the named stride, one not a multiple of
        // four, and one past 256 bytes.
        assert_eq!(ahead(one, 0x1000 + 8, LEFT, 12, 1), (12, 1));
        assert_eq!(ahead(one, 0x1000 + 34, LEFT, 12, 1), (12, 1));
        assert_eq!(ahead(one, 0x1000 + 260, LEFT, 12, 1), (12, 1));
        // A title that moves backwards (or does not move) ends it too.
        assert_eq!(ahead(window, 0x1000, LEFT, 12, 1), (12, 1));
    }

    /// The registry revision the tests of the fresh-table cache ask under:
    /// one of their own, so whatever the other tests do to
    /// `crate::shadow`'s counter cannot move it.
    const REV: u64 = 1;

    /// A refusal a draw meets on every frame is counted, and its line names
    /// which occurrence it is: the first, the tenth, the hundredth, and the
    /// running total (`victory_core::logging::warn_once`). A `Once` said it
    /// once and the log then read as if the refusal had stopped - reverting
    /// either site leaves its key out of the totals this reads.
    #[test]
    fn a_draw_refusal_says_which_occurrence_it_is() {
        assert!(refused_table_span(3, 4), "the first occurrence is logged");
        for nth in 2..10 {
            assert!(
                !refused_table_span(3, 4),
                "occurrence {nth} is counted, not logged"
            );
        }
        assert!(
            refused_table_span(3, 4),
            "the tenth occurrence is logged again"
        );
        assert!(refused_uav_unbound(), "the first UAV refusal is logged");
        let totals = victory_core::logging::warn_once_totals();
        let count = |key: &str| {
            totals
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, count)| *count)
        };
        assert_eq!(count("draw.table_span"), Some(10), "{totals:?}");
        assert_eq!(count("draw.uav_unbound"), Some(1), "{totals:?}");
    }

    /// A copy answer is used only while the registry of copies still says
    /// the same thing (`crate::shadow::revision`): title memory remapped
    /// under the same addresses, or a buffer or window added or let go, can
    /// make an address name another copy, and a list that kept the old
    /// answer would bind the copy made for what was there before until its
    /// tables were given back. An answer of a moved revision is asked again,
    /// and the answer asked again is the one used from then on.
    #[test]
    fn a_copy_answer_is_asked_again_when_the_registry_of_copies_changes() {
        let key = (0x7ff6_2a00_1000u64, 0x40u64);
        let mut memo = Memo::<u32>::default();
        // The answer made under revision 5: the copy's number, here.
        memo.put(key, 5, 1);
        assert_eq!(memo.get(key, 5), Some(1), "the revision it was made under keeps it");
        // The registry moved under it: not the copy the address names now.
        assert_eq!(memo.get(key, 6), None, "an answer made under 5 is not used under 6");
        // Asked again, the new answer is kept under the new revision.
        memo.put(key, 6, 2);
        assert_eq!(memo.get(key, 6), Some(2), "the answer made under 6");
        assert_eq!(memo.get(key, 5), None, "and not the answer it replaced");
        // A revision that moves back is no reason to serve the old answer:
        // answers are never of two revisions at once.
        memo.put(key, 7, 3);
        assert_eq!(memo.get(key, 7), Some(3));
        assert_eq!(memo.get(key, 6), None);
    }

    /// The memo keeps [`COPY_CACHE`] answers at most, starting over on the
    /// next: a list that binds a range per draw holds a copy of every window
    /// it ever saw otherwise.
    #[test]
    fn the_copy_memo_starts_over_at_its_cap() {
        let mut memo = Memo::<u32>::default();
        for k in 0..COPY_CACHE as u64 {
            memo.put((k, 1), 0, k as u32);
        }
        assert_eq!(memo.get((0, 1), 0), Some(0), "the first answer is there");
        // One past the cap: the memo starts over, so only the new answer is.
        memo.put((COPY_CACHE as u64, 1), 0, 7);
        assert_eq!(memo.get((0, 1), 0), None);
        assert_eq!(memo.get((COPY_CACHE as u64, 1), 0), Some(7));
    }

    /// The step the pass takes after looking at a texture it uploaded whole
    /// (`heap_textures`) ends the schedule after `RECHECK_FRAMES`' five
    /// looks, whether the memory changed each time or never. The fingerprint
    /// is a number here, so no card is needed; the step itself is the one
    /// the pass runs.
    #[test]
    fn a_texture_looked_at_again_settles_after_the_schedule() {
        let print = 0x1234_5678_9abc_def1u64;
        for changed in [false, true] {
            let mut frame = 0;
            let mut looks = 0u8;
            let mut made = 0;
            // The arm the pass makes for a first upload.
            let mut due = frame + RECHECK_FRAMES[0];
            loop {
                frame = due;
                made += 1;
                // The upload's own fingerprint, or another one each look.
                let seen = if changed { print ^ made as u64 } else { print };
                let looked = recheck_step(frame, looks, look_changed(print, seen, || true));
                assert_eq!(looked.changed, changed, "look {made}, changed {changed}");
                match looked.again {
                    Some((next_due, next_looks)) => {
                        assert_eq!(next_looks, looks + 1, "look {made}");
                        assert_eq!(
                            next_due,
                            frame + RECHECK_FRAMES[usize::from(next_looks)],
                            "look {made}"
                        );
                        (due, looks) = (next_due, next_looks);
                    }
                    None => break,
                }
                assert!(made <= RECHECK_FRAMES.len(), "looked at {made} times");
            }
            // Five looks, then nothing: a texture the title rewrites for ever
            // is left alone instead of uploaded every fourth frame.
            assert_eq!(made, RECHECK_FRAMES.len(), "changed {changed}");
        }
        // The first look is the first wait of the schedule.
        assert_eq!(RECHECK_FRAMES[0], 4);
    }

    /// A look at a texture whose memory is not there is no evidence of
    /// content: `texture::fingerprint` reads a page the title demapped as
    /// zeros, so a texture streamed out differs at every look with nothing
    /// written to it. Read as a change, it was warned about (`with nothing to
    /// tell of it`, 233 times in `run-20261125-055341.log`) and uploaded
    /// again with zeros. The mapping is asked only when the bytes differ.
    #[test]
    fn a_texture_streamed_out_is_no_change_of_content() {
        let print = 0x1234_5678_9abc_def1u64;
        // Its memory is gone: what the kernel reads is zeros, not content.
        assert!(!look_changed(print, 0, || false), "a demapped texture is no change");
        // Its memory is there and the bytes are its own: no change either,
        // and the mapping is not asked for.
        let mut asked = 0;
        assert!(!look_changed(print, print, || {
            asked += 1;
            true
        }));
        assert_eq!(asked, 0, "the mapping was asked of a texture that did not change");
        // Its memory is there and the bytes differ: content arrived.
        assert!(look_changed(print, print ^ 1, || true));
        // A texture streamed out is left to the pass's own check
        // (`texture_slot_gone`, `texture_slot_later`), on the same schedule.
        let looked = recheck_step(4, 0, look_changed(print, 0, || false));
        assert!(!looked.changed);
        assert_eq!(looked.again, Some((4 + RECHECK_FRAMES[1], 1)), "the look still counts");
    }

    /// The records of one descriptor table, as the title writes them: three
    /// dwords at the start of each 32-byte slot.
    struct Records {
        words: Vec<u32>,
        count: u32,
    }

    impl Records {
        fn new(count: u32) -> Records {
            Records {
                words: vec![0; count as usize * 8],
                count,
            }
        }

        /// Where the table is: the address the draw path reads its records
        /// from.
        fn at(&self) -> u64 {
            self.words.as_ptr() as u64
        }

        /// Rewrites one record: what the title does to a table in place.
        fn set(&mut self, slot: usize, record: [u32; 3]) {
            self.words[slot * 8..slot * 8 + 3].copy_from_slice(&record);
        }
    }

    /// The identity a fresh table had before: `DefaultHasher` over the
    /// records' three dwords, part of the key.
    fn hashed(table: u64, count: u32) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for k in 0..count {
            let at = table + u64::from(k) * 32;
            // SAFETY: a record of the test's own buffer.
            unsafe { (at as *const [u32; 3]).read_unaligned() }.hash(&mut h);
        }
        h.finish()
    }

    /// The dynamic slots each draw's tables were given, as the draw path
    /// decides them with the identity it has now.
    fn stream(records: &mut [Records], script: &[Vec<usize>]) -> Vec<(u64, u32, u32)> {
        let mut state = State::default();
        let mut out = Vec::new();
        let mut next = 0u32;
        for (draw, tables) in script.iter().enumerate() {
            if draw == SCRIPT / 2 {
                // The title rewrites a table in place: the slots made for
                // its old records must not be served for the new ones.
                records[1].set(2, [0x1234_5678, 0x9abc_def0, 7]);
            }
            for &table in tables {
                let record = &records[table];
                let identity = (record.at(), record.count);
                let words = Fingerprint::of(record.at(), record.count);
                let first = match state.fresh_table(identity, words, REV) {
                    Some(first) => first,
                    None => {
                        let first = next;
                        next += record.count;
                        state.fresh_made(identity, words, first, REV);
                        first
                    }
                };
                out.push((identity.0, identity.1, first));
            }
        }
        out
    }

    /// The same, with the identity the draw path had before.
    fn old_stream(records: &mut [Records], script: &[Vec<usize>]) -> Vec<(u64, u32, u32)> {
        let mut map: std::collections::HashMap<(u64, u32, u64), u32> =
            std::collections::HashMap::new();
        let mut out = Vec::new();
        let mut next = 0u32;
        for (draw, tables) in script.iter().enumerate() {
            if draw == SCRIPT / 2 {
                records[1].set(2, [0x1234_5678, 0x9abc_def0, 7]);
            }
            for &table in tables {
                let record = &records[table];
                let key = (record.at(), record.count, hashed(record.at(), record.count));
                let first = *map.entry(key).or_insert_with(|| {
                    let first = next;
                    next += record.count;
                    first
                });
                out.push((key.0, key.1, first));
            }
        }
        out
    }

    /// A list of `SCRIPT` draws, each binding one or three tables.
    fn script() -> Vec<Vec<usize>> {
        (0..SCRIPT)
            .map(|draw| {
                if draw % 3 == 0 {
                    vec![0]
                } else {
                    vec![0, 1, 2]
                }
            })
            .collect()
    }

    /// Draws in the synthetic list.
    const SCRIPT: usize = 5_000;

    /// The fresh tables a draw binds are given the same slots as before,
    /// draw for draw: the recorded commands do not change.
    #[test]
    fn the_tables_a_draw_binds_keep_their_slots() {
        let script = script();
        // One set of records, so the addresses both identities see are the
        // same ones.
        let mut records = [Records::new(4), Records::new(12), Records::new(64)];
        let old = old_stream(&mut records, &script);
        records[1].set(2, [0, 0, 0]);
        let new = stream(&mut records, &script);
        assert_eq!(old.len(), new.len());
        for (draw, (was, now)) in old.iter().zip(&new).enumerate() {
            assert_eq!(was, now, "draw {draw}");
        }
        // The rewrite in the middle of the list made the table afresh
        // again: the slots it had are not served for the new records.
        let rewritten = old
            .iter()
            .zip(&new)
            .filter(|(_, (_, _, first))| *first != 0)
            .count();
        assert!(rewritten > 0, "the list made no table");
    }

    /// A table the title rewrites in place is never served the slots made
    /// for its old records.
    #[test]
    fn a_table_rewritten_in_place_is_never_stale() {
        let mut record = Records::new(8);
        let identity = (record.at(), record.count);
        let mut state = State::default();
        let words = Fingerprint::of(record.at(), record.count);
        assert_eq!(state.fresh_table(identity, words, REV), None);
        state.fresh_made(identity, words, 10, REV);
        assert_eq!(state.fresh_table(identity, words, REV), Some(10));
        // The title rewrites it.
        record.set(3, [1, 2, 3]);
        let words = Fingerprint::of(record.at(), record.count);
        assert_eq!(state.fresh_table(identity, words, REV), None, "a stale table");
        state.fresh_made(identity, words, 20, REV);
        assert_eq!(state.fresh_table(identity, words, REV), Some(20));
        // And back to what it was: the old slots hold the old records, so
        // they are not served either.
        record.set(3, [0, 0, 0]);
        let words = Fingerprint::of(record.at(), record.count);
        assert_eq!(state.fresh_table(identity, words, REV), None, "a stale table");
    }

    /// A table of records the title did not rewrite is made again all the
    /// same when the registry of host copies answered otherwise since: its
    /// slots name the copies its descriptors found then, and the records
    /// themselves (all the fingerprint sees) are unchanged by a remap.
    #[test]
    fn a_fresh_table_is_made_again_when_the_registry_of_copies_changes() {
        let record = Records::new(8);
        let identity = (record.at(), record.count);
        let words = Fingerprint::of(record.at(), record.count);
        let mut state = State::default();
        state.fresh_made(identity, words, 10, REV);
        assert_eq!(state.fresh_table(identity, words, REV), Some(10), "the same revision");
        // Title memory remapped under the table, or a window it names let
        // go: the descriptors may name other copies from now on.
        assert_eq!(state.fresh_table(identity, words, REV + 1), None, "another revision");
        state.fresh_made(identity, words, 20, REV + 1);
        assert_eq!(state.fresh_table(identity, words, REV + 1), Some(20), "the slots made again");
        assert_eq!(state.fresh_table(identity, words, REV), None, "and not the ones they replaced");
    }

    /// The bookkeeping a draw runs allocates nothing once its tables are
    /// made: the vec it keeps is the one the list keeps.
    #[cfg(not(feature = "diagnostics"))]
    #[test]
    fn a_draw_allocates_nothing_once_its_tables_are_made() {
        let records = [Records::new(4), Records::new(12), Records::new(64)];
        let mut state = State::default();
        let mut words = [Fingerprint::default(); 3];
        for (at, record) in records.iter().enumerate() {
            words[at] = Fingerprint::of(record.at(), record.count);
            state.fresh_made((record.at(), record.count), words[at], at as u32, REV);
        }
        let (hits, blocks) = heap::counted(|| {
            let mut hits = 0u32;
            for _ in 0..SCRIPT {
                for (at, record) in records.iter().enumerate() {
                    let identity = (record.at(), record.count);
                    if state.fresh_table(identity, words[at], REV).is_some() {
                        hits += 1;
                    }
                }
            }
            hits
        });
        assert_eq!(hits as usize, SCRIPT * 3);
        assert_eq!(blocks, 0, "the draw path allocated");
    }

    /// What one of the state calls a draw makes costs in our own code, so
    /// the report need not guess at it: the list's state lock, and the
    /// clock a diagnostics build takes around each call.
    #[test]
    fn a_state_call_costs_a_lock() {
        let rounds = 20_000u32;
        let lock = std::sync::Mutex::new(0u32);
        let started = std::time::Instant::now();
        for _ in 0..rounds {
            let mut value = lock.lock().unwrap_or_else(|e| e.into_inner());
            *value += 1;
        }
        let per_call = started.elapsed().as_nanos() as f64 / f64::from(rounds);
        // Twelve to fifteen state calls come between two draws.
        eprintln!(
            "one state lock: {per_call:.0} ns, {:.3} µs a draw at 15 calls",
            per_call * 15.0 / 1_000.0
        );
        assert!(per_call < 5_000.0, "one lock cost {per_call:.0} ns");

        // `with_state`'s two clocks under `feature = "diagnostics"`.
        let started = std::time::Instant::now();
        for _ in 0..rounds {
            let began = std::time::Instant::now();
            std::hint::black_box(began.elapsed().as_nanos() as u64);
        }
        let per_call = started.elapsed().as_nanos() as f64 / f64::from(rounds);
        eprintln!(
            "one state call's clock pair: {per_call:.0} ns, {:.3} µs a draw at 15 calls",
            per_call * 15.0 / 1_000.0
        );
        assert!(per_call < 5_000.0, "one clock pair cost {per_call:.0} ns");
    }

    /// What a table's identity costs a draw: the hash the draw path ran over
    /// every record before, and the fingerprint it runs now.
    #[test]
    fn a_table_identity_costs_what_it_reads() {
        let records = [Records::new(4), Records::new(12), Records::new(64)];
        let rounds = 20_000u32;
        let started = std::time::Instant::now();
        for _ in 0..rounds {
            for record in &records {
                std::hint::black_box(hashed(record.at(), record.count));
            }
        }
        let before = started.elapsed().as_nanos() as f64 / f64::from(rounds);
        let started = std::time::Instant::now();
        for _ in 0..rounds {
            for record in &records {
                std::hint::black_box(Fingerprint::of(record.at(), record.count));
            }
        }
        let after = started.elapsed().as_nanos() as f64 / f64::from(rounds);
        eprintln!("three tables' identity: {before:.0} ns a draw before, {after:.0} ns after");
        assert!(
            after < before * 2.0,
            "the fingerprint costs {after:.0} ns, the hash {before:.0} ns"
        );
    }

    /// The bookkeeping a draw ran before: one vec per stage, table and
    /// slot, made afresh every time. What the draw path allocates now, and
    /// what it allocated before.
    #[cfg(not(feature = "diagnostics"))]
    #[test]
    fn the_bookkeeping_a_draw_ran_before_allocated_per_draw() {
        let tables = [(0u8, 4u32), (1, 12), (2, 64)];
        let old = heap::counted(|| {
            for _ in 0..SCRIPT {
                let fresh: Vec<u8> = tables.iter().map(|&(slot, _)| slot).collect();
                let mut made: Vec<(u8, u32)> = Vec::new();
                for &(slot, count) in &tables {
                    let entries: Vec<u32> = (0..count).collect();
                    let selected: Vec<u32> = vec![0];
                    let values = vec![0u32; 8];
                    made.push((slot, count));
                    std::hint::black_box((&fresh, &entries, &selected, &values));
                }
                std::hint::black_box(&made);
            }
        });
        assert!(
            old.1 as usize >= SCRIPT,
            "the old bookkeeping did not allocate"
        );
        eprintln!(
            "allocations for {SCRIPT} draws: before {}, after 0",
            old.1
        );
    }

    /// Heap blocks the calling thread allocated while a closure ran.
    #[cfg(not(feature = "diagnostics"))]
    mod heap {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        thread_local! {
            /// Whether this thread's blocks are counted.
            static ON: Cell<bool> = const { Cell::new(false) };
            /// The blocks this thread allocated while they were.
            static BLOCKS: Cell<u64> = const { Cell::new(0) };
        }

        struct Counting;

        // SAFETY: every method passes the layout and the pointer on to
        // `System`; counting the blocks is the only work it adds.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                count();
                // SAFETY: the caller's contract, passed on.
                unsafe { System.alloc(layout) }
            }

            unsafe fn dealloc(&self, at: *mut u8, layout: Layout) {
                // SAFETY: the caller's contract, passed on.
                unsafe { System.dealloc(at, layout) }
            }

            unsafe fn realloc(&self, at: *mut u8, layout: Layout, size: usize) -> *mut u8 {
                count();
                // SAFETY: the caller's contract, passed on.
                unsafe { System.realloc(at, layout, size) }
            }

            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                count();
                // SAFETY: the caller's contract, passed on.
                unsafe { System.alloc_zeroed(layout) }
            }
        }

        /// One block, by the thread that asked for it (the tests run side by
        /// side, and a shared count would be theirs too).
        fn count() {
            let _ = ON.try_with(|on| {
                if on.get() {
                    let _ = BLOCKS.try_with(|blocks| blocks.set(blocks.get() + 1));
                }
            });
        }

        #[global_allocator]
        static HEAP: Counting = Counting;

        /// What `f` returned, and the blocks this thread allocated in it.
        pub(super) fn counted<T>(f: impl FnOnce() -> T) -> (T, u64) {
            let before = BLOCKS.with(|blocks| blocks.get());
            ON.with(|on| on.set(true));
            let done = f();
            ON.with(|on| on.set(false));
            (done, BLOCKS.with(|blocks| blocks.get()) - before)
        }
    }

    /// Catch-up tripled a pass's bytes and span whenever a backlog was due,
    /// even with the card full: the pass then spent three times as long
    /// (250 to 750 ms) refusing the same slots and uploaded nothing. A full
    /// card never triples.
    #[test]
    fn catch_up_cannot_run_while_the_card_refuses() {
        use std::time::Duration;
        let (bytes, span) = (4 << 20, Duration::from_millis(50));
        assert_eq!(
            pass_budget(true, true, CATCHUP_SLOTS, bytes, span),
            (bytes * 3, span * 3),
            "a backlog with room"
        );
        assert_eq!(
            pass_budget(true, false, CATCHUP_SLOTS, bytes, span),
            (bytes, span),
            "no room: the catch-up would only refuse for longer"
        );
        assert_eq!(
            pass_budget(true, true, CATCHUP_SLOTS - 1, bytes, span),
            (bytes, span),
            "no backlog"
        );
        assert_eq!(
            pass_budget(false, true, CATCHUP_SLOTS * 8, bytes, span),
            (bytes, span),
            "catch-up off"
        );
    }

    /// A synthetic slot-due trace: a small persistent set sampled every
    /// frame and many one-touch streaming pages. Replayed through frequency
    /// admission ([`crate::resource::admit_step`]) and the sweep's veto
    /// ([`crate::resource::sweepable`]). The old pass restarted from the
    /// same head every time (a refused slot left due), so the head never
    /// moved; the reclaim reschedules the slot it took a copy from
    /// (`texture_slot_later`), so the head advances through every slot. The
    /// persistent set is protected after two samples and the sweep leaves it
    /// alone, while a page sampled once stays evictable.
    #[test]
    fn the_head_advances_and_the_persistent_set_survives() {
        /// Slots one pass looks at: the cap the refusing pass stopped at.
        const CAP: usize = 256;
        /// One-touch streaming pages, more than the head saw in a pass.
        const PAGES: usize = 700;
        /// The world set, sampled every frame.
        const PERSISTENT: usize = 16;
        let slots = PERSISTENT + PAGES;
        let mut hits = vec![0u8; slots];
        let mut first = vec![0u64; slots];
        let mut last = vec![0u64; slots];
        let mut admit = vec![0u8; slots];
        // The old pass: the same first CAP slots, refused, every pass.
        let mut old_head = std::collections::BTreeSet::new();
        // The new pass: each pass looks further along the queue.
        let mut new_head = std::collections::BTreeSet::new();
        let mut head = 0usize;
        for frame in 0..8u64 {
            for i in 0..slots {
                if i < PERSISTENT || frame == 0 {
                    let (h, f, a) = crate::resource::admit_step(hits[i], first[i], last[i], frame);
                    (hits[i], first[i], last[i], admit[i]) = (h, f, frame, a);
                }
            }
            // Refused slots left due: the same head every pass.
            old_head.extend(0..CAP);
            // Reclaimed slots rescheduled: the head moves on.
            let end = (head + CAP).min(slots);
            new_head.extend(head..end);
            head = end;
        }
        assert_eq!(old_head.len(), CAP, "the old head never moved");
        assert_eq!(new_head.len(), slots, "the rescheduled head reached every slot");
        for j in 0..PERSISTENT {
            assert_eq!(admit[j], 2, "persistent {j} protected");
            assert!(
                !crate::resource::sweepable(true, 1, 1, admit[j] == 2),
                "the sweep leaves the world set alone"
            );
        }
        assert_eq!(admit[PERSISTENT], 1, "a one-touch page is on probation");
        assert!(
            crate::resource::sweepable(true, 1, 1, admit[PERSISTENT] == 2),
            "a one-touch page is evictable"
        );
    }

    /// A draw that repeats the root arguments another draw of the list set
    /// leaves every `SetGraphicsRoot*View` out: 1,000 draws of three root
    /// views make three calls, which is what the list is told ([`RootArgs`]).
    #[test]
    fn repeated_root_arguments_are_set_once() {
        let mut args = RootArgs::default();
        let signature = 0x1000_0000usize;
        let draw = [
            (0u32, RootKind::Cbv, 0xc000_0000u64),
            (2, RootKind::Srv, 0xd000_0000),
            (3, RootKind::Uav, 0xe000_0000),
        ];
        let mut calls = 0;
        for _ in 0..1_000 {
            for &(param, kind, value) in &draw {
                if args.again(signature, false, param, kind, value) {
                    calls += 1;
                }
            }
        }
        assert_eq!(calls, draw.len(), "SetGraphicsRoot*View calls");
        // Another value at any of them is set: the title set another one.
        assert!(args.again(signature, false, 2, RootKind::Srv, 0xd000_0100));
        // Another kind of view at a parameter is another argument.
        assert!(args.again(signature, false, 3, RootKind::Cbv, 0xe000_0000));
        // The compute space is another set of parameters.
        assert!(args.again(signature, true, 0, RootKind::Cbv, 0xc000_0000));
        assert!(!args.again(signature, true, 0, RootKind::Cbv, 0xc000_0000));
    }

    /// The same 32-bit constants at a parameter are set once; another root
    /// signature, another value or a cleared list sets them again.
    #[test]
    fn repeated_root_constants_are_set_once() {
        let mut values = RootValues::default();
        let signature = 0x1000_0000usize;
        let user = [0x10u32, 0x20, 0x30, 0x40];
        let mut calls = 0;
        for _ in 0..1_000 {
            if values.again(signature, false, 1, &user) {
                calls += 1;
            }
        }
        assert_eq!(calls, 1, "SetGraphicsRoot32BitConstants calls for one draw's user data");
        // A table served from another slot moves the offset the shader is
        // told: another value at the same parameter, set again.
        assert!(values.again(signature, false, 1, &[0x10, 0x20, 0x30, 0x50]));
        // Another root signature leaves the parameter undefined.
        assert!(values.again(0x2000_0000, false, 1, &[0x10, 0x20, 0x30, 0x50]));
        // The compute space is another set of parameters.
        assert!(values.again(0x2000_0000, true, 1, &[0x10, 0x20, 0x30, 0x50]));
        assert!(!values.again(0x2000_0000, true, 1, &[0x10, 0x20, 0x30, 0x50]));
        // A new host list has none of them.
        values.clear();
        assert!(values.again(0x2000_0000, true, 1, &[0x10, 0x20, 0x30, 0x50]));
    }

    /// A run whose record a window already holds records no clamp, so the
    /// pipeline the list holds stands: the next draw spares it instead of
    /// setting it again. A clamp of records leaves the list holding the
    /// clamp's own pipeline and compute root signature.
    #[test]
    fn a_record_a_window_holds_leaves_the_lists_pipeline_set() {
        let mut state = State::default();
        state.host_pipeline = 0x5150;
        state.host_roots = [0x51, 0x52];
        let held = crate::indirect::Held {
            pipeline: 0x9130,
            root: 0x9131,
        };
        // A clamp of no record records no command: nothing to take.
        state.hold_clamp(0, &held);
        assert_eq!(state.host_pipeline, 0x5150, "the pipeline the list holds stands");
        assert_eq!(state.host_roots, [0x51, 0x52], "no root signature was replaced either");
        // A clamp of records set its compute pipeline and compute root
        // signature of its own; the graphics root signature is untouched (a
        // pipeline change does not clear one).
        state.hold_clamp(16, &held);
        assert_eq!(state.host_pipeline, 0x9130);
        assert_eq!(state.host_roots, [0x51, 0x9131]);
    }

    /// A clamp whose root signature the list already holds leaves the compute
    /// root arguments the list was given: the next dispatch that asks for the
    /// same constants spares them. A clamp of another signature leaves none.
    #[test]
    fn a_clamp_keeps_the_root_arguments_it_did_not_undefine() {
        let mut state = State::default();
        state.host_roots = [0x51, 0x52];
        assert!(state.root_values.again(0x52, true, 0, &[1, 2, 3, 4]));
        // A clamp right after another of its own signature: spared.
        state.hold_clamp(16, &crate::indirect::Held { pipeline: 0x9130, root: 0x52 });
        assert_eq!(state.host_roots, [0x51, 0x52], "the signature the list holds stands");
        assert!(!state.root_values.again(0x52, true, 0, &[1, 2, 3, 4]), "the constants stand");
        // Another clamp signature leaves every compute root argument undefined.
        state.hold_clamp(16, &crate::indirect::Held { pipeline: 0x9130, root: 0x9131 });
        assert_eq!(state.host_roots, [0x51, 0x9131]);
        assert!(state.root_values.again(0x52, true, 0, &[1, 2, 3, 4]));
    }

    /// A root signature of one space leaves the other space's root arguments
    /// and 32-bit constants standing: D3D12 keeps the graphics ones across a
    /// `SetComputeRootSignature` and the compute ones across a graphics
    /// signature, so a clamp (which sets a compute signature of its own) must
    /// not make every draw after it set every graphics constant and view
    /// again — the calls `State::set_root_arg` and `State::root_constants`
    /// leave out are the ones the list already holds.
    #[test]
    fn a_signature_of_one_space_undefines_only_that_spaces_arguments() {
        let mut state = State::default();
        // A draw: graphics signature 0x51, one root CBV and its user data.
        assert!(state.root_args.again(0x51, false, 0, RootKind::Cbv, 0x1000));
        assert!(state.root_values.again(0x51, false, 0, &[1, 2, 3, 4]));
        // A dispatch: compute signature 0x62.
        state.forget_space(true);
        assert!(state.root_args.again(0x62, true, 0, RootKind::Cbv, 0x2000));
        assert!(state.root_values.again(0x62, true, 0, &[5, 6]));
        // The next draw binds the graphics signature the list holds and asks
        // for the values it set: nothing of the graphics space was undefined,
        // so it records none of them (with the whole-cache clear it recorded
        // all five).
        assert!(!state.root_args.again(0x51, false, 0, RootKind::Cbv, 0x1000), "the CBV stands");
        assert!(!state.root_values.again(0x51, false, 0, &[1, 2, 3, 4]), "the constants stand");
        // A draw binding another graphics signature sets both again, exactly
        // as the whole-cache clear made it: a signature of its own space
        // undefines every argument of that space.
        state.forget_space(false);
        assert!(state.root_args.again(0x52, false, 0, RootKind::Cbv, 0x1000));
        assert!(state.root_values.again(0x52, false, 0, &[1, 2, 3, 4]));
        // And the compute space kept its own through that.
        assert!(!state.root_args.again(0x62, true, 0, RootKind::Cbv, 0x2000));
        assert!(!state.root_values.again(0x62, true, 0, &[5, 6]));
        // A new host list has none of them (`State::replay`, a cut).
        state.forget_roots();
        assert!(state.root_args.again(0x51, false, 0, RootKind::Cbv, 0x1000));
        assert!(state.root_values.again(0x51, false, 0, &[1, 2, 3, 4]));
    }

    /// The commands a draw after a dispatch records: `State::set_root_arg` and
    /// `State::root_constants` record one each, and nothing else of the root
    /// state moves, so a call that answers "already set" is a command the list
    /// does not get. The draw that follows a compute dispatch records none of
    /// its own — the whole-cache clear made it record both — and a draw that
    /// re-binds the signature the list holds records none either, which is
    /// what a pipeline that binds records today: its draw, and no root
    /// argument.
    #[test]
    fn a_draw_after_a_dispatch_records_no_root_argument_again() {
        /// The root commands one draw (or dispatch) records: one per call
        /// that answers that the list does not hold the value.
        fn record(state: &mut State, signature: usize, compute: bool) -> u64 {
            let mut calls = 0;
            if compute {
                // A dispatch: a root UAV and the user data it reads.
                if state.root_args.again(signature, true, 0, RootKind::Uav, 0x2000) {
                    calls += 1;
                }
                if state.root_values.again(signature, true, 0, &[5, 6]) {
                    calls += 1;
                }
            } else {
                // A draw: a root CBV and the user data it reads.
                if state.root_args.again(signature, false, 0, RootKind::Cbv, 0x1000) {
                    calls += 1;
                }
                if state.root_values.again(signature, false, 0, &[1, 2, 3, 4]) {
                    calls += 1;
                }
            }
            calls
        }
        let mut state = State::default();
        assert_eq!(record(&mut state, 0x51, false), 2, "a draw's two");
        // The dispatch sets a compute signature of its own.
        state.forget_space(true);
        assert_eq!(record(&mut state, 0x62, true), 2, "the dispatch's two");
        // The graphics root arguments stand: the draw records no root command
        // at all, where a whole-cache clear made it record both again.
        assert_eq!(record(&mut state, 0x51, false), 0, "the draw's own stand");
        // Bound again with the signature the list holds, as a pipeline that
        // binds does: still none.
        assert_eq!(record(&mut state, 0x51, false), 0);
    }

    /// A signature that undefines a space's constants does not give their
    /// buffers back: the draw that sets them again fills the vec the list
    /// held, so a root signature switch costs no allocation.
    #[test]
    fn a_signature_switch_reuses_the_constant_buffers() {
        let mut state = State::default();
        assert!(state.root_values.again(0x51, false, 0, &[1, 2, 3, 4]));
        let held = state.root_values.set[0].3.as_ptr();
        state.forget_space(false);
        assert!(state.root_values.again(0x52, false, 0, &[5, 6]), "set again");
        assert_eq!(state.root_values.set[0].3.as_ptr(), held, "the buffer it held");
        // The graphics arguments were the ones undefined; the compute space
        // keeps its own, and its entry is not touched.
        assert!(state.root_values.again(0x62, true, 0, &[7, 8]));
        let compute = state.root_values.set[1].3.as_ptr();
        state.forget_space(false);
        assert_eq!(state.root_values.set[1].3.as_ptr(), compute);
        // A new host list forgets them whole: the slots are another list's.
        state.forget_roots();
        assert!(state.root_values.set.is_empty());
    }

    /// The draw-buffer views a list holds are compared field by field: the
    /// D3D12 types derive no comparison, and a draw whose buffers resolve to
    /// the same host bytes is the same state.
    #[test]
    fn a_draw_buffer_view_is_the_same_state_only_with_the_same_bytes() {
        let a = D3D12_VERTEX_BUFFER_VIEW {
            BufferLocation: 0x1000_0000,
            SizeInBytes: 0x400,
            StrideInBytes: 32,
        };
        let b = D3D12_VERTEX_BUFFER_VIEW {
            StrideInBytes: 24,
            ..a
        };
        assert!(same_vertex(&a, &a));
        assert!(!same_vertex(&a, &b));
        let i = D3D12_INDEX_BUFFER_VIEW {
            BufferLocation: 0x1000_0000,
            SizeInBytes: 0x400,
            Format: DXGI_FORMAT(57),
        };
        let j = D3D12_INDEX_BUFFER_VIEW {
            Format: DXGI_FORMAT(58),
            ..i
        };
        assert!(same_index(&i, &i));
        assert!(!same_index(&i, &j));
    }

    /// The commands-per-draw line: every command named with its ratio, and
    /// only the calls a cache spared that happened.
    #[test]
    fn the_commands_per_draw_line_names_each_command() {
        let mut emitted = [0u64; COMMANDS];
        emitted[Command::Draw as usize] = 4;
        emitted[Command::Vertex as usize] = 6;
        emitted[Command::Index as usize] = 4;
        emitted[Command::RootTable as usize] = 8;
        emitted[Command::Root32 as usize] = 8;
        let mut spared = [0u64; COMMANDS];
        spared[Command::Vertex as usize] = 2;
        spared[Command::Pipeline as usize] = 4;
        let text = Counts { emitted, spared }.text(4, 2);
        assert!(text.starts_with("commands per draw: "), "{text}");
        // 30 calls over 4 draws.
        assert!(text.contains("7.50 on the host list for 4 draws in 2 frames"), "{text}");
        assert!(text.contains("draw 1.00"), "{text}");
        assert!(text.contains("vertex buffers 1.50"), "{text}");
        assert!(text.contains("root tables 2.00"), "{text}");
        assert!(text.contains("1.50 left out as the list already held them"), "{text}");
        assert!(text.contains("pipeline 1.00"), "{text}");
        // A command a cache never spared is not named in that list.
        assert!(!text[text.find("left out").unwrap()..].contains("index buffers"), "{text}");
        // A window with no draws says so rather than dividing by zero.
        let none = Counts {
            emitted: [0; COMMANDS],
            spared: [0; COMMANDS],
        }
        .text(0, 3);
        assert!(none.starts_with("commands per draw: none"), "{none}");
    }

    /// The commands-per-draw line of a synthetic frame, from the counters the
    /// recording path itself bumps: two draws of one pipeline that the list
    /// already holds, whose vertex buffers the second reuses. Prints the line
    /// for `--nocapture`, where a run's log shows the same shape.
    #[test]
    fn a_synthetic_frame_of_two_draws_reads_as_a_run_prints_it() {
        // Whatever another test left.
        let _ = Counts::take();
        for draw in 0..2 {
            emitted(Command::Draw, 1);
            emitted(Command::Root32, 2);
            spared(Command::Pipeline);
            // The second draw's buffers are the first's: spared, not set.
            if draw == 0 {
                emitted(Command::Vertex, 1);
            } else {
                spared(Command::Vertex);
            }
        }
        let text = Counts::take().text(2, 3);
        println!("{text}");
        // 2 draws, 1 vertex-buffer set, 4 root-constant sets: 7 calls.
        assert!(
            text.starts_with("commands per draw: 3.50 on the host list for 2 draws in 3 frames"),
            "{text}"
        );
        for expected in [
            "draw 1.00",
            "vertex buffers 0.50",
            "root constants 2.00",
            "1.50 left out as the list already held them (pipeline 1.00, vertex buffers 0.50)",
        ] {
            assert!(text.contains(expected), "{expected} missing from {text}");
        }
    }

    /// A descriptor the title rewrote between two draws resolves to another
    /// value at the same title address, and the root argument is set again: a
    /// title that rewrites its descriptors every draw gets every argument set
    /// (the cache is fed what this draw read, never the address the title
    /// set).
    #[test]
    fn a_rewritten_root_view_is_set_again() {
        let mut args = RootArgs::default();
        let signature = 0x1000_0000usize;
        // The title's root CBV address, and what the descriptor there
        // resolves to: `root_view` reads the record at the address on every
        // draw, so a rewrite is another value at the same address.
        let address = 0x7ff6_0000u64;
        let mut resolved = std::collections::HashMap::from([(address, 0xaaaa_0000u64)]);
        let mut calls = 0;
        for draw in 0..3 {
            if draw == 1 {
                // Another resource described where the old one was.
                resolved.insert(address, 0xbbbb_0000);
            }
            if args.again(signature, false, 1, RootKind::Cbv, resolved[&address]) {
                calls += 1;
            }
        }
        assert_eq!(calls, 2, "the rewrite was not read");
    }

    /// A root signature switch and a cut's new host list leave nothing set:
    /// every argument is set again.
    #[test]
    fn a_new_signature_or_list_sets_every_root_argument_again() {
        // Another signature, then a cut ([`State::bind`], [`State::replay`]).
        let script = [
            (0x1000_0000usize, false),
            (0x1000_0000, false),
            (0x2000_0000, true),
            (0x2000_0000, false),
            (0x2000_0000, true),
        ];
        let mut args = RootArgs::default();
        let mut calls = 0;
        for (signature, cleared) in script {
            if cleared {
                args.clear();
            }
            if args.again(signature, false, 0, RootKind::Cbv, 0xc000_0000) {
                calls += 1;
            }
        }
        assert_eq!(calls, 3);
    }
}
