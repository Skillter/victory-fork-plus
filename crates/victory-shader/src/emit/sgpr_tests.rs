//! The SGPRs the hardware writes before the first instruction, read on entry
//! (`emit::lower::hardware_sgpr`): the lane mask, which is the ballot of every
//! lane; the ordered-section arguments, which are zero because no host wave
//! overlaps another; and the registers a program may not depend on, each
//! refused under its own name.

use std::collections::HashSet;

use super::tests::{declares, ps_lower, ps_try};
use super::wave64_tests::{lowered, native_of, on_the_card};
use crate::ShaderError;
use crate::ir;
use crate::ir::EXEC;

/// The ballot of all lanes, as `lower` declares it.
const BALLOT: &str = "dx.op.waveActiveBallot";

/// SOP1 `s_mov_b32 sdst, src0` (`src0` a scalar code: 0-105 an SGPR, 106
/// VCC, 124 M0, 126 EXEC).
const fn s_mov_b32(sdst: u8, src0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | 0x03 << 8 | src0 as u32
}

/// VOP1 `v_mov_b32 vdst, src0` (`src0` 0-255 an SGPR).
const fn v_mov_b32(vdst: u8, src0: u8) -> u32 {
    0x7E00_0200 | (vdst as u32) << 17 | src0 as u32
}

/// `exp mrt0, v1, v1, v1, v1 done vm; s_endpgm`: a pixel program's export.
const EXPORT_V1: [u32; 3] = [0xF800_180F, 0x0101_0101, 0xBF81_0000];

/// `buffer_store_dword v0, off, s[8:11], 0`: a compute program's store.
const STORE_V0: [u32; 2] = [0x38 << 26 | 0x1C << 18, 0x80 << 24 | 2 << 16];

/// A pixel program that reads scalar `code` into s2 and exports it as v1.
fn pixel_reading(code: u8) -> Vec<u32> {
    let mut words = vec![s_mov_b32(2, code), v_mov_b32(1, 2)];
    words.extend_from_slice(&EXPORT_V1);
    words
}

/// The reason `words` is refused, if it is.
fn refused(words: &[u32]) -> &'static str {
    match ps_try(words, 2) {
        Err(ShaderError::Unsupported { what, .. }) => what,
        Err(e) => panic!("refused as {e:?}, not an unsupported construct"),
        Ok(_) => panic!("translated"),
    }
}

/// EXEC on entry holds every lane of the wave, so a program that reads it as
/// a number reads the wave's mask: the ballot of all of them. The title's
/// pixel programs keep it on one path of an ordered section
/// (`s_cselect_b64 s[0:1], exec, 0` at 0x2d90 of
/// `PS_PedTexturedCutOutTint_Zero_8aeae56d`), a use no mask primitive reaches.
#[test]
fn exec_on_entry_read_as_data_is_the_ballot_of_every_lane() {
    let l = ps_lower(&pixel_reading(EXEC), 2);
    assert!(declares(&l, BALLOT), "the mask of every lane is a ballot");
    // A console wave of 32 lanes on a host that takes 32: the number is the
    // mask of all of them.
    let mut words = vec![s_mov_b32(20, EXEC), v_mov_b32(0, 20)];
    words.extend_from_slice(&STORE_V0);
    let native = native_of(&words, false, 32);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
    if let Some(got) = on_the_card(&shader, [0, 0]) {
        assert_eq!(got, u32::MAX, "all 32 lanes of the console wave");
    }
}

/// The pixel stage's ordered-section arguments follow the user data:
/// `COLLISION_WAVEID` and `INTRAWAVE_COLLISION`, s0 and s1 without user data.
/// A host wave overlaps no other wave, so both are zero and the section below
/// them runs unordered instead of waiting for a wave to leave.
#[test]
fn the_ordered_section_arguments_are_zero() {
    for code in [0, 1] {
        let l = ps_lower(&pixel_reading(code), 2);
        assert!(!declares(&l, BALLOT), "s{code} is the constant zero");
    }
}

/// The XNACK mask names the waves to re-run to repair a repeated vector read.
/// A host never replays a wave, so no lane of one is in it.
#[test]
fn the_xnack_mask_is_zero() {
    for code in [104, 105] {
        let l = ps_lower(&pixel_reading(code), 2);
        assert!(!declares(&l, BALLOT), "s{code} is the constant zero");
    }
}

/// A register the hardware writes that has no value here: the refusal names
/// it, so the log says which one a program read.
#[test]
fn the_registers_the_hardware_writes_are_refused_by_name() {
    for (code, what) in [
        (106, "the vector compare mask (VCC) on entry"),
        (107, "the vector compare mask (VCC) on entry"),
        (124, "M0 on entry"),
        (102, "the flat scratch base"),
        (103, "the flat scratch base"),
        (108, "a trap temporary"),
        (123, "a trap temporary"),
        (5, "an SGPR past the root signature's user data"),
    ] {
        assert_eq!(refused(&pixel_reading(code)), what, "s{code}");
    }
}
