//! What a present shows when nothing wrote the frame the title asked for.
//!
//! A title renders the world at one size and presents a larger texture,
//! upscaled on the console by its scaler and on the PC by FidelityFX's compute
//! programs. While those programs do not translate, the presented texture is
//! never written, so the window is black although the frame exists at the size
//! the title did render (series-s: 1920x1080 presented, 1280x720 written, none
//! of 12 071 recorded operations at world frame 3318 naming a 1920-wide
//! resource).
//!
//! So every operation that renders into a texture (a draw or dispatch naming it
//! as a render target or an unordered access, a copy or a resolve into it) and
//! every shader read of one says so here, and a present whose frame was written
//! by none of them shows a target that was ([`pick`]). A clear is not one of
//! them: it says the texture exists, never that it holds a frame, and the title
//! clears exactly the texture its upscaler would have filled in.
//!
//! The rules are conservative on purpose: a wrong image is worse than a black
//! one.
//!
//! - A frame written since the last present - or the one before it, as a title
//!   that alternates two textures presents each one every second frame - is
//!   shown as it is, and the run is the one it was.
//! - A clear of the frame is not a write of it: it says the texture exists,
//!   and the target that did receive a frame is shown instead.
//! - A target a shader also read is the frame's own source, and is preferred
//!   to a larger one that nothing read.
//! - Only a target of the frame's shape (within [`ASPECT_TOLERANCE`]) may
//!   stand for it, and only the largest: a bloom chain's levels are the frame's
//!   shape too, at a quarter of its size and less.
//! - Two sizes as good as each other compete, and then nothing is shown: the
//!   frame of a title that pads 1080 lines to 1088 is either texture, and
//!   guessing between them draws the wrong one.
//!
//! `VICTORY_PRESENT_FALLBACK=1` turns the fallback on; it is off by default,
//! because a substituted frame can lack the layers the title composites into
//! the frame it presents (the world's own draw is one of them).
#![cfg_attr(not(windows), allow(dead_code))] // the decision is tested everywhere

use std::sync::Mutex;

use crate::object::This;

/// How far a target's shape may be from the frame's: 3 %, which holds a
/// 1088-line texture for a 1080-line frame and refuses anything of another
/// shape.
const ASPECT_TOLERANCE: f64 = 0.03;

/// How close two targets' areas may be, in percent, before neither is the
/// better guess: 1 %, which is 1080 lines against 1088.
const AREA_TOLERANCE: u64 = 1;

/// `D3D12_RESOURCE_DIMENSION_TEXTURE2D`: what the present pass reads.
const TEXTURE_2D: u32 = 3;

/// A texture an operation named as a target since the last present, with the
/// shape its host copy has now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    /// The title texture, as its address.
    key: usize,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// `DXGI_FORMAT`.
    pub(crate) format: i32,
    /// `D3D12_RESOURCE_FLAGS`.
    pub(crate) flags: u32,
    pub(crate) sample_count: u32,
    /// `D3D12_RESOURCE_DIMENSION`.
    dimension: u32,
    /// A shader read it since the last present as well.
    sampled: bool,
}

impl Target {
    /// The title texture this names.
    pub(crate) fn this(&self) -> This {
        self.key as This
    }

    /// One target, named for a log line: the texture, its shape, and whether a
    /// shader read it.
    pub(crate) fn describe(&self) -> String {
        format!(
            "{:#x} {}x{} format={} flags={:#x} sampled={}",
            self.key, self.width, self.height, self.format, self.flags, self.sampled
        )
    }
}

/// What the operations since the last present named, for one present.
#[derive(Debug, Default)]
pub(crate) struct Since {
    /// The presented texture was rendered into since the last present, or the
    /// one before it: it is shown as it is.
    pub(crate) written: bool,
    /// The presented texture was cleared instead: it exists, and holds the
    /// clear's colour, which is not a frame.
    pub(crate) cleared: bool,
    /// The other textures rendered into, with the shape they have now.
    pub(crate) targets: Vec<Target>,
}

