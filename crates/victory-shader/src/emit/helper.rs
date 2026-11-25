//! Helper lanes: the lanes of a quad no primitive covers.
//!
//! The console runs whole quads. A pixel of a quad at a triangle's edge that
//! no primitive covers is a helper lane, and it is in EXEC: it takes part in
//! every lane operation, so a cross-lane read of it answers, and a derivative
//! (its own, or a texture read's dependent coordinates) is the difference of
//! the quad's four values. The host leaves helper lanes out of wave
//! operations instead: a wave read from one of them reads no lane at all, and
//! a wave read that names one reads 0. Quad reads are the operations that
//! keep them, and a waterfall loop (read a lane's value, run the lanes that
//! hold it, take them out of EXEC) needs an answer for the lanes the loop
//! cannot serve.
//!
//! Both functions here belong to the emitter's state through [`Lower`], the
//! way the ray-tracing child module does.

use super::Lower;
use super::super::bitcode::{Attr, V};

/// Whether a read of another lane of this lane's quad must reach helper
/// lanes: in a pixel shader, always. At a triangle's edge a quad's other
/// pixels are helpers (a discarded pixel is one too), and only quad reads
/// see them: the console computed a derivative by hand as a swizzle or a quad
/// DPP of the value, so a wave read gave the whole value there, the texture
/// read that used it took its smallest level, and the triangles' edges showed
/// as lines across car paint, glass and skin.
pub(super) fn quad_reads_helpers(l: &Lower, quad: bool) -> bool {
    quad && l.s.pixel
}

/// `v_readfirstlane_b32`'s result `first` of `own` in a pixel shader: a
/// helper lane takes its own value instead.
///
/// On the console, whole-quad mode puts helper lanes in EXEC, so a waterfall
/// loop (read the first lane's value, run the lanes with that value, take
/// them out of EXEC) serves helper lanes too. On the host, wave operations
/// leave helper lanes out: once only helper lanes are left in such a loop,
/// `WaveReadLaneFirst` reads no lane, no lane matches, and the loop runs to
/// its limit in every such lane, with whatever descriptor that read gave.
/// With its own value a helper lane matches in the first pass and leaves;
/// what it computes only feeds derivatives (those of a texture read's
/// dependent coordinates, at triangle edges), and its stores are dropped.
pub(super) fn reads_own(l: &mut Lower, own: V, first: V) -> V {
    if !l.s.pixel {
        return first;
    }
    let c = l.cur;
    let helper = l
        .s
        .op(c, "isHelperLane.i1", Attr::ReadOnly, l.s.t.i1, 221, &[]);
    l.s.m.select(c, helper, own, first)
}
