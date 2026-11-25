# crates/victory-loader — running the title's image

- `prepare` parses and patches a **copy**: `IMAGE_FILE_DLL`, entry point 0,
  import directory emptied, TLS callbacks zeroed and their DIR64 reloc turned
  into `ABSOLUTE`. Do not "restore" those fields; `spawn_main` jumps to the
  recorded entry and `threads` attaches TLS and `DllMain` itself.
- The OS maps the image (`LoadLibraryExW`); we never map sections by hand.
  Title DLLs are mapped in post-order so dependencies initialise first, and
  bound before any `DllMain` runs.
- Delay-load imports are bound eagerly at startup, so the title's own delay
  helper never runs. Keep that: `HostPolicy` in `victory-hle` decides
  provenance.
- Import resolution is not here. `victory_hle::HleResolver` and
  `WithTitleDlls` define the order; this crate only writes the IAT.
- Console-only instructions (`clzero`, `clwb`, `rdpru`, `monitorx`, `rdpid`,
  SSE4a) are emulated by the vectored handler. `clwb` is patched in place to
  NOPs so it faults at most once. The handler must not allocate or take locks
  a faulting thread may hold.
- Crash reporting unwinds the faulting thread with `RtlVirtualUnwind`;
  buffers are preallocated for the same reason. Never add allocation to that
  path.
- `tests/fixtures/hle-probe` is a stand-in title built on demand. It is
  excluded from the workspace and contains only our own code.