/// Why a present has nothing better than its frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    /// The title wrote the presented texture: it is the frame.
    Written,
    /// No texture the title wrote could stand for it.
    Nothing,
    /// More than one size as good as another: which one is the frame's is not
    /// known, and a wrong image is worse than a black one.
    Competing(Vec<(u32, u32)>),
}

/// What the operations since the last present named.
#[derive(Debug, Default)]
struct Window {
    /// Rendered into since the last present: a draw or dispatch naming it as a
    /// render target or an unordered access, a copy or a resolve into it.
    written: Vec<usize>,
    /// Cleared since the last present, and not necessarily drawn into: a clear
    /// says the texture exists, never that it holds a frame.
    cleared: Vec<usize>,
    /// Read as a shader resource since the last present.
    sampled: Vec<usize>,
    /// Rendered into in the present before the last one.
    before: Vec<usize>,
}

/// What one present is told about the window it closes.
#[derive(Debug, Default, PartialEq, Eq)]
struct Decision {
    written: bool,
    cleared: bool,
    /// The textures rendered into, and whether a shader also read each.
    candidates: Vec<(usize, bool)>,
}

static WINDOW: Mutex<Option<Window>> = Mutex::new(None);

fn window() -> std::sync::MutexGuard<'static, Option<Window>> {
    WINDOW.lock().unwrap_or_else(|e| e.into_inner())
}

fn entries(window: &mut Option<Window>) -> &mut Window {
    window.get_or_insert_with(Window::default)
}

/// Names `at` in `list`, once.
fn note(list: &mut Vec<usize>, at: usize) {
    if !list.contains(&at) {
        list.push(at);
    }
}

/// Whether the fallback is on (`VICTORY_PRESENT_FALLBACK=1` turns it on).
fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PRESENT_FALLBACK").is_some_and(|v| v == "1"))
}

/// A draw, dispatch, copy or resolve rendered into `this`: it holds a frame.
pub(crate) fn note_target(this: This) {
    if this.is_null() || !on() {
        return;
    }
    let mut window = window();
    note(&mut entries(&mut window).written, this as usize);
}

/// A clear filled `this`: it exists, and holds no frame of its own. The title
/// clears the texture its upscaler would have filled in, so a clear counted as
/// a write would show the clear's colour as the frame.
pub(crate) fn note_cleared(this: This) {
    if this.is_null() || !on() {
        return;
    }
    let mut window = window();
    note(&mut entries(&mut window).cleared, this as usize);
}

/// A shader read `this` as a resource.
pub(crate) fn note_sampled(this: This) {
    if this.is_null() || !on() {
        return;
    }
    let mut window = window();
    note(&mut entries(&mut window).sampled, this as usize);
}

/// Takes what the operations since the last present named, for the present of
/// `presented`: whether it was written, and the other targets it named.
#[cfg(windows)]
pub(crate) fn take(presented: This) -> Since {
    let mut window = window();
    let Some(window) = window.as_mut() else {
        return Since {
            written: true,
            cleared: false,
            targets: Vec::new(),
        };
    };
    let decision = decide(window, presented as usize, on());
    let targets = decision
        .candidates
        .iter()
        .filter_map(|&(at, sampled)| target(at, sampled))
        .collect();
    Since {
        written: decision.written,
        cleared: decision.cleared,
        targets,
    }
}

/// Closes `window` for the present of `presented`: was the frame rendered into
/// (or in the present before), was it only cleared, and what else was rendered
/// into. A clear of the frame is not a write of it.
fn decide(window: &mut Window, presented: usize, on: bool) -> Decision {
    let before = std::mem::take(&mut window.before);
    let written = !on || window.written.contains(&presented) || before.contains(&presented);
    let cleared = window.cleared.contains(&presented);
    let mut candidates = Vec::new();
    if !written {
        for &at in &window.written {
            if at != presented {
                candidates.push((at, window.sampled.contains(&at)));
            }
        }
    }
    // This window's targets are the next present's "before": two textures a
    // title alternates carry a frame written one present ago.
    window.before = std::mem::take(&mut window.written);
    window.cleared.clear();
    window.sampled.clear();
    Decision {
        written,
        cleared,
        candidates,
    }
}

