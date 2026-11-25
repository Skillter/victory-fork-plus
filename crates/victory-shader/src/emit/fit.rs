//! A mesh shader's declared outputs, cut down to what the pixel program it is
//! paired with reads.
//!
//! The DXIL validator charges four bytes for every component a mesh program
//! declares, whatever its type ([`container::mesh_output_bytes`]): at 128
//! vertices, 65 components are 33280 bytes and over
//! [`container::MESH_OUTPUT_LIMIT`], and a 16-bit column costs as much as an
//! `f32` one. So no repacking brings a wider signature under the cap; the only
//! way is to declare fewer components — and the ones no paired pixel program
//! reads are free to drop.
//!
//! The cut is made on the console containers, never on the lowering: a trimmed
//! container goes through the ordinary [`super::translate`], which drops the
//! stores of the elements that are gone (an export with no output element for
//! its row stores nothing), recomputes the components each element writes and
//! re-emits both signatures. Semantics, indices, registers and interpolation
//! modes are kept, so the rows stay valid and D3D12 still links the stages by
//! semantic. The trimmed bytes are the shader cache's key, so a narrowed
//! variant can never be served for a pair that was not narrowed.

use super::container::{self, MESH_OUTPUT_LIMIT, SigElement};
use crate::dxbc::{Container, FourCC, build, u16_at, u32_at};

/// One semantic of a signature: the components it declares and the ones the
/// program reads or writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Components {
    pub name: String,
    pub index: u32,
    /// DXIL semantic kind: 0 arbitrary, 3 position, 13 front face...
    pub kind: u8,
    /// The declared components, as a mask (`bit 0` is `.x`).
    pub mask: u8,
    /// The components the program reads (inputs) or writes (outputs), as a
    /// mask.
    pub read: u8,
}

/// The components a signature element declares, as a mask.
fn mask(e: &SigElement) -> u8 {
    ((1u8 << e.cols) - 1) << e.col
}

/// The elements of one signature part of a container.
fn elements(c: &Container, fourcc: FourCC) -> Vec<SigElement> {
    match c.part(fourcc) {
        Some(part) => super::signature(part.data).unwrap_or_default(),
        None => Vec::new(),
    }
}

/// The elements of one signature part, with the components each declares and
/// reads: `signature` keeps the read byte out of the element (the lowering
/// recomputes it), but a cut needs it.
fn rows(part: &[u8]) -> Option<Vec<(SigElement, u8)>> {
    let elements = super::signature(part).ok()?;
    let base = u32_at(part, 4).ok()? as usize;
    Some(
        elements
            .into_iter()
            .enumerate()
            .map(|(i, e)| {
                let read = part.get(base + 32 * i + 25).copied().unwrap_or(0);
                let read = read & mask(&e);
                (e, read)
            })
            .collect(),
    )
}

/// The input signature of a container: what it declares and what it reads.
///
/// A pixel program made for a pair reads less than it declares; a union of
/// several pixel programs' interfaces is a safe trim target for a mesh one.
pub fn interface(container: &[u8]) -> Vec<Components> {
    Container::parse(container).map_or_else(|_| Vec::new(), |c| interface_of(&c))
}

/// [`interface`] of an already parsed container.
pub fn interface_of(c: &Container) -> Vec<Components> {
    signature_of(c, FourCC::ISG1)
}

/// The output signature of a container: what it declares and what it writes.
pub fn output_interface(container: &[u8]) -> Vec<Components> {
    Container::parse(container).map_or_else(|_| Vec::new(), |c| signature_of(&c, FourCC::OSG1))
}

fn signature_of(c: &Container, fourcc: FourCC) -> Vec<Components> {
    let Some(part) = c.part(fourcc) else {
        return Vec::new();
    };
    rows(part.data)
        .unwrap_or_default()
        .into_iter()
        .map(|(e, read)| {
            let declared = mask(&e);
            Components {
                name: e.name,
                index: e.index,
                kind: e.kind,
                mask: declared,
                read,
            }
        })
        .collect()
}

