# Ray tracing in this fork: what the title does and what has been built

Status, 2026-11-26: **nothing is lit by ray tracing yet.** Most of the
machinery translates and is unit-tested. With
`VICTORY_BVH_BUILDER=1 VICTORY_FIRSTLANE_OWN=1` the builder dispatches in a
real run and the title reaches the world, then faults about 20 s later
(§ The first world run). Everything here is off by default.

## What the title does

The title's indirect light (probes, bounce, reflections) and the
shadows of its lamps and sun are ray traced. Without them, anything the sun
does not hit directly is black: interiors, the shadowed side of a face, rooms
lit only by lamps.

The title does not use a ray-tracing API:

- Traversal is ordinary compute-shader code: a per-thread stack in shared
  memory, and loads through raw 40-bit addresses of title memory.
- The static world's acceleration structures ship pre-built in the title's
  data, in the console GPU's native node format (64-byte nodes: box nodes with
  four children and 16-bit float bounds, 128-byte box nodes with 32-bit float
  bounds, triangle nodes, instance nodes). They are unpacked on the GPU.
- A set of builder programs (from the title's `XboxBvhBuilder.dll`, 52 compute
  programs) builds and refits the dynamic structures on the GPU every frame.
- Only the per-node test is hardware: the instructions
  `image_bvh64_intersect_ray` and `image_bvh_intersect_ray`. One call tests a
  ray against a box node's four children (returning the hit children, nearest
  first) or against a triangle node (returning the intersection's numerator,
  denominator and barycentric numerators).

So mapping this onto the host's own ray tracing (DXR) is not practical: the
layer would have to know the geometry behind structures the title never
describes to it, and the traversal loops live in 64 of the title's own
programs. This fork runs the title's builder and traversal as translated
shaders and emulates the one instruction.

## What this fork adds

1. **The builder programs translate** (49 of 52; all that the title creates in
   the scenes tested). They pass buffer addresses in a spilled parameter table
   rather than in descriptors. Added: the spilled user-data table, root
   unordered-access views (translator and host binding), byte and 16-bit buffer
   accesses, and several control-flow shapes.
2. **Title memory by raw address from shaders.** A directory with one entry per
   64 KiB of title address space leads a shader to the layer's host copy of
   that memory; loads, stores, atomic add/sub and float min/max go through it.
   Before a dispatch the host makes ready the memory its root views and root
   constants name.
3. **The ray test, emulated in DXIL.** Four slab tests and a sorting network
   for box nodes; a watertight ray/triangle test for triangle nodes; both forms
   of the instruction. A plain Rust reference lives in
   `crates/victory-shader/src/bvh.rs`; tests run the emitted DXIL on the GPU
   over 311 ray/node cases and compare with the reference bit for bit.
4. **The trace and shading programs translate** (59 of 64 trace programs, and
   the irradiance-cache shading program). This needed lane masks treated as
   plain numbers on some paths, and exact forms for programs written for
   64-lane waves running on a GPU with 32-lane waves.
5. **A safety rule on the host side.** The builder's stages run only while
   every builder pipeline the title has created has a host pipeline; otherwise
   none run (`VICTORY_BVH_BUILDER=1` asks for it; `VICTORY_BVH_QUARANTINE=1`
   also keeps the builder's stores out of title memory, for a first test). A
   partly working builder wrote garbage bounds into title memory and crashed
   the title.

## The first world run

`run-20261125-224639.log` (and `run-20261125-225835.log`): with
`VICTORY_BVH_BUILDER=1 VICTORY_FIRSTLANE_OWN=1` the host logs
`pipeline: the ray-tracing structure builder runs: all of its programs have
host pipelines pipelines=53` at 22:49:51 and 23:02:42, and the builder's
dispatches run from there. Before that line the two runs held 13–24 fps
(48–74 ms a frame); the host report after it reads 0.1 fps, 12169.4 ms a frame,
of which 8808.2 ms are copies of 2002.7 MB of title memory (22:50:22), and
2.2 fps, 452.5 ms a frame (23:03:00).

Both runs then fault in the title's own code: exception `0xc0000005`, a **read
of `0xffffffffffffffff`** at `victory-image-*.dll+0x4f38ba5` (dump
`t:\fafdd3f0-e060-4a19-8a48-b6c2089c2b1b.dmp`, 32,377,494 B, title frame 1544,
`Prestream`; the second run's dump was written empty, title frame 2711,
`fbk_stry000_FBK_STRY000_INTRO`). A faulting address of −1 is an index of −1
(`count - 1` with a count of 0) or a `0xFFFFFFFF` sentinel read as one; the
title's image carries no symbols here, so which of the two is **not measured**.
This is a **different site** from the three crashes this title has without the
builder (all `0xc0000005`, a *write* to `0x0` at `victory-image-*.dll+0x23cf683`:
dumps `7f759f07`, `d1c56668`, `fb8d7775`), so the builder adds a fault instead
of explaining that one.

The three builder programs that `VICTORY_FIRSTLANE_OWN=1` is needed for:
`XboxBvhBuilder.dll@0x99160 #dc78c5de69c7c155`, `@0x9a830 #322ed4ad832bdea6`
and `@0x9be80 #d441b097704ab616`, each refused as `a first-lane read where not
every lane runs`. The read is `v_readfirstlane_b32 s17, v8` at 0x1d4, inside
`v_cmpx_ne_u32 exec, 0, v7` (`v7 = v0 >> 6`), in a loop whose exit compares the
scalar `s18` with `v7` — one iteration per console wave, and every lane of it
computes the same `v8` (`ds_read_b32` at the scalar address `s16`, plus the
scalar `s17`). The host wave's own first active lane is therefore the console's
for these three. `v_readfirstlane_b32` has no such argument in general: the
console's first lane of a divergent 64-lane wave may be in the other half, and
only a group-wide "every lane runs" (or a value the analysis proves alike in
every lane of the console wave) makes a half's own lane the right answer.

## Known gaps

- With `VICTORY_BVH_BUILDER=1` alone the builder stays off in a real run: 3 of
  its 53 pipelines have none, and its stages run all or none. With
  `VICTORY_FIRSTLANE_OWN=1` it runs, and what it then costs and how the title
  faults are in § The first world run.
- Nothing yet shows the light: with the builder on, the run faults before a
  frame in the yard can be judged.
- One shadow-mask trace program (`CS_TraceShadowRays_FullAdaptive`) does not
  translate yet: at its 0xae80 a lane mask holding lanes elsewhere is read as
  data. The other four trace programs do translate and validate.
- An access to a title page that has no host copy is lost (a read gives zero, a
  store is dropped).
- Not publicly documented, and assumed: the instruction's rounding, the order
  of two boxes entered at the same distance, and the result for a ray exactly
  through an edge.
- Cost on the GPU is unmeasured.

## Where to look

- `crates/victory-shader/src/bvh.rs`: the reference for the node test.
- `crates/victory-shader/src/emit/lower.rs`: the lowering of the two
  instructions and of memory by address (`Lower::memory`).
- `crates/victory-shader/src/resource.rs`: address tracking for descriptors
  and raw addresses.
- `crates/victory-gpu/src/pipeline.rs`, `draw.rs`: the builder rule and the
  host side of root views and title memory.
