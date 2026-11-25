# crates/victory-formats — data formats

- **Zero dependencies, std only.** This crate is deliberately independent so
  it can be tested and reused alone. Do not add one.
- Texture layouts follow GFX10.3 macro tiling (`texture::Layout`,
  `texture::Swizzle`). When adding a swizzle mode, add its offset and detile
  cases plus a round-trip test. XOR modes (`_X`), MSAA and metadata planes
  (HTILE, CMASK, FMASK, DCC) are documented gaps, not oversights.
- `bcpack` is **decode only**; BC2 is intentionally absent. Never add an
  encoder.
- `bcpack-tables.bin` is the single allowed binary in the repository
  (`docs/asset-policy.md`, allowlisted in `xtask/src/policy.rs`). Do not edit,
  regenerate, reformat or re-save it.
- `xma/tables.rs` is converted from FFmpeg's `wmaprodata.h`
  (LGPL-2.1-or-later, used under GPL-3.0-or-later). Keep the provenance
  header on derived tables.
- Decoders are pure Rust and allocation-conscious. Callers live in
  `victory-hle` (`dstorage`, `audio`, `apu`) and in `victory-gpu` texture
  staging. Keep them panic-free on malformed input: return an error.
