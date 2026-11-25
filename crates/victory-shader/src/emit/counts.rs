//! The output counts a mesh group promises the rasteriser
//! (`SetMeshOutputCounts`): the console's output allocation request, followed
//! back to the two scalar values it packs.

use std::collections::HashMap;

use super::lanes::all_lane_blocks;
use super::lower::Why;
use crate::gfx10::{Fields, Operand};
use crate::ir::{Program, Reg, Value};
use crate::structure::Structured;

/// `s_lshl_b32` shift of the primitive count in the allocation request's
/// `m0` (`vertices | primitives << 12`).
const ALLOC_PRIM_SHIFT: u32 = 12;

/// What a mesh program promises each group of outputs (`VICTORY_MESH_COUNTS`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum MeshCounts {
    /// The counts the console asks for, where the allocation request can be
    /// followed ([`output_counts`]); the declared maxima otherwise.
    Own,
    /// The declared maxima always, and a cull primitive per lane.
    Max,
    /// Nothing: the group outputs no mesh. A measurement, not a translation:
    /// it prices what the declared maxima cost the rasteriser.
    Zero,
}

/// `VICTORY_MESH_COUNTS=own|max|zero`: what a mesh program promises its
/// groups (read at its use site; `zero` is a measurement arm).
pub(super) fn mesh_counts() -> MeshCounts {
    match std::env::var("VICTORY_MESH_COUNTS").ok().as_deref() {
        Some("zero") => MeshCounts::Zero,
        Some("max") => MeshCounts::Max,
        _ => MeshCounts::Own,
    }
}

/// `VICTORY_MESH_COUNTS_LOG=1`: one line per mesh program saying whether its
/// group counts were followed (and why not, where they were not).
pub(super) fn mesh_counts_log() -> bool {
    static LOG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *LOG.get_or_init(|| std::env::var_os("VICTORY_MESH_COUNTS_LOG").is_some_and(|v| v != "0"))
}

/// A group's output counts as the console asks for them: the block that
/// makes them uniform, and the two instructions there that read them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct OutputCounts {
    /// The block whose end emits `setMeshOutputCounts`: it dominates every
    /// output store, as the host requires of the one call.
    pub(super) block: usize,
    /// Instruction indices in that block of the two `v_readfirstlane_b32`
    /// whose results are the vertex and the primitive count.
    pub(super) verts: usize,
    pub(super) prims: usize,
}

