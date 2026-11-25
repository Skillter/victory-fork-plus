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
- Those refreshed ranges reach their card copies on the direct queue, in a list
  ahead of the submission that reads them. A COPY queue of its own
  (`VICTORY_COPY_QUEUE=1`, off by default) buys overlap only where its wait
  does not hold it: the batch waits for the last value signalled on the direct
  fence (the flights already sent read those copies with the bytes from
  before), the direct queue waits for the batch's value before its next list
  (`host::CopyOrder`, `host::card_copies`), and the batch leaves each copy in
  `COMMON`, a copy queue touching nothing else and decay doing the rest, which
  `host::copied_state` hands the reader as `BeforeState`. Off because the value
  waited for is the previous submission: what overlaps is one DMA with the
  submitting thread, not with the card.
- Readback: declare shader writes (`Shadow::gpu_writes`), snapshot before
  submission (`Shadow::writing`), then compare per 64 KiB piece and merge only
  differing bytes (`Shadow::copy_back_with`). Title threads write the same
  buffers concurrently, so a blind write-back corrupts title records.
- The bytes of a copy that the write watch named go up as the fewest copies
  that hold them (`shadow::joined`), and no byte it did not name goes up: a
  shader write there would be recorded as sent, and the merge would skip it
  instead of taking it (`Shadow::card_merge_relay`'s skip list).
- A fetch is issued in runs too: `shadow::fetch_runs` holds the differing
  pieces of one copy in the fewest `CopyBufferRegion`s that cover them, and
  every piece still merges where it lies, so a run reaching past its pieces
  costs bytes and not correctness (`Host::card_back`,
  `Host::relay_card_fetch`).
- A landing merges what its flights' shaders wrote in the copies in the PC's
  memory; only with `VICTORY_LAZY_MERGE=1` (off by default, needs
  `VICTORY_RELAY`, inert under a pinned `VICTORY_LATE_WAIT`) does it hold a
  flight back, and only when that flight tells the title nothing (no
  bottom-of-pipe write, no fence signal after it, no settle waiting:
  `holds_back`, `Line::settles`). What it holds back merges at the first
  read, present, fence or landing after it (`drain_deferred`), and its ranges
  stay held in `Held` until then, per submission, so a range two flights
  declared stays closed until both have merged it. The card copies' fetch is
  always issued and merged by the landing that made it, after the copies in
  the PC's memory: the card runs it while that thread merges its own.
- `VICTORY_CARD_BUFFERS*` adds a card-side copy and finds writes with a
  compute hash per piece; `VICTORY_INPLACE*` places a buffer in an
  `OpenExistingHeapFromFileMapping` heap. Both are off by default.
- Cross-crate contract: CPU writes into mapped title memory call
  `kernel_will_write` first; page remapping holds `title_memory_remapping()`.
- A cache of `shadow`'s answers (a copy for a range, a fresh table's slots)
  carries `shadow::revision()` and is used only while that holds: remapped
  title memory, a buffer registered or released, a window made or let go can
  make the same address name another copy.

## Descriptors

- Host heaps mirror the title's slots and are re-made when a slot is written.
- Per-draw tables come from the dynamic ring in `descriptor::host`; a request
  that cannot fit asks the flights in the air to land once
  (`descriptor::host::make_room`, at most one ask per 250 ms) and is refused
  only if the second try fails, never served from unbuilt slots. Every refusal
  is counted and its line names the free slots and the lists in the air; the
  ring's room is printed every 5 s in every build
  (`descriptor::host::dynamic_line`), not only in a diagnostics one. The free
  slots are counted from the rings (`descriptor::host::gauge`), never credited
  as runs come back: a heap let go leaves the total with its ring.
- A command list bumps through a run it holds (`DynamicList`), given back whole
  at submission; the ring is never walked per table. A run names its heap by
  identity, a counter (`HostHeap::ident`) resolved in `descriptor::host::RINGS`,
  never the heap's address: a heap released and another made where it stood are
  two heaps, and a run goes back to the ring it came from or to none. A run
  given back twice is credited once (`Ring::give_back`), so `free` never passes
  the ring's capacity.
- Root arguments, a parameter's 32-bit constants and the draw buffers are
  cached per list (`draw::RootArgs`, `draw::RootValues`, `State::host_vertex`):
  a call repeats only when the resolved value changes, a root-signature change
  or a new list drops the root ones, and `draw::rectangles` marks the vertex
  buffers unknown (it sets its own). A draw therefore emits its pipeline
  switches, its live root arguments and one draw; what else it emits is a
  redundancy `draw` counts (see Diagnostics).
- The capped heap points past-the-end reads at a null slot for a reason: a
  real read past the heap end hung an NVIDIA GPU.
- A buffer view of no elements is the console's for the rest of its buffer:
  `descriptor::host::view_count` works the length out and
  `descriptor::host::buffer_copy` refreshes and writes back that rest. A view
  left null reads as zeros. A view of no elements of a buffer that declares no
  bytes has no rest: it stays null as `Refused::NoLength`, apart from a view of
  no elements of another dimension (`Refused::NoElements`).
- A view the host still refuses is counted per reason and logged once with its
  reason and its buffer (`descriptor::host::refused_census`).