/// A container whose only part is an input signature with these elements: a
/// stand-in pixel program, so [`pair`] can trim a mesh to a union of the
/// interfaces of the pixel programs that may be paired with it.
pub fn interface_container(inputs: &[Components]) -> Vec<u8> {
    let elements: Vec<SigElement> = inputs
        .iter()
        .map(|c| {
            let col = c.mask.trailing_zeros().min(3) as u8;
            SigElement {
                name: c.name.clone(),
                index: c.index,
                kind: c.kind,
                comp: 9,
                interp: 1,
                row: None,
                col,
                cols: (c.mask >> col).count_ones().max(1) as u8,
                usage: c.read >> col,
                half: false,
            }
        })
        .collect();
    let part = container::signature(&elements, true);
    build(&[(FourCC::ISG1, &part)])
}

/// The declared maxima of a mesh program, from its `PSV0`.
fn maxima(c: &Container) -> Option<(u32, u32)> {
    let psv = c.part(FourCC::PSV0)?.data;
    if u32_at(psv, 0).ok()? < 48 {
        return None;
    }
    // Runtime info version 2: 4-byte size, then the info block, whose
    // vertices and primitives sit at 12 and 14.
    let vertices = u32::from(u16_at(psv, 16).ok()?);
    let primitives = u32::from(u16_at(psv, 18).ok()?);
    (vertices > 0).then_some((vertices, primitives))
}

/// Whether a mesh container's declared outputs are over the cap the validator
/// enforces, as its own maxima measure it. `false` when the container does not
/// say how many vertices and primitives it asks for.
///
/// Only for a mesh container: a vertex program's outputs are not measured
/// against [`MESH_OUTPUT_LIMIT`].
pub fn over_cap(container: &[u8]) -> bool {
    Container::parse(container).is_ok_and(|c| over_cap_of(&c))
}

/// [`over_cap`] of an already parsed container.
pub fn over_cap_of(c: &Container) -> bool {
    let Some((vertices, primitives)) = maxima(c) else {
        return false;
    };
    let outputs = elements(c, FourCC::OSG1);
    if outputs.is_empty() {
        return false;
    }
    container::mesh_output_bytes(&outputs, &elements(c, FourCC::PSG1), vertices, primitives)
        > MESH_OUTPUT_LIMIT
}

/// One element with only `keep` of its declared components left; `None` when
/// none is left.
fn cut(e: &SigElement, keep: u8) -> Option<SigElement> {
    let keep = keep & mask(e);
    if keep == 0 {
        return None;
    }
    if keep == mask(e) {
        return Some(e.clone());
    }
    // A window cannot say "these two, not the one between them", so a mask
    // with a gap keeps the whole element: the pair then declares more than
    // the pixel reads (the old behaviour) instead of promising a link that
    // the emitted signature cannot make.
    if keep.trailing_ones() != u32::from(keep.count_ones()) {
        return Some(e.clone());
    }
    Some(SigElement {
        col: keep.trailing_zeros() as u8,
        cols: keep.count_ones() as u8,
        usage: 0,
        ..e.clone()
    })
}

