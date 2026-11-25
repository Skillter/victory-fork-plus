# crates/victory-shader — GFX10 to DXIL translator

Largest crate. Change it with the pipeline order in mind (`emit::translate`):

1. `dxbc::Container::parse` — Xbox DXBC parts.
2. `native::native_programs` — `XDAT` table, zlib-unpacked GFX10 code.
3. `gfx10` — `Instruction::fields`, opcode and field tables, `sp3` printer.
4. `ir::build` (with `spill::build` for spilled user data) — SSA, one
   instruction per machine instruction.
5. `structure::structure` — EXEC becomes per-lane booleans; `Bools` is a BDD.
6. `resource::track` — descriptors and raw addresses to root parameters.
7. `io::raise` — signatures, interpolation and exports.
8. `emit::lower::lower` — the lowering state machine (largest file).
9. `emit::shader`, `emit::bitcode`, `emit::container` — LLVM 3.7 bitcode,
   DXIL part and signature chunks.
10. `emit::bitcode::opt::Module::optimise` — clean-up passes.

## Adding or fixing an instruction

- Decode metadata: `gfx10/opcodes.rs`; operand layout: `gfx10/fields.rs`.
- Semantics: `emit::lower`. Return `ShaderError::Unsupported { offset, what }`
  for a valid construct we do not model. An instruction with no arm falls
  through as `Err(<mnemonic>)`.
- Add a case to `emit::valu_tests` (synthetic shader, validated with
  `dxil.dll`, run on GPU and WARP) or the relevant `*_tests` module.
- A packed operation encoded as VOP2 (`v_pk_fmac_f16`, opcode table `VOP2`)
  has no half selectors: each half reads its own half (`packed_src`). Only a
  VOP3P encoding carries `opsel`/`opsel_hi`.
- GFX10's DOT instructions treat a byte of `0x80` as absent, so that pair's
  product is zero rather than `-128 * x` (`emit::lower::dot_byte`).
- Where the accumulator lives follows the encoding, not the mnemonic:
  `v_dot4c_i32_i8` (VOP2) adds to its destination, `v_dot4_i32_i8` (VOP3P)
  adds its third source.
- Keep `what` strings stable and specific; logs and tests match on them.

## Wave size

64-lane console programs stay 64 only when lanes genuinely interact across
halves (`v_mbcnt`, lane exchange, masks used as data, shared LDS without
guards). Otherwise they become 32-lane. `VICTORY_WAVE_WIDE=1` opts into the
riskier widening retry; `VICTORY_HOST_WAVE=32` and `VICTORY_VIEW_LIMIT` let
`victory translate` emulate a smaller host. `emit::lanes` holds that analysis
(`wave_local`, `lane_uniform`, `lds_meetings`) and the per-block "every lane
runs this" scan (`all_lane_blocks`), which `emit::lower` re-exports; `lower.rs`
is near its 512 KiB policy limit, so new analysis belongs in its own module.

## Mesh output counts

- `SetMeshOutputCounts` is one call per program, before every output store:
  `emit::counts::output_counts` follows the console's request
  (`s_sendmsg GS_ALLOC_REQ`, `m0 = vertices | primitives << 12`) back to the
  two `v_readfirstlane_b32` values that pack it, and the call lands right
  after the later of the two reads (`counts_at`), not at the end of their
  block. A request it cannot follow (two of them, counts in different blocks,
  behind a guard, not run by every lane, a store in their block before them,
  or not dominating every store) promises the declared maxima, as the
  prologue always did.
- `VICTORY_MESH_COUNTS=max|zero` restores the maxima, or promises nothing.
  Both change a translation: keep them in `victory-gpu::shader_cache`'s
  switch list.
- The cull `emitIndices` writes stay, bounded by the promised primitive count
  (`IULT index, p` on every pass when the count is a value, on the last pass
  only against the constants): a promised slot holds its cull value until the
  title stores over it, which a mine emitted before the title's own `exp prim`
  would not.

## Optimiser

Pass bits are `DEAD | FOLD | REUSE | FACTS | LOADS | FIRST` in
`emit::bitcode::opt`; `VICTORY_SHADER_OPT` (read in `emit`) selects them.
Floating-point folds return only bits that nearest, up, down and toward-zero
all agree on; anything else stays unfolded.

## Tests

- `emit::*_tests` are `#[cfg(all(test, windows))]` and skip without a GPU,
  `dxil.dll` or DXC. Never make them fail hard on a missing device.
- `diff_tests` compares console-DXC input against PC-DXC output;
  `intersect_tests` compares emitted DXIL against `bvh`.
- `bvh` is the reference for `image_bvh*_intersect_ray`; change it and
  `emit/intersect` together.
- Synthetic shaders only. No title data, ever.

## Traps

- `emit::lower::take_needs` is thread-local state carried between programs.
- `resource::Tracer::patched` models only a four-dword buffer `V#`, gated on
  `Role::Resource`; a patched image `T#` (eight dwords) or sampler `S#` is
  refused as "a descriptor changed after loading", never read as a buffer.
- `fspx::Reflect` reads VGPR, LDS and thread-id bounds from the code because
  the package's `COMPUTE_PGM_RSRC2` layout is not established: a dynamic `DS`
  address adds `LDS_DYNAMIC_MARGIN` on top of the immediate offsets.
- `HostShader` is the output boundary; `victory-gpu`'s shader cache encodes
  its fields. Coordinate any layout change.
- `emit::host` compiles only with the `host` feature and creates real D3D12
  pipelines; keep it out of the library's default path.
