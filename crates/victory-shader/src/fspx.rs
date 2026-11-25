//! `ACCC` parts: console programs the container stores as GFX10 code alone.
//!
//! A `DXIL` part carries the Xbox program table `XDAT` that [`crate::native`]
//! reads, next to an LLVM bitcode body. The FidelityFX upscaler ships its
//! compute programs another way: containers with **no** `DXIL` part (and no
//! SM5 `SHDR` one either), whose code is the `ACCC` part, an `FSPX` package.
//!
//! ```text
//! +0x00 "FSPX" u32 version (1), u32 entry count (4), u32 total size
//! +0x10 per entry, 16 B: u32 kind, u32 offset, u32 size, u32 0
//! kind 0  0x1c bytes  a per-program header, not decoded: what it would say
//!                    (`COMPUTE_PGM_RSRC2`, VGPRs) is read off the code
//! kind 1  0x14 bytes  a version word and zeroes, as `XBCH`'s header
//! kind 6  the program's GFX10 code, as the console runs it
//! kind 7  the metadata below
//! ```
//!
//! `kind 7` opens with 0x18 header bytes: `u32 0x403`, `u32 size - 0x14`,
//! `u32 code size in dwords`, two zero words, then `u8 0x40, u8 flags,
//! u8 recorded descriptors, u8 0`. Entries of 32 bytes follow, tagged by
//! their first word:
//!
//! ```text
//! 0x41  a descriptor the program reads: at +4 the 20-byte entry
//!       [`crate::native`] reads from `XDAT` (u8 resource type, u8 root
//!       type, u16 0, u32 table offset in dwords, u32 lower bound, u32
//!       stride, u8 descriptor dwords, u8 first user-data dword, u8 root
//!       slot, u8 0), then u32 0xffffffff, u32 0
//! 0x42  its range's own record, not read
//! 0x43  the program's 16-byte hash (the container's `HASH`), then zeroes
//! ```
//!
//! `flags` bit 7 is wave32, else wave64. The three programs of the shipped
//! package that set it are the ones whose code never forms a 64-bit lane
//! mask: none of them uses `v_mbcnt_hi_u32_b32`, which every program that
//! forms one does next to the `_lo` form, and they read a one-dword scratch
//! wave offset (`s126` alone, not `s126:127`).
//!
//! The dispatch the hardware would be told is not in the package, so it is
//! worked out from the two parts that do describe it:
//!
//! - `PSV0` gives the stage (compute here) and the thread group.
//! - `RTS0` is the root signature the programs were built for: 6 header
//!   words (`version`, `count`, `parameters offset`, `static samplers`,
//!   their offset, `flags`), then 12 bytes per parameter, `{u32 type, u32
//!   visibility, u32 data offset}`: type 0 a descriptor table, whose data is
//!   `{u32 range count, u32 ranges offset}` and whose ranges are 24 bytes
//!   `{u32 type, u32 descriptors, u32 base register, u32 register space,
//!   u32 offset in table, u32 0xffffffff}`, and types 2, 3 and 4 a root
//!   CBV, SRV or UAV descriptor, whose data is `{u32 register, u32 space}`.
//!
//! The user data starts at dword 4 and holds one dword per table and two per
//! descriptor, in parameter order; the group ids follow it. The shipped
//! programs' live-in SGPRs say so: the three-parameter shape puts the group
//! ids at s8 and its programs read s8 and s9, the six-parameter one puts them
//! at s14 and its programs read s14, s15 and s16.
//!
//! What is left (`vgprs`, the LDS size, the thread ids) is read off the code,
//! because the bounds the code shows are exact where `kind 0`'s fields are
//! not established.
//!
//! This is the path the title takes when `DeserializeComputePipelineStateX`
//! answers `E_NOTIMPL` (`victory_gpu::device`'s slot 0x218, whose description
//! carries these very `kind 0` and `kind 1` blobs): a serialized console
//! pipeline state carries no bytecode a PC adapter would take, so the fallback
//! recreates the pipeline from the container the program's `CreateCompute\
//! PipelineState` was given, which is this one.

use crate::ShaderError::{self, Malformed, Unsupported};
use crate::dxbc::{Container, FourCC, u32_at};
use crate::gfx10::{Fields, Operand};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};

const MAGIC: &[u8] = b"FSPX";
const VERSION: u32 = 1;
const HEADER: usize = 0x10;
const ENTRY_SIZE: usize = 0x10;
const MAX_ENTRIES: u32 = 32;
const KIND_CODE: u32 = 6;
const KIND_METADATA: u32 = 7;