/// The mesh and pixel containers with the mesh's outputs cut to what the pixel
/// program reads.
///
/// `None` when the mesh already fits, when no cut fits it, or when either
/// container does not parse: the caller then translates what it has and the
/// validator still reports the overflow. Otherwise the mesh keeps, per
/// semantic, the components the pixel program reads (every system value keeps
/// its declared components, so position and the clip planes stay), and the
/// pixel programs keeps exactly what the mesh still declares.
pub fn pair(mesh: &[u8], ps: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let m = Container::parse(mesh).ok()?;
    let p = Container::parse(ps).ok()?;
    let (vertices, primitives) = maxima(&m)?;
    let outputs = elements(&m, FourCC::OSG1);
    let prims = elements(&m, FourCC::PSG1);
    let fits = |outputs: &[SigElement]| {
        container::mesh_output_bytes(outputs, &prims, vertices, primitives) <= MESH_OUTPUT_LIMIT
    };
    if outputs.is_empty() || fits(&outputs) {
        return None;
    }
    let reads = interface_of(&p);
    let kept: Vec<SigElement> = outputs
        .iter()
        .filter_map(|e| {
            let keep = if e.kind != 0 {
                mask(e)
            } else {
                let read = reads
                    .iter()
                    .find(|c| c.name == e.name && c.index == e.index)
                    .map_or(0, |c| c.read);
                mask(e) & read
            };
            cut(e, keep)
        })
        .collect();
    if kept.is_empty() || !fits(&kept) {
        return None;
    }
    let inputs: Vec<SigElement> = elements(&p, FourCC::ISG1)
        .iter()
        .filter_map(|e| {
            // What the mesh still declares for this semantic; an input no
            // mesh element ever produced (a front face) keeps its declared
            // components.
            let produced = if outputs.iter().any(|o| o.name == e.name && o.index == e.index) {
                kept.iter()
                    .find(|k| k.name == e.name && k.index == e.index)
                    .map_or(0, mask)
            } else {
                mask(e)
            };
            cut(e, produced)
        })
        .collect();
    Some((
        replace(&m, FourCC::OSG1, &container::signature(&kept, false)),
        replace(&p, FourCC::ISG1, &container::signature(&inputs, true)),
    ))
}

