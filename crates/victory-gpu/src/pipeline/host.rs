//! Host pipelines for title pipelines (decision 0007, step 3): the
//! title's graphics state with the translated shaders, under a root
//! signature of our own that the translated shaders' bindings define
//! (`victory_shader::emit`): per stage one block of root constants for the
//! user data, root views for the title's root views, static samplers for
//! the immediate samplers, and directly indexed heaps.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use victory_shader::emit::{HostShader, ROOT_CBV_SPACE, SAMPLER_SPACE, USER_DATA_SPACE};
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::PCSTR;

use super::{Shader, Translated};
use crate::host::host;

/// A stream's graphics state, owned (the title's stream lives only for the
/// creation call). Subobjects the stream leaves out keep D3D12's defaults.
pub(crate) struct GraphicsState {
    blend: D3D12_BLEND_DESC,
    sample_mask: u32,
    rasterizer: D3D12_RASTERIZER_DESC,
    depth: D3D12_DEPTH_STENCIL_DESC,
    depth_bounds: bool,
    /// Input elements, their names pointing into `names`.
    inputs: Vec<D3D12_INPUT_ELEMENT_DESC>,
    names: Vec<CString>,
    strip_cut: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE,
    pub(crate) topology: D3D12_PRIMITIVE_TOPOLOGY_TYPE,
    pub(crate) targets: D3D12_RT_FORMAT_ARRAY,
    depth_format: DXGI_FORMAT,
    samples: DXGI_SAMPLE_DESC,
}

impl Default for GraphicsState {
    fn default() -> Self {
        let target = D3D12_RENDER_TARGET_BLEND_DESC {
            SrcBlend: D3D12_BLEND_ONE,
            DestBlend: D3D12_BLEND_ZERO,
            BlendOp: D3D12_BLEND_OP_ADD,
            SrcBlendAlpha: D3D12_BLEND_ONE,
            DestBlendAlpha: D3D12_BLEND_ZERO,
            BlendOpAlpha: D3D12_BLEND_OP_ADD,
            LogicOp: D3D12_LOGIC_OP_NOOP,
            RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
            ..Default::default()
        };
        let keep = D3D12_DEPTH_STENCILOP_DESC {
            StencilFailOp: D3D12_STENCIL_OP_KEEP,
            StencilDepthFailOp: D3D12_STENCIL_OP_KEEP,
            StencilPassOp: D3D12_STENCIL_OP_KEEP,
            StencilFunc: D3D12_COMPARISON_FUNC_ALWAYS,
        };
        GraphicsState {
            blend: D3D12_BLEND_DESC {
                RenderTarget: [target; 8],
                ..Default::default()
            },
            sample_mask: u32::MAX,
            rasterizer: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_BACK,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            depth: D3D12_DEPTH_STENCIL_DESC {
                DepthEnable: true.into(),
                DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
                DepthFunc: D3D12_COMPARISON_FUNC_LESS,
                StencilReadMask: 0xff,
                StencilWriteMask: 0xff,
                FrontFace: keep,
                BackFace: keep,
                ..Default::default()
            },
            depth_bounds: false,
            inputs: Vec::new(),
            names: Vec::new(),
            strip_cut: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_DISABLED,
            topology: D3D12_PRIMITIVE_TOPOLOGY_TYPE_UNDEFINED,
            targets: D3D12_RT_FORMAT_ARRAY::default(),
            depth_format: DXGI_FORMAT_UNKNOWN,
            samples: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
        }
    }
}

impl GraphicsState {
    /// Takes in one subobject of a stream: its type, then its data at +4
    /// (+8 for those holding pointers).
    ///
    /// # Safety
    /// `at` points at a whole subobject of type `kind`.
    pub(crate) unsafe fn read(&mut self, kind: u32, at: *const u8) {
        // SAFETY: the caller passes a whole subobject.
        unsafe {
            let data = at.add(4);
            match kind {
                8 => self.blend = data.cast::<D3D12_BLEND_DESC>().read_unaligned(),
                9 => self.sample_mask = data.cast::<u32>().read_unaligned(),
                10 => self.rasterizer = data.cast::<D3D12_RASTERIZER_DESC>().read_unaligned(),
                11 => {
                    self.depth = data.cast::<D3D12_DEPTH_STENCIL_DESC>().read_unaligned();
                    self.depth_bounds = false;
                }
                21 => {
                    let d = data.cast::<D3D12_DEPTH_STENCIL_DESC1>().read_unaligned();
                    self.depth_bounds = d.DepthBoundsTestEnable.as_bool();
                    self.depth = D3D12_DEPTH_STENCIL_DESC {
                        DepthEnable: d.DepthEnable,
                        DepthWriteMask: d.DepthWriteMask,
                        DepthFunc: d.DepthFunc,
                        StencilEnable: d.StencilEnable,
                        StencilReadMask: d.StencilReadMask,
                        StencilWriteMask: d.StencilWriteMask,
                        FrontFace: d.FrontFace,
                        BackFace: d.BackFace,
                    };
                }
                12 => {
                    let layout = at.add(8).cast::<D3D12_INPUT_LAYOUT_DESC>().read_unaligned();
                    if layout.pInputElementDescs.is_null() {
                        return;
                    }
                    let elements = std::slice::from_raw_parts(
                        layout.pInputElementDescs,
                        layout.NumElements as usize,
                    );
                    self.names = elements
                        .iter()
                        .map(|e| CStr::from_ptr(e.SemanticName.0.cast()).to_owned())
                        .collect();
                    self.inputs = elements
                        .iter()
                        .zip(&self.names)
                        .map(|(e, name)| D3D12_INPUT_ELEMENT_DESC {
                            SemanticName: PCSTR(name.as_ptr().cast()),
                            Format: DXGI_FORMAT(
                                crate::resource::pc_format(e.Format.0 as u32) as i32
                            ),
                            ..*e
                        })
                        .collect();
                }
                13 => {
                    self.strip_cut =
                        D3D12_INDEX_BUFFER_STRIP_CUT_VALUE(data.cast::<i32>().read_unaligned())
                }
                14 => {
                    self.topology =
                        D3D12_PRIMITIVE_TOPOLOGY_TYPE(data.cast::<i32>().read_unaligned())
                }
                15 => {
                    self.targets = data.cast::<D3D12_RT_FORMAT_ARRAY>().read_unaligned();
                    for f in &mut self.targets.RTFormats {
                        f.0 = crate::resource::target_format(crate::resource::pc_format(
                            f.0 as u32,
                        )) as i32;
                    }
                }
                16 => {
                    let f = data.cast::<u32>().read_unaligned();
                    self.depth_format = DXGI_FORMAT(crate::resource::pc_format(f) as i32);
                }
                17 => self.samples = data.cast::<DXGI_SAMPLE_DESC>().read_unaligned(),
                _ => {}
            }
        }
    }

    /// Everything the state holds, as text: part of a pipeline's name in
    /// the library and of the key a title pipeline is started early by.
    pub(crate) fn describe(&self) -> String {
        let inputs: Vec<String> = self
            .inputs
            .iter()
            .zip(&self.names)
            .map(|(e, name)| {
                format!(
                    "{name:?} {} {} {} {} {} {}",
                    e.SemanticIndex, e.Format.0, e.InputSlot, e.AlignedByteOffset, e.InputSlotClass.0, e.InstanceDataStepRate
                )
            })
            .collect();
        let bounds = if self.depth_bounds {
            " depth-bounds"
        } else {
            ""
        };
        format!(
            "{:?} {} {:?} {:?}{bounds} {inputs:?} {} {} {:?} {} {:?}",
            self.blend,
            self.sample_mask,
            self.rasterizer,
            self.depth,
            self.strip_cut.0,
            self.topology.0,
            self.targets,
            self.depth_format.0,
            self.samples
        )
    }

    fn depth1(&self) -> D3D12_DEPTH_STENCIL_DESC1 {
        depth1(&self.depth, self.depth_bounds)
    }

    /// The vertex buffer slots the input layout reads.
    fn slots(&self) -> u32 {
        self.inputs
            .iter()
            .fold(0, |m, e| m | 1 << e.InputSlot.min(31))
    }

    /// The input elements with their byte offsets worked out.
    fn elements(&self) -> Vec<Input> {
        let mut next = [0u32; 32];
        self.inputs
            .iter()
            .map(|e| {
                let slot = e.InputSlot.min(31) as usize;
                let format = e.Format.0 as u32;
                let offset = if e.AlignedByteOffset == D3D12_APPEND_ALIGNED_ELEMENT {
                    next[slot]
                } else {
                    e.AlignedByteOffset
                };
                let bytes = crate::resource::element(format).map_or(0, |(b, _)| b as u32);
                next[slot] = offset + bytes;
                Input {
                    slot: slot as u32,
                    offset,
                    format,
                    per_instance: e.InputSlotClass == D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA,
                }
            })
            .collect()
    }
}

/// One input element: vertex buffer slot, byte offset, format.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Input {
    pub(crate) slot: u32,
    pub(crate) offset: u32,
    pub(crate) format: u32,
    pub(crate) per_instance: bool,
}

/// The console's own primitive topology types.
const RECTANGLES: i32 = 19;
const QUADS: i32 = 32;

/// Whether the title's root SRVs are bound as one-view tables of the
/// buffer's host copy, whose reads past its end return zeros, rather than
/// as root descriptors, which have no size: a mesh shader's vertex fetch
/// has no record limit, and a read past the copy reaches whatever the host
/// placed after it (memory it has freed: the device is removed, upstream
/// PR #493). `VICTORY_ROOT_SRV_DIRECT=1` goes back to root descriptors.
pub(crate) fn bounded_root_srvs() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ROOT_SRV_DIRECT").is_none())
}

/// One stage of a host pipeline and where its bindings go.
pub(crate) struct Stage {
    pub(crate) shader: Arc<Shader>,
    /// The root parameter of its user data, and its size in dwords.
    pub(crate) user_data: Option<(u32, u32)>,
    /// Root parameters of its root views: (title root parameter, ours).
    pub(crate) root_cbvs: Vec<(u8, u32)>,
    pub(crate) root_srvs: Vec<(u8, u32)>,
    /// Root UAVs: one-view tables like the SRVs (or root descriptors with
    /// `VICTORY_ROOT_SRV_DIRECT=1`).
    pub(crate) root_uavs: Vec<(u8, u32)>,
}

impl Stage {
    pub(crate) fn translated(&self) -> &Translated {
        self.shader
            .translated()
            .expect("host pipelines are made from translated shaders")
    }
}

pub(crate) struct HostPipeline {
    /// A compute pipeline (else graphics).
    pub(crate) compute: bool,
    /// A mesh pipeline (graphics without vertex input).
    pub(crate) mesh: bool,
    pub(crate) pso: ID3D12PipelineState,
    pub(crate) root: ID3D12RootSignature,
    /// Vertex, then pixel stage, or the compute stage.
    pub(crate) stages: Vec<Stage>,
    /// The vertex buffer slots it reads (bit per slot).
    pub(crate) slots: u32,
    pub(crate) inputs: Vec<Input>,
    /// Has the geometry stage that completes the console's rectangles
    /// (`victory_shader::emit::rect_list_gs`): its rectangle draws go out
    /// as the title gives them, three vertices a rectangle.
    pub(crate) rects: bool,
    /// Its shaders, for reports of slow submissions.
    pub(crate) name: String,
    /// Its depth, stencil, cull and blend state in short, for the frame
    /// listing (empty for compute).
    #[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
    pub(crate) state: String,
}

/// Logs a pipeline that gets no host pipeline, once per reason.
fn refuse(reason: &'static str, shaders: &[Arc<Shader>]) -> Option<HostPipeline> {
    static SEEN: std::sync::Mutex<Vec<&str>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.contains(&reason) {
        seen.push(reason);
        tracing::warn!(?shaders, "no host pipeline: {reason} (logged once)");
    }
    None
}

/// The length of a stage's code; `None` when the stage is absent.
fn stage_len(code: &D3D12_SHADER_BYTECODE) -> Option<usize> {
    (!code.pShaderBytecode.is_null()).then_some(code.BytecodeLength)
}

