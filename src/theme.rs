//! The light-glass palette: dark ink on a milky frost.
//!
//! The shell's backing is now a bright, cool-white veil (see
//! `island_view::glass`), so the old white ink would disappear. These tokens
//! keep text, icons, tracks, and control surfaces readable on that frost.

use gpui_kit::{Hsla, hsla};

/// Primary text and icon color.
pub(crate) const INK: u32 = 0x001b_1b1d;

/// Secondary text: album lines, meter labels, the clock.
pub(crate) fn soft(alpha: f32) -> Hsla {
    hsla(0.58, 0.05, 0.28, alpha)
}

/// Hairline tracks and inactive marks.
pub(crate) fn track(alpha: f32) -> Hsla {
    hsla(0.58, 0.06, 0.24, alpha)
}

/// Solid glyphs, progress fill, primary marks.
pub(crate) fn fill(alpha: f32) -> Hsla {
    hsla(0.58, 0.06, 0.16, alpha)
}

/// Hover or selected control surface.
pub(crate) fn surface(alpha: f32) -> Hsla {
    hsla(0.58, 0.08, 0.22, alpha)
}
