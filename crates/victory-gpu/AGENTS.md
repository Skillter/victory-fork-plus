# crates/victory-gpu — D3D12.X on host D3D12

## Invariants

- The title's GPU addresses are host CPU addresses. Never introduce a second
  address space: `GetGPUVirtualAddress`, heap pointers and descriptor handles
  all return real pointers.
- Command-list put limits stay at zero, so every inlined console command
  becomes a method call. Do not implement packet parsing.
- Console objects are static `Object` plus fixed `Table<N>` vtables, longer
  than any known interface. An unknown slot must reach `missing_table` and
  return `E_NOTIMPL`; never let it fall through.
- Interface layout is inferred: the device interface version and whether the
  title created `ID3D12Device11` decide shifted slots. Preserve both paths.

## Unified memory (the core design)

- Upload: only ranges a submission is known to read are refreshed, from
  64 KiB pages tracked by `GetWriteWatch` (`watch`); guard pages cover views
  that cannot be write-watched.
- Readback: declare shader writes (`Shadow::gpu_writes`), snapshot before
  submission (`Shadow::writing`), then compare per 64 KiB piece and merge only
  differing bytes (`Shadow::copy_back_with`). Title threads write the same
  buffers concurrently, so a blind write-back corrupts title records.
- `VICTORY_CARD_BUFFERS*` adds a card-side copy and finds writes with a
  compute hash per piece; `VICTORY_INPLACE*` places a buffer in an
  `OpenExistingHeapFromFileMapping` heap. Both are off by default.
- Cross-crate contract: CPU writes into mapped title memory call
  `kernel_will_write` first; page remapping holds `title_memory_remapping()`.

## Descriptors

- Host heaps mirror the title's slots and are re-made when a slot is written.
- Per-draw tables come from the dynamic ring in `descriptor::host`; a request
  that cannot fit must be refused, never served from unbuilt slots.
- A command list bumps through a run it holds (`DynamicList`), given back whole
  at submission; the ring is never walked per table.
- Root arguments are cached per list (`draw::RootArgs`) and re-issued only when
  the resolved value changes; a root-signature change or a new list drops them.
- The capped heap points past-the-end reads at a null slot for a reason: a
  real read past the heap end hung an NVIDIA GPU.
- A buffer view of no elements is the console's for the rest of its buffer:
  `descriptor::host::view_count` works the length out and
  `descriptor::host::buffer_copy` refreshes and writes back that rest. A view
  left null reads as zeros.
- A view the host still refuses is counted per reason and logged once with its
  reason and its buffer (`descriptor::host::refused_census`).

## Pipelines and signatures

- Host device: feature level 12_2, Agility SDK 1.614.1 when present, 12_0
  fallback. `d3d12_factory` and `ApplyToGlobalState` are both required.
- Our root signatures are v1.1 and keep the heap-direct-index flags; the
  title's own go through the v1.0 serializer, which refuses them, so `root`
  drops exactly `0x400|0x800` there.
- A root signature is built once per binding shape, not once per pipeline:
  `host::canonical_params` orders the parameters by their binding and
  `host::root_signature` caches the created object and its bytes by them, so
  a cosmetic slot-order difference is neither a new library entry nor a new
  driver compile. Each stage still reaches its own root views, through
  indices `canonical_params` returns.
- Translated shaders are cached by the translator's fingerprint (`build.rs`
  hashes `victory-shader`'s source, the cache encoding, `Cargo.lock` and the
  toolchain) plus the switches that change output, so a rebuild keeps them;
  `VICTORY_SHADER_CACHE_KEY` starts cold when one looks stale.
- The swap chain is sized from the window's client rectangle, never the frame,
  and the frame is drawn into it by the present pass with a viewport that keeps
  its shape (letterbox): a Win32 flip chain's `DXGI_SCALING_STRETCH` cannot.
- The pipelines folder keeps the live library and one older generation.
  `library::prune` drops a `.tmp` that no save finished and any generation
  unread for 30 days, and never the file this run opened or mapped.
- `pipeline census` prints at the save a run ends with (every save in a
  diagnostics build); `bytes_per_pipeline_made` is the library's growth over
  the pipelines this run made, never the whole library over this run's count.

## Textures

- `resource::room` refuses only when the card is at its ceiling; a refusal is
  now answered by reclaiming a copy that one idle slot alone holds, and a slot
  seen twice in 64 frames is protected from the sweep. Never raise the ceiling
  to make a refusal go away; reclaim or admit instead.
- Staging copies are capped in system RAM (`VICTORY_STAGING_MB`), coldest and
  not-held first: re-admitting from staging is one DMA, decoding the title's
  format is far more expensive.

## Diagnostics

- `capture` and `copystat` are compiled only with
  `feature = "diagnostics"`. `VICTORY_GPU_TIMING` forces `late_stage` to 0;
  `VICTORY_D3D12_DRED` names the faulting command.
- Most `shortcut:` comments live here. Read the one next to the code you
  touch; it states what the approximation costs.

## Tests

- Pure logic (descriptor bookkeeping, shadow merge, frame clock) is tested
  without a device. GPU paths are covered by `victory-shader`'s differential
  tests. Do not add a workspace test that requires a real card.