/// The combinations D3D12's documentation calls invalid, as far as a
/// description names them: a build without the debug layer gets
/// `E_INVALIDARG` and nothing else, so the reason has to be read from the
/// values the description holds. A `diagnostics` build has the debug
/// layer's own message as well.
fn invalid_combinations(
    stage: (&'static str, Option<usize>),
    root: bool,
    dsv: DXGI_FORMAT,
    depth: &D3D12_DEPTH_STENCIL_DESC,
    sample_mask: u32,
    samples: DXGI_SAMPLE_DESC,
    targets: &D3D12_RT_FORMAT_ARRAY,
) -> Vec<String> {
    let (name, code) = stage;
    let mut why = Vec::new();
    if !root {
        why.push("no root signature".to_owned());
    }
    if code.is_none() {
        why.push(format!("no {name} shader"));
    }
    if samples.Count == 0 {
        why.push("a sample count of zero".to_owned());
    }
    if sample_mask == 0 {
        why.push("a sample mask of zero".to_owned());
    }
    if targets.NumRenderTargets > 8 {
        why.push("more than eight render targets".to_owned());
    }
    let used = targets.NumRenderTargets.min(8) as usize;
    if used > 0 && targets.RTFormats[..used].iter().all(|f| *f == DXGI_FORMAT_UNKNOWN) {
        why.push("no render target format for its render targets".to_owned());
    }
    if dsv == DXGI_FORMAT_UNKNOWN && depth.DepthEnable.as_bool() {
        why.push("a depth test with no DSV format".to_owned());
    }
    if dsv == DXGI_FORMAT_UNKNOWN && depth.StencilEnable.as_bool() {
        why.push("a stencil test with no DSV format".to_owned());
    }
    why
}

/// Whether a depth or stencil test runs while the description names no DSV
/// format: D3D12 refuses that, and with no depth attachment the test reads
/// nothing, so the fallback below cannot change the pixels.
fn depth_without_attachment(depth: &D3D12_DEPTH_STENCIL_DESC, dsv: DXGI_FORMAT) -> bool {
    dsv == DXGI_FORMAT_UNKNOWN && (depth.DepthEnable.as_bool() || depth.StencilEnable.as_bool())
}

/// `depth` with the test off and nothing written: what a description with
/// no DSV format means, since it has no attachment for the test to read or
/// the write to reach.
fn without_depth(depth: D3D12_DEPTH_STENCIL_DESC) -> D3D12_DEPTH_STENCIL_DESC {
    D3D12_DEPTH_STENCIL_DESC {
        DepthEnable: false.into(),
        DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ZERO,
        StencilEnable: false.into(),
        ..depth
    }
}

/// `depth` as the one-bit-wider description subobject 21 carries, with the
/// depth bounds test `bounds`.
fn depth1(depth: &D3D12_DEPTH_STENCIL_DESC, bounds: bool) -> D3D12_DEPTH_STENCIL_DESC1 {
    D3D12_DEPTH_STENCIL_DESC1 {
        DepthEnable: depth.DepthEnable,
        DepthWriteMask: depth.DepthWriteMask,
        DepthFunc: depth.DepthFunc,
        StencilEnable: depth.StencilEnable,
        StencilReadMask: depth.StencilReadMask,
        StencilWriteMask: depth.StencilWriteMask,
        FrontFace: depth.FrontFace,
        BackFace: depth.BackFace,
        DepthBoundsTestEnable: bounds.into(),
    }
}

/// The blend state as D3D12 validates it: per render target, whether it
/// blends and what it writes.
fn blend_writes(blend: &D3D12_BLEND_DESC, targets: u32) -> String {
    let used = (targets.min(8) as usize).max(1);
    blend.RenderTarget[..used]
        .iter()
        .map(|t| format!("{}{:x}", u8::from(t.BlendEnable.as_bool()), t.RenderTargetWriteMask))
        .collect::<Vec<_>>()
        .join(",")
}

/// The fields D3D12 validates of a graphics description, in one line, with
/// the combinations it documents as invalid: its `E_INVALIDARG` names no
/// parameter by itself.
fn log_graphics_refusal(desc: &D3D12_GRAPHICS_PIPELINE_STATE_DESC, shaders: &[Arc<Shader>]) {
    let targets = D3D12_RT_FORMAT_ARRAY {
        RTFormats: desc.RTVFormats,
        NumRenderTargets: desc.NumRenderTargets,
    };
    let used = desc.NumRenderTargets.min(8) as usize;
    tracing::warn!(
        ?shaders,
        vs = ?stage_len(&desc.VS),
        ps = ?stage_len(&desc.PS),
        gs = ?stage_len(&desc.GS),
        hs = ?stage_len(&desc.HS),
        ds = ?stage_len(&desc.DS),
        root_signature = desc.pRootSignature.is_some(),
        targets = desc.NumRenderTargets,
        formats = format_args!("{:?}", &desc.RTVFormats[..used]),
        dsv_format = desc.DSVFormat.0,
        depth = desc.DepthStencilState.DepthEnable.as_bool(),
        depth_write = desc.DepthStencilState.DepthWriteMask.0,
        stencil = desc.DepthStencilState.StencilEnable.as_bool(),
        samples = desc.SampleDesc.Count,
        quality = desc.SampleDesc.Quality,
        sample_mask = format_args!("{:#x}", desc.SampleMask),
        strip_cut = desc.IBStripCutValue.0,
        topology = desc.PrimitiveTopologyType.0,
        blend = blend_writes(&desc.BlendState, desc.NumRenderTargets),
        input_elements = desc.InputLayout.NumElements,
        node_mask = desc.NodeMask,
        flags = desc.Flags.0,
        suspected = ?invalid_combinations(
            ("vertex", stage_len(&desc.VS)),
            desc.pRootSignature.is_some(),
            desc.DSVFormat,
            &desc.DepthStencilState,
            desc.SampleMask,
            desc.SampleDesc,
            &targets,
        ),
        "host graphics pipeline refused: the fields D3D12 validates"
    );
}

/// The `PSV0` stage of a compute shader. The container numbers programs as
/// `victory_shader` does: 0 pixel, 1 vertex, 2 geometry, 5 compute, 13 mesh.
const PSV_COMPUTE: u8 = 5;
/// As [`PSV_COMPUTE`], for the mesh shader of a mesh pipeline.
const PSV_MESH: u8 = 13;

/// What a stage's `PSV0` part declares: the stage it was built for, its
/// thread group, and the resources it names as (kind, space, register) in
/// the numbering `PSV0` itself uses — 1 sampler, 2 CBV, 4 raw SRV, 7 raw
/// UAV.
struct Psv0 {
    stage: u8,
    threads: [u32; 3],
    /// Mesh: the counts the program declares (`max_vertices`,
    /// `max_primitives`), which `emit::lower`'s `mesh_prologue` sets every
    /// group's counts to.
    declared: (u32, u32),
    bindings: Vec<(u32, u32, u32)>,
}

/// What the mesh pipelines made so far declare, so a frame can price the
/// outputs our translation promises for every group it dispatches: the
/// groups a frame dispatched (counted on the card, `MESH_GROUPS`) times the
/// vertices and primitives each group declares. Every mesh program the
/// title ships declares the same pair, so the pair is kept as the smallest
/// one seen and the third word says 1 when any pipeline declared another
/// (the frame line then says "at most").
static MESH_DECLARED: [AtomicU64; 3] = [const { AtomicU64::new(u64::MAX) }; 3];

/// Notes one mesh pipeline's declared counts ([`MESH_DECLARED`]).
fn note_mesh_declared(max_vertices: u32, max_primitives: u32) {
    let [v, p] = [max_vertices, max_primitives].map(u64::from);
    let seen = MESH_DECLARED[0].fetch_min(v, Ordering::Relaxed);
    MESH_DECLARED[1].fetch_min(p, Ordering::Relaxed);
    if seen != u64::MAX && (seen != v || MESH_DECLARED[1].load(Ordering::Relaxed) != p) {
        MESH_DECLARED[2].store(1, Ordering::Relaxed);
    }
}

/// The declared mesh counts a frame's groups each promise, and whether the
/// pipelines made so far disagreed about them (`None` before any is made).
pub(crate) fn mesh_declared() -> Option<((u64, u64), bool)> {
    let (v, p) = (MESH_DECLARED[0].load(Ordering::Relaxed), MESH_DECLARED[1].load(Ordering::Relaxed));
    (v != u64::MAX).then(|| ((v, p), MESH_DECLARED[2].load(Ordering::Relaxed) != 0))
}

/// A little-endian word of `data`, when it holds one.
fn word(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 4)
        .map(|b| u32::from_le_bytes(b.try_into().expect("four bytes")))
}

/// A little-endian halfword of `data`, when it holds one.
fn half(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 2)
        .map(|b| u32::from(u16::from_le_bytes(b.try_into().expect("two bytes"))))
}

/// Reads `PSV0`: a size for its runtime info, that info, then the bindings
/// as a count, a record stride and one record each. Every read is checked
/// and the error says which field of the part is inconsistent, so a refused
/// pipeline is never left with a field nobody looked at.
fn psv0(data: &[u8]) -> Result<Psv0, String> {
    let info = word(data, 0).ok_or("its PSV0 holds no runtime info size")? as usize;
    if !(48..=1024).contains(&info) {
        return Err(format!("its PSV0 declares {info} bytes of runtime info"));
    }
    // Runtime info version 2: the stage at 24, the thread group at 36.
    let stage = *data
        .get(4 + 24)
        .ok_or("its PSV0 runtime info is truncated")?;
    let mut threads = [0u32; 3];
    for (k, at) in [36, 40, 44].into_iter().enumerate() {
        threads[k] = word(data, 4 + at).ok_or("its PSV0 runtime info is truncated")?;
    }
    // Mesh runtime info: the declared counts, 16 bits each (`emit::container`).
    let declared = if stage == PSV_MESH {
        match (half(data, 4 + 12), half(data, 4 + 14)) {
            (Some(v), Some(p)) => (v, p),
            _ => return Err("its PSV0 runtime info is truncated".to_owned()),
        }
    } else {
        (0, 0)
    };
    let mut bindings = Vec::new();
    let at = 4 + info;
    if let Some(count) = word(data, at) {
        let count = count as usize;
        if count > 64 {
            return Err(format!(
                "its PSV0 declares {count} bindings, more than the 64 a root signature can name"
            ));
        }
        if count > 0 {
            let stride = word(data, at + 4).ok_or("its PSV0 bindings have no stride")? as usize;
            if !(12..=256).contains(&stride) {
                return Err(format!("its PSV0 declares a binding stride of {stride} bytes"));
            }
            for i in 0..count {
                let at = at + 8 + i * stride;
                match (word(data, at), word(data, at + 4), word(data, at + 8)) {
                    (Some(kind), Some(space), Some(register)) => {
                        bindings.push((kind, space, register));
                    }
                    _ => return Err("its PSV0 binding records are truncated".to_owned()),
                }
            }
        }
    }
    Ok(Psv0 {
        stage,
        threads,
        declared,
        bindings,
    })
}

/// A `PSV0` binding kind's name.
fn binding_kind(kind: u32) -> String {
    match kind {
        1 => "sampler".to_owned(),
        2 => "cbv".to_owned(),
        4 => "srv".to_owned(),
        7 => "uav".to_owned(),
        n => format!("kind {n}"),
    }
}

/// A root parameter as the log prints it, and the bindings it holds in the
/// numbering [`Psv0`] uses (a constants parameter is the user data cbuffer at
/// `b<register>` in its space).
fn param_facts(p: &D3D12_ROOT_PARAMETER1) -> (String, Vec<(u32, u32, u32)>) {
    if p.ParameterType == D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS {
        // SAFETY: every parameter we build sets the member its
        // `ParameterType` names, and this arm reads that member only.
        let c = unsafe { p.Anonymous.Constants };
        return (
            format!(
                "constants {}:{} {}dw",
                c.RegisterSpace, c.ShaderRegister, c.Num32BitValues
            ),
            vec![(2, c.RegisterSpace, c.ShaderRegister)],
        );
    }
    let kind = if p.ParameterType == D3D12_ROOT_PARAMETER_TYPE_CBV {
        2
    } else if p.ParameterType == D3D12_ROOT_PARAMETER_TYPE_SRV {
        4
    } else if p.ParameterType == D3D12_ROOT_PARAMETER_TYPE_UAV {
        7
    } else if p.ParameterType != D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE {
        return (format!("parameter of type {}", p.ParameterType.0), Vec::new());
    } else {
        0
    };
    if kind != 0 {
        // SAFETY: as above; a leaf parameter holds a descriptor.
        let d = unsafe { p.Anonymous.Descriptor };
        return (
            format!("{} {}:{}", binding_kind(kind), d.RegisterSpace, d.ShaderRegister),
            vec![(kind, d.RegisterSpace, d.ShaderRegister)],
        );
    }
    // SAFETY: as above; a table parameter holds a range list.
    let t = unsafe { p.Anonymous.DescriptorTable };
    let n = t.NumDescriptorRanges as usize;
    if n == 0 || t.pDescriptorRanges.is_null() {
        return (format!("table of {n} ranges"), Vec::new());
    }
    // SAFETY: the description names `n` ranges and the caller keeps them
    // alive while the root signature is made, which is what this reports on.
    let ranges = unsafe { std::slice::from_raw_parts(t.pDescriptorRanges, n) };
    let held: Vec<(u32, u32, u32)> = ranges
        .iter()
        .map(|r| {
            let kind = if r.RangeType == D3D12_DESCRIPTOR_RANGE_TYPE_CBV {
                2
            } else if r.RangeType == D3D12_DESCRIPTOR_RANGE_TYPE_UAV {
                7
            } else if r.RangeType == D3D12_DESCRIPTOR_RANGE_TYPE_SAMPLER {
                1
            } else {
                4
            };
            (kind, r.RegisterSpace, r.BaseShaderRegister)
        })
        .collect();
    let names: Vec<String> = held
        .iter()
        .map(|&(kind, space, register)| format!("{} {space}:{register} (table)", binding_kind(kind)))
        .collect();
    (names.join(","), held)
}