/// Follows the console's output allocation request back to the two counts it
/// sends. The title's mesh shaders compute a vertex and a primitive count as
/// scalars, make them uniform with a `v_readfirstlane_b32` pair, pack them
/// (`s_lshl_b32 primitives, 12` then `s_or_b32 vertices, shifted`) and one
/// lane sends the request: `m0 = vertices | primitives << 12`.
///
/// Returns an error, with the reason, when the program's counts are not that
/// shape: the caller then promises the declared maxima, as it always did.
/// `writes` are the blocks that store mesh outputs; the pair's block must
/// dominate them all, because DXIL's `SetMeshOutputCounts` is one call and
/// the validator wants it before every store, and it must run in every lane
/// (a call from divergent flow is undefined).
pub(super) fn output_counts(
    p: &Program,
    st: &Structured,
    writes: &[usize],
) -> Result<OutputCounts, Why> {
    // One request for the program: a group's counts are one value.
    let mut request = None;
    for (b, block) in p.blocks.iter().enumerate() {
        for (i, inst) in block.insts.iter().enumerate() {
            if inst.name() == "s_sendmsg"
                && matches!(inst.fields, Fields::Sopp { imm } if imm & 0xF == super::lower::GS_ALLOC_REQ)
            {
                if request.is_some() {
                    return Err("two allocation requests");
                }
                request = Some((b, i));
            }
        }
    }
    let (send_b, send_i) = request.ok_or("no allocation request")?;
    let mut defs: HashMap<Value, (usize, usize)> = HashMap::new();
    for (b, block) in p.blocks.iter().enumerate() {
        for (i, inst) in block.insts.iter().enumerate() {
            for &(_, v) in &inst.writes {
                defs.entry(v).or_insert((b, i));
            }
        }
    }
    let at = |v: Value| defs.get(&v).map(|&(b, i)| (b, i, &p.blocks[b].insts[i]));
    // `s_mov_b32 m0, x`: the packed counts, sent a few instructions later.
    let send = &p.blocks[send_b].insts;
    let mov = (0..send_i)
        .rev()
        .find(|&j| send[j].write(Reg::S(crate::ir::M0)).is_some())
        .ok_or("no m0 before the request")?;
    let packed = send[mov].reads.first().ok_or("no packed counts")?.1;
    // `s_or_b32 vertices, s_lshl_b32(primitives, 12)`.
    let (_, _, or) = at(packed).ok_or("the pack is a lane value")?;
    if or.name() != "s_or_b32" || or.reads.len() != 2 {
        return Err("the pack is not an or of two counts");
    }
    let shifted = or
        .reads
        .iter()
        .map(|r| r.1)
        .find(|&v| {
            at(v).is_some_and(|(_, _, s)| {
                s.name() == "s_lshl_b32"
                    && matches!(&s.fields, Fields::Sop { src, .. }
                        if src.len() == 2 && shift_of(&src[1]) == Some(ALLOC_PRIM_SHIFT))
            })
        })
        .ok_or("no shifted primitive count")?;
    let (_, _, shl) = at(shifted).ok_or("the shift is a lane value")?;
    let prims = shl.reads.first().ok_or("no primitive count")?.1;
    let verts = or
        .reads
        .iter()
        .map(|r| r.1)
        .find(|&v| v != shifted)
        .ok_or("no vertex count")?;
    // Both come from the same block, as `v_readfirstlane_b32` results.
    let (vb, vi, vinst) = at(verts).ok_or("the vertex count is a lane value")?;
    let (pb, pi, pinst) = at(prims).ok_or("the primitive count is a lane value")?;
    if vb != pb {
        return Err("the counts are read in two blocks");
    }
    if vinst.name() != "v_readfirstlane_b32" || pinst.name() != "v_readfirstlane_b32" {
        return Err("a count is not a first-lane read");
    }
    let a = vb;
    let order = crate::ir::reverse_postorder(&p.blocks);
    let mut index = vec![usize::MAX; p.blocks.len()];
    for (n, &b) in order.iter().enumerate() {
        index[b] = n;
    }
    // One request: a count read in a loop header is asked for per iteration.
    if p.blocks[a]
        .preds
        .iter()
        .any(|&q| index[q] != usize::MAX && index[a] <= index[q])
    {
        return Err("the counts are read in a loop header");
    }
    // Every lane runs the block, and the reads are not behind a value guard.
    match all_lane_blocks(st) {
        Some(all) if all.get(a).copied().unwrap_or(false) => {}
        Some(_) => return Err("the counts are read where not every lane runs"),
        None => return Err("an indirect exit"),
    }
    if st.guards[a][vi].is_some() || st.guards[a][pi].is_some() {
        return Err("a count is read behind a guard");
    }
    let idom = crate::ir::idoms(&p.blocks, &order, &index);
    let dominates = |mut b: usize| {
        loop {
            if b == a {
                return true;
            }
            if b == usize::MAX || idom[b] == b || idom[b] == usize::MAX {
                return false;
            }
            b = idom[b];
        }
    };
    if !writes.iter().all(|&w| dominates(w)) {
        return Err("an output store the counts do not dominate");
    }
    Ok(OutputCounts {
        block: a,
        verts: vi,
        prims: pi,
    })
}