/// The shape of the title texture at `at`, if it has a host copy: every path
/// that names a texture as a target makes one first, so this makes none.
#[cfg(windows)]
fn target(at: usize, sampled: bool) -> Option<Target> {
    let texture = crate::resource::host_resource(at as This)?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    Some(Target {
        key: at,
        width: d.Width as u32,
        height: d.Height,
        format: d.Format.0,
        flags: d.Flags.0 as u32,
        sample_count: d.SampleDesc.Count,
        dimension: d.Dimension.0 as u32,
        sampled,
    })
}

/// The target of `since` to show instead of the frame of `size`, or why the
/// frame is shown as it is. `presentable` is the present pass's own test of a
/// target: it names a format the pass can read.
pub(crate) fn pick(
    size: (u32, u32),
    since: &Since,
    presentable: impl Fn(&Target) -> bool,
) -> Result<usize, Why> {
    if since.written {
        return Err(Why::Written);
    }
    if size.0 == 0 || size.1 == 0 {
        return Err(Why::Nothing);
    }
    let shape = f64::from(size.0) / f64::from(size.1);
    let shaped: Vec<usize> = (0..since.targets.len())
        .filter(|&i| {
            let t = &since.targets[i];
            t.width > 0
                && t.height > 0
                && t.dimension == TEXTURE_2D
                && presentable(t)
                && (f64::from(t.width) / f64::from(t.height) - shape).abs()
                    <= shape * ASPECT_TOLERANCE
        })
        .collect();
    if shaped.is_empty() {
        return Err(Why::Nothing);
    }
    // A fallback may only show a frame the title drew smaller than the one it
    // presents by more than a few lines of padding: that is the upscale's
    // source, and the presented frame provably cannot hold it. A target of the
    // frame's own size, or larger, is a guess with no evidence behind it, so
    // the frame is shown as it is instead.
    let presented = u64::from(size.0) * u64::from(size.1);
    let fits: Vec<usize> = shaped
        .iter()
        .copied()
        .filter(|&i| {
            let t = &since.targets[i];
            u64::from(t.width) * u64::from(t.height) * 100 < presented * 99
        })
        .collect();
    if fits.is_empty() {
        return Err(Why::Nothing);
    }
    let shaped = fits;
    // A target a shader read is the frame's own source; failing that, the
    // largest of the frame's shape.
    let read: Vec<usize> = shaped
        .iter()
        .copied()
        .filter(|&i| since.targets[i].sampled)
        .collect();
    let tier = if read.is_empty() { &shaped } else { &read };
    let area = |i: usize| u64::from(since.targets[i].width) * u64::from(since.targets[i].height);
    let largest = tier.iter().copied().map(area).max().unwrap_or(0);
    let floor = largest - largest * AREA_TOLERANCE / 100;
    let near: Vec<usize> = tier.iter().copied().filter(|&i| area(i) >= floor).collect();
    let mut sizes: Vec<(u32, u32)> = Vec::new();
    for &i in &near {
        let shape = (since.targets[i].width, since.targets[i].height);
        if !sizes.contains(&shape) {
            sizes.push(shape);
        }
    }
    if sizes.len() > 1 {
        return Err(Why::Competing(sizes));
    }
    near.iter()
        .copied()
        .max_by_key(|&i| area(i))
        .ok_or(Why::Nothing)
}

#[cfg(test)]
mod tests {
    use super::{Since, Target, Why, pick};