/// A stage's container as a refusal prints it, what of the bytes is provably
/// invalid, and the resources `PSV0` names.
struct StageFacts {
    /// `container=... dxil=... psv0=... sfi0=...`
    text: String,
    /// What is provably invalid about the bytes; empty when nothing is.
    causes: Vec<String>,
    /// The resources `PSV0` names, when its binding records could be read.
    bindings: Option<Vec<(u32, u32, u32)>>,
}

/// Reads a stage's DXIL container: the part table, the `DXIL` part's own two
/// lengths and bitcode magic, `SFI0`'s feature bits and `PSV0`'s stage,
/// thread group and bindings. `stage` and `threads` say what the pipeline
/// needs of `PSV0` (compute and mesh declare a thread group; the graphics
/// stages do not).
fn stage_facts(code: &[u8], stage: u8, threads: bool) -> StageFacts {
    let mut facts = StageFacts {
        text: String::new(),
        causes: Vec::new(),
        bindings: None,
    };
    let container = match victory_shader::Container::parse(code) {
        Ok(c) => c,
        Err(e) => {
            facts.text = "container=none".to_owned();
            facts.causes.push(format!("its container is not readable: {e}"));
            return facts;
        }
    };
    let parts: Vec<String> = container
        .parts
        .iter()
        .map(|p| format!("{}:{}", p.fourcc, p.data.len()))
        .collect();
    facts.text = format!(
        "container=v{}.{} of {} bytes parts=[{}]",
        container.version.0,
        container.version.1,
        code.len(),
        parts.join(",")
    );
    match container
        .part(victory_shader::FourCC::DXIL)
        .or_else(|| container.part(victory_shader::FourCC::ACCC))
    {
        None => facts
            .causes
            .push("its container has no DXIL part".to_owned()),
        Some(part) if part.fourcc == victory_shader::FourCC::ACCC => {
            if !victory_shader::fspx::is_package(part.data) {
                facts
                    .causes
                    .push("its ACCC part is not an FSPX package".to_owned());
            }
        }
        Some(part) => {
            let dxil = part.data;
            if dxil.len() < 24 {
                facts.causes.push(format!(
                    "its DXIL part is {} bytes, shorter than its 24-byte header",
                    dxil.len()
                ));
            } else if &dxil[8..12] != b"DXIL" {
                facts.causes
                    .push("its DXIL part has no DXIL code at its offset 8".to_owned());
            } else {
                // The part's own size is in dwords, at 4, and the length of
                // the bitcode that follows the 24-byte header is at 20: both
                // must agree with the part the container hands over.
                let size = word(dxil, 4).unwrap_or(0) as usize * 4;
                let bitcode = word(dxil, 20).unwrap_or(0) as usize;
                facts
                    .text
                    .push_str(&format!(" dxil={bitcode} of {} bytes", dxil.len()));
                if size > dxil.len() {
                    facts.causes.push(format!(
                        "its DXIL part declares {size} bytes and the container gives it {}",
                        dxil.len()
                    ));
                } else if 24 + bitcode > dxil.len() {
                    facts.causes.push(format!(
                        "its DXIL part declares {bitcode} bytes of bitcode and holds {}",
                        dxil.len() - 24
                    ));
                } else if !dxil[24..].starts_with(&[0x42, 0x43, 0xc0, 0xde]) {
                    facts
                        .causes
                        .push("its bitcode does not start with the LLVM bitcode magic".to_owned());
                }
            }
        }
    }
    if let Some(part) = container.part(victory_shader::FourCC::SFI0) {
        let features = word(part.data, 0).unwrap_or(0) as u64
            | (word(part.data, 4).unwrap_or(0) as u64) << 32;
        facts.text.push_str(&format!(" sfi0={features:#x}"));
    }
    match container.part(victory_shader::FourCC::PSV0) {
        None => facts
            .causes
            .push("its container has no PSV0 part".to_owned()),
        Some(part) => match psv0(part.data) {
            Err(e) => facts.causes.push(e),
            Ok(psv) => {
                let names: Vec<String> = psv
                    .bindings
                    .iter()
                    .map(|&(kind, space, register)| {
                        format!("{} {space}:{register}", binding_kind(kind))
                    })
                    .collect();
                facts.text.push_str(&format!(
                    " psv0=stage={} threads={}x{}x{} bindings=[{}]",
                    psv.stage, psv.threads[0], psv.threads[1], psv.threads[2], names.join(",")
                ));
                if psv.stage != stage {
                    facts.causes.push(format!(
                        "its PSV0 declares stage {} where the pipeline is stage {stage}",
                        psv.stage
                    ));
                }
                if psv.stage == PSV_MESH {
                    note_mesh_declared(psv.declared.0, psv.declared.1);
                }
                if threads && psv.threads.contains(&0) {
                    facts.causes.push(format!(
                        "its PSV0 declares a thread group of {}x{}x{}",
                        psv.threads[0], psv.threads[1], psv.threads[2]
                    ));
                }
                facts.bindings = Some(psv.bindings);
            }
        },
    }
    facts
}

/// What closes a refusal no CPU-side re-check can name: the code itself is
/// the only input left, and the driver owns its reading. It also says that
/// nothing is retried, which is the decision this path makes where the
/// graphics path turns a missing DSV off: every field here is one the
/// console's own state forces, and every parameter is made from a binding
/// the same translation declared, so there is nothing to drop or to add.
const CLOSES: &str = "nothing in the description or in the bytes of its shaders is invalid, so the \
     driver's compile of the code is what is left, and only a diagnostics build's debug layer, \
     VICTORY_D3D12_DRED, or VICTORY_SHADER_SAVE=<dir> with dxc.exe -dumpbin on the saved .dxil \
     names it; nothing is retried, since every field is one the console's own state forces and \
     every parameter is made from a binding this code declares";

/// The causes as one line of log text: what is provably invalid, or `no cause
/// known` and what closes it.
fn cause_text(causes: &[String]) -> String {
    if causes.is_empty() {
        format!("no cause known ({CLOSES})")
    } else {
        causes.join("; ")
    }
}

/// As [`log_graphics_refusal`], for a mesh pipeline: the same fields, from
/// the stream's state.
fn log_mesh_refusal(
    ms: D3D12_SHADER_BYTECODE,
    ps: D3D12_SHADER_BYTECODE,
    g: &GraphicsState,
    depth: &D3D12_DEPTH_STENCIL_DESC,
    shaders: &[Arc<Shader>],
) {
    let used = g.targets.NumRenderTargets.min(8) as usize;
    let facts = stage_facts(&code_bytes(&ms), PSV_MESH, true);
    let mut causes = invalid_combinations(
        ("mesh", stage_len(&ms)),
        true,
        g.depth_format,
        depth,
        g.sample_mask,
        g.samples,
        &g.targets,
    );
    causes.extend(facts.causes);
    tracing::warn!(
        ?shaders,
        ms = ?stage_len(&ms),
        ps = ?stage_len(&ps),
        root_signature = true,
        targets = g.targets.NumRenderTargets,
        formats = format_args!("{:?}", &g.targets.RTFormats[..used]),
        dsv_format = g.depth_format.0,
        depth = depth.DepthEnable.as_bool(),
        depth_write = depth.DepthWriteMask.0,
        stencil = depth.StencilEnable.as_bool(),
        samples = g.samples.Count,
        quality = g.samples.Quality,
        sample_mask = format_args!("{:#x}", g.sample_mask),
        topology = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE.0,
        blend = blend_writes(&g.blend, g.targets.NumRenderTargets),
        node_mask = 0,
        flags = 0,
        container = %facts.text,
        cause = %cause_text(&causes),
        "host mesh pipeline refused: the fields D3D12 validates"
    );
}

