//! Everything specific to the title: its container formats, build
//! identities, and the hooks we place in its code (located by per-build
//! offsets or byte-pattern signatures). What may and may not be stored here
//! is defined in docs/asset-policy.md.

pub mod sgd5;

/// One title build whose hash we can name. Carries no code and gates
/// nothing: an unlisted executable still runs.
#[derive(Debug, Clone, Copy)]
pub struct KnownBuild {
    /// Our own label, e.g. "1.0.0 retail".
    pub label: &'static str,
    /// BLAKE3 of the original executable, hex.
    pub blake3: &'static str,
}

/// Builds we can name. Deliberately empty in this tree: identification only
/// changes the banner `victory run` prints, and every unlisted build reports
/// `build unknown (<hash>)`. Add an entry only with a hash verified on the
/// build it names.
pub const KNOWN_BUILDS: &[KnownBuild] = &[];

/// Looks up a build by the BLAKE3 hash of its executable.
pub fn identify(blake3_hex: &str) -> Option<&'static KnownBuild> {
    KNOWN_BUILDS
        .iter()
        .find(|b| b.blake3.eq_ignore_ascii_case(blake3_hex))
}
