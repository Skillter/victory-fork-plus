# VIctory fork-plus — notes for coding agents

Windows-native Xbox Series compatibility layer, Rust. The title's original
x86-64 image runs unmodified inside our process; this tree supplies what was
the console: loader, system APIs (HLE), the D3D12.X GPU layer, and a
GFX10 to DXIL shader translator.

Read first: `docs/architecture.md` (crate map and data flow). `README.md` has
the user-facing status and switches. `MEMORY.md` is a dated handoff of
measurements from one machine: useful history, never the specification.

**Rule #1: no game-derived material in the repository.** No assets, extracted
tables, shader dumps, screenshots or title binaries. See
`docs/asset-policy.md`; `cargo xtask policy` enforces it and runs on commit.

## Commands

```powershell
cargo build --release -p victory
cargo build --profile diag -p victory --features diagnostics   # target\diag
cargo test --workspace                    # needs no game data
cargo test -p victory-shader --lib <substring>
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo xtask install-hooks                 # once per clone
cargo xtask policy [--staged]
target\release\victory.exe config
target\release\victory.exe run <title.exe> --execute
```

- CI is Windows-only: both clippy lines above plus
  `cargo test --workspace --locked`.
- There is no `cargo fmt` gate; `rustfmt.toml` is authoritative
  (`max_width = 100`).
- Most GPU and HLE code is `#[cfg(windows)]`, and their tests say so. Do not
  "fix" another platform by deleting those gates.
- Tests that need a GPU, DXC or title files print "skipped" and pass when the
  environment is absent.
- `target\release\D3D12\{D3D12Core.dll,d3d12SDKLayers.dll}` (Agility SDK
  1.614.1) supplies feature level 12_2 on Windows 10. A missing folder is not
  an error on Windows 11. `cargo clean` deletes it; `d3d12_factory` then falls
  back to the inbox core, which refuses heap-direct root-signature flags, and
  every pipeline is dropped. `VICTORY_D3D12_SDK=<dir>` overrides the folder.

## Boundaries

- **Never** add game files, converted assets, dumps or data-derived fixtures.
  Synthetic inputs only.
- **Never** write cache, logs or saves inside the repository. New
  path-taking code calls `victory_core::ensure_outside_repo`.
- **Never** commit `victory.toml` (a developer's local paths) or `target/`.
- **Never** edit `crates/victory-formats/src/bcpack-tables.bin` or its
  allowlist entry in `xtask/src/policy.rs`; it is the one documented
  exception.
- **Never** rewrite a `MEMORY.md` measurement without evidence; correct the
  wrong line in place and say where you saw it.
- **Ask first** before `cargo clean`, adding a dependency, changing the
  encoded layout of `HostShader`, disabling a lint, or widening the title-DLL
  rule in `apps/victory/src/run.rs` (tried before; black-screened).
- Keep the layer **offline**. `modules::net` and `modules::media` refuse
  non-loopback traffic by design; do not add a bypass.

## Conventions

- Edition 2024, `rust-version = 1.88`, toolchain pinned by
  `rust-toolchain.toml`. Dependencies live in `[workspace.dependencies]`.
- `unsafe_op_in_unsafe_fn` and `unused_must_use` are `deny`. Write `// SAFETY:`
  on every `unsafe` block even though `undocumented_unsafe_blocks` is allowed.
- HLE exports are plain `extern "system"` functions named exactly like the
  import, registered with `hle_export!`; CRT functions use `extern "C"`.
- Translators signal valid-but-unsupported constructs with
  `ShaderError::Unsupported { offset, what }`; the `what` text is a test and
  log contract.
- Mark deliberate console approximations with a `shortcut:` comment stating
  the trade-off. Do not remove one without replacing the behaviour.
- Tests assert properties, not snapshots. Differential and GPU tests compare
  against a Rust reference or a second compiler.
- Durable docs cite `path::symbol`; chat and commits may cite `path:line`.

## Docs

- Dense Markdown: one fact per line, no preamble, no filler, no restatement.
- Adding prose must delete or shorten prose, unless it records a new measured
  fact or a new rule.
- No fact twice: state it once, point at it.
- `MEMORY.md` = measurements, `AGENTS.md` = rules, `README.md` = user surface.
- No changelogs, no "we now", no dates in `AGENTS.md`.

## Architecture in brief

- `apps/victory/src/run.rs` is the startup spine: config, `prepare`, `load`,
  title DLLs, `bind_imports`, `spawn_main`.
- Import order: HLE registry, host allowlist, title-shipped DLL, stub.
- `victory-gpu`: every console D3D12.X method arrives as a Rust call because
  command-list put limits are held at zero. GPU addresses are CPU addresses.
- Unified memory is bridged with host shadow copies, write-watch upload and
  snapshot-compare readback. Details: `crates/victory-gpu/AGENTS.md`.
- `victory-shader`: DXBC, Xbox extras, GFX10 decode, SSA IR, EXEC
  structuring, descriptor tracing, lowering, bitcode/DXIL, optimiser.
  Details: `crates/victory-shader/AGENTS.md`.
- `victory-hle` owns console APIs and `QueryApiImpl` vtables. Details:
  `crates/victory-hle/AGENTS.md`.
- `victory-formats`, `victory-pipeline`, `victory-game`, `victory-pe` and
  `victory-core` are leaves; keep them dependency-light.
- `xtask` runs the asset policy; `.githooks/pre-commit` calls it.

## Pitfalls

- `cargo clean` deletes the `D3D12` folder, and rendering stops with it.
- CPU writes into title memory from `files` or `dstorage` must call
  `victory_gpu::kernel_will_write` first, or the title retries forever.
  `XMemMapPhysicalPages` must hold `victory_gpu::title_memory_remapping()`.
- A new field on `HostShader` must be added to the shader cache's `encode` and
  `decode`, or the disk cache returns truncated shaders.
- Untranslatable instructions report the opcode name (for example
  `v_cvt_f32_f64`), not a friendly sentence; grep the opcode.
- `modules::all()` registers only a subset of HLE areas; the rest are
  `QueryApiImpl` objects with their own vtables, not DLL exports.
- `victory_game::KNOWN_BUILDS` is intentionally empty: `identify` returns
  `None` and `victory run` prints `build unknown (<hash>)`. The label is
  cosmetic; add a `KnownBuild` only with a verified executable hash.
- `VICTORY_*` switches are read at their use sites. Search the variable name
  before documenting one; several are diagnostics-only.

## Definition of done

- Both clippy invocations clean, `cargo test --workspace` green and
  `cargo xtask policy` clean.
- Diff is scoped; no new dependency or data file; no commented-out code.
- Behaviour changes update the nearest doc (`README.md`, `docs/` or the
  directory `AGENTS.md`) in the same commit.