/// The line a compute refusal logs: every field of the description, the code
/// and the root signature it was given, what that code's container holds, and
/// the causes the re-check can name — `no cause known` and what closes it
/// when none of those is provably invalid.
///
/// The list this used to print (`suspected`) named two things a compute
/// description cannot be wrong about here: [`HostPipeline::new`] makes the
/// signature itself and returns before this when that fails, and the code is
/// the translation of a shader that translated. So it was empty for every
/// real refusal, while every field D3D12 can reject of a compute pipeline
/// lives in the bytes of the shader and not in the description's four fields.
fn compute_refusal(
    desc: &D3D12_COMPUTE_PIPELINE_STATE_DESC,
    root: Option<&[u8]>,
    params: &[D3D12_ROOT_PARAMETER1],
    samplers: usize,
    shader: Option<&HostShader>,
) -> String {
    let code = code_bytes(&desc.CS);
    let facts = stage_facts(&code, PSV_COMPUTE, true);
    let mut causes = Vec::new();
    let (root_bytes, root_hash) = match root {
        Some(bytes) if !bytes.is_empty() => (
            bytes.len().to_string(),
            blake3::hash(bytes).to_hex()[..8].to_owned(),
        ),
        _ => {
            causes.push("no root signature".to_owned());
            (0.to_string(), "none".to_owned())
        }
    };
    if desc.CS.pShaderBytecode.is_null() || desc.CS.BytecodeLength == 0 {
        causes.push("no compute shader".to_owned());
    }
    causes.extend(facts.causes);
    let mut names = Vec::with_capacity(params.len());
    let mut held: Vec<(u32, u32, u32)> = Vec::new();
    for p in params {
        let (name, bindings) = param_facts(p);
        names.push(name);
        held.extend(bindings);
    }
    if let Some(bindings) = &facts.bindings {
        for &(kind, space, register) in bindings {
            // A static sampler is the shader's own declaration, not one of
            // the draw's root arguments.
            if kind == 1 || held.contains(&(kind, space, register)) {
                continue;
            }
            causes.push(format!(
                "its PSV0 declares {} space {space} register {register} and no root parameter names it",
                binding_kind(kind)
            ));
        }
    }
    // Parameters the shader never reads are legal, and are what a fallback
    // would drop; there is nothing to drop here, since every parameter is
    // made from a binding the same translation declared.
    let unused = match &facts.bindings {
        Some(bindings) => held.iter().filter(|b| !bindings.contains(b)).count(),
        None => 0,
    };
    let mut line = format!(
        "cs={} root_bytes={root_bytes} root_hash={root_hash} root_flags={:#x} params={} samplers={} \
         node_mask={} flags={} cached_pso={} {}",
        code.len(),
        root_flags(true).0,
        params.len(),
        samplers,
        desc.NodeMask,
        desc.Flags.0,
        u8::from(!desc.CachedPSO.pCachedBlob.is_null()),
        facts.text,
    );
    if let Some(s) = shader {
        let slots = |slots: &[u8]| {
            slots
                .iter()
                .map(|&r| u32::from(r).to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        // Root views are declared in space 1 (`ROOT_CBV_SPACE`), user data
        // at `b0` in space 0.
        line.push_str(&format!(
            " declared=user_data={}dw cbvs=[{}] srvs=[{}] uavs=[{}] samplers={}",
            s.user_data_dwords(),
            slots(&s.root_cbvs),
            slots(&s.root_srvs),
            slots(&s.root_uavs),
            s.static_samplers.len(),
        ));
    }
    line.push_str(&format!(" signature=[{}]", names.join(",")));
    if unused > 0 {
        line.push_str(&format!(" parameters_the_code_does_not_name={unused}"));
    }
    line.push_str(&format!(" cause={}", cause_text(&causes)));
    line
}

/// As [`log_graphics_refusal`], for a compute description.
fn log_compute_refusal(
    desc: &D3D12_COMPUTE_PIPELINE_STATE_DESC,
    root: Option<&[u8]>,
    params: &[D3D12_ROOT_PARAMETER1],
    samplers: usize,
    shader: Option<&HostShader>,
    shaders: &[Arc<Shader>],
) {
    let report = compute_refusal(desc, root, params, samplers, shader);
    tracing::warn!(?shaders, "host compute pipeline refused: {report}");
}

impl HostPipeline {
    /// A host pipeline for vertex and pixel shaders with the graphics
    /// state `g`, or for one compute shader.
    pub(crate) fn new(shaders: &[Arc<Shader>], g: Option<&GraphicsState>) -> Option<HostPipeline> {
        // For the report when the host device is removed: what this thread
        // has in the library from here on.
        super::library::label(format!("{shaders:?}"));
        let find = |stage| shaders.iter().find(|s| s.stage == stage);
        let compute = match shaders {
            [cs] if cs.stage == "compute" => Some(cs),
            _ => None,
        };
        if compute.is_none()
            && shaders
                .iter()
                .any(|s| !matches!(s.stage, "vertex" | "pixel" | "mesh"))
        {
            return refuse(
                "only vertex, mesh, pixel and compute stages are translated",
                shaders,
            );
        }
        let mesh = find("mesh");
        let stages = if let Some(cs) = compute {
            vec![(cs, D3D12_SHADER_VISIBILITY_ALL)]
        } else {
            let first = match (mesh, find("vertex")) {
                (Some(ms), _) => (ms, D3D12_SHADER_VISIBILITY_MESH),
                (None, Some(vs)) => (vs, D3D12_SHADER_VISIBILITY_VERTEX),
                (None, None) => return refuse("no vertex or mesh shader", shaders),
            };
            let mut stages = vec![first];
            stages.extend(find("pixel").map(|ps| (ps, D3D12_SHADER_VISIBILITY_PIXEL)));
            stages
        };
        if mesh.is_some() && !host()?.mesh_shaders() {
            return refuse("the host GPU has no mesh shaders (or no shader model 6.6)", shaders);
        }
        for (s, _) in &stages {
            s.translated()?;
        }
        let host = host()?;

        // The root signature the translated shaders' bindings define.
        let mut params: Vec<Param> = Vec::new();
        // The ranges table parameters point at, alive until the root
        // signature is made.
        let mut ranges: Vec<Box<D3D12_DESCRIPTOR_RANGE1>> = Vec::new();
        let mut samplers = Vec::new();
        let mut bound = Vec::new();
        for &(shader, visibility) in &stages {
            let t = &shader.translated()?.host;
            let mut stage = Stage {
                shader: shader.clone(),
                user_data: None,
                root_cbvs: Vec::new(),
                root_srvs: Vec::new(),
                root_uavs: Vec::new(),
            };
            let dwords = t.user_data_dwords();
            if dwords > 0 {
                stage.user_data = Some((params.len() as u32, dwords));
                params.push(Param {
                    key: ParamKey {
                        visibility: visibility.0,
                        kind: Kind::Constants,
                        space: USER_DATA_SPACE,
                        register: 0,
                        size: dwords,
                    },
                    desc: D3D12_ROOT_PARAMETER1 {
                        ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                        Anonymous: D3D12_ROOT_PARAMETER1_0 {
                            Constants: D3D12_ROOT_CONSTANTS {
                                ShaderRegister: 0,
                                RegisterSpace: USER_DATA_SPACE,
                                Num32BitValues: dwords,
                            },
                        },
                        ShaderVisibility: visibility,
                    },
                });
            }
            for (slots, kind, out) in [
                (
                    &t.root_cbvs,
                    D3D12_ROOT_PARAMETER_TYPE_CBV,
                    &mut stage.root_cbvs,
                ),
                (
                    &t.root_srvs,
                    D3D12_ROOT_PARAMETER_TYPE_SRV,
                    &mut stage.root_srvs,
                ),
                (
                    &t.root_uavs,
                    D3D12_ROOT_PARAMETER_TYPE_UAV,
                    &mut stage.root_uavs,
                ),
            ] {
                for &slot in slots {
                    let uav = kind == D3D12_ROOT_PARAMETER_TYPE_UAV;
                    // Ours, not the title's, stay root descriptors: the
                    // table of strides, the table of view addresses, the
                    // directory of title memory by address and its fault
                    // notes.
                    let ours = if uav {
                        slot == victory_shader::emit::SLOT_FAULTS
                    } else {
                        slot == victory_shader::emit::SLOT_STRIDES
                            || slot == victory_shader::emit::SLOT_VIEW_ADDRESSES
                            || slot == victory_shader::emit::SLOT_MEMORY
                    };
                    if (uav || kind == D3D12_ROOT_PARAMETER_TYPE_SRV) && !ours && bounded_root_srvs() {
                        // A one-view table: the view's size bounds the
                        // reads (and a root UAV's writes), which a root
                        // descriptor leaves unbounded.
                        // (Our own table of strides stays a descriptor.)
                        ranges.push(Box::new(D3D12_DESCRIPTOR_RANGE1 {
                            RangeType: if uav {
                                D3D12_DESCRIPTOR_RANGE_TYPE_UAV
                            } else {
                                D3D12_DESCRIPTOR_RANGE_TYPE_SRV
                            },
                            NumDescriptors: 1,
                            BaseShaderRegister: u32::from(slot),
                            RegisterSpace: ROOT_CBV_SPACE,
                            Flags: D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE
                                | D3D12_DESCRIPTOR_RANGE_FLAG_DATA_VOLATILE,
                            OffsetInDescriptorsFromTableStart: 0,
                        }));
                        let range: *const D3D12_DESCRIPTOR_RANGE1 = &**ranges.last()?;
                        out.push((slot, params.len() as u32));
                        params.push(Param {
                            key: ParamKey {
                                visibility: visibility.0,
                                kind: if uav { Kind::UavTable } else { Kind::SrvTable },
                                space: ROOT_CBV_SPACE,
                                register: u32::from(slot),
                                size: 1,
                            },
                            desc: D3D12_ROOT_PARAMETER1 {
                                ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                                    DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE1 {
                                        NumDescriptorRanges: 1,
                                        pDescriptorRanges: range,
                                    },
                                },
                                ShaderVisibility: visibility,
                            },
                        });
                        continue;
                    }
                    out.push((slot, params.len() as u32));
                    params.push(Param {
                        key: ParamKey {
                            visibility: visibility.0,
                            kind: if kind == D3D12_ROOT_PARAMETER_TYPE_CBV {
                                Kind::Cbv
                            } else if uav {
                                Kind::Uav
                            } else {
                                Kind::Srv
                            },
                            space: ROOT_CBV_SPACE,
                            register: u32::from(slot),
                            size: 1,
                        },
                        desc: D3D12_ROOT_PARAMETER1 {
                            ParameterType: kind,
                            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                                Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                                    ShaderRegister: u32::from(slot),
                                    RegisterSpace: ROOT_CBV_SPACE,
                                    Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                                },
                            },
                            ShaderVisibility: visibility,
                        },
                    });
                }
            }
            for (n, s) in t.static_samplers.iter().enumerate() {
                samplers.push(D3D12_STATIC_SAMPLER_DESC {
                    Filter: D3D12_FILTER(s.filter as i32),
                    AddressU: D3D12_TEXTURE_ADDRESS_MODE(s.address[0] as i32),
                    AddressV: D3D12_TEXTURE_ADDRESS_MODE(s.address[1] as i32),
                    AddressW: D3D12_TEXTURE_ADDRESS_MODE(s.address[2] as i32),
                    MipLODBias: s.mip_lod_bias,
                    MaxAnisotropy: s.max_anisotropy,
                    ComparisonFunc: D3D12_COMPARISON_FUNC(s.comparison as i32),
                    BorderColor: D3D12_STATIC_BORDER_COLOR(s.border as i32),
                    MinLOD: s.min_lod,
                    MaxLOD: s.max_lod,
                    ShaderRegister: n as u32,
                    RegisterSpace: SAMPLER_SPACE,
                    ShaderVisibility: visibility,
                });
            }
            bound.push(stage);
        }
        // One order for one binding shape: the same bindings declared in
        // another order (the slots a shader names, the order its stages were
        // walked in) then make the same bytes, so they find one entry in the
        // host's pipeline library and one compiled signature in the driver's
        // cache instead of two. Each stage's own indices follow the move.
        let place = canonical_params(&mut params);
        for stage in &mut bound {
            if let Some((param, _)) = &mut stage.user_data {
                *param = place[*param as usize];
            }
            for views in [&mut stage.root_cbvs, &mut stage.root_srvs, &mut stage.root_uavs] {
                for (_, param) in views.iter_mut() {
                    *param = place[*param as usize];
                }
            }
        }
        let samplers = canonical_samplers(&samplers);
        let params: Vec<D3D12_ROOT_PARAMETER1> = params.into_iter().map(|p| p.desc).collect();
        let (root, root_blob) = match root_signature(&host.device, &params, &samplers, compute.is_some()) {
            Ok(root) => root,
            Err(e) => {
                tracing::warn!(error = %e, ?shaders, "host root signature refused");
                return None;
            }
        };
        let code = |i: usize| {
            stages.get(i).and_then(|(s, _)| s.translated()).map_or(
                D3D12_SHADER_BYTECODE::default(),
                |t| D3D12_SHADER_BYTECODE {
                    pShaderBytecode: t.host.dxil.as_ptr().cast(),
                    BytecodeLength: t.host.dxil.len(),
                },
            )
        };
        if compute.is_some() {
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                // SAFETY: a borrowed pointer the description never releases.
                pRootSignature: unsafe { std::mem::transmute_copy(&root) },
                CS: code(0),
                ..Default::default()
            };
            let name = super::library::name("compute", &root_blob, &[&code_bytes(&code(0))], "");
            let made = super::library::pipeline(
                &name,
                // SAFETY: the description points at live data.
                |library, name| unsafe { library.LoadComputePipeline(name, &desc) },
                // SAFETY: as above.
                || unsafe { host.device.CreateComputePipelineState(&desc) },
            );
            let pso = match made {
                Ok(pso) => pso,
                Err(e) => {
                    tracing::warn!(error = %e, ?shaders, "host pipeline refused");
                    log_compute_refusal(
                        &desc,
                        Some(&root_blob),
                        &params,
                        samplers.len(),
                        bound.first().and_then(|s| s.shader.translated()).map(|t| &t.host),
                        shaders,
                    );
                    host.log_messages();
                    // Nothing here is ours to change: the shader, the root
                    // signature and the flags are what the title asked for,
                    // so a compute pipeline the host refuses is dropped.
                    return None;
                }
            };
            tracing::info!(?shaders, "host pipeline");
            crate::host::name(&pso, || format!("{shaders:?}"));
            return Some(HostPipeline {
                compute: true,
                mesh: false,
                pso,
                root,
                stages: bound,
                name: format!("{shaders:?}"),
                slots: 0,
                inputs: Vec::new(),
                rects: false,
                state: String::new(),
            });
        }
        let Some(g) = g else {
            return refuse("graphics state not kept (plain description)", shaders);
        };
        let depth_bounds = if g.depth_bounds {
            if host.depth_bounds() {
                true
            } else {
                static SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                if SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 4 {
                    tracing::warn!(
                        "a pipeline enables the depth bounds test, which the host GPU does not support: its draws pass at every depth"
                    );
                }
                false
            }
        } else {
            false
        };
        let state = g.describe();
        if mesh.is_some() {
            // "triangles": the stream names its topology since it did not
            // load from the library without one (see `mesh_pipeline`); a
            // new name keeps such entries, stored before, from being
            // looked up again. A fallback's name says what it changed of
            // the state, so no stored pipeline is found by another state's
            // name.
            let mesh_name = |fallback: &str| {
                let state = if fallback.is_empty() {
                    state.clone()
                } else {
                    format!("{state} {fallback}")
                };
                super::library::name(
                    "mesh triangles",
                    &root_blob,
                    &[&code_bytes(&code(0)), &code_bytes(&code(1))],
                    &state,
                )
            };
            let attempt = |depth: (D3D12_DEPTH_STENCIL_DESC, bool), name: &str| {
                mesh_pipeline(&host.device, &root, code(0), code(1), g, depth, name)
            };
            let pso = match attempt((g.depth, depth_bounds), &mesh_name("")) {
                Ok(pso) => pso,
                Err(e) => {
                    tracing::warn!(error = %e, ?shaders, "host mesh pipeline refused");
                    log_mesh_refusal(code(0), code(1), g, &g.depth, shaders);
                    host.log_messages();
                    if !depth_without_attachment(&g.depth, g.depth_format) {
                        return None;
                    }
                    // The one fallback that cannot change the pixels: no
                    // DSV format means no attachment for the test to read.
                    tracing::warn!(
                        ?shaders,
                        "a mesh pipeline retried with the depth and stencil tests off: its description names no DSV format"
                    );
                    match attempt((without_depth(g.depth), false), &mesh_name("depth off (no DSV format)")) {
                        Ok(pso) => pso,
                        Err(e) => {
                            tracing::warn!(error = %e, ?shaders, "host mesh pipeline refused");
                            host.log_messages();
                            return None;
                        }
                    }
                }
            };
            tracing::info!(?shaders, "host mesh pipeline");
            crate::host::name(&pso, || format!("{shaders:?}"));
            return Some(HostPipeline {
                compute: false,
                mesh: true,
                pso,
                root,
                stages: bound,
                name: format!("{shaders:?}"),
                slots: 0,
                inputs: Vec::new(),
                rects: false,
                state: String::new(),
            });
        }

        // A pipeline the title makes for rectangles gets a geometry stage
        // that completes each one from the three vertices the vertex
        // shader made. Without it (refused here or by the host) the draws
        // fall back on what `draw` does for rectangles.
        let mut rect_gs = None;
        if g.topology.0 == RECTANGLES {
            let ps = stages.get(1).and_then(|(s, _)| s.translated());
            let vs = &stages[0].0.translated()?.host.dxil;
            match victory_shader::emit::rect_list_gs(vs, ps.map(|t| &t.host.dxil[..])) {
                Ok(gs) => rect_gs = Some(gs),
                Err(e) => tracing::warn!(error = %e, ?shaders, "no rectangle geometry stage"),
            }
        }
        let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            // SAFETY: a borrowed pointer the description never releases.
            pRootSignature: unsafe { std::mem::transmute_copy(&root) },
            VS: code(0),
            PS: code(1),
            GS: rect_gs.as_ref().map_or(D3D12_SHADER_BYTECODE::default(), |gs| {
                D3D12_SHADER_BYTECODE {
                    pShaderBytecode: gs.as_ptr().cast(),
                    BytecodeLength: gs.len(),
                }
            }),
            BlendState: g.blend,
            SampleMask: g.sample_mask,
            RasterizerState: g.rasterizer,
            DepthStencilState: g.depth,
            InputLayout: D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: g.inputs.as_ptr(),
                NumElements: g.inputs.len() as u32,
            },
            IBStripCutValue: g.strip_cut,
            // The console's rectangle and quad types draw as triangles on
            // the host (see `draw`).
            PrimitiveTopologyType: match g.topology.0 {
                RECTANGLES | QUADS => D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                _ => g.topology,
            },
            NumRenderTargets: g.targets.NumRenderTargets,
            RTVFormats: g.targets.RTFormats,
            DSVFormat: g.depth_format,
            SampleDesc: g.samples,
            ..Default::default()
        };
        // A pipeline with the geometry stage has a name of its own in the
        // library; the others keep theirs. `fallback` says what a retry
        // changed of the state, so that no stored pipeline is found by
        // another state's name.
        let name = |gs: Option<&Vec<u8>>, fallback: &str| {
            let state = if fallback.is_empty() {
                state.clone()
            } else {
                format!("{state} {fallback}")
            };
            match gs {
                Some(gs) => super::library::name(
                    "graphics rectangles",
                    &root_blob,
                    &[&code_bytes(&code(0)), &code_bytes(&code(1)), &gs[..]],
                    &state,
                ),
                None => super::library::name(
                    "graphics",
                    &root_blob,
                    &[&code_bytes(&code(0)), &code_bytes(&code(1))],
                    &state,
                ),
            }
        };
        // The depth description subobject the attempt hands over: the bounds
        // form when the draw asks for the depth bounds test, `None` for the
        // plain subobject 13. The fallback below turns the bounds test off with
        // the depth and stencil tests, so it passes `None`: a bounds test with
        // no DSV format has no depth value to compare and its PSO would be
        // refused again.
        let make = |desc: &D3D12_GRAPHICS_PIPELINE_STATE_DESC,
                    name: &str,
                    bounds: Option<D3D12_DEPTH_STENCIL_DESC1>| {
            if let Some(depth) = bounds {
                return graphics_depth_bounds_pipeline(&host.device, &root, desc, depth, name);
            }
            super::library::pipeline(
                name,
                // SAFETY: the description points at live data.
                |library, name| unsafe { library.LoadGraphicsPipeline(name, desc) },
                // SAFETY: as above.
                || unsafe { host.device.CreateGraphicsPipelineState(desc) },
            )
        };
        // Whether the pipeline that was finally made carries the depth bounds
        // test: the fallback below turns it off with the depth and stencil
        // tests, and the state line has to say what the PSO actually is.
        let mut made_bounds = depth_bounds;
        let mut made = make(&desc, &name(rect_gs.as_ref(), ""), depth_bounds.then(|| g.depth1()));
        if made.is_err() && rect_gs.is_some() {
            // The host refused the geometry stage: once more without it.
            tracing::warn!(?shaders, "host pipeline refused with the rectangle geometry stage");
            host.log_messages();
            desc.GS = D3D12_SHADER_BYTECODE::default();
            rect_gs = None;
            made = make(&desc, &name(None, ""), depth_bounds.then(|| g.depth1()));
        }
        if made.is_err() {
            log_graphics_refusal(&desc, shaders);
            if depth_without_attachment(&desc.DepthStencilState, desc.DSVFormat) {
                // The one fallback that cannot change what the title draws:
                // with no DSV format there is no depth attachment, so the
                // depth and stencil tests have nothing to read and their
                // writes nothing to reach, and the depth bounds test has no
                // depth value to compare against its two bounds. Every other
                // check D3D12 makes
                // (a format or sample count the host lacks, a blend or
                // target count it cannot take) would be answered by
                // changing the pixels, so a refusal there stays a refusal.
                tracing::warn!(
                    ?shaders,
                    "a host pipeline retried with the depth and stencil tests off: its description names no DSV format"
                );
                desc.DepthStencilState = without_depth(desc.DepthStencilState);
                made_bounds = false;
                made = make(&desc, &name(rect_gs.as_ref(), "depth off (no DSV format)"), None);
            }
        }
        let pso = match made {
            Ok(pso) => pso,
            Err(e) => {
                tracing::warn!(error = %e, ?shaders, "host pipeline refused");
                host.log_messages();
                return None;
            }
        };
        tracing::info!(?shaders, "host pipeline");
        crate::host::name(&pso, || format!("{shaders:?}"));
        tracing::debug!(
            ?shaders,
            depth = ?g.depth,
            depth_bounds,
            dsv = g.depth_format.0,
            targets = ?&g.targets.RTFormats[..g.targets.NumRenderTargets.min(8) as usize],
            blend = ?g.blend.RenderTarget[0],
            cull = g.rasterizer.CullMode.0,
            "host graphics state"
        );
        Some(HostPipeline {
            compute: false,
            mesh: false,
            pso,
            root,
            stages: bound,
            name: format!("{shaders:?}"),
            slots: g.slots(),
            inputs: g.elements(),
            rects: rect_gs.is_some(),
            state: {
                let (d, r, b) = (&g.depth, &g.rasterizer, &g.blend.RenderTarget[0]);
                let face = |f: &D3D12_DEPTH_STENCILOP_DESC| {
                    format!(
                        "fail {} depth-fail {} pass {} func {}",
                        f.StencilFailOp.0, f.StencilDepthFailOp.0, f.StencilPassOp.0, f.StencilFunc.0
                    )
                };
                let bounds = if made_bounds { " bounds true" } else { "" };
                format!(
                    "depth {} write {} func {}{bounds}; stencil {} read {:#x} write {:#x} front [{}] back [{}]; cull {} front-ccw {}; blend {} src {} dst {} op {} mask {:#x}",
                    d.DepthEnable.as_bool(),
                    d.DepthWriteMask.0,
                    d.DepthFunc.0,
                    d.StencilEnable.as_bool(),
                    d.StencilReadMask,
                    d.StencilWriteMask,
                    face(&d.FrontFace),
                    face(&d.BackFace),
                    r.CullMode.0,
                    r.FrontCounterClockwise.as_bool(),
                    b.BlendEnable.as_bool(),
                    b.SrcBlend.0,
                    b.DestBlend.0,
                    b.BlendOp.0,
                    b.RenderTargetWriteMask
                )
            },
        })
    }
}

