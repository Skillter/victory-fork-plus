//! The SGPRs a program finds written before its first instruction, and what a
//! read of each one is worth: the verdicts `emit::lower::inputs` applies (they
//! live here because `lower.rs` is at its size limit).

use super::lower::Parts;
use crate::ir::{EXEC, PARKED, M0, VCC};
use crate::native::{HwStage, RootParam};

/// The reason a read of scalar code `n` on entry has no value, or `None` where
/// the register reads zero.
///
/// Above [`crate::native::ComputeIo::initialized_sgprs`] a register holds what
/// the last wave left, so no program depends on it: a read the paths only seem
/// to allow (a branch past the write, taken only with one past the read) reads
/// zero, as VGPRs do.
pub(super) fn on_entry(n: u8, x: &Parts) -> Option<&'static str> {
    let (stage, layout) = (x.native.stage, &x.native.layout[..]);
    if n <= 105 && x.io.initialized_sgprs.is_some_and(|k| n >= k) {
        return None;
    }
    // A scalar parked in a VGPR lane (`ir::unspill`), read where the paths only
    // seem to allow it before any save: the lane of a VGPR nobody wrote, and
    // VGPRs nobody set read as zero.
    if n >= PARKED {
        return None;
    }
    let end = user_end(stage, layout);
    let what = match n {
        // Every lane of the wave: `lower::get` derives its number from
        // `x.st.masks`, which seeds the entry mask as `Bool::TRUE`.
        n if n == EXEC || n == EXEC + 1 => "the lane mask (EXEC) on entry",
        // The result of the last vector compare, and no program reads it
        // before one its own writes reach makes one.
        n if n == VCC || n == VCC + 1 => "the vector compare mask (VCC) on entry",
        // Set by `s_mov_b32 m0, ...` before any instruction that uses it as a
        // base or a count.
        n if n == M0 => "M0 on entry",
        // The base of the per-lane scratch segment the hardware points at. We
        // model no scratch segment, so a read of it would be an address into
        // nothing rather than a null pointer.
        102 | 103 => "the flat scratch base",
        // Which waves are re-run to repair a repeated vector read: the host
        // never replays a wave, so no lane of one is in the mask.
        104 | 105 => return None,
        108..=123 => "a trap temporary",
        125 => "the null SGPR, which reads zero",
        // Past the user data: the pixel stage's `COLLISION_WAVEID` and
        // `INTRAWAVE_COLLISION`, or a register nobody set. `COLLISION_WAVEID`
        // of zero is a wave that overlapped nothing and has no intrawave
        // collision, which is what a host wave is: `s_cselect_b64 s[0:1],
        // exec, 0` then sets POPS_PACKER to 1 (the safe order) and takes the
        // branch past the `s_sleep` wait loop, so the ordered section below
        // runs unordered, and `src_pops_exiting_wave_id` (operand 239), which
        // the wait loop reads, is zero: no wave exits a section here.
        // shortcut: an overlapped console wave is translated as one that is
        // not, so a section the console ran ordered runs unordered, without a
        // RasterizerOrderedView.
        n if stage == HwStage::Ps && u32::from(n) >= end && u32::from(n) <= end + 1 => return None,
        n if u32::from(n) >= end => "an SGPR past the root signature's user data",
        _ => "a user SGPR that no root parameter binds",
    };
    Some(what)
}

/// The first SGPR past the root signature's user data.
pub(super) fn user_end(stage: HwStage, layout: &[RootParam]) -> u32 {
    layout
        .iter()
        .map(|e| u32::from(stage.user_sgpr(e.user_data)) + e.sgprs)
        .max()
        .unwrap_or(0)
}