/// The shift an `s_lshl_b32` operand applies, when it is a constant.
fn shift_of(src: &Operand) -> Option<u32> {
    match *src {
        // Inline constants are operand codes 128-192 for 0-64.
        Operand::Const(code) if (128..=192).contains(&code) => Some(u32::from(code) - 128),
        Operand::Literal(v) if v <= 64 => Some(v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{output_counts, shift_of};
    use crate::gfx10::Operand;
    use crate::ir::Program;
    use crate::structure::Structured;
    use crate::{ir, structure};

    const S_ENDPGM: u32 = 0xBF81_0000;

    /// The two counts a group is promised are the ones the console's
    /// allocation request packs: `m0 = vertices | primitives << 12`, from a
    /// `v_readfirstlane_b32` pair.
    #[test]
    fn a_groups_counts_are_the_pair_the_request_packs() {
        let words = [
            0x7E0C_0516, // v_readfirstlane_b32 s6, v22
            0x7E0E_0522, // v_readfirstlane_b32 s7, v34
            0x8F07_8C07, // s_lshl_b32 s7, s7, 12
            0x8806_0607, // s_or_b32 s6, s7, s6
            0xBEFC_0306, // s_mov_b32 m0, s6
            0xBF90_0009, // s_sendmsg sendmsg(MSG_GS_ALLOC_REQ, GS_OP_NOP, 0)
            0xF800_0941,
            0x0000_0016, // exp prim, v22, off, off, off done
            S_ENDPGM,
        ];
        let (p, st) = program(&words);
        let counts = output_counts(&p, &st, &[0]).expect("the request's pair");
        assert_eq!(
            (counts.block, counts.verts, counts.prims),
            (0, 0, 1),
            "the vertex count is the one not shifted"
        );
    }

    /// A program with no allocation request is not one whose counts can be
    /// followed: it promises the declared maxima, as it always did.
    #[test]
    fn a_program_without_a_request_keeps_the_maxima() {
        let words = [
            0x7E0C_0516, // v_readfirstlane_b32 s6, v22
            0xF800_0941,
            0x0000_0016, // exp prim, v22, off, off, off done
            S_ENDPGM,
        ];
        let (p, st) = program(&words);
        assert_eq!(output_counts(&p, &st, &[0]), Err("no allocation request"));
    }

    /// A request whose counts are not one first-lane pair: here the vertex
    /// count is a plain scalar move, so the translation cannot name the
    /// value the group is promised.
    #[test]
    fn a_request_without_the_first_lane_pair_keeps_the_maxima() {
        let words = [
            0xBE86_0380, // s_mov_b32 s6, 0
            0x7E0E_0522, // v_readfirstlane_b32 s7, v34
            0x8F07_8C07, // s_lshl_b32 s7, s7, 12
            0x8806_0607, // s_or_b32 s6, s7, s6
            0xBEFC_0306, // s_mov_b32 m0, s6
            0xBF90_0009, // s_sendmsg sendmsg(MSG_GS_ALLOC_REQ, GS_OP_NOP, 0)
            0xF800_0941,
            0x0000_0016, // exp prim, v22, off, off, off done
            S_ENDPGM,
        ];
        let (p, st) = program(&words);
        assert_eq!(
            output_counts(&p, &st, &[0]),
            Err("a count is not a first-lane read")
        );
    }

    /// A request whose counts do not dominate an output store: the store is
    /// in the other arm of a branch, where the one `SetMeshOutputCounts`
    /// cannot be placed.
    #[test]
    fn counts_that_do_not_dominate_every_store_keep_the_maxima() {
        let words = [
            0xBF88_0007, // s_cbranch_scc1 label_+7 (the store's arm)
            0x7E0C_0516, // v_readfirstlane_b32 s6, v22
            0x7E0E_0522, // v_readfirstlane_b32 s7, v34
            0x8F07_8C07, // s_lshl_b32 s7, s7, 12
            0x8806_0607, // s_or_b32 s6, s7, s6
            0xBEFC_0306, // s_mov_b32 m0, s6
            0xBF90_0009, // s_sendmsg sendmsg(MSG_GS_ALLOC_REQ, GS_OP_NOP, 0)
            0xBF82_0002, // s_branch label_+2
            0xF800_0941,
            0x0000_0016, // exp prim, v22, off, off, off done
            S_ENDPGM,
        ];
        let (p, st) = program(&words);
        let why = output_counts(&p, &st, &[0]);
        assert_eq!(why, Err("an output store the counts do not dominate"));
    }

    /// The shift of the pack's `s_lshl_b32`, from an inline constant or a
    /// literal.
    #[test]
    fn the_pack_s_shift_reads_from_either_constant_form() {
        assert_eq!(shift_of(&Operand::Const(128 + 12)), Some(12));
        assert_eq!(shift_of(&Operand::Literal(12)), Some(12));
        assert_eq!(shift_of(&Operand::Scalar { code: 7, dwords: 1 }), None);
    }

    /// The instruction stream as a program, with its structure.
    fn program(words: &[u32]) -> (Program, Structured) {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let p = ir::build(&code, false, 256).unwrap();
        let st = structure::structure(&p).unwrap();
        (p, st)
    }
}