/// A stream subobject: its type, then its description, pointer-aligned.
#[repr(C, align(8))]
struct Sub<T>(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE, T);

fn stream_pipeline<S>(
    device: &ID3D12Device,
    stream: &mut S,
    name: &str,
) -> windows::core::Result<ID3D12PipelineState> {
    use windows::core::Interface;
    let desc = D3D12_PIPELINE_STATE_STREAM_DESC {
        SizeInBytes: size_of::<S>(),
        pPipelineStateSubobjectStream: std::ptr::from_mut(stream).cast(),
    };
    let device = device.cast::<ID3D12Device2>()?;
    super::library::pipeline(
        name,
        // SAFETY: the stream holds whole subobjects pointing at live data.
        |library, name| unsafe { library.LoadPipeline(name, &desc) },
        // SAFETY: as above.
        || unsafe { device.CreatePipelineState(&desc) },
    )
}

/// A graphics pipeline enabling the depth bounds test: `D3D12_GRAPHICS_PIPELINE_STATE_DESC`
/// has no field for `DepthBoundsTestEnable`, so it is created and loaded as a
/// pipeline-state stream with `DEPTH_STENCIL1`.
fn graphics_depth_bounds_pipeline(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    d: &D3D12_GRAPHICS_PIPELINE_STATE_DESC,
    depth: D3D12_DEPTH_STENCIL_DESC1,
    name: &str,
) -> windows::core::Result<ID3D12PipelineState> {
    use windows::core::Interface;
    #[repr(C)]
    struct Stream {
        root: Sub<*mut std::ffi::c_void>,
        vs: Sub<D3D12_SHADER_BYTECODE>,
        ps: Sub<D3D12_SHADER_BYTECODE>,
        blend: Sub<D3D12_BLEND_DESC>,
        mask: Sub<u32>,
        raster: Sub<D3D12_RASTERIZER_DESC>,
        depth: Sub<D3D12_DEPTH_STENCIL_DESC1>,
        input: Sub<D3D12_INPUT_LAYOUT_DESC>,
        strip_cut: Sub<D3D12_INDEX_BUFFER_STRIP_CUT_VALUE>,
        topology: Sub<D3D12_PRIMITIVE_TOPOLOGY_TYPE>,
        targets: Sub<D3D12_RT_FORMAT_ARRAY>,
        dsv: Sub<DXGI_FORMAT>,
        samples: Sub<DXGI_SAMPLE_DESC>,
    }
    #[repr(C)]
    struct StreamGs {
        base: Stream,
        gs: Sub<D3D12_SHADER_BYTECODE>,
    }
    let targets = D3D12_RT_FORMAT_ARRAY {
        RTFormats: d.RTVFormats,
        NumRenderTargets: d.NumRenderTargets,
    };
    let base = Stream {
        root: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE, root.as_raw()),
        vs: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VS, d.VS),
        ps: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, d.PS),
        blend: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND, d.BlendState),
        mask: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK, d.SampleMask),
        raster: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER, d.RasterizerState),
        depth: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1, depth),
        input: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_INPUT_LAYOUT, d.InputLayout),
        strip_cut: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_IB_STRIP_CUT_VALUE, d.IBStripCutValue),
        topology: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY, d.PrimitiveTopologyType),
        targets: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS, targets),
        dsv: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT, d.DSVFormat),
        samples: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC, d.SampleDesc),
    };
    if d.GS.pShaderBytecode.is_null() {
        let mut stream = base;
        stream_pipeline(device, &mut stream, name)
    } else {
        let mut stream = StreamGs {
            base,
            gs: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_GS, d.GS),
        };
        stream_pipeline(device, &mut stream, name)
    }
}

