# Memory: running and debugging this fork

Handoff notes for maintainers returning to this tree. Measurements were taken
on the reference rig (Windows 10 19045, AMD RX 6800 XT gfx1030, driver
2.0.353) at 4K unless stated; other hardware will differ. Claims marked
*(re-measured)* come from a later session on a different machine and name the
file each one was observed in, because the figures depend on the card and the
driver. The preparation in §2 is what makes the world render at
all on Windows 10. Windows 11 already ships a new enough D3D12 core, so the
redistributed one is optional there.

Placeholders used below: `<repo>` is this checkout, `<game-folder>` is the
title package (outside the repository, read-only by preference), `<data-dir>`
is the configured data directory (`VICTORY_DATA_DIR` / `VICTORY_CACHE_DIR` or
`victory.toml`; default `%LOCALAPPDATA%\victory`), and `<title-id>` is the
package's title id from its manifest. Neither tree contains game data.

**If the title no longer starts at all, §2 is the answer.**

---

# 0. This tree and upstream

This is a fork of upstream VIctory that fixes the world. Upstream boots the
menus but its world submits, then renders dark. Work here; keep an upstream
checkout only as a reference for what the fork changed (the fork keeps
upstream's history).

Two directories hold all derived state, both outside `<repo>`: `<cache-dir>`
(`VICTORY_CACHE_DIR`, default `%LOCALAPPDATA%\victory\cache`) and `<data-dir>`
(`VICTORY_DATA_DIR`, default `%LOCALAPPDATA%\victory\data`).

    <cache-dir>/shaders/     translated shaders, keyed by this build's hash
    <cache-dir>/pipelines/   host-pipelines.bin, early-pipelines.txt
    <cache-dir>/images/      prepared copies of the title's images
    <data-dir>/<title-id>/  saves, settings, crashes.log, temp
    <data-dir>/logs/         run-<YYYYMMDD>-<HHMMSS>.log from `victory run`
    <data-dir>/capture/      captured frames
    <data-dir>/world*/       frame dumps from earlier sessions
    <data-dir>/agility/      unpacked Agility SDK, if you keep one there

The game folder is read-only by preference. Nothing should ever need to write
there: the title sees the package as `G:` and its writable storage as `Z:`/`T:`
over `<data-dir>/<title-id>/`.

---

# 1. Working configuration

    cd <repo>
    cargo build --release -p victory
    target\release\victory.exe run "<game-folder>\game_scarlett_master.exe" --execute

with `VICTORY_D3D12_BACKGROUND=0` set. That is the configuration that rendered
the cutscenes and the world; its 473 ms/frame (2.1 fps) is the reference rig's,
on an older build (§5.1 splits it and has the newest numbers).

`VICTORY_KEYS=12:Enter,16:Enter,...` drives the first-boot screens and the
intro (diagnostics builds only; the key list is behind
`feature = "diagnostics"` in `victory_hle::modules::gameinput`).

---

# 2. Why it needs a patch to run at all on Windows 10

Windows 10 19045's inbox `d3d12.dll` (`10.0.19041.5794`) is **feature level
12_1**: no shader model 6.6, no descriptor-heap direct indexing. Every pipeline
this title builds declares

    D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED  (0x400)
    D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED      (0x800)

(root signature flags live in `victory_gpu::pipeline::host`). The 12_1 core
refuses them — `Unsupported bit-flag set (root signature flags c01)` — so no
root signature serializes, no pipeline is created, and **every draw is
dropped**. The title then runs at a clean 30 fps presenting an empty frame,
which looks like a rendering bug and is not one.

Three changes fix it. Keep all three.

**2a. Ship the Agility SDK beside the executable.** D3D12 Agility SDK 1.614.1
runtime:

    target\release\D3D12\D3D12Core.dll
    target\release\D3D12\d3d12SDKLayers.dll

Source: NuGet `Microsoft.Direct3D.D3D12` 1.614.1,
`build/native/bin/x64/`. **`cargo clean` deletes this folder and the title
stops working.** It is build output, so it is not committed; `VICTORY_D3D12_SDK`
points the loader at a folder kept elsewhere.

**2b. `crates/victory-gpu/src/host.rs`** — `d3d12_factory()` and
`create_device()`, called before the device is created:

    D3D12GetInterface(&CLSID_D3D12SDKConfiguration, &mut config)   // ID3D12SDKConfiguration1
    config.CreateDeviceFactory(AGILITY_SDK_VERSION = 614, "<exe dir>\\D3D12")
    factory.ApplyToGlobalState()

Two traps, both cost hours:

- `ID3D12SDKConfiguration::SetSDKVersion` **cannot** be used: it must be the
  process's first D3D12 call, and import binding has already run. It returns
  `DXGI_ERROR_INVALID_CALL` (0x887A0001) for every path spelling.
- `ApplyToGlobalState()` is **required**. Without it the *device* comes from the
  new core but `d3d12.dll`'s own exports keep using the old one, so
  `D3D12SerializeVersionedRootSignature` still rejects 0xc01 and nothing changes.

Then the device is created at `D3D_FEATURE_LEVEL_12_2` with a 12_0 fallback
(`HIGHEST_FEATURE_LEVEL` in `crates/victory-gpu/src/device.rs` is `0xc200`).

**2c. `crates/victory-gpu/src/root.rs`** — the title serializes its own root
signatures through the version-1.0 entry point, which refuses 0x400/0x800 even
on the new core. `without_console_flags()` drops them there, with a warning:

    const VERSION_11_FLAGS: u32 = 0x400 | 0x800;

VIctory's own version-1.1 signatures keep them (2b makes those work).

---

# 3. Switches that exist, and what they actually do

| Switch | Effect | Notes |
|---|---|---|
| `VICTORY_D3D12_BACKGROUND=0` | driver's background recompiles off; that is now the default, so the variable is no longer read (`VICTORY_BACKGROUND_PROCESSING=normal` gives the older behaviour back) | **in the working config** |
| `VICTORY_RELAY=2` | relay thread lands flights; card work overlaps submission | **unstable**: 473 → 350 ms on the reference rig, then `device removed after 3 submissions sent ahead` (TDR) |
| `VICTORY_LATE_WAIT=1/2/3` | stages of the same send-ahead machinery | same family as `RELAY`; assume the same risk |
| `VICTORY_WIDE_BACK=1` | snapshot compares spread across cores | safe; sold as part of the fast config |
| `VICTORY_CARD_BUFFERS=0` | disables the card-side buffer shadow copies | **did not change the cutscene ghosting** |
| `VICTORY_DISPLAY_MODE=WxH` | display size reported to the title | **useless for resolution**, see §6 |
| `--console series-s` | title targets 1080p | renders at 1920x1080; the black screen on record is unverified against this tree, see §6 |
| `VICTORY_MESH_GROUPS=n` | caps the groups one mesh dispatch runs (default 65,535) | **present** since the mesh-cost measurement (§5.1): a measurement switch, so the title sees fewer groups than it asked for; it moved nothing measurable |
| `VICTORY_SHADER_LOOP_LIMIT=n` | bounds translated loops | did not stop the upstream TDR |
| `VICTORY_D3D12_DRED=1` | on device loss, name the faulting command | how the upstream TDR was identified |
| `VICTORY_CAPTURE=<dir>` / `VICTORY_CAPTURE_FRAMES=` / `VICTORY_DUMP_TARGETS=1` | save presented frames / every render target | diagnostics builds only |
| `VICTORY_D3D12_WARP=1` | software adapter; wild accesses become AVs, no driver reset | diagnostics builds only; slow |
| `VICTORY_GPU_TIMING=1` | per-list timing on the card | forces `late_stage()` to 0 |
| `VICTORY_LOG_FILE=<path>` | log goes to that file as well as stderr | `0` turns the file off; see below |
| `VICTORY_SHADER_CACHE_KEY=<text>` | starts the shader cache cold | a cache keyed by the translator survives rebuilds; set this only when a translation looks stale |

Log destination. `VICTORY_LOG_FILE=<path>` writes the log to that file in
addition to stderr. `VICTORY_LOG_FILE=0` (or `off`) disables the file copy, so
the log is stderr only. When the variable is unset, `victory run` logs to
`<data-dir>/logs/run-<YYYYMMDD>-<HHMMSS>.log` (UTC, one file per run, so no
run overwrites another; the newest 20 are kept and older ones are removed at
start-up), and every other command logs to stderr only. If the data directory cannot hold the file (asset
policy, permissions), the run falls back to stderr instead of refusing to
start. A capture that must survive a closed window is therefore
`VICTORY_LOG_FILE=<path> target\release\victory.exe run ...`; a relative value
is relative to the working directory, so do not pass a bare name from inside
`<repo>`. The default path is under `<data-dir>` because nothing may be written
inside `<repo>`.

Read a run log from its markers, in this order. `grep '^RUN '` is what the run
was given (build `version+commit`, the `VICTORY_*` switches that were set, the
resolved paths, the image's BLAKE3, title id, console). `grep WINDOW` is one
line per interval of named counters and timers, the unit in the key (`_ns`,
`_us`, `_ms`, `_s`, `_b`, `_mb`, `_n`) and the aggregate in the field: a
counter reads `name=total`, a sampled step `name.sum= name.max= name.last=
name.n=`. `grep '^STOP '` is what it came to: `STOP end` (reason, duration,
frames, mean and worst fps), `STOP meter` (the meter totalled), `STOP cache`
(MB on disk) and `STOP warn` (every repeating condition with its count).
The 4.2 s stall of §5.1 stayed invisible because the frame line accounted for
copies and textures and named nothing else — a step that no key names cannot
be found in a log, so a new step gets a `victory_core::meter` call. A refusal
that repeats is logged by `victory_core::logging::warn_once`: first occurrence,
then 10/100/1 000, and the footer carries the total; the `Once` that printed
4 359 lost draws a single time is the failure that pattern removes.

Diagnostics build (needed for capture, keys, WARP, GPU timing):

    cargo build --profile diag -p victory --features diagnostics
    # and copy the two DLLs into target\diag\D3D12\ as well

---

# 4. What works, measured

- Boot, first-boot screens, menus, start screen, profile, saves
  (`saves\0009000000000001\...`).
- **In-engine cutscenes render**: characters, skin, hair, fabric, specular,
  sun flares, floating motes, correct single-line subtitles. Frames were
  captured to `<data-dir>/world/`.
- **The world renders** with working controls, HUD and pause menu.
- Pipeline library accumulates across sessions. *(re-measured)*
  `<cache-dir>/pipelines/host-pipelines.bin` went, loaded → saved per run,
  465,924,678 (02:56) → 501,078,276 → 580,067,610 → 582,268,008 → 737,846,212
  (03:53) → 752,217,796 B (04:21) over ten runs; `bytes=` is what a save wrote,
  `library=` beside it the size held in memory. `early-pipelines.txt` grew with
  it: `early=2907` at the 02:56 save, `pipelines=2934` in every run from 03:44.
  The blob is rewritten whole at most every 60 s (`pipeline::library::save`,
  `SAVE_EVERY`) and nothing prunes it, so it only grows; it is coupled to the
  driver, so a driver update discards it and the next run rebuilds from empty.
  The census's `bytes_per_pipeline` is the library's bytes over the pipelines
  this session made or loaded (`pipeline::library::report_census`), an average
  that falls as a session loads more, not a pipeline's size: 1,443,536 at 88
  made + 336 loaded, 275,537 at 295 + 2,435.
  The load of it is the file's pages, not the parse: the 753,134,320 B library
  took 9,407 ms with its pages on the disk (80 MB/s) and 49 ms with them in
  memory, and a sequential read of the same file took 2,711 ms (278 MB/s)
  (`VICTORY_LIBRARY_BENCH`), so the mapping is read ahead before the parse.
  `dropped N (0 being made)` means the backlog is drained; `M being made` means
  compilation is the limit that frame.
- Drops with the library warm *(re-measured 04:23)*: 153.5/frame `BuilderOff`
  against 0.1 `NoPipeline` at 114 draws; 3,362, 1,895 `Unbindable` and 118 at
  2,298 draws.

---

# 5. The four open problems

## 5.1 Frame rate

`--console series-s` makes the title render **1920x1080** instead of 3840x2160
(`frame_width=1920` in `run-20261125-073731.log`, against 3840 on series-x);
that run reached 20.4 fps / 49.1 ms on the loading screen, then the device was
removed at frame 1937 with `0x887A0005` after a 1920x1080 `R9G9B9E5` 3D texture
failed to create with `0x80070057`. `VICTORY_DISPLAY_MODE=1920x1080` and
`resolutionFullscreen` 1920x1080 in `settings.xml` change nothing on series-x:
the title sizes its targets from the console model, not from either.

*(re-measured, `run-20261125-135405.log`)* reading the FidelityFX package's
programs (`61bb4d6`, `ACCC`/`FSPX` parts) makes the title take its upscale
chain. Four of its compute programs still refuse (`a lane mask exchanged where
not every lane runs`, `a lane mask holding lanes elsewhere read as data`, `a
descriptor's address or format read as data`, `a 16-bit image atomic`), so the
title **clears the frame it presents and never draws into it**: `present.
fallback.cleared` 302 times in `run-20261125-132005.log`, and the window shows
its overlays over black. Refusing the package again (`VICTORY_FSPX` off by
default, `run-20261125-135405.log`) restores the path the title took before:
`the window a present closes ... written=true` and 13.8 fps / 72.4 ms on the
loading screen.

*(re-measured, `victory translate` with `VICTORY_FSPX=1`, 25 Nov)* three of the
package's refusals were instructions with no arm at all, and they are gone:
`v_dot4c_i32_i8`/`v_dot4_i32_i8` (1 → 0), `a packed 16-bit operation` (1 → 0,
`v_pk_fmac_f16`) and `a special operand (vccz, execz, scc, lds_direct)` (2 → 0).
Compute programs emitted 2728 → **2731** of 2762; vertex/pixel 14211/14259 and
mesh 8762/8765 unchanged, so nothing that translated before stopped. What is
left of the package is the lane model: `a lane mask read as data` 17 ×,
`a lane mask holding lanes elsewhere read as data` 9 → 10 (one program now
reaches that refusal instead of stopping at a special operand), `a 16-bit image
atomic` 1, `resources: a descriptor neither loaded nor built` 1. The lane-mask
pair is the EXEC_LO idiom (`s_mov_b32 s16, exec_lo` … `s_andn2_b32 exec_lo, s16,
exec_lo`): `structure::key` needs both dwords of a wave64 mask, so the saved
copy is never a mask and the SOP op consuming it reads data.

*(re-measured, `run-20261125-092842.log`)* series-s now reaches the world with
**no failed texture create and no device removal**: `R9G9B9E5` (console format
67) has neither `RENDER_TARGET` nor `TYPED_UNORDERED_ACCESS_VIEW` support on
this card (`support1=0x8013f0`) and stands in as `R16G16B16A16_FLOAT`
(`resource.rs::target_format`), and a clear value is passed only to a
`ALLOW_RENDER_TARGET`/`ALLOW_DEPTH_STENCIL` description
(`resource.rs::takes_clear_value`), which every `0x80070057` create was guilty
of. What a removal is now says so once: `host.rs::note_removal` logs
`the host device was removed` with `GetDeviceRemovedReason()` — the 07:39 run
died of `0x887A0001` (`DXGI_ERROR_INVALID_CALL`), not a TDR. The world then
runs at **5.1 fps / 197.7 ms/frame, 4517 draws/frame, 1920x1080** and the
window is **black**: the title draws only into 1280x720 targets (0 of 12,071
operations at frame 3318 name a 1920-wide resource) and never writes the
1920x1080 f28 texture it presents. Series-x draws 2560x1440 and then four
draws into 3840x2160, so its upscale is graphics; series-s's is FidelityFX
compute (29 `FFX FSR4` lines, series-s only). Those 29 shaders have no `DXIL`
part at all: their program is an `FSPX` package of GFX10 code in an `ACCC`
part (`fspx.rs::programs`), and all 29 now reach the emitter, where
they stop on seven named gaps (census 2026-11-25, `victory translate
--validate` over the installed package: of their 29 programs 17 stop on `a
lane mask read as data` and 6 on `... holding lanes elsewhere read as data`,
2 on a special operand, and one each on `v_dot4c_i32_i8`, a packed 16-bit
operation, a 16-bit image atomic and a descriptor neither loaded nor built;
2728 of 2762 compute programs still emit and validate), so the upscale never
runs and the presented texture stays as created.

Pixel shading is not what the world costs. `VICTORY_SHADING_RATE=2x2|4x4` sets
the base shading rate on every host graphics list (`host.rs::shading_rate`,
preludes excepted; the card reports variable shading rate tier 2), and the same
world measured **4.90 and 5.10 fps / 203.3 and 195.6 ms** at 2x2, **5.05 fps /
198.1 ms** at 4x4 and **4.60 and 5.00 fps / 217.9 and 199.1 ms** with the
switch off, `on the GPU` 93.8-102.5 ms against 97.7-98.1, the card 75-81 %
busy in every arm: the spread inside one arm is as wide as between arms, so
the switch stays off by default, and 4,500 draws cost their submission, not
the pixels each one covers.

The meshes are what the card does spend, and they cost their dispatches. Five
runs of the same world (`run-20261125-120438` normal, `-121011`
`VICTORY_MESH_TEST=scissor`, `-121518` `=skip`, `-122247` and `-122817`
`VICTORY_MESH_GROUPS=4096` and `=256`):

| arm | fps | ms/frame | `on the GPU` | draws/frame |
|---|---|---|---|---|
| normal | 4.8 | 208.9 | 87.8 | 4,398 |
| mesh shaders run, one pixel rasterised | 4.7 | 212.3 | 102.8 | 4,555 |
| no mesh dispatch at all | 10.4 | 96.0 | 6.6 | 787 |
| groups capped at 4,096 | 5.8 | 173.8 | 76.3 | 4,404 |
| groups capped at 256 | 5.5 | 182.2 | 85.5 | 4,462 |

Mesh geometry is 81 of the 88 ms the card spends, and it is neither the pixels
it covers (scissor, which runs every shader and rasterises one pixel each,
does not move the frame) nor the groups inside each dispatch (capping the
groups 256-fold leaves 85.5 ms against 87.8, and 4,096 leaves 76.3: the spread
inside one arm is again as wide as between arms). A frame asks for 23,000 to
98,000 groups in 3,588 mesh dispatches, so it is roughly 20 µs per dispatch
(`the card's clock per frame` over `4,404 draws/frame` in the same lines),
against an average of a dozen groups each: the title's own submission, which
the console pays too. The frame line prints the groups the title asked for
beside what a group is promised at most (`host.rs::MESH_GROUPS`,
`pipeline::host::mesh_declared`): 128 output vertices and 128 primitives each,
3 to 12 million a frame. "At most" is literal now: `emit::counts::output_counts`
follows the console's allocation request (`s_sendmsg GS_ALLOC_REQ`,
`m0 = vertices | primitives << 12`) back to the two `v_readfirstlane_b32`
values that pack it, and a group is promised that pair, clamped to 128 and
emitted where it dominates every output store. All 8,797 mesh programs of this
title follow (`VICTORY_MESH_COUNTS_LOG=1`); a program whose request cannot be
followed keeps the declared maxima, which is the `shortcut:` still in
`emit::lower::mesh_prologue` for it. `VICTORY_MESH_COUNTS=max` restores the
maxima, `=zero` promises nothing.

A mesh primitive word is bit-packed the way the console compiler reads it
back: three 10-bit indices at bits 0, 10 and 20, `0x80000000` (bit 31) for a
cull. Census of `shaders.rpf` alone (26,503 containers, 614 of them
duplicates, 9,282 mesh sections, 18,564 `exp prim`): 1,435 sites build the
word with shifts 10 and 20, none with 9 or 18, none mentions `0x1FF` within
50 instructions. The title's own bounds cull is `v_bfe_u32 v1, v13, 10, 10`,
`v_bfe_u32 v2, v13, 20, 10`, `v_and_b32 v3, lit(0x000003ff), v13`, compared
against the declared vertices and applied as `v_cndmask_b32 v1, v13,
lit(0x80000000)`: bits 9, 19 and 29 are index bits, not edge flags. The
archive holds 32,645 ten-bit `v_bfe_u32` reads and 55 nine-bit ones, none of
them next to an `exp prim`.

The size of the promise is not what the frame pays; the caps above say so, and
so does promising nothing. Last ten frames of three runs minutes apart
(`run-20261125-125520` `zero`, `-131331` `max`, `-132005` `own`), `on the GPU`
and draws/frame: 74.7 at 4,440, 96.5 at 6,909, 62.0 at 4,502. The same draw
count costs different times in different runs: 4,550 to 4,610 draws cost 91 to
158 ms with `own` (`-132005`) and 108 to 161 ms with `max` (`-133938`), while
4,430 to 4,850 cost 74 to 93 ms with `zero` (`-125520`). Two runs of one arm
spread as widely as any two arms, and `copying back` grew from 175 to 200 ms as
the machine's pagefile filled, so a promise this size does not move the frame:
the 81 ms is inside the dispatches, where neither the group count nor the
promised output changes it.


Split on the reference rig at 4K (473 ms/frame, 2.1 fps, older build):

    168 ms preparing (62 ms copies / 158 MB up, 5 ms textures)
    249 ms GPU
     87 ms copying back (74 MB) — a serial stall, the card idles

*(re-measured 04:23 in `run-20261125-042134.log`)* the first 114-draw frame took
221.1 ms (4.5 fps): 158.8 ms preparing (copies 39.8 ms over 42.4 MB, textures
1.8 ms over 6.4 MB), 1.7 ms on the GPU, 1.7 ms copying back 1.4 MB; the frame
that first built 2,298 draws took 11,076.5 ms and dropped 3,763 of them.

Copies out of title memory run at 1.2–2.5 GB/s when the pages are in memory and
33–68 MB/s in the intro (`run-20261125-043856.log`: 96.3 MB in 1,409 ms), which
nothing that stays inside the process accounts for: a 64 MB `ReadProcessMemory`
of this process's own pages takes 10–17 ms (3.8–6.7 GB/s), the same bytes one
4 KiB page a call 1.2–2.1 µs a page (2.0–3.5 GB/s), and 17,494 faults from a
page trimmed out of the working set cost nothing measurable (the same read came
back at 5.6–9.3 GB/s) — measured 25 Nov by
`cargo test -p victory-gpu --lib what_a_copy_costs -- --ignored --nocapture`.

"Is this range committed" was asked a page at a time until 25 Nov: over 170 MB
of a committed block that is 43,520 one-byte kernel reads, 41.7 ms, each
faulting its page back in. One `VirtualQuery` per run of pages in one state
answers the same span in 1.09 ms, and the run it keeps answers it in 0.12 µs
thereafter. The kernel's answer costs the committed run it walks — 3.0–4.5 ms
inside the title's 1.2 GB block, 0.3–0.7 µs in a single 4 KiB page, 1.1–1.8 µs
in a 1.2 GB reservation with nothing committed — so a negative answer is cheap
and is never kept, and a positive one in a big block is worth keeping
(`cargo test -p victory-gpu --lib what_a_committed_question_costs -- --ignored
--nocapture`, same date; `texture::committed`).

The readback exists because the console has unified memory and the PC does not:
compute shaders write buffers that the title's CPU code then reads, so VIctory
must find what changed and copy it back, before the title proceeds.

The line after each frame line splits the preparing (`host::phase`): `asking`
the one write-watch asking (`watch::ask_once`), then `recording`, `flush`,
`snapshots`, `card`, `send` (`ExecuteCommandLists`) and `settle`. Its tail,
`asking the kernel N times for M pages (X ms finding their runs, Y ms in the
kernel)`, counts the same asking (`watch::take_askings`): on a frame of seconds
`asking` is the phase to read, and X against Y says whether the journal's search
or the kernel call costs.

Ceiling on the older build: 3–4 fps with every stable trick. The fork's own
authors report 420–450 ms → 240–350 ms with relay on a 4070, and the 04:23 run
reached 4.5 fps, so the limit is the port, not a bug.

A buffer view of no elements (`NumElements == 0`) reads *and writes* the rest of
its buffer (`crates/victory-gpu/src/descriptor/host.rs::buffer_copy`). What that
declaration costs, measured 25 Nov: the world's heap is 182,452,224 B and the
view in the log sits 10,880,256 B in, so 171,571,968 B — 2,618 pieces — are
declared as the shaders' to write. That copy is in the card's memory
(`host copies in the card's memory: 1 copies 174 MB`), where a declaration costs
one span in the plan and no host snapshot: the card already hashes 10,217 pieces
(669 MB) a frame inside a 2.1–3.5 ms card phase, and the rest about doubles it
(`run-20261125-043856.log`). What is fetched and merged back is only the pieces
that differ — 115–332 a frame that run, 7.5–21.8 MB, none of them the rest's.
With the card's copies off (`VICTORY_CARD_BUFFERS=0`) the same declaration is
host work: 10.3 ms of snapshot plus 9.7 ms of compare a submission, measured
here at 14–15 GB/s (145,752,064 B memcpy 10.3 ms, u64 piece compare 9.7 ms),
~80 ms a frame at 4 submissions.
A world run (5,138 draws a frame, 324 ms, `run-20261125-062758.log`) divides
the frame between both sides: 209 ms waiting for the card (107 ms of work
queued, 78 % busy) and 165 ms of CPU (recording 41, flush 53, copies 72), so
neither side is idle. `flow` prices those copies a second apart: 13,034
deferred copies of 6.7 MB in 9.6 ms, 113 MB read out of title memory in place
in 6.2 ms (18 GB/s), 25 MB snapshotted for shaders in 0.2 ms — the 72 ms is
per-call work, not bytes. Writing into title memory is the slow direction:
2.1 MB in 256 calls at 1.3 GB/s, a partial page's read-modify-write.

Card windows are not the lever: `VICTORY_CARD_WINDOWS_MB=3072` gave 2.4 fps
(410 ms, 261 ms on the GPU) against 3.6 fps (276 ms, 120 ms on the GPU) with
the default of none, on 5,044 against 4,518 draws a frame
(`run-20261125-063925.log` against `-062758.log`).

The relay's fetch wait is not queue depth: at 194 ms a frame
(`run-20261125-095133.log`) its 110–120 ms is the card's work queued ahead of
the fetch, which goes on the in-order queue's tail at the landing, so a frame's
card work is waited for once wherever the landing splits it —
`VICTORY_LATE_WAIT_LISTS=32` takes the wait to 49 ms and the frame to 219 ms
(the submitting thread's own wait 59 → 123 ms, the card's idle 49 → 73 ms,
`run-20261125-100317.log`), and `VICTORY_COPY_QUEUE=1` only moves it into
preparing, 96 → 168 ms, 205 ms a frame (`run-20261125-101119.log`); measured
25 Nov. That run's copy-queue line says nothing about overlap: its pool held
one batch, so `host::CopyLists::take` recorded a batch only after the one
before had completed and `still running` read 0 by construction. Up to
`host::COPY_LISTS` batches may now be in the air at once, so that figure must
be measured before it is quoted.

The argument clamp is not a lever either: with the indirect window off
(`indirect.rs::record_clamp_as` once a record; the default since the window
cost the picture) the world ran 4,459 draws at 172.1 ms/frame, 73.3 ms on the
GPU and 1.04 clamps a draw; with the window on, 4,423 draws at 217.6 ms and
0.52 clamps a draw (`run-20261125-123322.log` against `-114852.log`) — doubling the clamps did not
slow the frame, and the 45 ms between the two runs (21 % of the frame, world
state and machine load included) is larger than anything the clamp
contributes. A clamp emits 5–6 barrier records (`argument clamp: N clamped and
M barrier records`), so at 0.52 clamps a draw it owns 3.1 of the 3.20 barrier
records a draw on the list, against the title's own 0.07. It cannot be removed
to check: `VICTORY_CLAMP=off` crashed the title in the loading scene after
2 min 36 s with `0xC0000005` and no frame line (`run-20261125-115523.log`), so
the arguments there are genuinely malformed and the clamp is load-bearing.
Measured 25 Nov.

Indirect windows are off by default (`VICTORY_INDIRECT_WINDOW=1` asks for one)
because they cost the picture: with windows on the world's draws are recorded
as one windowed run and nothing is drawn into the presented frame — black at
4.8 fps (`run-20261125-140122.log`); with them off the same build shows the
scene at 4.0 fps (`run-20261125-141510.log`). Until the window path draws what
it records, it stays opt-in.

## 5.2 Missing content — clothes and materials

Most of what is dropped each frame is not a shader problem. In the 02:56 run
(`run-20261125-025614.log`) the drops were **382.0/frame `BuilderOff` against
5.0/frame `NoPipeline`** (§4 has the newer split). The ray-tracing structure
builder is off (`VICTORY_BVH_BUILDER` unset, `pipeline::builder_runs`), and its
draw stages are dropped on purpose. Set `VICTORY_BVH_BUILDER=1` before blaming
a rule.

The refusals that remain *(re-measured 04:23, 3 programs, all compute)*: 1× `a
descriptor's address or format read as data`, 1× `a lane mask exchanged where
not every lane runs`, 1× `a lane mask holding lanes elsewhere read as data`.
The 02:56 run had 7: 4× `a descriptor read only as data` (pixel), 2× `a lane
mask exchanged where not every lane runs`, 1× `a descriptor's address or format
read as data`.

Separately, the host refuses 7 compute pipelines and 1 mesh pipeline with
`0x80070057`, in both runs. `pipeline::host::compute_refusal` logs the
description, the root signature's bytes, the container's parts, `DXIL`'s lengths
and `PSV0`'s stage, thread group and bindings, then `cause=<what is provably
invalid>` or `cause=no cause known (<what closes it>)`; the 04:21 run prints the
latter, and no cause is known yet.

In the log a refusal appears as `shader does not translate; its pipelines'
draws are dropped ... stage=... error=converter shader.dxil v1: GFX10 code at
0x....`, a bounded rule in `crates/victory-shader/src/emit/lower.rs`.

Archive-wide census (`victory translate --list` over `shaders.rpf`): 26,503
containers of which 614 duplicate, 26,084 programs; DXIL for 14,199 of 14,247
vertex and pixel, 2,672 of 2,682 compute, 8,762 of 8,765 mesh. Its reason list
counts refusals, not programs (256 against the 61 that emit none): `resources:
an address in an SGPR that is no root parameter` 195, `emit: a descriptor chosen
by control flow` 40, `emit: a hardware SGPR input` 10, `emit: a descriptor read
only as data` 5 (all five `shadowmask:CS_TraceShadowRays_*`), `a lane mask
holding lanes elsewhere read as data` 2, `a wave sum of 16-bit floats where not
every lane runs` 2, and 1 each of two others.

## 5.3 Light in shadow — black interiors

Not a bug: the fork's ray-traced bounce light and RT lamp/sun shadows are not
implemented. `docs/fork-ray-tracing.md` has the status. Off by default
(`VICTORY_BVH_BUILDER=1` asks for the builder; its own notes say it then stays
off in a real run). Anything the sun does not hit directly is black.

## 5.4 Cutscene ghosting

During the intro, frames appear to go back and forth and two different subtitle
lines can appear superimposed. Hypotheses eliminated:

- not stale card-side copies (`VICTORY_CARD_BUFFERS=0` changes nothing)
- not the present path in the default configuration (it is upstream's:
  `self.execute(&[copy])` then `Present`; the sent-ahead variant only runs at
  `late_stage() > 2`, i.e. with `RELAY` / `LATE_WAIT=3`)

Not yet eliminated: a missing clear or composite pass among the dropped draws
(§5.2) (the doubled subtitles are two *different* strings, which is temporal
blending, not a repeated frame). **Fix 5.2 first and re-check this** — it may be
the same root cause.

**Also missing, in one line:** `settings.xml`'s `vrsEnabled` key (§6) is still
not read — the title's own request for variable-rate shading is ignored — and
the layer's own lever, `VICTORY_SHADING_RATE`, measured no gain at all (§5.1).

- **The two unknown pipeline creations** (`device::unknown_pipeline`, slots
  `0x1c8` and `0x218`, 16,384 + 2,048 calls) are handed one pointer each, not a
  description: `title+0x2477578` passes a task's record word at `node+0x40`
  and `title+0x24800d5` a hash map's value slot at `node+0x18`, so every byte
  the dumps read past word 0 is record or bucket memory.

---

# 6. Dead ends — do not retry these

- **`--console series-s`.** It genuinely renders at 1920x1080 (confirmed:
  `title window fitted to the screen width=1920 height=1080`, `presenting ...
  1920x1080`) and that is still the only 4x pixel win found. Earlier notes here
  blamed a black screen on the title's upscaler being refused and quoted
  `refused dll="amd_fidelityfx_upscaler_xbox.dll"`. **That line exists nowhere
  in the current tree** — `git log -S'refused dll'` finds the string only in
  this file. What the tree does today is `apps/victory/src/run.rs::LateDlls`,
  documented as handling *the title's upscaler behind its FidelityFX loader*:
  every `LoadLibrary` the title makes goes through it, so a `LoadLibrary` of the
  upscaler is mapped like any other title DLL. Its filter is
  `wanted = |dll| title_dll(&host, dll) && image_named(dll).is_none()`, and
  `apps/victory/src/run.rs::title_dll` is true for every DLL the host policy does
  not allow through and no HLE module serves — so the upscaler qualifies. The
  *(re-tested)* the black screen is not the refused DLL: the upscaler is
  mapped, and series-s draws the world at 1280x720 and never writes the
  1920x1080 texture it presents, because the FidelityFX compute upscale that
  would bridge them does not translate (§5.1).
  **Do not fix it by mapping every DLL beside the exe**: the game folder also
  holds `msvcp140.dll`, `vccorlib140.dll`, `libHttpClient.GDK.dll` and friends,
  and mapping the title's copies of those instead of the host's is a much larger
  risk than the feature is worth (it was tried and black-screened, see below).
- **`VICTORY_DISPLAY_MODE=1920x1080`.** Accepted and reported, no effect: the
  title creates its 3840x2160 window *before* it queries the display (sample
  run: window 21:41:55, `display mode reported` 21:41:59).
  `victory_gpu::dxgi::modes()` already returns `display_size()`; the title
  ignores the mode list.
- **In-game resolution setting.** Earlier notes called this a dead end; it
  exists. They claimed `settings.xml` had no resolution key and no ray-tracing
  key, and that was wrong. *(re-measured)*
  `<data-dir>/<title-id>/storage/settings/settings.xml` is readable text
  and carries, among others, `resolutionFullscreen` (`3840x2160`),
  `frameRateLimit` (`30`), `vSync`, `hdr`, `fsrQuality`,
  `graphicsQualityPreset`, `rtDynamicShadows` (**`true` by default**),
  `rtLODLightShadows` (`false`), `ssroSampleCount` (`2`),
  `shadowAlphaTintedShadows`, `shadowParticleShadows`, `anisotropicFiltering`
  (`0`), `mapMSAA` and `vrsEnabled`. The title rewrites the file during a run,
  so edit it before starting one; the RT keys are requests the fork does not
  answer yet (§5.3). **UNTESTED:** copy the file, set `resolutionFullscreen` to
  `1920x1080`, and compare the frame time — 4x fewer pixels for one key. Nobody
  has run that experiment.
- **`fsrQuality` is not by itself upscaled rendering.** *(re-measured)* the
  settings request `fsrQuality`, but `<cache-dir>/images` held no
  `amd_fidelityfx_upscaler_xbox.dll` (786,432 bytes) — only its 13,312-byte
  loader shim — so the 4x path was not taken and the frame ran at the native
  3840x2160. That is an observation, not a conclusion: the upscaler may never
  have been requested, or may have been requested and failed to map.
  **Hypothesis to test:** capture a run's log and grep it for `title dll` —
  `apps/victory/src/run.rs` prints one line per title DLL it maps, with
  `(loaded by the title)` for a `LoadLibrary`. No
  `amd_fidelityfx_upscaler_xbox.dll` line means the title never asked for it.
- **`VICTORY_SHADER_LOOP_LIMIT`** as a TDR fix (upstream). Did not stop it.
- **`VICTORY_MESH_GROUPS`** as a TDR fix (upstream). Did not stop it; the
  failing command moved to a 1x1x1 compute dispatch with one thread.
- **Widening `wanted()` in `run.rs`.** Tried, black-screened, reverted. See
  above.

---

# 7. What the title does at startup, for orientation

Sample run clock:

    21:41:55  title asks for a 3840x2160 window   → lifecycle::fit() logs it
    21:41:59  display mode reported (dxgi::display_size)
    21:42:00  presenting to a window 3840x2160
              menus at 30 fps, 0 dropped
              pipeline prewarm queue grows (799 → 1119) during menus
     …        intro: cutscene renders, ~1-2 fps, drops split as in §5.2
              world: 2.1 fps, 473 ms/frame on the reference rig

*(re-measured 02:56)* the same world at 3.8 fps / 264 ms/frame; the ~385 drops
per frame split 382.0 `BuilderOff` + 5.0 `NoPipeline` (§5.2), so the drop count
is not a shader-refusal count.

Saves and settings live under `<data-dir>/<title-id>/`
(`saves\0009000000000001\...\Player~<hash>\v5\{0,1,manifest}`,
`SettingsProfileSettings~<hash>\v2\...`). *(re-measured)* the version folder is
part of the title's format and was **v5** (`Player~<hash>`), **v2**
(`SettingsProfileSettings~<hash>`) and **v1** (`SARC0015~<hash>`) on the session
that measured it; do not hardcode those numbers, read the directory. Save blobs
are binary, but the settings themselves are readable text in
`storage/settings/settings.xml` (§6), so resolution and quality **are**
greppable there.

---

# 8. Reference material outside this tree

A separate Vulkan + SPIR-V port of the same title has measured documentation
for this game's formats. It has never run the retail executable, so its code is
not authoritative, but its docs map onto the gaps above:

| Document | Use here |
|---|---|
| `docs/gbuffer.md` | deferred targets: 5- and 6-target signatures, `float32` declared but **compressed f16 exported**, 24-bit octahedral normal across `mrt1.gba` (scale 2047.5), sqrt-encoded `mrt2`. Binding these at the wrong format mangles normals silently |
| `docs/shader_inputs.md` | 512-byte light records, eight-light loop, plane count at `+0x4C`, ACES LUT splines, the `+0xD0` sky-LUT output-format dword |
| `docs/material_pass_replay.md` | how one mesh writes packed draw indices and which compute programs read them |
| `tools/lane_masks.py` | the path-sensitive lane-mask analysis, with its docstring explaining exactly the class of failure in 5.2 ("a register counts as a mask only if every path wrote a mask", ballot materialised where the mask is written) |
| `docs/design.md` | ~650 lines of RDNA2→SPIR-V contract; several rows (f16 semantics, VINTRP) describe instructions the fork still refuses, while `v_cvt_f32_f64` and `v_permlane16_b32`/`v_permlanex16_b32` now translate (`emit::lower::Emitter::valu_op`) |

Its **first** lesson is worth keeping: 100% of that title's `*Deferred*` pixel
shaders export **compressed half-float** while declaring `float32`. If the
deferred path here ever looks wrong rather than dark, check target formats first.

---

# 9. Gotchas that cost time

- `cargo clean` → the `D3D12` folder goes with it → the title silently stops
  rendering (see §2a). Re-copy the two DLLs after any clean, or point
  `VICTORY_D3D12_SDK` at a folder outside `target\`.
- Task-killing `victory.exe` is fine; the pipeline library still grows. Prefer
  Esc or a clean exit when the session went well.
- A GPU hang resets the display and shows up as
  `host GPU device removed ... (0x887A0006)`. Set `VICTORY_D3D12_DRED=1` first
  if a run might die — it names the faulting command.
  `<data-dir>/<title-id>/crashes.log` and the title's own
  `temp\victory-*.crash.log` both survive a crash.
- `VICTORY_CAPTURE` frames 1-1024 are black during boot; the menu art only
  streams in around frame 700-768. Pick later frames or you will debug a black
  image that is simply early.
- Builds need the MSVC toolchain and the Windows SDK (10.0.26100). Rust is
  pinned by `rust-toolchain.toml`.
- In git-bash, `$USERPROFILE` contains backslashes and breaks an `export PATH`;
  use a forward-slash path to the cargo bin directory instead.
