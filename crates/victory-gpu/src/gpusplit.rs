//! Where the card's time goes, per group of draws: `VICTORY_GPU_SPLIT=1` in
//! a diagnostics build puts one GPU timestamp in front of every run of
//! commands that share a pipeline, and the line every 5 s says what each run
//! cost the card. The frame line splits a frame into CPU work and "on the
//! GPU"; this says which draws that GPU time is in, which is the question a
//! draw count of five thousand a frame asks.
//!
//! The grouping is what the card can see: the command's kind (a graphics
//! draw, a mesh dispatch, an indirect run, a compute dispatch, the clamp of
//! ours behind an indirect run) and the host pipeline it ran, named by its
//! shaders' hashes (`pipeline::host::HostPipeline::name`). Two runs of the
//! same pipeline that other commands interrupt are two segments, and both
//! count into their group.
//!
//! shortcut: a segment's time runs from the first command of the group it
//! names to the first command of the group after it, so the state commands
//! recorded between two groups (the next draw's pipeline, root arguments and
//! barriers) are charged to the group before them. Recording one query a
//! group costs the card one `EndQuery` a group: a group of one draw pays
//! about a microsecond for being measured, which is why the µs/draw figures
//! of two runs with the switch on compare, and why they are not mixed with
//! a run with it off.
//!
//! Our own passes over the copies in the card's memory (the compare, the
//! hash) and the copies themselves are lists of ours that nothing waits
//! for, so no fence could be placed behind their queries without changing
//! what they do; their counts are logged beside this line instead
//! (`host`'s "our passes over copies in the card's memory" and "copies into
//! the card's memory on the copy queue").

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

use crate::host::{HostList, host};
use crate::object::This;

/// Timestamps a list gets a frame ([`Split`]): a segment a group, and one
/// more to close the last. A list that runs past them is timed no further,
/// and the draws it loses are counted (`unattributed`), so the line never
/// pretends to have timed what it did not.
const QUERIES: u32 = 16_384;

/// The kind of command a segment is: what it does, which is what a reader
/// wants first, and the pipeline only after.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Group {
    /// `DrawInstanced` and `DrawIndexedInstanced`.
    Draw,
    /// `DispatchMesh`.
    Mesh,
    /// `ExecuteIndirect`: the title's records, whatever they dispatch.
    Indirect,
    /// `Dispatch`, from the title.
    Compute,
    /// The clamp dispatch of ours behind an indirect run
    /// (`indirect::bound`).
    Clamp,
}

impl Group {
    /// The key's kind (`Split::segments` keeps this, not the enum).
    fn tag(self) -> u8 {
        match self {
            Group::Draw => 0,
            Group::Mesh => 1,
            Group::Indirect => 2,
            Group::Compute => 3,
            Group::Clamp => 4,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Group::Draw => "graphics draws",
            Group::Mesh => "mesh dispatches",
            Group::Indirect => "indirect runs",
            Group::Compute => "compute dispatches",
            Group::Clamp => "our clamp dispatches",
        }
    }
}

/// Whether the card is timed per group (`VICTORY_GPU_SPLIT`, diagnostics
/// builds). Off unless asked for: it adds a query a group and a fence
/// (`host::late` stands still while it is on, so every submission is waited
/// for before its timestamps are read).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_GPU_SPLIT").is_ok_and(|v| v == "1"))
}

/// What a list has timed since the last submission: the query heap and its
/// read-back buffer, the segments in query order, and the key of the
/// segment still open.
#[derive(Default)]
pub(crate) struct Split {
    /// Made with the first segment of the list, and kept for the process.
    heap: Option<(ID3D12QueryHeap, ID3D12Resource)>,
    /// Queries used: also the index the next segment starts at.
    used: u32,
    /// One per segment, in query order: the group's key, and the draws it
    /// recorded (a run of one key).
    segments: Vec<(u32, u32)>,
    /// The key of the last segment, if one is open.
    open: Option<u32>,
}

impl Split {
    /// Forget the window's segments (a list reset or harvested).
    pub(crate) fn clear_window(&mut self) {
        self.used = 0;
        self.segments.clear();
        self.open = None;
    }
}

/// One group's tally over a window.
pub(crate) struct Entry {
    group: Group,
    /// The pipeline, named by its shaders (or the group, when it is ours).
    name: String,
    /// Commands it ran, and the nanoseconds the card spent in its segments.
    draws: u64,
    nanos: u64,
}

#[derive(Default)]
struct Registry {
    /// (kind, host pipeline address) to its tally.
    ids: HashMap<(u8, usize), u32>,
    entries: Vec<Entry>,
}

/// The tallies of the window, and the keys among them. Taken (and so
/// emptied) by [`line`].
static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);

/// Commands a list's query cap left untimed since the last line.
static UNATTRIBUTED: AtomicU64 = AtomicU64::new(0);

fn registry() -> std::sync::MutexGuard<'static, Option<Registry>> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

