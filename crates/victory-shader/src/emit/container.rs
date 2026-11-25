//! The PC container around emitted DXIL: feature flags (`SFI0`), the
//! signatures (`ISG1`, `OSG1`), pipeline state validation (`PSV0`) and the
//! program (`DXIL`), laid out as validator version 1.6 lays them out, plus
//! the container hash that D3D12 checks.

/// One signature element: one row (D3D12 links stages by semantic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SigElement {
    pub name: String,
    pub index: u32,
    /// DXIL semantic kind (0 arbitrary, 1 vertex id, 3 position, 16 target...).
    pub kind: u8,
    /// DXIL component type (4 i32, 5 u32, 9 f32, 1 i1).
    pub comp: u8,
    /// DXIL interpolation mode (0 for none).
    pub interp: u8,
    /// Start row, if the element has a register.
    pub row: Option<u32>,
    pub col: u8,
    pub cols: u8,
    /// Components read (inputs) or written (outputs), relative to `col`.
    pub usage: u8,
    /// 16-bit on the console (declared 32-bit here).
    pub half: bool,
}

impl SigElement {
    fn system_value(&self) -> u32 {
        match self.kind {
            1 => 6,  // vertex id
            2 => 8,  // instance id
            3 => 1,  // position
            4 => 4,  // render target array index
            5 => 5,  // viewport array index
            6 => 2,  // clip distance
            7 => 3,  // cull distance
            10 => 7, // primitive id
            12 => 10,
            13 => 9, // front face
            14 => 66,
            15 => 70,
            16 => 64, // target
            17 => 65, // depth
            18 => 68,
            19 => 67,
            20 => 69,
            28 => 23, // barycentrics
            30 => 25, // cull primitive
            _ => 0,
        }
    }

    fn sig_comp(&self) -> u32 {
        match self.comp {
            1 | 5 => 1, // bool and u32
            4 => 2,
            9 => 3,
            _ => 0,
        }
    }

    fn mask(&self) -> u8 {
        ((1u8 << self.cols) - 1) << self.col
    }
}

/// `ISG1`/`OSG1`: 32-byte rows, then the names.
pub(crate) fn signature(elements: &[SigElement], input: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend((elements.len() as u32).to_le_bytes());
    out.extend(8u32.to_le_bytes());
    // `dxil.dll` regenerates this part with `DxilProgramSignatureWriter` and
    // compares it byte for byte against ours ("Container part 'Program Output
    // Signature' does not match expected for module"), so the order here is
    // the writer's order and nothing else: rows by `(stream, register)` with
    // ties left in element order — `std::sort` there is `sort_sig`, whose key
    // is `(Stream, Register, SemanticName-offset)`, and the offset of two
    // elements of one register ties only when they share a semantic, so in
    // practice a stable sort by register — and the names in *element* order
    // (`calcSizes` registers them before any row is written), one per element
    // and not deduplicated, because our modules declare `dx.valver` 1.6 and
    // the writer only dedups and 4-aligns for 1.7 and later. Sorting the rows
    // by column inside a register, as this did, and writing them in element
    // order, both disagree with the writer: 81 of the title's programs placed
    // an element in a register that is not its element's register order.
    let mut offsets: Vec<u32> = Vec::new();
    let mut next = 8 + 32 * elements.len() as u32;
    let mut names = Vec::new();
    for (i, e) in elements.iter().enumerate() {
        let shared = (e.kind != 0)
            .then(|| (0..i).find(|&j| elements[j].kind == e.kind))
            .flatten();
        match shared {
            Some(j) => offsets.push(offsets[j]),
            None => {
                offsets.push(next);
                next += e.name.len() as u32 + 1;
                names.push(&e.name);
            }
        }
    }
    // Stable, so elements sharing a register keep the order the module's
    // metadata declares them in.
    let mut order: Vec<usize> = (0..elements.len()).collect();
    order.sort_by_key(|&i| elements[i].row.unwrap_or(u32::MAX));
    for i in order {
        let (e, offset) = (&elements[i], offsets[i]);
        let used = e.usage << e.col;
        let rw = if input { used } else { !used & 0xf };
        for v in [
            0,
            offset,
            e.index,
            e.system_value(),
            e.sig_comp(),
            e.row.unwrap_or(u32::MAX),
        ] {
            out.extend(v.to_le_bytes());
        }
        out.extend([e.mask(), rw, 0, 0]);
        out.extend(0u32.to_le_bytes());
    }
    for name in names {
        out.extend(name.as_bytes());
        out.push(0);
    }
    out
}