const META_HEADER: usize = 0x18;
const META_VERSION: u32 = 0x403;
const META_SIZE_BIAS: usize = 0x14;
const META_FLAGS: usize = 0x15;
const META_RECORD: usize = 32;
const TAG_DESCRIPTOR: u32 = 0x41;
const TAG_HASH: u32 = 0x43;
/// A record sits at +4 of its 32-byte entry, the tag being the first word.
const RECORD: usize = 4;
const RECORD_SIZE: usize = 20;
const FLAG_WAVE32: u8 = 0x80;

const SIGNATURE_VERSION_MAX: u32 = 2;
const MAX_PARAMS: u32 = 32;
const PARAM_SIZE: usize = 12;
const PARAM_TABLE: u32 = 0;
const PARAM_CONSTANTS: u32 = 1;
const PARAM_DESCRIPTOR: u32 = 2;
const PARAM_LAST_DESCRIPTOR: u32 = 4;
const RANGE_SIZE: usize = 24;
const MAX_RANGES: u32 = 64;
/// One dword per table and two per descriptor, after four the root signature
/// does not describe (no program of the shipped package reads them).
const USER_DATA_BASE: u8 = 4;
const MAX_USER_DATA: u32 = 32;
/// `COMPUTE_PGM_RSRC2` holds the user SGPR count in five bits.
const MAX_USER_SGPRS: u32 = 32;

const PSV_STAGE: usize = 0x1c;
const PSV_THREADS: [usize; 3] = [0x28, 0x2c, 0x30];
const STAGE_COMPUTE: u8 = 5;
const MAX_THREADS: u32 = 1024;

/// The widest LDS access there is, in bytes.
const DS_MAX_BYTES: usize = 16;
const LDS_GRANULE: usize = 512;
const LDS_GRANULES_MAX: u32 = 0x1FF;
/// What a program that computes LDS addresses may reach past what its
/// immediates name: one granule, the unit the size is stated in.
const LDS_DYNAMIC_MARGIN: usize = LDS_GRANULE;

/// Whether `part` is an `FSPX` package.
pub fn is_package(part: &[u8]) -> bool {
    part.get(..MAGIC.len()) == Some(MAGIC)
}

/// The programs an `FSPX` part holds: its one compute program.
pub fn programs(part: &[u8], container: &Container) -> Result<Vec<NativeProgram>, ShaderError> {
    if !is_package(part) {
        return Err(Malformed("an ACCC part that is not an FSPX package"));
    }
    if u32_at(part, 4)? != VERSION {
        return Err(Malformed("unknown FSPX version"));
    }
    let count = u32_at(part, 8)?;
    if count > MAX_ENTRIES {
        return Err(Malformed("implausible FSPX entry count"));
    }
    if u32_at(part, 12)? as usize != part.len() {
        return Err(Malformed("FSPX size disagrees with its part"));
    }
    let (mut code, mut metadata) = (None, None);
    for i in 0..count as usize {
        let at = HEADER + ENTRY_SIZE * i;
        let kind = u32_at(part, at)?;
        let offset = u32_at(part, at + 4)? as usize;
        let size = u32_at(part, at + 8)? as usize;
        let body = offset
            .checked_add(size)
            .and_then(|end| part.get(offset..end))
            .ok_or(Malformed("FSPX entry out of bounds"))?;
        match kind {
            KIND_CODE => code = Some(body),
            KIND_METADATA => metadata = Some(body),
            _ => {}
        }
    }
    let code = code.ok_or(Malformed("FSPX package without code"))?;
    let metadata = metadata.ok_or(Malformed("FSPX package without metadata"))?;
    let (used, wave32) = metadata_of(metadata, code.len())?;
    let (threads, stage) = stage_and_threads(container)?;
    if stage != STAGE_COMPUTE {
        return Err(Unsupported {
            offset: 0,
            what: "an ACCC program of a stage other than compute",
        });
    }
    let layout = container
        .part(FourCC::RTS0)
        .ok_or(Unsupported {
            offset: 0,
            what: "an ACCC program of a container without a root signature part",
        })
        .and_then(|part| signature(part.data))?;
    let (vgprs, lds) = extents(code, !wave32)?;
    let user_sgprs = layout
        .last()
        .map_or(u32::from(USER_DATA_BASE), |p| u32::from(p.user_data) + p.sgprs);
    // `USER_SGPR` holds five bits, so 32 dwords of user data would spill into
    // `TRAP_PRESENT` and `TGID_XYZ_EN` (bits 6 to 9) and the group ids would
    // land outside the SGPRs a 32-thread group initialises.
    if user_sgprs >= MAX_USER_SGPRS {
        return Err(Unsupported {
            offset: 0,
            what: "an ACCC program whose root signature takes 32 or more user SGPRs",
        });
    }
    let mut rsrc2 = user_sgprs << 1;
    // The group ids are the SGPRs after the user data, all three of them: the
    // shipped programs read s14 to s16 of a group of one thread each way,
    // which only a dispatch's own dimensions can be.
    rsrc2 |= 0b111 << 7;
    // Thread ids: `TIDIG_COMP_CNT` is the highest component the group enables,
    // not how many dimensions are above one, so a group of [1, 64, 1] still
    // gets v0 and v1.
    let dimensions = threads.iter().rposition(|t| *t > 1).map_or(1, |h| h + 1);
    rsrc2 |= ((dimensions - 1) as u32) << 11;
    rsrc2 |= (lds.div_ceil(LDS_GRANULE as u32)).min(LDS_GRANULES_MAX) << 15;
    Ok(vec![NativeProgram {
        stage: HwStage::Cs,
        wave64: !wave32,
        vgprs,
        layout,
        used,
        io: StageIo::Cs(ComputeIo { rsrc2, threads }),
        code: code.to_vec(),
    }])
}