/// A mesh pipeline from the title's graphics state: the pipeline-state
/// stream form (no input layout).
///
/// The stream names a triangle topology, which D3D12 does not need for a
/// mesh shader: without it the pipeline library refuses to load a stored
/// pipeline with the very stream it was made from (`E_INVALIDARG`; debug
/// layer message 972, "does not match the one used to create the PSO"), so
/// every mesh pipeline was compiled again, and its store then refused (970,
/// "already exists"). The translation makes triangle mesh shaders only
/// (`mesh_prologue` refuses others), and the debug layer wants the two to
/// agree (1323).
fn mesh_pipeline(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    ms: D3D12_SHADER_BYTECODE,
    ps: D3D12_SHADER_BYTECODE,
    g: &GraphicsState,
    // `g`'s, or with the tests off when a retry has established that the
    // description names no DSV format; and the bounds test of `g`'s, off
    // with that retry: with no DSV format the bounds test has nothing to
    // read either.
    depth: (D3D12_DEPTH_STENCIL_DESC, bool),
    name: &str,
) -> windows::core::Result<ID3D12PipelineState> {
    let (depth, depth_bounds) = depth;
    #[repr(C)]
    struct Stream<D> {
        root: Sub<*mut std::ffi::c_void>,
        ms: Sub<D3D12_SHADER_BYTECODE>,
        ps: Sub<D3D12_SHADER_BYTECODE>,
        blend: Sub<D3D12_BLEND_DESC>,
        mask: Sub<u32>,
        raster: Sub<D3D12_RASTERIZER_DESC>,
        depth: Sub<D>,
        targets: Sub<D3D12_RT_FORMAT_ARRAY>,
        dsv: Sub<DXGI_FORMAT>,
        samples: Sub<DXGI_SAMPLE_DESC>,
        topology: Sub<D3D12_PRIMITIVE_TOPOLOGY_TYPE>,
    }
    /// The stream with `depth` in the subobject of `kind`.
    fn stream<D>(
        root: &ID3D12RootSignature,
        ms: D3D12_SHADER_BYTECODE,
        ps: D3D12_SHADER_BYTECODE,
        g: &GraphicsState,
        depth: Sub<D>,
    ) -> Stream<D> {
        use windows::core::Interface;
        Stream {
            root: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE, root.as_raw()),
            ms: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS, ms),
            ps: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, ps),
            blend: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND, g.blend),
            mask: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK, g.sample_mask),
            raster: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER, g.rasterizer),
            depth,
            targets: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS, g.targets),
            dsv: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT, g.depth_format),
            samples: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC, g.samples),
            topology: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY,
                D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            ),
        }
    }
    // The depth bounds test has no field in `D3D12_DEPTH_STENCIL_DESC`: its
    // stream names subobject 21 instead, which holds one more bit
    // (`depth1`).
    if depth_bounds {
        let mut stream = stream(
            root,
            ms,
            ps,
            g,
            Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1, depth1(&depth, true)),
        );
        stream_pipeline(device, &mut stream, name)
    } else {
        let mut stream = stream(
            root,
            ms,
            ps,
            g,
            Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL, depth),
        );
        stream_pipeline(device, &mut stream, name)
    }
}

/// The bytes of shader code (none for an absent stage).
fn code_bytes(code: &D3D12_SHADER_BYTECODE) -> Vec<u8> {
    if code.pShaderBytecode.is_null() {
        return Vec::new();
    }
    // SAFETY: a translation's own bytes, alive while its shader is.
    unsafe { std::slice::from_raw_parts(code.pShaderBytecode.cast::<u8>(), code.BytecodeLength) }.to_vec()
}

/// The kind of binding a root parameter holds, in the order the canonical
/// signature groups them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// 32-bit constants (a stage's user data).
    Constants,
    Cbv,
    Srv,
    SrvTable,
    Uav,
    UavTable,
}

/// What a root parameter binds: the stage that sees it, its kind, where the
/// shader names it, and its size (dwords of constants, descriptors of a
/// table). Its order is the canonical order of the signature's parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ParamKey {
    /// `D3D12_SHADER_VISIBILITY`: a stage's own block stays a block.
    visibility: i32,
    kind: Kind,
    space: u32,
    register: u32,
    size: u32,
}

/// A root parameter and the binding it holds.
#[derive(Clone, Copy)]
struct Param {
    key: ParamKey,
    desc: D3D12_ROOT_PARAMETER1,
}

/// Puts the parameters in one order for one binding shape, and gives back
/// where each built parameter went: a stage's recorded indices are the ones
/// it had while the block was built.
///
/// The order holds no fact about the pipeline: a root parameter is named by
/// its register and space, and the draws reach theirs through the indices
/// this returns. Two pipelines whose bindings differ only in the order the
/// slots were named in therefore make one signature, and one blob in the
/// host library's name.
fn canonical_params(params: &mut [Param]) -> Vec<u32> {
    let mut order: Vec<u32> = (0..params.len() as u32).collect();
    order.sort_by_key(|&at| params[at as usize].key);
    let mut place = vec![0u32; params.len()];
    for (to, &from) in order.iter().enumerate() {
        place[from as usize] = to as u32;
    }
    // `sort_by_key` is stable and so is the sort above: equal keys keep the
    // order they were built in on both sides, so `place` matches.
    params.sort_by_key(|p| p.key);
    place
}

/// The static samplers of the whole signature, one per register when every
/// stage that names that register names the same sampler (visible to all of
/// them then), sorted; as built — one per stage — when two stages name
/// different samplers there, which is legal and which the console's own
/// shaders do use: a static sampler is the shader's own declaration, not a
/// root argument of the draw.
fn canonical_samplers(built: &[D3D12_STATIC_SAMPLER_DESC]) -> Vec<D3D12_STATIC_SAMPLER_DESC> {
    let seen = |mut s: D3D12_STATIC_SAMPLER_DESC| {
        s.ShaderVisibility = D3D12_SHADER_VISIBILITY_ALL;
        s
    };
    let mut out: Vec<D3D12_STATIC_SAMPLER_DESC> = Vec::with_capacity(built.len());
    for &sampler in built {
        let sampler = seen(sampler);
        match out
            .iter()
            .find(|s| (s.RegisterSpace, s.ShaderRegister) == (sampler.RegisterSpace, sampler.ShaderRegister))
        {
            Some(same) if *same == sampler => {}
            Some(_) => return built.to_vec(),
            None => out.push(sampler),
        }
    }
    out.sort_by_key(|s| (s.RegisterSpace, s.ShaderRegister));
    out
}

/// The root signatures this run has made, by their serialised bytes: the
/// bytes are the shape, and they are also what the host's pipeline library
/// keys a pipeline by and what the driver keys its own compiled signatures
/// by, so a shape met again is neither serialised nor created again.
type Roots = HashMap<Vec<u8>, (ID3D12RootSignature, Arc<Vec<u8>>)>;

fn roots() -> std::sync::MutexGuard<'static, Option<Roots>> {
    static ROOTS: Mutex<Option<Roots>> = Mutex::new(None);
    let mut roots = ROOTS.lock().unwrap_or_else(|e| e.into_inner());
    roots.get_or_insert_with(Roots::new);
    roots
}

/// Root signatures found already made, and created, this run (the census).
pub(crate) fn root_signature_counts() -> (u64, u64) {
    (ROOT_HITS.load(Ordering::Relaxed), ROOT_CREATED.load(Ordering::Relaxed))
}

static ROOT_HITS: AtomicU64 = AtomicU64::new(0);
static ROOT_CREATED: AtomicU64 = AtomicU64::new(0);

/// The flags every signature this module makes carries: the input layout only
/// where a stage draws, and both heaps directly indexed, which is how the
/// translated code reaches a descriptor it was given the address of.
fn root_flags(compute: bool) -> D3D12_ROOT_SIGNATURE_FLAGS {
    let input = if compute {
        D3D12_ROOT_SIGNATURE_FLAG_NONE
    } else {
        D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT
    };
    input
        | D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED
        | D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED
}