    /// A written 16:9 texture of `width` x `height`, read by a shader or not.
    fn target(key: usize, width: u32, height: u32, sampled: bool) -> Target {
        Target {
            key,
            width,
            height,
            format: 28,
            flags: 0,
            sample_count: 1,
            dimension: super::TEXTURE_2D,
            sampled,
        }
    }

    fn since(written: bool, targets: Vec<Target>) -> Since {
        Since {
            written,
            cleared: false,
            targets,
        }
    }

    /// The series-s world present as it reaches `take`: the title cleared its
    /// 1920x1080 frame every frame and drew nothing into it, while the world
    /// went to a 1280x720 target. A clear is not a frame.
    #[test]
    fn a_cleared_frame_is_not_a_written_one() {
        let mut window = super::Window::default();
        window.cleared.push(0x10);
        window.written.push(0x20);
        window.sampled.push(0x20);
        let decision = super::decide(&mut window, 0x10, true);
        assert!(!decision.written, "a clear of the frame is not a write of it");
        assert!(decision.cleared, "and it is the case the log names");
        assert_eq!(decision.candidates, vec![(0x20, true)]);
    }

    /// A frame that was rendered into is shown as it is, cleared or not.
    #[test]
    fn a_drawn_frame_is_written_even_when_it_was_cleared() {
        let mut window = super::Window::default();
        window.cleared.push(0x10);
        window.written.push(0x10);
        let decision = super::decide(&mut window, 0x10, true);
        assert!(decision.written);
        assert!(decision.cleared);
        assert!(decision.candidates.is_empty());
    }

    /// A cleared texture is no candidate either: it holds the clear's colour,
    /// and it was named by an operation all the same.
    #[test]
    fn a_cleared_target_is_no_candidate() {
        let mut window = super::Window::default();
        window.cleared.push(0x10);
        let decision = super::decide(&mut window, 0x20, true);
        assert!(!decision.written);
        assert!(!decision.cleared);
        assert!(decision.candidates.is_empty());
    }

    /// A window closed leaves the frame it wrote for the next present, and no
    /// further: two textures a title alternates carry a frame one present old.
    #[test]
    fn a_window_closed_hands_its_frame_to_the_next_present() {
        let mut window = super::Window::default();
        window.written.push(0x20);
        window.cleared.push(0x10);
        window.sampled.push(0x20);
        let first = super::decide(&mut window, 0x10, true);
        assert_eq!(first.candidates, vec![(0x20, true)]);
        let second = super::decide(&mut window, 0x20, true);
        assert!(second.written, "the frame written one present ago still stands");
        assert!(
            !super::decide(&mut window, 0x20, true).written,
            "and not two presents later"
        );
    }

    /// With the switch off nothing is remembered and every frame is its own.
    #[test]
    fn the_switch_off_leaves_every_frame_as_it_is() {
        let mut window = super::Window::default();
        window.written.push(0x20);
        let decision = super::decide(&mut window, 0x10, false);
        assert!(decision.written);
        assert!(!decision.cleared);
        assert!(decision.candidates.is_empty());
    }

    /// The series-s frame: the presented 1920x1080 texture was written by
    /// nothing, and the world exists at 1280x720.
    #[test]
    fn an_untouched_frame_shows_the_one_target_it_wrote() {
        let targets = vec![target(0x10, 1280, 720, false)];
        assert_eq!(pick((1920, 1080), &since(false, targets), |_| true), Ok(0));
    }

    /// A frame the title wrote is the frame: series-x's two draws leave it
    /// alone, and nothing about that present changes.
    #[test]
    fn a_written_frame_is_shown_as_it_is() {
        let targets = vec![target(0x10, 1280, 720, false)];
        assert_eq!(
            pick((1920, 1080), &since(true, targets), |_| true),
            Err(Why::Written)
        );
    }