/// The tally a (group, pipeline) pair counts into, made on first use: the
/// pipeline is named here, once, not once a draw.
fn key_of(group: Group, pipeline: usize) -> u32 {
    let mut guard = registry();
    let reg = guard.get_or_insert_with(Registry::default);
    let id = (group.tag(), pipeline);
    if let Some(&at) = reg.ids.get(&id) {
        return at;
    }
    let name = name_of(group, pipeline);
    let at = reg.entries.len() as u32;
    reg.entries.push(Entry { group, name, draws: 0, nanos: 0 });
    reg.ids.insert(id, at);
    at
}

/// What a group of `pipeline` is called in the line.
fn name_of(group: Group, pipeline: usize) -> String {
    if group == Group::Clamp {
        return "the clamp behind indirect runs".to_owned();
    }
    let Some(p) = crate::pipeline::host_pipeline(pipeline as This) else {
        return "(no host pipeline)".to_owned();
    };
    // Shader hashes, stage by stage: long enough to name a shader, short
    // enough that twelve lines of them stay readable.
    p.name.chars().take(120).collect()
}

/// A query heap and the read-back buffer its timestamps are resolved into.
fn make_heap() -> Option<(ID3D12QueryHeap, ID3D12Resource)> {
    let device = &host()?.device;
    let mut heap: Option<ID3D12QueryHeap> = None;
    let mut back: Option<ID3D12Resource> = None;
    let readback = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: u64::from(QUERIES) * 8,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    // SAFETY: plain object creation; the descriptors are local and the
    // pointers are out parameters.
    unsafe {
        device
            .CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC {
                    Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                    Count: QUERIES,
                    NodeMask: 0,
                },
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
}

/// The list is about to record a command of `group` on `pipeline`: a
/// timestamp here starts its segment, unless the segment open is of the
/// same group already (then this command is one more of it).
pub(crate) fn mark(list: &HostList, group: Group, pipeline: usize) {
    if !on() {
        return;
    }
    let key = key_of(group, pipeline);
    let mut split = list.split.lock().unwrap_or_else(|e| e.into_inner());
    if split.open == Some(key) {
        if let Some(last) = split.segments.last_mut() {
            last.1 += 1;
        }
        return;
    }
    // The heap, the last query and the resolve need two indexes: one to
    // close the open segment and one to close the list's.
    if split.used + 1 >= QUERIES {
        UNATTRIBUTED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if split.heap.is_none() {
        split.heap = make_heap();
    }
    let Some((heap, _)) = split.heap.as_ref() else {
        return;
    };
    // SAFETY: an open list and our query heap, with the index in range.
    unsafe {
        list.list
            .EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, split.used)
    };
    split.segments.push((key, 1));
    split.used += 1;
    split.open = Some(key);
}

/// The list is closing: the last segment's time ends here, and the
/// timestamps are resolved into the read-back buffer for [`harvest`].
pub(crate) fn close(list: &HostList) {
    if !on() {
        return;
    }
    let split = list.split.lock().unwrap_or_else(|e| e.into_inner());
    let Some((heap, back)) = split.heap.as_ref().filter(|_| split.used > 0) else {
        return;
    };
    // SAFETY: an open list, our heap, and a read-back buffer of exactly
    // this length.
    unsafe {
        list.list
            .EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, split.used);
        list.list
            .ResolveQueryData(heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, split.used + 1, back, 0);
    }
}

/// The list's submission has finished: its segments' times go to their
/// groups. `frequency` is the queue's ([`ID3D12CommandQueue::GetTimestampFrequency`]).
pub(crate) fn harvest(list: &HostList, frequency: u64) {
    if !on() {
        return;
    }
    let mut split = list.split.lock().unwrap_or_else(|e| e.into_inner());
    let used = split.used;
    let segments = std::mem::take(&mut split.segments);
    split.clear_window();
    let (Some((_, back)), true) = (split.heap.as_ref(), used > 0) else {
        return;
    };
    let mut data = std::ptr::null_mut();
    let range = D3D12_RANGE {
        Begin: 0,
        End: (used as usize + 1) * 8,
    };
    // SAFETY: a read-back buffer this list resolved its own timestamps
    // into, and whose submission has been waited for.
    let stamps: Vec<u64> = unsafe {
        if back.Map(0, Some(&range), Some(&mut data)).is_err() {
            return;
        }
        let s = std::slice::from_raw_parts(data.cast::<u64>(), used as usize + 1).to_vec();
        back.Unmap(0, None);
        s
    };
    let mut guard = registry();
    let reg = guard.get_or_insert_with(Registry::default);
    accumulate(&mut reg.entries, &segments, &stamps, frequency);
}

/// Adds each segment's time (stamps `i` to `i + 1`) to its group, in
/// nanoseconds (`frequency` ticks a second).
fn accumulate(entries: &mut [Entry], segments: &[(u32, u32)], stamps: &[u64], frequency: u64) {
    let frequency = frequency.max(1);
    for (i, &(key, draws)) in segments.iter().enumerate() {
        let (Some(&from), Some(&to)) = (stamps.get(i), stamps.get(i + 1)) else {
            continue;
        };
        let Some(entry) = entries.get_mut(key as usize) else {
            continue;
        };
        entry.nanos += to.saturating_sub(from).saturating_mul(1_000_000_000) / frequency;
        entry.draws += u64::from(draws);
    }
}