/// A container with one part replaced.
fn replace(c: &Container, fourcc: FourCC, part: &[u8]) -> Vec<u8> {
    let parts: Vec<(FourCC, &[u8])> = c
        .parts
        .iter()
        .map(|p| (p.fourcc, if p.fourcc == fourcc { part } else { p.data }))
        .collect();
    build(&parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emit::shader::Mesh;

    /// Four components per element, `n` elements, `row` from 0 up.
    fn wide(n: u8, kind: u8) -> Vec<SigElement> {
        (0..n)
            .map(|i| SigElement {
                name: "TEXCOORD".to_string(),
                index: u32::from(i),
                kind,
                comp: 9,
                interp: 1,
                row: Some(u32::from(i)),
                col: 0,
                cols: 4,
                usage: 0,
                half: false,
            })
            .collect()
    }

    /// A mesh container: its outputs and its maxima, no program.
    fn mesh(outputs: &[SigElement], vertices: u32) -> Vec<u8> {
        let psv = container::psv(&container::Psv {
            stage: 5,
            flag: false,
            inputs: &[],
            outputs,
            prims: &[],
            mesh: Some((
                Mesh {
                    max_vertices: vertices,
                    max_primitives: 128,
                    topology: 2,
                },
                0,
            )),
            geometry: None,
            bindings: &[],
            threads: [128, 1, 1],
            wave_size: None,
        });
        let osg = container::signature(outputs, false);
        build(&[(FourCC::PSV0, &psv), (FourCC::OSG1, &osg)])
    }

    /// A pixel container: its inputs, declared and read.
    fn pixel(inputs: &[SigElement], read: &[u8]) -> Vec<u8> {
        let elements: Vec<SigElement> = inputs
            .iter()
            .enumerate()
            .map(|(i, e)| SigElement {
                row: Some(i as u32),
                ..e.clone()
            })
            .collect();
        let mut blob = container::signature(&elements, true);
        // Rewrite the read byte of every row: `signature` writes the
        // element's own usage, which is 0 here.
        let base = u32_at(&blob, 4).unwrap() as usize;
        for (i, &r) in read.iter().enumerate() {
            blob[base + 32 * i + 25] = r;
        }
        build(&[(FourCC::ISG1, &blob)])
    }

    #[test]
    fn a_mesh_of_68_components_is_over_the_cap_at_128_vertices() {
        let m = mesh(&wide(17, 0), 128);
        assert!(over_cap(&m));
        assert!(!over_cap(&mesh(&wide(16, 0), 128)));
        // 64 is the most a group of 128 may declare; a group of 64 may
        // declare 128.
        assert!(over_cap(&mesh(&wide(33, 0), 64)));
        assert!(!over_cap(&mesh(&wide(32, 0), 64)));
    }

    #[test]
    fn the_cut_keeps_what_the_pixel_program_reads() {
        let outputs = wide(17, 0);
        let m = mesh(&outputs, 128);
        // The pixel program declares all 68 and reads the first 50.
        let inputs: Vec<SigElement> = outputs
            .iter()
            .map(|e| SigElement {
                row: None,
                ..e.clone()
            })
            .collect();
        let mut read = vec![0x0f; 17];
        for r in read.iter_mut().skip(12) {
            *r = 0;
        }
        read[12] = 0x03;
        let p = pixel(&inputs, &read);
        let (mesh_after, pixel_after) = pair(&m, &p).expect("a cut fits");
        assert!(!over_cap(&mesh_after));
        let kept = elements(&Container::parse(&mesh_after).unwrap(), FourCC::OSG1);
        assert_eq!(
            kept.iter().map(|e| u32::from(e.cols)).sum::<u32>(),
            48 + 2
        );
        assert_eq!(kept.iter().filter(|e| e.cols == 2).count(), 1);
        // The pixel program declares exactly what the mesh still writes.
        let narrowed = interface(&pixel_after);
        assert_eq!(narrowed.len(), kept.len());
        for (a, b) in narrowed.iter().zip(kept.iter()) {
            assert_eq!((a.name.as_str(), a.index, a.mask), (b.name.as_str(), b.index, mask(b)));
        }
    }

    #[test]
    fn a_mesh_that_fits_is_left_alone() {
        let m = mesh(&wide(16, 0), 128);
        let p = pixel(&wide(16, 0), &[0x0f; 16]);
        assert_eq!(pair(&m, &p), None);
    }

    #[test]
    fn a_read_with_a_gap_keeps_the_whole_element() {
        // `.x` and `.z`: a window cannot name that pair, so nothing is cut —
        // the pair must not promise a link the emitted signature cannot make.
        let e = SigElement {
            name: "TEXCOORD".to_string(),
            index: 0,
            kind: 0,
            comp: 9,
            interp: 1,
            row: Some(0),
            col: 0,
            cols: 4,
            usage: 0,
            half: false,
        };
        let whole = cut(&e, 0b0101).expect("the element stays");
        assert_eq!(whole.cols, 4, "the whole four components");
        assert_eq!(whole.col, e.col);
        // A contiguous read still narrows to its own window.
        let narrow = cut(&e, 0b0011).expect("two components stay");
        assert_eq!((narrow.col, narrow.cols), (0, 2));
    }

    #[test]
    fn a_system_value_keeps_its_components() {
        let mut outputs = wide(16, 0);
        outputs.push(SigElement {
            name: "SV_Position".to_string(),
            index: 0,
            kind: 3,
            comp: 9,
            interp: 1,
            row: Some(16),
            col: 0,
            cols: 4,
            usage: 0,
            half: false,
        });
        let m = mesh(&outputs, 128);
        assert!(over_cap(&m));
        // No pixel program reads anything: position stays, the rest goes.
        let p = interface_container(&[]);
        let (after, _) = pair(&m, &p).expect("position alone fits");
        let kept = elements(&Container::parse(&after).unwrap(), FourCC::OSG1);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].kind, 3);
        assert_eq!(kept[0].cols, 4);
    }

    #[test]
    fn a_stand_in_pixel_program_carries_the_union_of_interfaces() {
        let a = Components {
            name: "TEXCOORD".to_string(),
            index: 3,
            kind: 0,
            mask: 0x0f,
            read: 0x03,
        };
        let b = Components {
            name: "TEXCOORD".to_string(),
            index: 3,
            kind: 0,
            mask: 0x03,
            read: 0x02,
        };
        let union = Components {
            name: "TEXCOORD".to_string(),
            index: 3,
            kind: 0,
            mask: a.mask | b.mask,
            read: a.read | b.read,
        };
        assert_eq!(
            interface(&interface_container(std::slice::from_ref(&union))),
            vec![union.clone()]
        );
    }
}