    /// Two sizes as good as each other: neither is the frame's, so the frame
    /// stays black rather than showing the wrong picture.
    #[test]
    fn two_competing_sizes_are_no_frame() {
        // Read by shaders as well, and 1280x722 is within a percent of
        // 1280x720's area: the tie is what refuses the guess.
        let targets = vec![target(0x10, 1280, 720, true), target(0x20, 1280, 722, true)];
        assert_eq!(
            pick((1920, 1080), &since(false, targets), |_| true),
            Err(Why::Competing(vec![(1280, 720), (1280, 722)]))
        );
    }

    /// A target a shader read is the frame's source, even when another target
    /// of the frame's shape is larger.
    #[test]
    fn a_sampled_target_is_the_frames_source() {
        // The write-hiding pair of the console's upsample: 4x4 tiles of the
        // frame, and the frame itself, read by the pass that would have shown
        // it.
        let targets = vec![target(0x10, 1280, 720, true), target(0x20, 1920, 1080, false)];
        assert_eq!(pick((1920, 1080), &since(false, targets), |_| true), Ok(0));
    }

    /// A bloom chain's levels are the frame's shape at a quarter of its size:
    /// the largest is the frame's, and it is not a tie.
    #[test]
    fn the_largest_of_the_frames_shape_wins() {
        let targets = vec![
            target(0x10, 320, 180, false),
            target(0x20, 1280, 720, false),
            target(0x30, 640, 360, false),
        ];
        assert_eq!(pick((1920, 1080), &since(false, targets), |_| true), Ok(1));
    }

    /// A target larger than the frame is never the frame: the world is drawn
    /// at 1280x720 and upscaled, so a written 3840x2160 target of the same
    /// shape is a layer over it, not the picture.
    #[test]
    fn a_target_larger_than_the_frame_is_not_the_frame() {
        let targets = vec![
            target(0x10, 1280, 720, false),
            target(0x20, 3840, 2160, false),
        ];
        assert_eq!(pick((1920, 1080), &since(false, targets), |_| true), Ok(0));
    }

    /// When every written target is larger than the frame, the frame is shown
    /// as it is: a target of its own size or larger is a guess.
    #[test]
    fn a_larger_target_is_shown_when_it_is_all_there_is() {
        let targets = vec![target(0x20, 3840, 2160, false)];
        assert_eq!(
            pick((1920, 1080), &since(false, targets), |_| true),
            Err(Why::Nothing)
        );
    }

    /// Another shape is another picture: a shadow atlas or a 4:3 probe of the
    /// scene is never the frame.
    #[test]
    fn another_shape_is_no_frame() {
        let targets = vec![target(0x10, 1024, 1024, false), target(0x20, 1440, 1080, false)];
        assert_eq!(
            pick((1920, 1080), &since(false, targets), |_| true),
            Err(Why::Nothing)
        );
    }

    /// 1088 lines for 1080 is the same shape (scanout padding), but a padded
    /// target is not a frame the title drew smaller: the frame is shown as it
    /// is rather than guessed at.
    #[test]
    fn padding_of_a_few_lines_is_no_smaller_frame() {
        let targets = vec![target(0x10, 1920, 1088, false)];
        assert_eq!(
            pick((1920, 1080), &since(false, targets), |_| true),
            Err(Why::Nothing)
        );
    }

    /// A target the pass cannot read (an integer or shader-unreadable format)
    /// is no frame.
    #[test]
    fn a_target_the_pass_cannot_read_is_no_frame() {
        let targets = vec![target(0x10, 1280, 720, false)];
        assert_eq!(
            pick((1920, 1080), &since(false, targets), |_| false),
            Err(Why::Nothing)
        );
    }

    /// A texture of one mip that is a depth buffer or another dimension is not
    /// a frame, however it is sized.
    #[test]
    fn only_a_texture2d_is_a_frame() {
        let mut depth = target(0x10, 1280, 720, false);
        depth.dimension = 1; // D3D12_RESOURCE_DIMENSION_BUFFER
        assert_eq!(
            pick((1920, 1080), &since(false, vec![depth]), |_| true),
            Err(Why::Nothing)
        );
    }
}