/// The root signature the parameters and samplers describe, and its
/// serialised bytes. A signature whose bytes this run already has is handed
/// out again rather than created anew.
fn root_signature(
    device: &ID3D12Device,
    params: &[D3D12_ROOT_PARAMETER1],
    samplers: &[D3D12_STATIC_SAMPLER_DESC],
    compute: bool,
) -> windows::core::Result<(ID3D12RootSignature, Arc<Vec<u8>>)> {
    let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
        Version: windows::Win32::Graphics::Direct3D12::D3D_ROOT_SIGNATURE_VERSION_1_1,
        Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
            Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                NumParameters: params.len() as u32,
                pParameters: params.as_ptr(),
                NumStaticSamplers: samplers.len() as u32,
                pStaticSamplers: samplers.as_ptr(),
                Flags: root_flags(compute),
            },
        },
    };
    // SAFETY: the description points at live arrays; the blob is read in
    // full before it is released.
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        let mut error: Option<ID3DBlob> = None;
        if let Err(e) = D3D12SerializeVersionedRootSignature(&desc, &mut blob, Some(&mut error)) {
            if let Some(error) = error {
                let text = std::slice::from_raw_parts(
                    error.GetBufferPointer().cast::<u8>(),
                    error.GetBufferSize(),
                );
                tracing::warn!("root signature: {}", String::from_utf8_lossy(text));
            }
            return Err(e);
        }
        let blob = blob.ok_or_else(windows::core::Error::empty)?;
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        // A shape this run has already made: the same signature, made once.
        let found = {
            let roots = roots();
            roots
                .as_ref()
                .and_then(|roots| roots.get(bytes))
                .map(|(root, made)| (root.clone(), made.clone()))
        };
        if let Some(found) = found {
            ROOT_HITS.fetch_add(1, Ordering::Relaxed);
            return Ok(found);
        }
        // Made outside the lock: a create is a driver call. Two threads that
        // meet on a new shape may both make one; the second is dropped, and
        // the bytes — what the library and the driver key on — are the same.
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        ROOT_CREATED.fetch_add(1, Ordering::Relaxed);
        let made = Arc::new(bytes.to_vec());
        let mut roots = roots();
        let map = roots.as_mut().expect("the map is made by `roots`");
        Ok(map.entry(made.as_ref().clone()).or_insert((root, made)).clone())
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;

    use super::*;
    use windows::core::Interface;

    /// A triangle mesh shader of our own and a pixel shader for it.
    const MS: &str = "struct V { float4 p : SV_Position; };
        [outputtopology(\"triangle\")] [numthreads(1, 1, 1)]
        void main(out vertices V v[3], out indices uint3 t[1]) {
            SetMeshOutputCounts(3, 1);
            v[0].p = float4(0, 0, 0, 1);
            v[1].p = float4(1, 0, 0, 1);
            v[2].p = float4(0, 1, 0, 1);
            t[0] = uint3(0, 1, 2);
        }";
    const PS: &str = "float4 main() : SV_Target { return 1; }";

    /// The PC `dxc.exe` (`VICTORY_PC_DXC`, else the newest Windows SDK's).
    fn dxc() -> Option<PathBuf> {
        let dxc = match std::env::var_os("VICTORY_PC_DXC") {
            Some(p) => PathBuf::from(p),
            None => victory_shader::emit::validator::Validator::default_path()?
                .with_file_name("dxc.exe"),
        };
        dxc.exists().then_some(dxc)
    }

    fn compile(dxc: &Path, dir: &Path, src: &str, profile: &str) -> Vec<u8> {
        let (hlsl, out) = (
            dir.join(format!("{profile}.hlsl")),
            dir.join(format!("{profile}.dxil")),
        );
        std::fs::write(&hlsl, src).unwrap();
        let status = std::process::Command::new(dxc)
            .args(["-nologo", "-T", profile, "-E", "main", "-Fo"])
            .arg(&out)
            .arg(&hlsl)
            .status()
            .unwrap();
        assert!(status.success(), "dxc failed for {profile}");
        std::fs::read(out).unwrap()
    }

    fn code(bytes: &[u8]) -> D3D12_SHADER_BYTECODE {
        D3D12_SHADER_BYTECODE {
            pShaderBytecode: bytes.as_ptr().cast(),
            BytecodeLength: bytes.len(),
        }
    }

    /// A mesh pipeline made once is loaded from the library the next time,
    /// not compiled again (a stream without a topology never loads).
    #[test]
    fn a_mesh_pipeline_loads_from_the_library() {
        let Some(host) = host() else {
            return; // no host GPU here
        };
        if !host.mesh_shaders() {
            eprintln!("no host mesh shader support; skipped");
            return;
        }
        let Some(dxc) = dxc() else {
            eprintln!("no PC dxc.exe; skipped");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let ms = compile(&dxc, dir.path(), MS, "ms_6_5");
        let ps = compile(&dxc, dir.path(), PS, "ps_6_5");
        let (root, root_blob) = root_signature(&host.device, &[], &[], false).unwrap();
        let mut g = GraphicsState::default();
        // This pipeline has only a color target, with no depth buffer.
        g.depth.DepthEnable = false.into();
        g.targets.NumRenderTargets = 1;
        g.targets.RTFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
        let name = super::super::library::name("mesh test", &root_blob, &[&ms, &ps], &g.describe());
        mesh_pipeline(&host.device, &root, code(&ms), code(&ps), &g, (g.depth, false), &name).expect("made");
        let loaded = super::super::library::LOADED.load(Ordering::Relaxed);
        mesh_pipeline(&host.device, &root, code(&ms), code(&ps), &g, (g.depth, false), &name).expect("made again");
        assert!(
            super::super::library::LOADED.load(Ordering::Relaxed) > loaded,
            "the mesh pipeline was compiled again instead of loaded from the library"
        );
    }

    /// What a refusal is read from: the combination D3D12 documents as
    /// invalid is named, and the fallback turns off exactly that, leaving
    /// the rest of the state as the title asked for it.
    #[test]
    fn a_refusal_names_what_is_invalid_and_the_fallback_turns_only_tests_off() {
        let mut g = GraphicsState::default();
        g.targets.NumRenderTargets = 1;
        g.targets.RTFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
        // The default state has the depth test on and names no DSV.
        let why = invalid_combinations(
            ("vertex", Some(8)),
            true,
            g.depth_format,
            &g.depth,
            g.sample_mask,
            g.samples,
            &g.targets,
        );
        assert_eq!(why, ["a depth test with no DSV format"]);
        assert!(depth_without_attachment(&g.depth, g.depth_format));
        let off = without_depth(g.depth);
        assert!(!off.DepthEnable.as_bool());
        assert_eq!(off.DepthWriteMask, D3D12_DEPTH_WRITE_MASK_ZERO);
        assert!(!off.StencilEnable.as_bool());
        assert_eq!(off.DepthFunc, g.depth.DepthFunc);
        assert_eq!(off.FrontFace.StencilFunc, g.depth.FrontFace.StencilFunc);
        // With a depth attachment of its own nothing is wrong and nothing
        // is turned off: the test reads the title's depth.
        g.depth_format = DXGI_FORMAT_D32_FLOAT;
        assert!(!depth_without_attachment(&g.depth, g.depth_format));
        assert!(
            invalid_combinations(
                ("vertex", Some(8)),
                true,
                g.depth_format,
                &g.depth,
                g.sample_mask,
                g.samples,
                &g.targets,
            )
            .is_empty()
        );
        // What a description cannot have at all is named as well.
        assert_eq!(
            invalid_combinations(
                ("vertex", None),
                false,
                g.depth_format,
                &g.depth,
                0,
                g.samples,
                &g.targets,
            ),
            [
                "no root signature",
                "no vertex shader",
                "a sample mask of zero"
            ]
        );
    }

    /// The `PSV0` the emitter writes: 48 bytes of runtime info (the stage at
    /// 24, the thread group at 36), then the bindings as a count, a stride
    /// and one record each.
    fn psv0_bytes(stage: u8, threads: [u32; 3], bindings: &[(u32, u32, u32)]) -> Vec<u8> {
        let mut info = [0u8; 48];
        info[24] = stage;
        for (k, n) in threads.into_iter().enumerate() {
            info[36 + 4 * k..40 + 4 * k].copy_from_slice(&n.to_le_bytes());
        }
        let mut out = 48u32.to_le_bytes().to_vec();
        out.extend(info);
        out.extend((bindings.len() as u32).to_le_bytes());
        if !bindings.is_empty() {
            out.extend(24u32.to_le_bytes());
            for &(kind, space, register) in bindings {
                for n in [kind, space, register, register, 11, 0] {
                    out.extend(n.to_le_bytes());
                }
            }
        }
        out
    }

    /// A `DXBC` container of these parts, as `victory_shader::emit` builds
    /// one: a header, the part table, then each part's header and data.
    fn container_bytes(parts: Vec<(&[u8; 4], Vec<u8>)>) -> Vec<u8> {
        let table = 0x20 + 4 * parts.len();
        let total = table + parts.iter().map(|(_, d)| 8 + d.len()).sum::<usize>();
        let mut out = Vec::with_capacity(total);
        out.extend(b"DXBC");
        out.extend([0u8; 16]);
        out.extend(1u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend((total as u32).to_le_bytes());
        out.extend((parts.len() as u32).to_le_bytes());
        let mut offset = table;
        for (_, d) in &parts {
            out.extend((offset as u32).to_le_bytes());
            offset += 8 + d.len();
        }
        for (fourcc, d) in &parts {
            out.extend(*fourcc);
            out.extend((d.len() as u32).to_le_bytes());
            out.extend(d);
        }
        out
    }

    /// A `DXIL` part: the 24-byte header the emitter writes (a dword size,
    /// the four-character code, then the bitcode's offset and length), then
    /// bitcode that only has to start with the magic.
    fn dxil_part(bitcode: &[u8]) -> Vec<u8> {
        let mut out = ((5u32 << 16) | 0x66).to_le_bytes().to_vec();
        out.extend((((24 + bitcode.len()) / 4) as u32).to_le_bytes());
        out.extend(b"DXIL");
        out.extend(0x106u32.to_le_bytes());
        out.extend(16u32.to_le_bytes());
        out.extend((bitcode.len() as u32).to_le_bytes());
        out.extend(bitcode);
        out
    }

    /// LLVM bitcode: the magic, then nothing the re-check reads.
    const BITCODE: [u8; 8] = [0x42, 0x43, 0xc0, 0xde, 0, 0, 0, 0];

    fn constants_param(space: u32, register: u32, dwords: u32) -> D3D12_ROOT_PARAMETER1 {
        D3D12_ROOT_PARAMETER1 {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                Constants: D3D12_ROOT_CONSTANTS {
                    ShaderRegister: register,
                    RegisterSpace: space,
                    Num32BitValues: dwords,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        }
    }

    fn uav_param(space: u32, register: u32) -> D3D12_ROOT_PARAMETER1 {
        D3D12_ROOT_PARAMETER1 {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_UAV,
            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                    ShaderRegister: register,
                    RegisterSpace: space,
                    Flags: D3D12_ROOT_DESCRIPTOR_FLAG_NONE,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        }
    }

    /// A translated compute shader reading these user-data dwords and root
    /// UAVs.
    fn compute_shader(user_data: u32, uavs: &[u8]) -> HostShader {
        HostShader {
            stage: victory_shader::HwStage::Cs,
            dxil: Vec::new(),
            user_data: (0..user_data)
                .map(|index| victory_shader::emit::UserData {
                    index,
                    slot: 0,
                    root: victory_shader::RootType::Constants,
                    dword: 0,
                })
                .collect(),
            samplers: Vec::new(),
            static_samplers: Vec::new(),
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
            root_uavs: uavs.to_vec(),
            root_indexed_views: Vec::new(),
        }
    }

    /// A mesh `PSV0` names its thread group and the counts it declares, which
    /// are what `emit::lower`'s `mesh_prologue` sets every group's counts to
    /// (`MESH_DECLARED` prices a frame's promised outputs with them).
    #[test]
    fn a_mesh_psv0_declares_its_counts_and_thread_group() {
        let mut bytes = psv0_bytes(PSV_MESH, [128, 1, 1], &[]);
        // The declared counts follow the group-shared bytes, 16 bits each.
        bytes[4 + 12..4 + 14].copy_from_slice(&128u16.to_le_bytes());
        bytes[4 + 14..4 + 16].copy_from_slice(&96u16.to_le_bytes());
        let psv = psv0(&bytes).expect("a mesh PSV0");
        assert_eq!(psv.stage, PSV_MESH);
        assert_eq!(psv.threads, [128, 1, 1]);
        assert_eq!(psv.declared, (128, 96), "its counts");
        // A stage that is not mesh declares none, whatever its bytes hold.
        let other = psv0(&{
            let mut bytes = psv0_bytes(PSV_COMPUTE, [8, 8, 1], &[]);
            bytes[4 + 12..4 + 14].copy_from_slice(&64u16.to_le_bytes());
            bytes
        })
        .expect("a compute PSV0");
        assert_eq!(other.declared, (0, 0), "a compute shader declares no counts");
    }

    /// A compute refusal names the fields it read and the values of the bytes
    /// it was given, and says `no cause known` when none of them is invalid —
    /// it used to print an empty list that named nothing.
    #[test]
    fn a_compute_refusal_names_its_fields_or_says_no_cause_is_known() {
        let psv = psv0_bytes(5, [8, 8, 1], &[(2, 0, 0), (7, 1, 7)]);
        let container = container_bytes(vec![
            (b"SFI0", vec![0u8; 8]),
            (b"PSV0", psv),
            (b"DXIL", dxil_part(&BITCODE)),
        ]);
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            CS: code(&container),
            ..Default::default()
        };
        // The signature of a shader that reads u7 and 16 dwords of user data.
        let params = [constants_param(0, 0, 16), uav_param(1, 7)];
        let shader = compute_shader(16, &[7]);
        let root: &[u8] = &[0xc0, 0x0f];
        let line = compute_refusal(&desc, Some(root), &params, 0, Some(&shader));
        assert!(line.contains("cause=no cause known ("), "{line}");
        assert!(line.contains(&format!("cs={}", container.len())), "{line}");
        assert!(
            line.contains(&format!(
                "root_bytes=2 root_hash={}",
                &blake3::hash(root).to_hex()[..8]
            )),
            "{line}"
        );
        assert!(
            line.contains("params=2 samplers=0 node_mask=0 flags=0 cached_pso=0"),
            "{line}"
        );
        assert!(line.contains("root_flags=0xc00"), "{line}");
        assert!(
            line.contains(&format!(
                "container=v1.0 of {} bytes parts=[SFI0:8,PSV0:",
                container.len()
            )),
            "{line}"
        );
        assert!(line.contains("psv0=stage=5 threads=8x8x1"), "{line}");
        assert!(line.contains("bindings=[cbv 0:0,uav 1:7]"), "{line}");
        assert!(
            line.contains("declared=user_data=16dw cbvs=[] srvs=[] uavs=[7]"),
            "{line}"
        );
        assert!(
            line.contains("signature=[constants 0:0 16dw,uav 1:7]"),
            "{line}"
        );
        assert!(!line.contains("suspected="), "{line}");
    }

    /// A resource the code declares that no root parameter names is a
    /// description D3D12 rejects, and the refusal names it by kind, space and
    /// register instead of leaving the list empty.
    #[test]
    fn a_compute_refusal_names_a_resource_no_root_parameter_names() {
        let psv = psv0_bytes(5, [8, 8, 1], &[(2, 0, 0), (7, 1, 7)]);
        let container = container_bytes(vec![(b"PSV0", psv), (b"DXIL", dxil_part(&BITCODE))]);
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            CS: code(&container),
            ..Default::default()
        };
        let line = compute_refusal(&desc, Some(&[0; 4][..]), &[constants_param(0, 0, 16)], 0, None);
        assert!(
            line.contains("its PSV0 declares uav space 1 register 7 and no root parameter names it"),
            "{line}"
        );
        assert!(!line.contains("no cause known"), "{line}");
    }

    /// A zero thread group, a `PSV0` built for another stage, a missing part
    /// and a `DXIL` part whose header disagrees with the container are each
    /// named, and a description with no signature says so first.
    #[test]
    fn a_compute_refusal_names_a_zero_thread_group_a_wrong_stage_and_a_truncated_part() {
        let psv = psv0_bytes(1, [0, 0, 0], &[]);
        let container = container_bytes(vec![(b"PSV0", psv)]);
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            CS: code(&container),
            ..Default::default()
        };
        let line = compute_refusal(&desc, Some(&[0; 4][..]), &[constants_param(0, 0, 4)], 0, None);
        assert!(
            line.contains("its PSV0 declares stage 1 where the pipeline is stage 5"),
            "{line}"
        );
        assert!(
            line.contains("its PSV0 declares a thread group of 0x0x0"),
            "{line}"
        );
        assert!(line.contains("its container has no DXIL part"), "{line}");
        let mut dxil = dxil_part(&BITCODE);
        dxil[20..24].copy_from_slice(&100u32.to_le_bytes());
        let psv = psv0_bytes(5, [1, 1, 1], &[]);
        let container = container_bytes(vec![(b"PSV0", psv), (b"DXIL", dxil)]);
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            CS: code(&container),
            ..Default::default()
        };
        let line = compute_refusal(&desc, None, &[], 0, None);
        assert!(
            line.contains(
                "cause=no root signature; its DXIL part declares 100 bytes of bitcode and holds 8"
            ),
            "{line}"
        );
        // A container that is not one at all is named as well.
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            CS: code(&[0u8; 64]),
            ..Default::default()
        };
        let line = compute_refusal(&desc, Some(&[0; 4][..]), &[], 0, None);
        assert!(
            line.contains("its container is not readable: not a DXBC container"),
            "{line}"
        );
    }

    const VS: &str = "float4 main(uint i : SV_VertexID) : SV_Position {
        float2 p = float2((i << 1) & 2, i & 2);
        return float4(p * 2.0 - 1.0, 0.5, 1.0);
    }";

    /// A graphics pipeline with `DepthBoundsTestEnable` loads back from the
    /// pipeline library and culls or passes fragments according to
    /// `OMSetDepthBounds` (including across a command-list cut).
    #[test]
    fn depth_bounds_pipeline_loads_and_filters_draws() {
        let Some(host) = host() else {
            return;
        };
        if !host.depth_bounds() {
            eprintln!("no host depth bounds support; skipped");
            return;
        }
        let Some(dxc) = dxc() else {
            eprintln!("no PC dxc.exe; skipped");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let vs = compile(&dxc, dir.path(), VS, "vs_6_5");
        let ps = compile(&dxc, dir.path(), PS, "ps_6_5");
        let (root, root_blob) = root_signature(&host.device, &[], &[], false).unwrap();

        let mut g = GraphicsState::default();
        // Subobject 21 (`DEPTH_STENCIL1`) with `DepthBoundsTestEnable = true`,
        // depth test itself off so only depth bounds gates the draw.
        #[repr(C, align(8))]
        struct Sub21(u32, D3D12_DEPTH_STENCIL_DESC1);
        let sub = Sub21(
            21,
            D3D12_DEPTH_STENCIL_DESC1 {
                DepthEnable: false.into(),
                DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ZERO,
                DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                DepthBoundsTestEnable: true.into(),
                ..Default::default()
            },
        );
        // SAFETY: `sub` is a complete subobject 21.
        unsafe { g.read(21, (&raw const sub).cast()) };
        assert!(g.depth_bounds);
        assert!(g.describe().contains("depth-bounds"));
        g.rasterizer.CullMode = D3D12_CULL_MODE_NONE;
        g.targets.NumRenderTargets = 1;
        g.targets.RTFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
        g.depth_format = DXGI_FORMAT_D32_FLOAT;

        let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            // SAFETY: borrowed pointer, never released by the descriptor.
            pRootSignature: unsafe { std::mem::transmute_copy(&root) },
            VS: code(&vs),
            PS: code(&ps),
            BlendState: g.blend,
            SampleMask: g.sample_mask,
            RasterizerState: g.rasterizer,
            DepthStencilState: g.depth,
            IBStripCutValue: g.strip_cut,
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            RTVFormats: g.targets.RTFormats,
            DSVFormat: g.depth_format,
            SampleDesc: g.samples,
            ..Default::default()
        };
        let name = super::super::library::name(
            "graphics depth-bounds test",
            &root_blob,
            &[&vs, &ps],
            &g.describe(),
        );
        let pso = graphics_depth_bounds_pipeline(&host.device, &root, &desc, g.depth1(), &name)
            .expect("depth-bounds PSO made");
        let loaded = super::super::library::LOADED.load(Ordering::Relaxed);
        graphics_depth_bounds_pipeline(&host.device, &root, &desc, g.depth1(), &name)
            .expect("depth-bounds PSO made again");
        assert!(
            super::super::library::LOADED.load(Ordering::Relaxed) > loaded,
            "the depth-bounds pipeline was compiled again instead of loaded from the library"
        );

        // Execute two draws against a depth buffer cleared to 0.5:
        // 1) bounds [0.6, 1.0] -> rejected (target stays 0)
        // 2) bounds [0.4, 0.6] replayed onto a fresh HostList -> passes (target becomes 255)
        let make_tex = |format, flags, state, clear: D3D12_CLEAR_VALUE| {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let rd = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 4,
                Height: 4,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: flags,
                ..Default::default()
            };
            let mut out: Option<ID3D12Resource> = None;
            // SAFETY: valid heap and resource descriptions.
            unsafe {
                host.device
                    .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &rd, state, Some(&clear), &mut out)
                    .unwrap();
            }
            out.unwrap()
        };
        let color = make_tex(
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            D3D12_CLEAR_VALUE {
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                Anonymous: D3D12_CLEAR_VALUE_0 { Color: [0.0; 4] },
            },
        );
        let depth = make_tex(
            DXGI_FORMAT_D32_FLOAT,
            D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
            D3D12_CLEAR_VALUE {
                Format: DXGI_FORMAT_D32_FLOAT,
                Anonymous: D3D12_CLEAR_VALUE_0 {
                    DepthStencil: D3D12_DEPTH_STENCIL_VALUE {
                        Depth: 0.5,
                        Stencil: 0,
                    },
                },
            },
        );
        let readback = {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_READBACK,
                ..Default::default()
            };
            let rd = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 1024,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut out: Option<ID3D12Resource> = None;
            // SAFETY: valid readback buffer description.
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &rd,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        None,
                        &mut out,
                    )
                    .unwrap();
            }
            out.unwrap()
        };

        let run_draw = |min: f32, max: f32| -> u8 {
            let mut state = crate::draw::State::default();
            let first_list = host.new_list().unwrap();
            state.set_depth_bounds(&first_list, min, max);
            first_list.close();
            // Replay onto a second host list, exercising `State::replay`.
            let list = host.new_list().unwrap();
            state.replay(&list);
            let rtv = host.rtv(Some(&color), None);
            let dsv = host.dsv(&depth, None);
            let vp = D3D12_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: 4.0,
                Height: 4.0,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            let rect = windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: 4,
                bottom: 4,
            };
            // SAFETY: open command list, live resources and views.
            unsafe {
                list.list.ClearRenderTargetView(rtv, &[0.0; 4], None);
                list.list.ClearDepthStencilView(dsv, D3D12_CLEAR_FLAG_DEPTH, 0.5, 0, None);
                list.list.OMSetRenderTargets(1, Some(&rtv), false, Some(&dsv));
                list.list.RSSetViewports(&[vp]);
                list.list.RSSetScissorRects(&[rect]);
                list.list.SetGraphicsRootSignature(&root);
                list.list.SetPipelineState(&pso);
                list.list
                    .IASetPrimitiveTopology(windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                list.list.DrawInstanced(3, 1, 0, 0);
                list.list.ResourceBarrier(&[crate::host::transition(
                    &color,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                let dst = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&readback),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                            Offset: 0,
                            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                                Width: 4,
                                Height: 4,
                                Depth: 1,
                                RowPitch: 256,
                            },
                        },
                    },
                };
                let src = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&color),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
                };
                list.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
                list.list.ResourceBarrier(&[crate::host::transition(
                    &color,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                )]);
            }
            list.close();
            host.execute(&[&list]);
            let mut ptr = std::ptr::null_mut();
            // SAFETY: `execute` waited for the GPU; readback holds 1024 bytes.
            unsafe {
                readback.Map(0, None, Some(&mut ptr)).unwrap();
                let pixel = *ptr.cast::<u8>();
                readback.Unmap(0, None);
                pixel
            }
        };

        assert_eq!(run_draw(0.6, 1.0), 0, "depth 0.5 outside [0.6, 1.0] must be rejected");
        assert_eq!(run_draw(0.4, 0.6), 255, "depth 0.5 inside [0.4, 0.6] must pass");
    }

    /// One bounded root view, as `HostPipeline::new` builds it.
    fn view(slot: u8, visibility: D3D12_SHADER_VISIBILITY) -> Param {
        Param {
            key: ParamKey {
                visibility: visibility.0,
                kind: Kind::Srv,
                space: ROOT_CBV_SPACE,
                register: u32::from(slot),
                size: 1,
            },
            desc: D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_SRV,
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                        ShaderRegister: u32::from(slot),
                        RegisterSpace: ROOT_CBV_SPACE,
                        Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                    },
                },
                ShaderVisibility: visibility,
            },
        }
    }

    /// A stage's user data, as `HostPipeline::new` builds it.
    fn constants(dwords: u32, visibility: D3D12_SHADER_VISIBILITY) -> Param {
        Param {
            key: ParamKey {
                visibility: visibility.0,
                kind: Kind::Constants,
                space: USER_DATA_SPACE,
                register: 0,
                size: dwords,
            },
            desc: D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    Constants: D3D12_ROOT_CONSTANTS {
                        ShaderRegister: 0,
                        RegisterSpace: USER_DATA_SPACE,
                        Num32BitValues: dwords,
                    },
                },
                ShaderVisibility: visibility,
            },
        }
    }

    fn keys(params: &[Param]) -> Vec<ParamKey> {
        params.iter().map(|param| param.key).collect()
    }

    /// One signature per binding shape: two pipelines whose root views are
    /// declared in another order make one block (the parameters are named by
    /// their register, and each stage reaches its own through the indices
    /// `canonical_params` hands back), and a pipeline whose bindings really
    /// differ keeps a signature of its own.
    #[test]
    fn one_root_signature_per_binding_shape() {
        let mut one = vec![view(3, D3D12_SHADER_VISIBILITY_VERTEX), view(1, D3D12_SHADER_VISIBILITY_VERTEX), constants(8, D3D12_SHADER_VISIBILITY_VERTEX)];
        let mut two = vec![constants(8, D3D12_SHADER_VISIBILITY_VERTEX), view(1, D3D12_SHADER_VISIBILITY_VERTEX), view(3, D3D12_SHADER_VISIBILITY_VERTEX)];
        let place = canonical_params(&mut one);
        canonical_params(&mut two);
        assert_eq!(keys(&one), keys(&two), "the same bindings, in another order, are one shape");
        // The stage's own indices follow: its root view 3 is the third
        // parameter now, its user data the first.
        assert_eq!(keys(&one)[place[0] as usize], view(3, D3D12_SHADER_VISIBILITY_VERTEX).key);
        assert_eq!(keys(&one)[place[2] as usize], constants(8, D3D12_SHADER_VISIBILITY_VERTEX).key);
        // A root view more is another shape.
        let mut three = vec![
            view(3, D3D12_SHADER_VISIBILITY_VERTEX),
            view(1, D3D12_SHADER_VISIBILITY_VERTEX),
            constants(8, D3D12_SHADER_VISIBILITY_VERTEX),
            view(0, D3D12_SHADER_VISIBILITY_VERTEX),
        ];
        canonical_params(&mut three);
        assert_ne!(keys(&one), keys(&three));
        // Another stage's view of the same register keeps its own parameter:
        // the draw sets each stage's from that stage's binding.
        let mut both = vec![view(3, D3D12_SHADER_VISIBILITY_VERTEX), view(3, D3D12_SHADER_VISIBILITY_PIXEL)];
        canonical_params(&mut both);
        assert_eq!(both.len(), 2);
        assert_ne!(both[0].key.visibility, both[1].key.visibility);
    }

    fn sampler(register: u32, filter: D3D12_FILTER, visibility: D3D12_SHADER_VISIBILITY) -> D3D12_STATIC_SAMPLER_DESC {
        D3D12_STATIC_SAMPLER_DESC {
            Filter: filter,
            AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            ComparisonFunc: D3D12_COMPARISON_FUNC_ALWAYS,
            ShaderRegister: register,
            RegisterSpace: SAMPLER_SPACE,
            ShaderVisibility: visibility,
            ..Default::default()
        }
    }

    /// Two stages that name the same sampler at one register share one static
    /// sampler, visible to both; two that name different samplers there keep
    /// one each, which is legal and which the console's shaders use.
    #[test]
    fn one_static_sampler_per_register_when_the_stages_agree() {
        let (point, linear) = (D3D12_FILTER_MIN_MAG_MIP_POINT, D3D12_FILTER_MIN_MAG_MIP_LINEAR);
        let (vs, ps) = (D3D12_SHADER_VISIBILITY_VERTEX, D3D12_SHADER_VISIBILITY_PIXEL);
        let one = canonical_samplers(&[sampler(0, point, vs), sampler(0, point, ps)]);
        assert_eq!(one.len(), 1, "one register, one sampler, visible to both stages");
        assert_eq!(one[0].ShaderVisibility, D3D12_SHADER_VISIBILITY_ALL);
        let both = canonical_samplers(&[sampler(0, point, vs), sampler(0, linear, ps)]);
        assert_eq!(both.len(), 2, "different samplers at one register stay apart");
        assert!(both.iter().any(|s| s.ShaderVisibility == vs));
        assert!(both.iter().any(|s| s.ShaderVisibility == ps));
    }

    /// A shape met again is not created again: two pipelines whose root views
    /// are declared in another order are handed the same root signature, and
    /// the same bytes into the library's name, and a pipeline whose bindings
    /// differ is handed another.
    #[test]
    fn a_shape_met_again_is_not_created_again() {
        let Some(host) = host() else {
            return; // no host GPU here
        };
        let mut one = vec![view(3, D3D12_SHADER_VISIBILITY_VERTEX), view(1, D3D12_SHADER_VISIBILITY_VERTEX)];
        let mut two = vec![view(1, D3D12_SHADER_VISIBILITY_VERTEX), view(3, D3D12_SHADER_VISIBILITY_VERTEX)];
        canonical_params(&mut one);
        canonical_params(&mut two);
        let descs = |params: &[Param]| params.iter().map(|param| param.desc).collect::<Vec<_>>();
        let Ok((root, blob)) = root_signature(&host.device, &descs(&one), &[], false) else {
            eprintln!("the host refused a root signature; skipped");
            return;
        };
        let Ok((same, blob_again)) = root_signature(&host.device, &descs(&two), &[], false) else {
            eprintln!("the host refused a root signature; skipped");
            return;
        };
        assert_eq!(root.as_raw(), same.as_raw(), "one root signature for one binding shape");
        assert_eq!(blob.as_ref(), blob_again.as_ref(), "and one block of bytes, the library's key");
        let mut other = one.clone();
        other.push(view(0, D3D12_SHADER_VISIBILITY_VERTEX));
        canonical_params(&mut other);
        let Ok((different, _)) = root_signature(&host.device, &descs(&other), &[], false) else {
            eprintln!("the host refused a root signature; skipped");
            return;
        };
        assert_ne!(root.as_raw(), different.as_raw(), "another binding shape is another signature");
    }
}
