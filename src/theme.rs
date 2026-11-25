//! The clear-glass palette: white ink over the refracted wallpaper.
//!
//! The reference material has no scrim: its face is the page itself, so the
//! ink has to read against whatever is behind the shell. White text is the
//! reference's choice and it is what the dock's dark wallpaper needs. The
//! tokens below are the same hue family, stepped by lightness, so icons,
//! tracks, and hover surfaces still read as one material.

use gpui_kit::{Hsla, hsla};

/// Primary text and icon color.
pub(crate) const INK: u32 = 0x00f4_f6f9;

/// Secondary text: album lines, meter labels, the clock.
pub(crate) fn soft(alpha: f32) -> Hsla {
    hsla(0.58, 0.05, 0.86, alpha)
}

/// Hairline tracks and inactive marks.
pub(crate) fn track(alpha: f32) -> Hsla {
    hsla(0.58, 0.06, 0.90, alpha)
}

/// Solid glyphs, progress fill, primary marks.
pub(crate) fn fill(alpha: f32) -> Hsla {
    hsla(0.58, 0.06, 0.98, alpha)
}

/// Hover or selected control surface.
pub(crate) fn surface(alpha: f32) -> Hsla {
    hsla(0.58, 0.08, 0.92, alpha)
}