/// Registers of four components a stage's signature may use (D3D12).
pub(crate) const SIGNATURE_ROWS: u32 = 32;

/// The elements packed into one register must declare one interpolation
/// mode: D3D interpolates a row one way, and `dxil.dll` rejects a signature
/// whose row mixes modes (`signature element ... has interpolation mode
/// that differs from another element packed into the same row`), the
/// largest class of invalid DXIL the title's programs hit.
///
/// The console interpolated a register channel by channel, per instruction
/// (`v_interp_*` reads one channel and takes its barycentric from a VGPR),
/// so its own signature and code ask for two modes in one row, which D3D
/// cannot express. `shortcut:` the element that owns the row keeps it —
/// the lowest-column element the program reads, else its lowest-column
/// element — and an element that cannot take the owner's mode moves to a
/// register of its own, carrying its column with it, so no read loses the
/// mode its arithmetic was lowered for. Only `row` may move: the emitted
/// `loadInput`/`attributeAtVertex` calls name the element id and its
/// column, and D3D12 links stages by semantic, never by register.
///
/// An integer component type cannot be interpolated at all (the validator
/// rejects modes 2..=7 on one), so such an element is `constant`. A row
/// holding a system value is not unpacked — D3D supplies that element, the
/// title does not declare it — so it takes one mode, the flat one when an
/// integer element is in it. An element the program does not read takes its
/// row's mode rather than a register, and with no register left the row
/// falls back to one mode.
pub(crate) fn share_row_interp(elements: &mut [SigElement]) {
    let can_take = |e: &SigElement, mode: u8| mode <= 1 || e.comp == 9;
    for e in elements.iter_mut() {
        if !can_take(e, e.interp) {
            e.interp = 1;
        }
    }
    let mut rows: Vec<u32> = elements.iter().filter_map(|e| e.row).collect();
    rows.sort_unstable();
    rows.dedup();
    let mut next = rows.last().map_or(0, |&r| r + 1);
    for row in rows {
        let mut of: Vec<usize> = (0..elements.len())
            .filter(|&i| elements[i].row == Some(row))
            .collect();
        of.sort_by_key(|&i| (elements[i].col, i));
        if of.len() < 2 || of.iter().all(|&i| elements[i].interp == elements[of[0]].interp) {
            continue;
        }
        let owner = of
            .iter()
            .copied()
            .find(|&i| elements[i].usage != 0)
            .unwrap_or(of[0]);
        let mode = elements[owner].interp;
        let moves = of
            .iter()
            .filter(|&&i| elements[i].interp != mode)
            .filter(|&&i| elements[i].usage != 0 || !can_take(&elements[i], mode))
            .count() as u32;
        // A row holding a system value, or one with no register left,
        // declares a single mode: the flat one when an integer element is
        // in it.
        if of.iter().any(|&i| elements[i].kind != 0) || next + moves > SIGNATURE_ROWS {
            let flat = of.iter().any(|&i| elements[i].comp != 9);
            let mode = if flat { 1 } else { mode };
            for i in of {
                elements[i].interp = mode;
            }
            continue;
        }
        for i in of {
            if i == owner || elements[i].interp == mode {
                continue;
            }
            if elements[i].usage == 0 && can_take(&elements[i], mode) {
                elements[i].interp = mode;
            } else {
                elements[i].row = Some(next);
                next += 1;
            }
        }
    }
}

