# VIctory fork (work in progress)

A work-in-progress fork of [VIctory](https://github.com/Marvisak/VIctory), a
compatibility layer that runs an Xbox Series title natively on Windows from
files the user supplies. This fork follows upstream
`main` and adds fixes aimed at one goal: getting the game's world to render
correctly and playably on one particular PC.

**This repository contains no game files, code or assets.** You need your own
copy of the game. It is not affiliated with or endorsed by the game's
developer or publisher.

## Credits

- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its
  contributors is the base of everything here: the loader, the runtime
  services, the Direct3D 12 back end and the shader recompiler. Licensed
  GPL-3.0-or-later; so is this fork. Upstream's history is kept intact below
  the single commit that carries this fork's changes.
- **[XCL](https://github.com/ivall/xcl)** by ivall and its contributors
  (GPL-2.0-only) was used as a reference for how the same title behaves under
  another layer. Fixes inspired by it were written anew in VIctory's code.
- This fork's changes were developed with [Claude Code](https://claude.com/claude-code).

## Status of this fork (2026-11-25)

**Not playable yet: 3.6 to 4.5 frames per second once the prologue draws, 0.1
while a scene's pipelines are still being made, and shadowed areas are black.**
Tested on one machine: Windows 11, Intel i7-14700KF, NVIDIA RTX 4070 12 GB,
32 GB RAM. The Series X profile is used throughout.

| Stage | State in this fork |
|-------|--------------------|
| Boot, menus, start screen | works, 30 fps (as upstream) |
| Opening cutscene | plays from start to finish, in order, lit: title cards, credits cards, night and dusk shots |
| Saves | the game autosaves a checkpoint and loads it again |
| First gameplay scene (the prologue) | renders: characters, textures, sky, sunlit exterior; HUD, pause menu and pad input work |
| Light in shadow | missing: the game's ray-traced bounce light and ray-traced lamp and sun shadows do not run yet, so anything the sun does not hit directly is black |
| Frame rate in the world | the newest run took 221 ms/frame (4.5 fps) at 114 draws, and 11,076 ms at 2,298 draws while their pipelines were made; an earlier build on this machine took 420-450 ms per frame by default, 240-350 ms with the newer switches (`VICTORY_RELAY=2`, or `VICTORY_LATE_WAIT=3 VICTORY_WIDE_BACK=1 VICTORY_CARD_BUFFERS_MIN_MB=1 VICTORY_CARD_BUFFERS_MB=1024`); it started at 3-6 seconds per frame |
| Stability | no driver hang or device loss in normal runs (a hang introduced by the wave64 update was found and fixed: its barrier form for 64-lane programs is now off unless `VICTORY_WAVE_WIDE=1`) |

### What this fork changed (summary)

Rendering:
- Meshlet culling read its scene buffers as the wrong kind of view, so the world drew almost nothing; fixed.
- Per-draw descriptor tables could run out, after which draws used slots never made for them and the card hung; fixed.
- Light packed by the game's compute shaders was read back through empty copies (sky, probes, fog); fixed for every mip level and layer.
- The fog reconstruction, shadow-mask, hair, blend-shape skinning, fire and several material programs did not translate; fixed.
- Gathers at a named mip level (with and without an offset) read level 0; fixed.
- Implicit-level samples outside pixel shaders produced invalid DXIL in 730 compute programs; fixed.
- Rectangle lists, depth and stencil twins for depth textures written by compute, texture read-backs.
- Ray tracing, in progress and off by default: the game's own structure-builder programs translate, title memory can be read by raw address from shaders, the hardware ray/box and ray/triangle test is emulated in DXIL (checked against a reference on the GPU), and 59 of the 64 trace programs translate. Nothing is lit by it yet. See [docs/fork-ray-tracing.md](docs/fork-ray-tracing.md).

Speed and memory:
- Waits for memory values, write-watch queries and page queries no longer walk page tables per call.
- Large title buffers get a copy in card memory, with shader writes found by hashing on the card.
- Submissions can be sent ahead of the wait for the card (`VICTORY_LATE_WAIT`), with a relay thread that lands results as the card finishes (`VICTORY_RELAY`): in a first five-minute run the card's idle time behind the submitting thread fell from about 136 to 30 ms per frame.
- Title memory can be opened as a graphics heap so that buffers are used in place instead of copied (`VICTORY_INPLACE`, first stage only; a probe confirmed the approach on the test GPU).
- The bindless texture queue no longer uploads stale views; the view table no longer grows without bound; the pipeline library is mapped instead of read into memory.
- Placed textures are staged in bounded bursts (192 MB and 250 ms of staging per window by default, three times those while a backlog of a thousand waits: `VICTORY_TEXTURE_STAGE_MB`, `VICTORY_TEXTURE_STAGE_MS`, `VICTORY_TEXTURE_CATCHUP=0`); the host copies of filled textures are let go after 2-10 s idle, sooner when the card has less free memory (was a fixed 5 s); a texture uploaded whole is looked at again at 4, 12, 40, 120 and 400 frames and re-uploaded if the title filled its memory later (`VICTORY_HEAP_RECHECK=0`); a texture view past the memory of the host view heap reads a null slot instead of faulting; the pipeline library's pages are trimmed after that same 2-10 s quiet window.
- An intermittent device loss was traced to the pipeline library file being replaced while mapped; fixed.

- Translated shaders get clean-up passes over the finished module (dead code, constant folding, reuse of values, branch facts); the pack's host instruction count falls by about 43 %, worth about 20 ms of GPU time per frame on the test machine (`VICTORY_SHADER_OPT=0` turns them off).

Most of the newer speed changes sit behind environment switches that are off
by default (`VICTORY_LATE_WAIT`, `VICTORY_RELAY`, `VICTORY_LAZY_MERGE`, `VICTORY_WIDE_BACK`,
`VICTORY_CARD_BUFFERS_MIN_MB`, `VICTORY_INPLACE`, `VICTORY_COPY_QUEUE`, `VICTORY_BVH_BUILDER`, ...),
and some translate a shader another way (`VICTORY_FIRSTLANE_OWN=1` takes a
half-wave's own first lane where the translator would otherwise refuse the
shader; own shader cache entries). Each is described in a comment where it is
read.

### Known problems

- Everything in shadow is black (see above).
- The interior of the first gameplay room is sometimes not drawn after the cutscene hands over to gameplay.
- A few style lints are switched off for this fork in the workspace `Cargo.toml` until the newer code is tidied.
- Many changes here are specific to this title and this GPU, and some are marked `shortcut:` in the code where they approximate what the console does.

Everything below this line is upstream VIctory's own README, unchanged; its
status section describes upstream, not this fork.

---

# VIctory

Runs the title (Xbox Series X|S) natively on Windows, from files
the user supplies.

The title's x86-64 code runs unchanged inside our process; VIctory supplies
the platform underneath it and converts the title's data, ahead of time where
it can and on first use where it must (shaders). See
[docs/architecture.md](docs/architecture.md).

## Status (2026-11-23)

**Not playable yet.** The title boots, runs its menus at its own 30 fps and
loads the world, where rendering fails.

| Stage | State |
|-------|-------|
| Boot, first-boot screens (accessibility, language, settings, legal) | works, 30 fps |
| Intro video with sound | works |
| Start screen, profile and saves | works |
| Loading into the world (about 50 s) | works |
| The world (first mission's intro) | reached and runs with indirect draws; its title card renders, the scene after it is still dark; 0.5-1.8 s per frame |

Tested on one machine: Windows 11, AMD RX 6700 XT.

> **Warning.** Entering the world on a real GPU can currently hang it, and
> Windows then resets the display driver (every monitor goes black for a
> moment). To look further, use a diagnostics build with
> `VICTORY_D3D12_WARP=1`, which renders on the software adapter.

What each part does today and what it still needs:

| Subsystem | Today | Needs |
|-----------|-------|-------|
| Loader | Runs the title's executable and its DLLs in our process; crash reports name the faulting code and are kept in `crashes.log` | – |
| Runtime services | One signed-in offline user, licences, packages, saves that survive a kill at any step, text entry dialog | Speech is silent; the dialog and relaunch are untested live |
| Network | Offline by design: the title cannot reach the network | – |
| Files and streaming | Archives and streamed data read, zlib and BCPack decoded in parallel | Loading is limited by writes into title memory |
| Input | XInput pads, keyboard, keyboard stand-in pad | Trigger rumble |
| Audio | Menu, video and loading sound; XMA voices and Opus streams decode | Reverb is silent; Opus results unverified against the title |
| GPU: menus | Draws, compute and presentation on host D3D12; only changed title memory is copied | Frame pacing drifts on one screen in release builds |
| GPU: world | Mesh pipelines, shader-written buffers, descriptor lifetimes, indirect execution (arguments clamped), pipelines made by worker threads and kept on disk | Most of the scene is still missing (untranslated shaders drop draws); frames take seconds; real queries, depth bounds, HDR and scaling are missing |
| Shaders | Console GPU code recompiled to DXIL at first use, cached on disk. Menus: all. World: mesh 8,191 of 8,506, vertex/pixel 10,617 of 12,346, compute 1,643 of 2,539 | 64-bit image atomics, merged geometry stages, the ray-tracing builder's shaders; some compute programs need a GPU with 64-lane waves |
| Textures | Console tiled layouts and detiling for the modes the title uses | Render-target metadata planes |
| Series S mode | Runs through the menus to the loading screen | The world, as above |

## Getting started

Requirements: Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with
shader model 6.6 (and mesh shaders for the world), Rust (pinned in
`rust-toolchain.toml`).

The title also needs several GiB of available Windows commit at startup.
Check **Committed** under **Task Manager → Performance → Memory**. If
`crashes.log` reports Windows error 1455, close other applications or increase
the Windows paging file size, then retry.

```powershell
cargo xtask install-hooks                # asset-policy check on every commit
copy victory.example.toml victory.toml   # then set your paths (outside the repo)
cargo test --workspace                   # needs no game data
cargo run -p victory -- config           # check your paths
```

Run the title:

```powershell
cargo build --release -p victory
target\release\victory.exe run <game dir>\<the title's exe> --execute
```

The window caption shows the framerate. Saves, settings and `crashes.log`
are kept under the data folder (default `%LOCALAPPDATA%\victory\data`), derived
data under the cache folder.

Input: XInput pads. With none connected the keyboard is a pad while the
title's window has focus:

| Keys | Pad |
|------|-----|
| W A S D / I J K L | left / right stick |
| arrows | D-pad |
| Enter, Backspace, X, Y | A, B, X, Y |
| Q / E, Z / C | shoulders, triggers |
| F / H | stick clicks |
| Esc, Tab | Menu, View |

## Commands

```
victory inspect <exe|dll> [--imports] [--exports]
victory census [paths...] [--missing] [--strict]
victory lib-exports <import.lib> [--list]
victory shader-info [paths...] [--verbose] [--disasm]
victory translate [paths...] [--validate] [--pipelines] [--list] [--jobs N]
victory run <exe> [--execute] [--unimplemented trap|zero] [--strict]
            [--console series-x|series-s] [-- <title arguments>]
victory config
```

Logging: `VICTORY_LOG=victory_loader=trace,info` (a release build logs
info and above). `victory run` also writes the same log to
`<data-dir>\logs\run-<YYYYMMDD>-<HHMMSS>.log` (UTC, one file per run, so a run
never overwrites the last one and a run that outlives its terminal still leaves
a record; the newest 20 are kept). `VICTORY_LOG_FILE=<path>` puts that file somewhere else;
`VICTORY_LOG_FILE=0` keeps the log on stderr only.

### Reading a run log

Three markers, in this order; grep them before reading anything else.

- `RUN` — one line per fact the run started with: build (`version`, `commit`,
  `+dirty` when tracked files differ from it), UTC and local start, the filter
  and cap in effect, the log file, the config file or `(none)`, resolved game,
  cache and data directories, the image's BLAKE3, title id, manifest and
  storage quota, console model, and every `VICTORY_*` variable that was set,
  sorted, one per line (`grep '^RUN env'`).
- `WINDOW` — one line per measurement interval from the frame loop:
  `key=value` fields, integers only, sorted by key, the unit in the key (`_ns`,
  `_us`, `_ms`, `_s`, `_b`, `_mb`, `_n`). A counter reads `name=total`; a
  sampled step reads `name.sum= name.max= name.last= name.n=`.
- `STOP` — what the run came to: `STOP end` (reason, duration, frames,
  `fps_mean`, `fps_worst`), `STOP meter` (the same fields totalled since the
  start), `STOP cache` (MB on disk per region), `STOP warn` (each repeating
  condition and how often it was seen).

A condition that repeats logs its first occurrence, then the 10th, 100th,
1 000th … and the first one after a minute of quiet; every line carries
`count=` and `suppressed=`, so a storm is counted instead of hidden. `RUN`,
`STOP` and warn lines bypass `VICTORY_LOG`: a filter is for volume, not for the
three records that explain a run.

The API for a call site is `victory_core::meter`: `meter().count("draws_n", 1)`
per draw, `meter().observe("preparing_ms", ms)`, `meter().time("copy_us", d)`,
and `meter().report_every(FIVE_SECONDS)` from the frame loop for the `WINDOW`
line.

## Debugging

A release build has no debugging aids. Build them in with

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

| Variable | Effect |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | debug and trace output |
| `VICTORY_LOG_FILE=<path>` | also write the log to that file; `0` disables the file |
| `VICTORY_SHADER_CACHE_KEY=<text>` | start the shader cache cold (it is keyed by the translator, so rebuilds keep it) |
| `VICTORY_MESH_GROUPS=<n>` | a measurement, not a fix: a mesh dispatch runs `n` groups at most (default 65,535), fewer than the title asked for where it asked for more, so the image changes; `on the GPU` did not follow it (see `MEMORY.md` §5.1) |
| `VICTORY_STACKS=20,60` | every thread's stack at those seconds |
| `VICTORY_WATCH=<rva>,...` | logs a title u32 when it changes |
| `VICTORY_KEYS=30:Enter,35:X` | presses keys at those seconds, focus or not |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | saves title memory to the cache folder on a crash |
| `VICTORY_CAPTURE=<dir>` | saves presented frames (powers of two, or `VICTORY_CAPTURE_FRAMES=<list>`) |
| `VICTORY_DUMP_TARGETS=1` | with the two above, saves every render target of those frames |
| `VICTORY_PRESENT_FALLBACK=0` | a present whose frame nothing rendered into since the last present (an upscale whose compute programs do not translate yet; a clear of it is not a frame) shows the target the title did write instead of a black window: the largest target of the frame's shape, in the same letterboxed pass, logged once. On by default; `0` presents the frame as it is |
| `VICTORY_PIPELINE_DUMP=<path>` | write the pipeline calls the device does not implement to that file, whole; `E_NOTIMPL` is still returned |
| `VICTORY_CALLSITE_PROBE=1` | with either unknown pipeline creation (slots `0x1c8`, `0x218`), dump the title's code at the return address and the argument's bytes either side of it, once per slot; still `E_NOTIMPL` |
| `VICTORY_D3D12_DEBUG=1` | host debug layer |
| `VICTORY_D3D12_GBV=1` | with the above, GPU-based validation (bad descriptor use reported, not faulted; slow) |
| `VICTORY_D3D12_DRED=1` | on a GPU failure, the allocation hit and the commands in flight |
| `VICTORY_BACKGROUND_PROCESSING=off\|normal\|measure` | what the driver may do on its own background threads: `off` (the default) disables background compilation and measurements, `normal` leaves the runtime's choice, `measure` allows intrusive measurements and commits them. The log line `background processing mode` gives the mode asked for and whether the runtime accepted it. `measure` slows every submission while it is on: a menu or a loading screen, never a scene. `VICTORY_D3D12_BACKGROUND=0` asked for the mode that is now the default; the variable is no longer read |
| `VICTORY_STAGING_MB=<n>` | system-RAM staging copies kept for textures refused twice, 6144 by default; the coldest unheld copy goes first |
| `VICTORY_SHADING_RATE=off\|1x2\|2x1\|2x2\|4x4` | the pixel shading rate every host graphics list carries, `off` (1x1, the console's own rate) by default; text naming no rate is warned about once and stays off. A device below variable shading rate tier 2, or without `ID3D12GraphicsCommandList5`, keeps 1x1 and says why; the log line `variable shading rate` gives the tier asked for and the rate in force |
| `VICTORY_PIPELINE_WAIT_MS=24` | reduce the per-frame wait for background pipeline creation from the default of 500 ms. A draw can be omitted until its pipeline is ready |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | omit draws and dispatches, including indirect ones, whose pipeline contains a listed 16-digit shader hash |
| `VICTORY_GPU_TIMING=1` | with the above, each list of a submission run and timed on its own; slow lists logged with their slowest commands |
| `VICTORY_ASK_ONCE=0` | ask the write watch about each refreshed range separately instead of once per submission; on unless `0`, so `0` inflates the `submission phases per frame` line's `asking` |
| `VICTORY_ASK_PAGES=<n>` | pages one submission may ask the kernel about, 262,144 (1 GiB) by default; the rest wait for a later submission, so a title that declares more dirty ranges than it wrote costs latency, not a stall |
| `VICTORY_COPY_THREADS=<n>` | threads that read title memory back, by default the machine's less one (2 to 6) |
| `VICTORY_LATE_WAITS=1` | a blocking wait sleeps on to the next tick even when it could end on the tick that passed: the old pacing, for a title that polls instead of waiting |
| `VICTORY_PACE=none` | a blocking wait answers at once, so our frame clock stops pacing and only the title's own limiter does: the experiment that tells a title doubling its frame time from one that is merely slow |
| `VICTORY_DSTORAGE_WORKERS=<n>` | threads that read the package, by default four per core up to 64 |
| `VICTORY_DSTORAGE_STAGING_MB=<n>` | bytes of package reads in flight, 32 MiB by default (the console's staging buffer), so a bigger pool has more to fill |
| `VICTORY_PIPELINE_WAIT_SHARED=0` | give each thread its own 500 ms pipeline wait per frame instead of sharing one budget across the frame |
| `VICTORY_D3D12_WARP=1` | host device on the software adapter (slow; cannot reset the driver) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | translated shader loops stop after `n` iterations per lane instead of 2^20 (own shader cache entries); if a lower `n` makes world frames much faster, loops hitting the limit are what makes them slow |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | records what the title plays (our process only) |
| `VICTORY_XMA_DUMP=<dir>` | saves the first 32 XMA voices' context and input |
| `VICTORY_LIBRARY_PREFETCH=0` | the host pipeline library's pages are read ahead before the host parses them; `0` lets the parse fault them in one at a time instead |
| `VICTORY_LIBRARY_MB=<n>` | cap on the library file: past it a run stores no more pipelines, so the next start reads less. Off by default (a capped library re-makes what it did not keep) |

Every build logs, after each frame line, `submission phases per frame: asking …, recording …, flush …, snapshots …, card …, send …, settle …` and the write watch's `asking the kernel N times for M pages (X ms finding their runs, Y ms in the kernel)`, and `where the frame's bytes went`: each call site that moved title memory, its MB, its ms and its rate (a `memcpy` on the machine measured does 14.1 GB/s). When the card sits idle while frames take seconds, `asking` is where to look, and its tail says whether the time goes in finding a range's runs or in the kernel call (MEMORY.md §5.1 describes each phase).

Every build appends crashes and host GPU failures to `crashes.log` in the
title's data folder.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
