# crates/victory-hle — console API implementations

## Adding an API

1. Write `pub unsafe extern "system" fn Name(...)` (CRT: `extern "C"`) in the
   owning module under `src/modules/`.
2. Register it in that module's `module()` with `hle_export!(Name)`. The
   exported name is the Rust identifier; rename the function when the import
   differs, and hand-write ordinals as `HleExport { name: "#n", .. }`.
3. Add the module to `modules::all()` only if it serves DLL exports. Objects
   reached through `QueryApiImpl` are not DLL modules.
4. Run `victory census` over a title to confirm coverage.

## Resolution

- HLE always wins over the host. `HostPolicy` is an allowlist of shared Windows
  API sets. `GetProcAddress` on a host module re-checks our registry, so path
  and network overrides cannot be bypassed.
- Pseudo handles for console-only DLLs are `*const HleModule`.

## QueryApiImpl

- Objects are static vtables registered by `(provider GUID, iid GUID)`.
  Titles often ask with newer (2026) IIDs, so most areas register two pairs.
- Unimplemented slots return `E_NOTIMPL` after one log. That default is wrong
  for methods returning `bool` or a value: add the method.

## Files and storage

- Console letters are mounted with `files::mount` or `mount_at`. `run.rs`
  mounts `G:` (package) and `T:` (temp); storage gets its own drive. Never
  assume a fixed host path.
- Path-taking imports rewrite `X:\` to the mount. Narrow (`A`) calls are
  widened and sent to the host's `W` twin; do not rewrite twice.
- Any CPU write into title memory calls `victory_gpu::kernel_will_write` first
  (`ReadFile`, `fread` and DirectStorage destinations already do).
- DirectStorage reads the package through eight unbuffered handles per file
  (`modules::dstorage::direct_handles`), each opened `FILE_FLAG_NO_BUFFERING`
  and `FILE_FLAG_OVERLAPPED`: 208 GB through the file cache evicts the title's
  working set (its GPU memory in host RAM), and a handle without
  `FILE_FLAG_OVERLAPPED` serialises every read at the file object. A read is
  `ReadFile` and `GetOverlappedResult` on its own `OVERLAPPED`
  (`dstorage::read_at`), never `seek_read`, which cannot wait for a pending
  one. A request off a sector is read as the sectors that hold it and copied
  out (`sectors`); a read the drive refuses in the file's partial last sector
  is finished through the cached handle, so the title sees `END_OF_FILE`. Only
  a file the host will not open unbuffered falls back to the title's cached
  handle, and the five-second line counts it.
- Saves are versioned directories committed by one `rename`, with `sync_all`
  per file. Keep that shape; it is what makes a kill at any step safe.

## Behaviour that is intentional

- Network is loopback-only; store, user and service calls report an offline
  console. Do not add real connectivity.
- DSP and reverb convolution produce silence; only XMA and Opus decode.
- With no pad, the keyboard is a stand-in pad. Scripted keys
  (`VICTORY_KEYS`) exist only in diagnostics builds.
- Audio capture runs in a child process on purpose: an in-process WASAPI
  client changes the title's timer behaviour.

## Tests

`#[cfg(all(test, windows))]`, no title data. Save atomicity is proven by a
child process killed at every step; extend that test rather than mocking the
filesystem.