/// The most a mesh shader's outputs may take, in bytes (D3D12).
pub(crate) const MESH_OUTPUT_LIMIT: u64 = 32 * 1024;

/// A mesh shader's output size as the DXIL validator measures it against
/// [`MESH_OUTPUT_LIMIT`]: 4 bytes per component of every vertex element
/// and primitive element, times the maxima rounded up to multiples of 32.
///
/// Only the number of declared components counts: register rows, how many
/// components an element puts in a row, and whether a component is 16-bit
/// on the console ([`SigElement::half`]) do not enter it. So at the 128
/// lanes a group has, a mesh program may declare 64 vertex components and
/// not 65 (4 * 65 * 128 = 33280), and no repacking of the signature can
/// bring a wider one under the cap: only dropping components does.
pub(crate) fn mesh_output_bytes(
    vertex: &[SigElement],
    primitive: &[SigElement],
    max_vertices: u32,
    max_primitives: u32,
) -> u64 {
    let scalars = |els: &[SigElement]| els.iter().map(|e| u64::from(e.cols)).sum::<u64>();
    let align = |n: u32| u64::from(n).next_multiple_of(32);
    4 * (scalars(vertex) * align(max_vertices) + scalars(primitive) * align(max_primitives))
}

/// A resource bound by register, as `PSV0` lists it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Binding {
    /// PSV resource type: 1 sampler, 2 CBV, 4 raw SRV, 7 raw UAV.
    pub kind: u32,
    pub space: u32,
    pub register: u32,
    /// DXIL resource kind (13 cbuffer, 14 sampler).
    pub dxil_kind: u32,
}

pub(crate) struct Psv<'a> {
    /// PSV shader kind: 0 pixel, 1 vertex, 2 geometry, 5 compute.
    pub stage: u8,
    /// Vertex: writes `SV_Position`. Pixel: writes depth.
    pub flag: bool,
    pub inputs: &'a [SigElement],
    pub outputs: &'a [SigElement],
    /// Mesh: per-primitive outputs.
    pub prims: &'a [SigElement],
    /// Mesh: its state and the group-shared bytes it declares.
    pub mesh: Option<(super::shader::Mesh, u32)>,
    /// Geometry (triangles in, triangle strips out on stream 0): its
    /// maximum vertex count. `flag`: writes `SV_Position`.
    pub geometry: Option<u32>,
    pub bindings: &'a [Binding],
    /// Compute: thread-group size and required wave size.
    pub threads: [u32; 3],
    pub wave_size: Option<u32>,
}