/// The window's line, every 5 s (`draw::commands_line`): the heaviest groups
/// first, in the card's microseconds a draw.
pub(crate) fn line(frames: f64) {
    if !on() {
        return;
    }
    let Some(mut reg) = registry().take() else {
        return;
    };
    let unattributed = UNATTRIBUTED.swap(0, Ordering::Relaxed);
    if reg.entries.is_empty() {
        return;
    }
    reg.entries.sort_by_key(|e| std::cmp::Reverse(e.nanos));
    for text in text(&reg.entries, frames, unattributed) {
        tracing::info!("{text}");
    }
}

/// [`line`]'s lines: a total, then the twelve heaviest groups.
fn text(entries: &[Entry], frames: f64, unattributed: u64) -> Vec<String> {
    let frames = if frames > 0.0 { frames } else { 1.0 };
    let total: u64 = entries.iter().map(|e| e.nanos).sum();
    let draws: u64 = entries.iter().map(|e| e.draws).sum();
    let per = |nanos: u64, draws: u64| nanos as f64 / 1e3 / draws.max(1) as f64;
    let mut out = vec![format!(
        "where the card's time went: {:.1} ms a frame in {} groups, {:.0} draws timed \
         ({:.1} µs a draw), {} draws past a list's cap of {QUERIES}",
        total as f64 / 1e6 / frames,
        entries.len(),
        draws as f64 / frames,
        per(total, draws),
        unattributed as f64 / frames
    )];
    for e in entries.iter().take(12) {
        out.push(format!(
            "where the card's time went: {} [{}] {:.0} draws {:.1} ms a frame ({:.1} µs a draw)",
            e.group.label(),
            e.name,
            e.draws as f64 / frames,
            e.nanos as f64 / 1e6 / frames,
            per(e.nanos, e.draws)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Entry, Group, accumulate, text};

    fn entry(group: Group, name: &str) -> Entry {
        Entry {
            group,
            name: name.to_owned(),
            draws: 0,
            nanos: 0,
        }
    }

    /// A segment's time is the difference of its two stamps, in
    /// nanoseconds, and its draws are its own: a group of 64 draws that
    /// took 64 ms is a millisecond a draw.
    #[test]
    fn a_segment_reaches_its_group() {
        let mut entries = vec![entry(Group::Draw, "vertex a, pixel b")];
        // 100 MHz: 6,400,000 ticks are 64 ms.
        accumulate(&mut entries, &[(0, 64)], &[1_000, 6_401_000], 100_000_000);
        assert_eq!(entries[0].draws, 64, "{:?}", entries[0].draws);
        assert_eq!(entries[0].nanos, 64_000_000, "{}", entries[0].nanos);
        // A second segment of the same group adds to it.
        accumulate(&mut entries, &[(0, 2)], &[7_000_000, 7_100_000], 100_000_000);
        assert_eq!(entries[0].draws, 66);
        assert_eq!(entries[0].nanos, 65_000_000);
    }

    /// A segment whose key names no group, or whose stamps are missing,
    /// adds nothing: the line never counts draws it did not time.
    #[test]
    fn a_segment_with_no_group_or_no_stamp_adds_nothing() {
        let mut entries = vec![entry(Group::Mesh, "mesh c")];
        accumulate(&mut entries, &[(7, 4)], &[0, 100], 100_000_000);
        accumulate(&mut entries, &[(0, 4)], &[0], 100_000_000);
        assert_eq!(entries[0].draws, 0);
        assert_eq!(entries[0].nanos, 0);
    }

    /// The line: groups heaviest first, each with its draws a frame and its
    /// microseconds a draw, and the total first.
    #[test]
    fn the_line_names_the_groups_by_their_time() {
        let mut entries = vec![entry(Group::Draw, "vertex 3f2a, pixel 91bc"), entry(Group::Mesh, "mesh c")];
        entries[0].draws = 4_000;
        entries[0].nanos = 200_000_000;
        entries[1].draws = 1_000;
        entries[1].nanos = 50_000_000;
        entries.sort_by_key(|e| std::cmp::Reverse(e.nanos));
        let lines = text(&entries, 2.0, 0);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[0].starts_with("where the card's time went: 125.0 ms a frame in 2 groups, 2500 draws timed (50.0 µs a draw)"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].starts_with("where the card's time went: graphics draws [vertex 3f2a, pixel 91bc] 2000 draws 100.0 ms a frame (50.0 µs a draw)"),
            "{}",
            lines[1]
        );
        assert!(lines[2].starts_with("where the card's time went: mesh dispatches [mesh c] 500 draws 25.0 ms a frame (50.0 µs a draw)"), "{}", lines[2]);
        // No frames (a window that presented none) still divides by one.
        assert!(text(&entries, 0.0, 3)[0].contains("3 draws past a list's cap"));
    }
}