/// The descriptors a program reads, and whether it runs 32 lanes to a wave.
fn metadata_of(
    metadata: &[u8],
    code_bytes: usize,
) -> Result<(Vec<UsedResource>, bool), ShaderError> {
    if u32_at(metadata, 0)? != META_VERSION {
        return Err(Malformed("unknown shader package metadata version"));
    }
    let stated = u32_at(metadata, 4)? as usize;
    if stated.checked_add(META_SIZE_BIAS) != Some(metadata.len()) {
        return Err(Malformed("shader package metadata size disagrees with its part"));
    }
    if u32_at(metadata, 8)? as usize * 4 != code_bytes {
        return Err(Malformed("shader package metadata code size disagrees with the code"));
    }
    let flags = metadata
        .get(META_FLAGS)
        .copied()
        .ok_or(Malformed("shader package metadata truncated"))?;
    let mut used = Vec::new();
    let mut at = META_HEADER;
    while at + META_RECORD <= metadata.len() {
        let tag = u32_at(metadata, at)?;
        if tag == TAG_DESCRIPTOR {
            let r = at + RECORD;
            let bytes = metadata
                .get(r..r + RECORD_SIZE)
                .ok_or(Malformed("shader package descriptor out of bounds"))?;
            let word = |n: usize| u32::from_le_bytes(bytes[4 * n..4 * n + 4].try_into().expect("4"));
            used.push(UsedResource {
                kind: match bytes[0] {
                    0 => ResourceType::Srv,
                    1 => ResourceType::Sampler,
                    2 => ResourceType::Cbv,
                    3 => ResourceType::Uav,
                    n => ResourceType::Other(n),
                },
                root: match bytes[1] {
                    0 => RootType::Constants,
                    1 => RootType::Descriptor,
                    3 => RootType::Table,
                    n => RootType::Other(u32::from(n)),
                },
                table_offset_dwords: word(1),
                lower_bound: word(2),
                stride: word(3),
                dwords: bytes[16],
                user_data: bytes[17],
                slot: bytes[18],
            });
        } else if tag == TAG_HASH {
            break;
        }
        at += META_RECORD;
    }
    if used.is_empty() {
        return Err(Malformed("shader package naming no descriptor"));
    }
    Ok((used, flags & FLAG_WAVE32 != 0))
}

/// The thread group a package states and its stage, from its `PSV0` part.
fn stage_and_threads(container: &Container) -> Result<([u32; 3], u8), ShaderError> {
    let part = container.part(FourCC::PSV0).ok_or(Unsupported {
        offset: 0,
        what: "an ACCC program of a container without a PSV0 part",
    })?;
    let stage = part
        .data
        .get(PSV_STAGE)
        .copied()
        .ok_or(Malformed("PSV0 part truncated"))?;
    let mut threads = [1u32; 3];
    for (i, at) in PSV_THREADS.iter().enumerate() {
        threads[i] = u32_at(part.data, *at)?;
    }
    let total = threads.iter().product::<u32>();
    if total == 0 || total > MAX_THREADS || threads.contains(&0) {
        return Err(Malformed("implausible thread group size"));
    }
    Ok((threads, stage))
}