/// `PSV0` with runtime info version 2 (validator 1.6).
pub(crate) fn psv(p: &Psv) -> Vec<u8> {
    let vectors = |els: &[SigElement]| {
        els.iter()
            .filter_map(|e| e.row)
            .map(|r| r + 1)
            .max()
            .unwrap_or(0)
    };
    let (in_vectors, out_vectors) = (vectors(p.inputs), vectors(p.outputs));
    let mut out = Vec::new();
    let u32s = |out: &mut Vec<u8>, vs: &[u32]| {
        for v in vs {
            out.extend(v.to_le_bytes());
        }
    };
    u32s(&mut out, &[48]);
    let mut info = [0u8; 48];
    info[0] = u8::from(p.flag);
    if p.stage == 0 {
        // PSInfo.SampleFrequency: an input interpolated per sample, or the
        // sample index.
        info[1] = u8::from(
            p.inputs
                .iter()
                .any(|e| matches!(e.interp, 6 | 7) || e.kind == 12),
        );
    }
    let (min, max) = p.wave_size.map_or((0, u32::MAX), |n| (n, n));
    info[16..20].copy_from_slice(&min.to_le_bytes());
    info[20..24].copy_from_slice(&max.to_le_bytes());
    info[24] = p.stage;
    for (k, n) in p.threads.into_iter().enumerate() {
        info[36 + 4 * k..40 + 4 * k].copy_from_slice(&n.to_le_bytes());
    }
    info[28] = p.inputs.len() as u8;
    info[29] = p.outputs.len() as u8;
    info[31] = in_vectors as u8;
    info[32] = out_vectors as u8;
    if let Some((mesh, shared)) = p.mesh {
        info[0..4].copy_from_slice(&shared.to_le_bytes());
        info[12..14].copy_from_slice(&(mesh.max_vertices as u16).to_le_bytes());
        info[14..16].copy_from_slice(&(mesh.max_primitives as u16).to_le_bytes());
        info[26] = vectors(p.prims) as u8;
        info[27] = mesh.topology;
        info[30] = p.prims.len() as u8;
    }
    if let Some(max_vertices) = p.geometry {
        // Input primitive (3: triangles), output topology (5: triangle
        // strips), stream mask, position written; the vertex maximum.
        info[0..4].copy_from_slice(&3u32.to_le_bytes());
        info[4..8].copy_from_slice(&5u32.to_le_bytes());
        info[8..12].copy_from_slice(&1u32.to_le_bytes());
        info[12] = u8::from(p.flag);
        info[26..28].copy_from_slice(&(max_vertices as u16).to_le_bytes());
    }
    out.extend(info);
    u32s(&mut out, &[p.bindings.len() as u32]);
    if !p.bindings.is_empty() {
        u32s(&mut out, &[24]);
        for b in p.bindings {
            u32s(
                &mut out,
                &[b.kind, b.space, b.register, b.register, b.dxil_kind, 0],
            );
        }
    }
    // Names of arbitrary semantics (system values use ""), then indices.
    let mut strings = vec![0u8];
    let mut indices: Vec<u32> = Vec::new();
    let mut elements = Vec::new();
    for e in p.inputs.iter().chain(p.outputs).chain(p.prims) {
        let name = if e.kind == 0 {
            let needle: Vec<u8> = e.name.bytes().chain([0]).collect();
            match find(&strings, &needle) {
                Some(at) => at,
                None => {
                    strings.extend(&needle);
                    strings.len() - needle.len()
                }
            }
        } else {
            0
        };
        let index = match indices.iter().position(|&i| i == e.index) {
            Some(at) => at,
            None => {
                indices.push(e.index);
                indices.len() - 1
            }
        };
        let (start, alloc) = match e.row {
            Some(r) => (r as u8, 0x40 | (e.col << 4)),
            None => (0, 0),
        };
        elements.push((name as u32, index as u32, start, e.cols | alloc, e));
    }
    strings.resize(strings.len().next_multiple_of(4), 0);
    u32s(&mut out, &[strings.len() as u32]);
    out.extend(&strings);
    u32s(&mut out, &[indices.len() as u32]);
    u32s(&mut out, &indices);
    if !elements.is_empty() {
        u32s(&mut out, &[16]);
        for (name, index, start, cols, e) in elements {
            u32s(&mut out, &[name, index]);
            out.extend([1, start, cols, e.kind, e.sig_comp() as u8, e.interp, 0, 0]);
        }
    }
    // Input-to-output dependencies, as the view-id state declares them:
    // every output scalar on every input scalar.
    if in_vectors > 0 && out_vectors > 0 {
        let scalars = |els: &[SigElement]| {
            els.iter()
                .filter_map(|e| e.row.map(|r| r * 4 + u32::from(e.col + e.cols)))
                .max()
                .unwrap_or(0)
        };
        let (ni, no) = (scalars(p.inputs), scalars(p.outputs));
        let per = (out_vectors * 4).div_ceil(32);
        for i in 0..in_vectors * 4 {
            for k in 0..per {
                let bits = if i < ni {
                    no.saturating_sub(32 * k).min(32)
                } else {
                    0
                };
                let mask = if bits == 32 {
                    u32::MAX
                } else {
                    (1u32 << bits) - 1
                };
                u32s(&mut out, &[mask]);
            }
        }
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The container: parts in order, hashed.
pub(crate) fn build(parts: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
    let table = 0x20 + 4 * parts.len();
    let total = table + parts.iter().map(|(_, d)| 8 + d.len()).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.extend(b"DXBC");
    out.extend([0; 16]);
    out.extend(1u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend((total as u32).to_le_bytes());
    out.extend((parts.len() as u32).to_le_bytes());
    let mut offset = table;
    for (_, d) in parts {
        out.extend((offset as u32).to_le_bytes());
        offset += 8 + d.len();
    }
    for (fourcc, d) in parts {
        out.extend(*fourcc);
        out.extend((d.len() as u32).to_le_bytes());
        out.extend(*d);
    }
    let digest = hash(&out[20..]);
    out[4..20].copy_from_slice(&digest);
    out
}

/// The container hash D3D12 checks: MD5 whose last block carries the bit
/// count first and `2 × bytes + 1` last instead of standard padding.
pub(crate) fn hash(data: &[u8]) -> [u8; 16] {
    let len = data.len();
    let left = len % 64;
    let mut tail = Vec::with_capacity(128);
    if left < 56 {
        tail.extend(((len as u32) << 3).to_le_bytes());
        tail.extend(&data[len - left..]);
        tail.push(0x80);
        tail.resize(60, 0);
    } else {
        tail.extend(&data[len - left..]);
        tail.push(0x80);
        tail.resize(64, 0);
        tail.extend(((len as u32) << 3).to_le_bytes());
        tail.resize(124, 0);
    }
    tail.extend(((len as u32) << 1 | 1).to_le_bytes());
    let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    for block in data[..len - left]
        .as_chunks::<64>()
        .0
        .iter()
        .chain(tail.as_chunks::<64>().0)
    {
        md5_block(&mut state, block);
    }
    let mut out = [0; 16];
    for (o, s) in out.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        o.copy_from_slice(&s.to_le_bytes());
    }
    out
}

fn md5_block(state: &mut [u32; 4], block: &[u8; 64]) {
    const S: [u32; 16] = [7, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];
    let x: Vec<u32> = block
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();
    let [mut a, mut b, mut c, mut d] = *state;
    for i in 0..64 {
        let (f, g) = match i / 16 {
            0 => ((b & c) | (!b & d), i),
            1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
            2 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | !d), (7 * i) % 16),
        };
        let k = ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32;
        let rotated = a
            .wrapping_add(f)
            .wrapping_add(k)
            .wrapping_add(x[g])
            .rotate_left(S[(i / 16) * 4 + i % 4]);
        (a, b, c, d) = (d, b.wrapping_add(rotated), b, c);
    }
    for (s, v) in state.iter_mut().zip([a, b, c, d]) {
        *s = s.wrapping_add(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A signature element, as the emitter keeps one: `row`/`col` its place
    /// in the register, `usage` the components read (inputs).
    fn el(row: Option<u32>, col: u8, cols: u8, interp: u8, usage: u8) -> SigElement {
        SigElement {
            name: "TEXCOORD".into(),
            index: row.unwrap_or(0),
            kind: 0,
            comp: 9,
            interp,
            row,
            col,
            cols,
            usage,
            half: false,
        }
    }

    /// D3D interpolates a register one way, so two elements packed into one
    /// row must declare one interpolation mode; the console interpolated
    /// channel by channel, so its reads ask for two (245 of the title's
    /// programs failed validation for mixing them in a row).
    #[test]
    fn elements_packed_into_one_row_share_their_interpolation_mode() {
        // A read of the packed second element (linear, 4) behind an unread
        // owner (perspective, 2), a row of its own, and one with no
        // register (a system value).
        let mut els = [
            el(Some(3), 0, 2, 2, 0),
            el(Some(3), 2, 2, 4, 0b0100),
            el(Some(4), 0, 4, 2, 0),
            el(None, 0, 3, 3, 0),
        ];
        share_row_interp(&mut els);
        // The row takes the mode the read asked for: the arithmetic of the
        // element the program reads is what the declaration must match.
        assert_eq!(els[0].interp, 4, "the unread owner takes the row's mode");
        assert_eq!(els[1].interp, 4, "the read keeps its own");
        assert_eq!(els[2].interp, 2, "a row of its own is untouched");
        assert_eq!(els[3].interp, 3, "no register, untouched");
        // Two reads that disagree (the console could, D3D cannot): the
        // element that owns the row, the lowest column, keeps it and the
        // other read moves to a register of its own, carrying its column,
        // so neither read loses the mode its arithmetic was lowered for.
        let mut els = [el(Some(1), 0, 4, 2, 0b1111), el(Some(1), 2, 2, 4, 0b1100)];
        share_row_interp(&mut els);
        assert_eq!([els[0].interp, els[1].interp], [2, 4], "both reads keep");
        assert_eq!([els[0].row, els[1].row], [Some(1), Some(2)], "the second moved");
        assert_eq!(els[1].col, 2, "a moved element keeps its column");
        // An integer element cannot be interpolated at all: it is constant,
        // and a row whose other element interpolates gives it its own.
        let mut els = [el(Some(0), 0, 2, 2, 0b0011), el(Some(0), 2, 2, 2, 0)];
        els[1].comp = 5;
        share_row_interp(&mut els);
        assert_eq!([els[0].interp, els[1].interp], [2, 1], "the integer is flat");
        assert_eq!(els[1].row, Some(1), "and unpacked");
        // With no register left (32 a stage) the row declares one mode, the
        // flat one when an integer element is in it.
        let mut els = [el(Some(31), 0, 2, 2, 0b0011), el(Some(31), 2, 2, 4, 0b1100)];
        share_row_interp(&mut els);
        assert_eq!([els[0].interp, els[1].interp], [2, 2], "no register left");
        let mut els = [el(Some(31), 0, 2, 2, 0b0011), el(Some(31), 2, 2, 4, 0b1100)];
        els[1].comp = 4;
        share_row_interp(&mut els);
        assert_eq!([els[0].interp, els[1].interp], [1, 1], "an integer flattens all");
        // Rows that already agree are left alone, and elements a program
        // never reads keep their kind's default when their row agrees.
        let mut els = [el(Some(0), 0, 4, 1, 0), el(Some(0), 2, 2, 1, 0)];
        share_row_interp(&mut els);
        assert_eq!([els[0].interp, els[1].interp], [1, 1]);
    }

    /// The title's `VS_PedTransformSkin_*` writes `TEXCOORD22` into `.yz` of
    /// register 14 and `COLOR0` into `.x` of it, and the elements in between
    /// are in no register order at all; a part that breaks either tie is
    /// rejected whole ("Container part 'Program Output Signature' does not
    /// match expected for module").
    #[test]
    fn rows_follow_the_register_and_keep_element_order_within_one() {
        // `dxil.dll` rewrites this part with `DxilProgramSignatureWriter` and
        // compares it byte for byte, so both halves of that writer's order are
        // load-bearing: rows ascend by register, and two elements the console
        // packed into one register stay in the order the module's metadata
        // declares them, columns included.
        let el = |name: &str, index: u32, row: u32, col: u8, cols: u8| SigElement {
            name: name.into(),
            index,
            kind: 0,
            comp: 9,
            interp: 1,
            row: Some(row),
            col,
            cols,
            usage: 0,
            half: false,
        };
        let part = signature(
            &[
                // The high column of a shared register comes first, and the
                // element in the lower register comes last: element order,
                // register order and column order all differ.
                el("TEXCOORD", 22, 2, 1, 2),
                el("COLOR", 0, 2, 0, 1),
                el("TEXCOORD", 1, 1, 0, 4),
            ],
            false,
        );
        let back = super::super::signature(&part).expect("our own part parses");
        let order: Vec<(&str, u32, u32, u8)> = back
            .iter()
            .map(|e| (e.name.as_str(), e.index, e.row.unwrap_or(u32::MAX), e.col))
            .collect();
        assert_eq!(
            order,
            [
                ("TEXCOORD", 1, 1, 0),
                ("TEXCOORD", 22, 2, 1),
                ("COLOR", 0, 2, 0)
            ],
            "rows must ascend by register and keep element order inside one"
        );
        // The names keep element order, so the first row names the second
        // element: `calcSizes` registers them before any row is written.
        let base = 8 + 32 * back.len();
        assert_eq!(
            &part[base..],
            b"TEXCOORD\0COLOR\0TEXCOORD\0",
            "the name blob is in element order, one name per element"
        );
    }

    #[test]
    fn md5_core_matches_the_standard() {
        // Standard MD5 of "abc" with standard padding, through md5_block.
        let mut block = b"abc".to_vec();
        block.push(0x80);
        block.resize(56, 0);
        block.extend(24u64.to_le_bytes());
        let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
        md5_block(&mut state, block.as_slice().try_into().unwrap());
        let hex: String = state
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hex, "900150983cd24fb0d6963f7d28e17f72");
    }

    /// A `TEXCOORD`, `cols` components from component `col` of `row`.
    fn element(row: u32, col: u8, cols: u8) -> SigElement {
        SigElement {
            name: "TEXCOORD".into(),
            index: 0,
            kind: 0,
            comp: 9,
            interp: 1,
            row: Some(row),
            col,
            cols,
            usage: (((1u16 << cols.min(8)) - 1) as u8) << col,
            half: false,
        }
    }

    /// 4 bytes per declared component times the maxima, and the maxima
    /// rounded up to 32: 64 components fit at the 128 lanes a group has,
    /// 65 do not, and 127 vertices cost what 128 cost.
    #[test]
    fn mesh_output_budget_is_four_bytes_a_component_at_the_maxima() {
        let wide = |cols: u8| vec![element(0, 0, cols)];
        assert_eq!(mesh_output_bytes(&wide(64), &[], 128, 128), MESH_OUTPUT_LIMIT);
        assert_eq!(mesh_output_bytes(&wide(65), &[], 128, 128), 33280);
        assert_eq!(
            mesh_output_bytes(&wide(65), &[], 127, 128),
            mesh_output_bytes(&wide(65), &[], 128, 128)
        );
        assert_eq!(mesh_output_bytes(&wide(65), &[], 96, 128), 4 * 65 * 96);
        assert_eq!(mesh_output_bytes(&[], &wide(64), 128, 128), MESH_OUTPUT_LIMIT);
    }

    /// The same components cost the same in one row, in 66 rows, or
    /// declared 16-bit: only how many a signature declares counts, so
    /// neither repacking rows nor narrowing a component's type brings a
    /// program under the cap. The pedestrian mesh programs declare 20 rows
    /// of 66 components and land on 33792.
    #[test]
    fn mesh_output_budget_counts_components_only() {
        let spaced: Vec<SigElement> = (0..66).map(|row| element(row, 0, 1)).collect();
        let mut packed: Vec<SigElement> = Vec::new();
        for row in 0..14 {
            packed.push(element(row, 0, 4));
        }
        for row in 14..16 {
            packed.push(element(row, 0, 3));
        }
        for row in 16..20 {
            packed.push(element(row, 0, 1));
        }
        assert_eq!(spaced.len(), 66);
        assert_eq!(packed.iter().map(|e| u32::from(e.cols)).sum::<u32>(), 66);
        assert_eq!(mesh_output_bytes(&packed, &[], 128, 128), 33792);
        assert_eq!(
            mesh_output_bytes(&spaced, &[], 128, 128),
            mesh_output_bytes(&packed, &[], 128, 128)
        );
        for e in &mut packed {
            e.half = true;
        }
        assert_eq!(mesh_output_bytes(&packed, &[], 128, 128), 33792);
    }
}