- A shader that picks a buffer from a view heap by an index it computes reads
  that slot's title address from `descriptor::HostHeap::addresses_gpu` (two
  dwords a slot, root SRV `victory_shader::emit::SLOT_VIEW_ADDRESSES`), written
  where the slot's element stride is, as one aligned eight-byte store.

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
  The pass's view of the frame names the frame's own format unless the frame is
  typeless (`host::frame_view`): a `_UNORM` view of a typed `_UNORM_SRGB`
  resource is one D3D12 refuses in silence.
- A frame nothing rendered into since the last present is not shown as it is:
  the present shows the target the title did render into instead
  (`present::pick`), at that target's size, format and tracked state, so the
  same pass letterboxes it as a frame. Only the largest target of the frame's
  shape (3 %) that the pass can read may stand for it, a target a shader also
  read first, and nothing when two sizes are within 1 % of each other's area: a
  wrong image is worse than a black one. `present::note_target` is called by a
  draw where it runs (`draw::prepare_for`), by the copies and resolves, and by
  the unordered access of a dispatch; `present::note_sampled` by a shader read.
  A clear is not a write (`present::note_cleared`): the title clears the texture
  its upscaler would have filled in, and counting that as a frame showed the
  clear's colour. `VICTORY_PRESENT_FALLBACK=0` turns this off.
- The pipelines folder keeps the live library and one older generation.
  `library::prune` drops a `.tmp` that no save finished and any generation
  unread for 30 days, and never the file this run opened or mapped.
- `pipeline census` prints at the save a run ends with (every save in a
  diagnostics build); `bytes_per_pipeline_made` is the library's growth over
  the pipelines this run made, never the whole library over this run's count.
- The host parses the whole library at every start, and its pages are the
  file's: a cold 753 MB library cost 9.4 s of page faults (80 MB/s) against
  49 ms of parse with the bytes resident (`VICTORY_LIBRARY_BENCH`). So
  `library` reads the mapping ahead in one pass before the parse
  (`VICTORY_LIBRARY_PREFETCH`) and keeps the phases apart in the
  `pipeline library load` line: check those three figures before changing how
  the library is opened. The only lever on what a start must read is a
  smaller file, and `VICTORY_LIBRARY_MB` (`library::room`) is where a run
  stops adding to it.

## Textures

- `resource::room` refuses only when the card is at its ceiling; a refusal is
  answered by reclaiming a copy that one idle slot alone holds. A slot seen
  twice inside 64 frames is protected from the sweep, and protection lapses
  after 600 frames or 10 s without a sample of it — whichever comes first,
  since 600 frames are 10 s only at 60 fps — and a sample after a gap of more
  than 64 frames spends the count (`resource::protected_at`,
  `resource::admit_step`): a set that could only grow filled the budget and
  left `room` refusing for the rest of the run. The admission line counts
  copies the sweep manages, never one the host wrote (a render target keeps
  itself). Never raise the ceiling to make a refusal go away; reclaim or
  admit instead.
- Staging copies are capped in system RAM (`VICTORY_STAGING_MB`), coldest and
  not-held first: re-admitting from staging is one DMA, decoding the title's
  format is far more expensive.
- A slot whose host view is unmade is looked at again in the same step
  (`texture_slot_later`), whether or not the copy was reclaimed: unmade and
  left due, the queue's next look takes the slot `done` and it is never made
  again for the run.

## Diagnostics

- `capture` and `copystat` are compiled only with
  `feature = "diagnostics"`. `VICTORY_GPU_TIMING` forces `late_stage` to 0;
  `VICTORY_D3D12_DRED` names the faulting command.
- `draw` counts what a draw emits on the host list and what those caches
  spare, per 5 s in every build (`draw::commands_line`), including the
  barriers and state an indirect clamp emits; `draw::windowed_line` splits
  those clamps by why their window closed. A per-draw cost neither line shows
  is `host`'s copies, not a command of the draw's.
- `list::census` (`VICTORY_BARRIER_CENSUS=1`, diagnostics only) splits a
  frame's barrier decisions by source: the draw path's `ensure_state` asks by
  the state asked for (a target, the depth buffer, a shader read, other), and
  the title's own `ResourceBarrier` calls (barriers passed, mapped, dropped).
  `draw::commands_line` counts only the first; the census names both.
- `flow` counts in every build, and logs after each frame line: which call
  site moved how many bytes between title memory and ours, in how long, and
  at what rate. `copystat` names the ranges and the memory kinds behind them.
  A new copy path adds its bytes there, or the frame line's `copies` and
  `copying back` stay unattributed.
- The title's buffer copies are deferred steps, run at submission in runs
  whose destinations carry on (`host::copy_run`): a run shares one flush
  before it, one opening of the guards over all it writes, one
  `watch::wrote` and one `flow` figure, and its copies still go one after
  another, in the order recorded, each failing on its own. The line after
  `flow`'s splits what they cost (`host::copy_stages_line`), because the
  frame line's `copies` is one number for five costs.
- Most `shortcut:` comments live here. Read the one next to the code you
  touch; it states what the approximation costs.

## Tests

- Pure logic (descriptor bookkeeping, shadow merge, frame clock) is tested
  without a device. GPU paths are covered by `victory-shader`'s differential
  tests. Do not add a workspace test that requires a real card.