/// The root signature a package's programs were built for, as the layout
/// [`crate::native::NativeProgram`] carries: where each parameter's user data
/// starts.
fn signature(data: &[u8]) -> Result<Vec<RootParam>, ShaderError> {
    if u32_at(data, 0)? > SIGNATURE_VERSION_MAX {
        return Err(Malformed("unknown root signature version"));
    }
    let count = u32_at(data, 4)?;
    if count == 0 {
        return Err(Malformed("root signature without parameters"));
    }
    if count > MAX_PARAMS {
        return Err(Malformed("implausible root parameter count"));
    }
    let first = u32_at(data, 8)? as usize;
    let mut params: Vec<(RootType, u32, u32)> = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        let at = first + PARAM_SIZE * i;
        let kind = u32_at(data, at)?;
        let data_at = u32_at(data, at + 8)? as usize;
        let param = match kind {
            PARAM_TABLE => {
                let ranges = u32_at(data, data_at)?;
                if ranges > MAX_RANGES {
                    return Err(Malformed("implausible descriptor range count"));
                }
                let ranges_at = u32_at(data, data_at + 4)? as usize;
                let mut base = None;
                for r in 0..ranges as usize {
                    let register = u32_at(data, ranges_at + RANGE_SIZE * r + 8)?;
                    base = Some(base.map_or(register, |b: u32| b.min(register)));
                }
                (RootType::Table, 1, base.unwrap_or(0))
            }
            PARAM_CONSTANTS => {
                return Err(Unsupported {
                    offset: 0,
                    what: "an ACCC program's root signature with a constant parameter, whose \
                           size this serializer does not state",
                });
            }
            PARAM_DESCRIPTOR..=PARAM_LAST_DESCRIPTOR => {
                (RootType::Descriptor, 2, u32_at(data, data_at)?)
            }
            _ => {
                return Err(Unsupported {
                    offset: 0,
                    what: "an ACCC program's root signature with a parameter type it does not \
                           describe",
                });
            }
        };
        params.push(param);
    }
    let mut next = u32::from(USER_DATA_BASE);
    let mut layout = Vec::with_capacity(params.len());
    for (slot, (root, dwords, base_register)) in params.into_iter().enumerate() {
        let user_data = next as u8;
        next += dwords;
        layout.push(RootParam {
            root,
            table_offset_dwords: 0,
            sgprs: dwords,
            base_register,
            user_data,
            slot: slot as u8,
        });
    }
    if next > MAX_USER_DATA {
        return Err(Malformed("implausible root signature size"));
    }
    Ok(layout)
}

/// The VGPR past the highest the code names, and the LDS bytes it reaches.
///
/// What is still unknown: the LDS size the program itself states. The package
/// has a `kind 0` header per program that would hold its
/// `COMPUTE_PGM_RSRC2`, but its layout is not established (nothing of the
/// shipped package has been decoded from it), so the reach is taken from the
/// code: the immediates of every `ds_*` access, scaled as the instruction
/// scales them, plus one granule when the code addresses LDS by an address it
/// computes, which no immediate bounds.
fn extents(code: &[u8], wave64: bool) -> Result<(u16, u32), ShaderError> {
    let mut vgprs = 0u16;
    let mut lds = 0usize;
    // Whether an access adds an address the code computes to its immediates.
    let mut dynamic = false;
    for inst in crate::gfx10::decode(code)? {
        let fields = inst.fields(wave64);
        let mut see = |o: &Operand| {
            if let Operand::Vector { reg, dwords } = *o {
                vgprs = vgprs.max(u16::from(reg) + u16::from(dwords.max(1)));
            }
        };
        match &fields {
            Fields::Sop { dst, src, .. } => {
                dst.iter().chain(src).for_each(&mut see);
            }
            Fields::Sopp { .. } => {}
            Fields::Smem {
                data, base, soffset, ..
            } => {
                data.iter().chain([base, soffset]).for_each(&mut see);
            }
            Fields::Valu(v) => {
                v.dst.iter().chain(&v.carry).chain(&v.src).for_each(&mut see);
            }
            Fields::Vintrp { dst, src, attr } => [dst, src, attr].iter().for_each(|o| see(o)),
            Fields::Ds {
                dst,
                addr,
                data,
                offset0,
                offset1,
                gds,
            } => {
                dst.iter().chain(data).chain(addr.as_ref()).for_each(&mut see);
                if !gds {
                    let name = inst.name().unwrap_or_default();
                    for bytes in ds_offsets(name, *offset0, *offset1) {
                        lds = lds.max(bytes + DS_MAX_BYTES);
                    }
                    // A swizzle or a permute names a lane, not a place in the
                    // allocation.
                    let lane_file =
                        name.starts_with("ds_swizzle") || name.starts_with("ds_bpermute");
                    dynamic |= addr.is_some() && !lane_file;
                }
            }
            Fields::Buffer {
                data,
                addr,
                rsrc,
                soffset,
                lds: through_lds,
                ..
            } => {
                data.iter().chain([addr, rsrc, soffset]).for_each(&mut see);
                if *through_lds {
                    lds = lds.max(DS_MAX_BYTES);
                }
            }
            Fields::Image {
                data,
                addr,
                rsrc,
                sampler,
                ..
            } => {
                std::iter::once(data)
                    .chain(addr)
                    .chain([rsrc])
                    .chain(sampler.as_ref())
                    .for_each(&mut see);
            }
            Fields::Flat {
                dst, addr, data, saddr, ..
            } => {
                dst.iter()
                    .chain(addr.as_ref())
                    .chain(data.as_ref())
                    .chain(saddr.as_ref())
                    .for_each(&mut see);
            }
            Fields::Exp { src, .. } => {
                src.iter().flatten().for_each(&mut see);
            }
        }
    }
    if dynamic && lds > 0 {
        lds += LDS_DYNAMIC_MARGIN;
    }
    Ok((vgprs.max(1), lds as u32))
}

/// The byte offsets a `ds_*` instruction's immediates add to its address. The
/// pair forms (`ds_read2_b32`, `ds_write2st64_b64`) count in elements of their
/// width, `_st64` in 64 of those; every other form names bytes, and only
/// `offset0` is an offset (see `emit::lower`'s `ds`).
fn ds_offsets(name: &str, offset0: u8, offset1: u8) -> Vec<usize> {
    let pair = ["read2", "write2", "wrxchg2"]
        .iter()
        .any(|p| name.contains(p));
    if !pair {
        return vec![usize::from(offset0).max(usize::from(offset1))];
    }
    let width = match name.rsplit('_').next() {
        Some("b64") => 2,
        Some("b96") => 3,
        Some("b128") => 4,
        _ => 1,
    };
    let scale = 4 * width * if name.contains("st64") { 64 } else { 1 };
    vec![usize::from(offset0) * scale, usize::from(offset1) * scale]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dxbc::build;
    use crate::gfx10::{Encoding, Instruction};

    /// `s_endpgm`.
    const ENDPGM: u32 = 0xBF81_0000;

    /// The opcode the decoder gives `name`, so a test never spells one out.
    fn opcode(encoding: Encoding, name: &str) -> u16 {
        (0..=u16::MAX)
            .find(|&opcode| {
                Instruction {
                    offset: 0,
                    encoding,
                    opcode,
                    words: vec![],
                }
                .name()
                    == Some(name)
            })
            .unwrap_or_else(|| panic!("no opcode named {name}"))
    }

    /// A descriptor record as `kind 7` stores it.
    fn record(kind: u8, root: u8, table_offset: u32, lower_bound: u32, dwords: u8, user_data: u8, slot: u8) -> [u8; META_RECORD] {
        let mut out = [0u8; META_RECORD];
        out[0..4].copy_from_slice(&TAG_DESCRIPTOR.to_le_bytes());
        let r = &mut out[RECORD..RECORD + RECORD_SIZE];
        r[0] = kind;
        r[1] = root;
        r[4..8].copy_from_slice(&table_offset.to_le_bytes());
        r[8..12].copy_from_slice(&lower_bound.to_le_bytes());
        r[12..16].copy_from_slice(&1u32.to_le_bytes());
        r[16] = dwords;
        r[17] = user_data;
        r[18] = slot;
        out
    }

    /// A `kind 7` part over a program of `code` dwords.
    fn metadata(code: &[u8], wave32: bool, records: &[[u8; META_RECORD]]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&META_VERSION.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // size, filled in below
        out.extend_from_slice(&((code.len() / 4) as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&[
            0x40,
            if wave32 { 0x84 } else { 0x04 },
            records.len() as u8,
            0,
        ]);
        for r in records {
            out.extend_from_slice(r);
        }
        let mut hash = [0u8; META_RECORD];
        hash[0..4].copy_from_slice(&TAG_HASH.to_le_bytes());
        hash[RECORD..RECORD + 4].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        out.extend_from_slice(&hash);
        let size = (out.len() - META_SIZE_BIAS) as u32;
        out[4..8].copy_from_slice(&size.to_le_bytes());
        out
    }

    /// An `FSPX` package holding `code` and `metadata`, and the two parts the
    /// reader skips.
    fn package(code: &[u8], metadata: &[u8]) -> Vec<u8> {
        let header = HEADER + ENTRY_SIZE * 4;
        let code_at = header;
        let meta_at = code_at + code.len();
        let total = meta_at + metadata.len();
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        for (kind, at, size) in [
            (KIND_CODE, code_at, code.len()),
            (KIND_METADATA, meta_at, metadata.len()),
            (0u32, header, 0usize),
            (1u32, header, 0usize),
        ] {
            out.extend_from_slice(&kind.to_le_bytes());
            out.extend_from_slice(&(at as u32).to_le_bytes());
            out.extend_from_slice(&(size as u32).to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
        }
        out.extend_from_slice(code);
        out.extend_from_slice(metadata);
        out
    }

    /// A `PSV0` part for a compute program of `threads` threads.
    fn psv0(threads: [u32; 3]) -> Vec<u8> {
        let mut out = vec![0u8; PSV_THREADS[2] + 4];
        out[PSV_STAGE] = STAGE_COMPUTE;
        for (i, t) in threads.iter().enumerate() {
            out[PSV_THREADS[i]..PSV_THREADS[i] + 4].copy_from_slice(&t.to_le_bytes());
        }
        out
    }

    /// A root signature of `tables` descriptor tables, then `descriptors` root
    /// descriptors, each table holding one empty-except-register range.
    fn signature_part(tables: u32, descriptors: u32) -> Vec<u8> {
        let count = (tables + descriptors) as usize;
        let first = 0x18usize;
        let mut out = vec![0u8; first + PARAM_SIZE * count];
        out[0..4].copy_from_slice(&SIGNATURE_VERSION_MAX.to_le_bytes());
        out[4..8].copy_from_slice(&(count as u32).to_le_bytes());
        out[8..12].copy_from_slice(&(first as u32).to_le_bytes());
        out[16..20].copy_from_slice(&0u32.to_le_bytes()); // no static samplers
        let mut data_at = out.len();
        for i in 0..count {
            let at = first + PARAM_SIZE * i;
            let table = (i as u32) < tables;
            let kind = if table { PARAM_TABLE } else { PARAM_DESCRIPTOR };
            out[at..at + 4].copy_from_slice(&kind.to_le_bytes());
            out[at + 8..at + 12].copy_from_slice(&(data_at as u32).to_le_bytes());
            if table {
                out.resize(data_at + 8 + RANGE_SIZE, 0);
                out[data_at..data_at + 4].copy_from_slice(&1u32.to_le_bytes());
                out[data_at + 4..data_at + 8].copy_from_slice(&((data_at + 8) as u32).to_le_bytes());
                // The range's base register is its parameter's index, so a
                // test can tell which range a parameter read.
                out[data_at + 8 + 8..data_at + 8 + 12].copy_from_slice(&(i as u32).to_le_bytes());
                data_at += 8 + RANGE_SIZE;
            } else {
                out.resize(data_at + 8, 0);
                out[data_at..data_at + 4].copy_from_slice(&(i as u32 - tables).to_le_bytes());
                data_at += 8;
            }
        }
        out
    }

    fn container(
        threads: [u32; 3],
        tables: u32,
        descriptors: u32,
        records: &[[u8; META_RECORD]],
        code: &[u8],
        wave32: bool,
    ) -> Vec<u8> {
        let meta = metadata(code, wave32, records);
        build(&[
            (FourCC::PSV0, &psv0(threads)),
            (FourCC::RTS0, &signature_part(tables, descriptors)),
            (FourCC::ACCC, &package(code, &meta)),
        ])
    }

    fn program(bytes: &[u8]) -> NativeProgram {
        let c = Container::parse(bytes).unwrap();
        let programs = programs(c.part(FourCC::ACCC).unwrap().data, &c).unwrap();
        assert_eq!(programs.len(), 1);
        programs[0].clone()
    }

    #[test]
    fn reads_the_user_data_the_root_signature_gives() {
        // The shipped package's six-parameter shape: two tables, then four
        // root descriptors, after four dwords it does not describe.
        let records = [
            record(3, 3, 0, 11, 4, 4, 0),
            record(0, 3, 8, 17, 4, 5, 1),
            record(2, 1, 0, 0, 2, 6, 2),
        ];
        let bytes = container([64, 1, 1], 2, 4, &records, &ENDPGM.to_le_bytes(), false);
        let p = program(&bytes);
        assert_eq!(p.stage, HwStage::Cs);
        assert!(p.wave64);
        let layout = &p.layout;
        assert_eq!(layout.len(), 6);
        assert_eq!(layout[0].root, RootType::Table);
        assert_eq!(layout[0].user_data, 4);
        assert_eq!(layout[1].user_data, 5);
        assert_eq!(layout[2].root, RootType::Descriptor);
        assert_eq!(layout[2].user_data, 6);
        assert_eq!(layout[2].sgprs, 2);
        assert_eq!(layout[5].user_data, 12);
        // The descriptors as the metadata states them.
        assert_eq!(p.used.len(), 3);
        assert_eq!(p.used[0].lower_bound, 11);
        assert_eq!(p.used[0].kind, ResourceType::Uav);
        assert_eq!(p.used[1].user_data, 5);
        assert_eq!(p.used[2].kind, ResourceType::Cbv);
        assert_eq!(p.used[2].root, RootType::Descriptor);
        let StageIo::Cs(cs) = p.io else {
            panic!("a compute program");
        };
        assert_eq!(cs.threads, [64, 1, 1]);
        // User data ends at dword 14, so the group ids are s14, s15 and s16.
        assert_eq!(cs.user_sgprs(), 14);
        let (ids, info) = cs.system_sgprs();
        assert_eq!(ids, [Some(14), Some(15), Some(16)]);
        assert_eq!(info, None);
        assert_eq!(cs.initialized_sgprs(), 17);
        // One VGPR of thread ids: the group is one thread two ways.
        assert_eq!(cs.thread_id_vgprs(), 1);
        assert_eq!(p.vgprs, 1);
        assert_eq!(cs.lds_bytes(), 0);
    }

    #[test]
    fn a_thread_group_of_two_dimensions_gets_two_thread_ids() {
        let records = [record(3, 3, 0, 0, 4, 4, 0)];
        let bytes = container([8, 8, 1], 1, 0, &records, &ENDPGM.to_le_bytes(), false);
        let StageIo::Cs(cs) = program(&bytes).io else {
            panic!("a compute program");
        };
        assert_eq!(cs.threads, [8, 8, 1]);
        assert_eq!(cs.thread_id_vgprs(), 2);
        // The three-parameter shape's user data is one table and one
        // descriptor, so the group ids start at s8.
        let records = [record(3, 3, 0, 0, 4, 4, 0), record(2, 1, 0, 0, 2, 5, 1)];
        let bytes = container([8, 8, 1], 1, 1, &records, &ENDPGM.to_le_bytes(), false);
        let p = program(&bytes);
        assert_eq!(p.layout[1].user_data, 5);
        let StageIo::Cs(cs) = p.io else {
            panic!("a compute program");
        };
        assert_eq!(cs.system_sgprs().0, [Some(7), Some(8), Some(9)]);
    }

    #[test]
    fn a_wave32_package_says_so() {
        let records = [record(3, 3, 0, 0, 4, 4, 0)];
        let bytes = container([64, 1, 1], 1, 0, &records, &ENDPGM.to_le_bytes(), true);
        assert!(!program(&bytes).wave64);
        let bytes = container([64, 1, 1], 1, 0, &records, &ENDPGM.to_le_bytes(), false);
        assert!(program(&bytes).wave64);
    }

    #[test]
    fn the_lds_bound_follows_the_code() {
        let ds = opcode(Encoding::Ds, "ds_write_b128");
        let mut code = Vec::new();
        // `ds_write_b128 v0, v2 offset:0x100`, then `s_endpgm`.
        code.extend_from_slice(&(0xD800_0000 | ((ds as u32) << 18) | 0x100).to_le_bytes());
        code.extend_from_slice(&(2u32 << 8).to_le_bytes());
        code.extend_from_slice(&ENDPGM.to_le_bytes());
        let records = [record(3, 3, 0, 0, 4, 4, 0)];
        let bytes = container([64, 1, 1], 1, 0, &records, &code, false);
        let p = program(&bytes);
        // v0 to v5: the four dwords written start at v2.
        assert_eq!(p.vgprs, 6);
        let StageIo::Cs(cs) = p.io else {
            panic!("a compute program");
        };
        // 0x100 + a 16-byte access is one granule, and the address v0 is
        // computed: a second one is the margin such an access gets.
        assert_eq!(cs.lds_bytes(), 2 * LDS_GRANULE as u32);
    }

    /// The address of a `ds_*` access counts towards the VGPR total as much
    /// as its data does.
    #[test]
    fn a_vgpr_used_only_as_an_lds_address_is_counted() {
        let ds = opcode(Encoding::Ds, "ds_write_b32");
        let mut code = Vec::new();
        // `ds_write_b32 v5, v0`, then `s_endpgm`.
        code.extend_from_slice(&(0xD800_0000 | ((ds as u32) << 18)).to_le_bytes());
        code.extend_from_slice(&5u32.to_le_bytes());
        code.extend_from_slice(&ENDPGM.to_le_bytes());
        let records = [record(3, 3, 0, 0, 4, 4, 0)];
        let bytes = container([64, 1, 1], 1, 0, &records, &code, false);
        // v5 is the address and v0 the data: the program names v0 to v5.
        assert_eq!(program(&bytes).vgprs, 6);
    }

    /// `ds_read2_b32` counts its two offsets in dwords, not in bytes.
    #[test]
    fn a_pair_access_scales_its_offsets() {
        let ds = opcode(Encoding::Ds, "ds_read2_b32");
        let mut code = Vec::new();
        // `ds_read2_b32 v[0:1], v2 offset0:0x40 offset1:0x80`, then `s_endpgm`.
        code.extend_from_slice(&(0xD800_0000 | ((ds as u32) << 18) | 0x8040).to_le_bytes());
        code.extend_from_slice(&(2u32 << 8).to_le_bytes());
        code.extend_from_slice(&ENDPGM.to_le_bytes());
        let records = [record(3, 3, 0, 0, 4, 4, 0)];
        let bytes = container([64, 1, 1], 1, 0, &records, &code, false);
        let StageIo::Cs(cs) = program(&bytes).io else {
            panic!("a compute program");
        };
        // The offsets count dwords: 0x80 of them reach 0x200 bytes, and a
        // four-byte access follows it. Read as bytes they would reach 0x80
        // (one granule); with the margin a computed address gets, 0x210 and
        // one granule are three.
        assert_eq!(cs.lds_bytes(), 3 * LDS_GRANULE as u32);
    }

    #[test]
    fn a_group_of_one_thread_two_ways_still_gets_two_thread_ids() {
        // `TIDIG_COMP_CNT` names the highest component the group enables, so a
        // group that is one thread in X and many in Y enables both.
        let records = [record(3, 3, 0, 0, 4, 4, 0)];
        for (threads, ids) in [([1, 64, 1], 2), ([1, 1, 64], 3), ([1, 32, 4], 3)] {
            let bytes = container(threads, 1, 0, &records, &ENDPGM.to_le_bytes(), false);
            let StageIo::Cs(cs) = program(&bytes).io else {
                panic!("a compute program");
            };
            assert_eq!(cs.thread_id_vgprs(), ids, "{threads:?}");
        }
    }

    /// 32 dwords of user data do not fit the five-bit field the register
    /// holds them in.
    #[test]
    fn a_root_signature_of_32_user_sgprs_is_refused() {
        let records = [record(3, 3, 0, 0, 4, 4, 0)];
        let bytes = container([64, 1, 1], 28, 0, &records, &ENDPGM.to_le_bytes(), false);
        let c = Container::parse(&bytes).unwrap();
        assert!(
            matches!(
                programs(c.part(FourCC::ACCC).unwrap().data, &c).unwrap_err(),
                Unsupported {
                    what: "an ACCC program whose root signature takes 32 or more user SGPRs",
                    ..
                }
            ),
            "a 32-dword root signature"
        );
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        let records = [record(3, 3, 0, 0, 4, 4, 0)];
        let good = container([64, 1, 1], 1, 0, &records, &ENDPGM.to_le_bytes(), false);
        let c = Container::parse(&good).unwrap();
        let part = c.part(FourCC::ACCC).unwrap().data;
        assert_eq!(
            programs(b"NOPE", &c).unwrap_err(),
            Malformed("an ACCC part that is not an FSPX package")
        );
        // A metadata version this reader does not know.
        let mut bad = part.to_vec();
        let at = u32::from_le_bytes(part[0x24..0x28].try_into().unwrap()) as usize;
        bad[at..at + 4].copy_from_slice(&9u32.to_le_bytes());
        assert!(matches!(
            programs(&bad, &c).unwrap_err(),
            Malformed("unknown shader package metadata version")
        ));
        // A container without the root signature the user data needs.
        let no_signature = build(&[
            (FourCC::PSV0, &psv0([64, 1, 1])),
            (FourCC::ACCC, part),
        ]);
        let c = Container::parse(&no_signature).unwrap();
        assert!(matches!(
            programs(c.part(FourCC::ACCC).unwrap().data, &c).unwrap_err(),
            Unsupported { .. }
        ));
    }
}
