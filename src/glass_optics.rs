//! Pure liquid-glass optics: a rounded-rect SDF, a bevel profile bent by
//! Snell, and one CPU resampling pass that displaces pixels along the SDF
//! normal.
//!
//! `wallpaper::RawGlass` warps here before the image reaches GPUI, so the
//! lens paints once and the UI thread never resamples. Nothing in this
//! module touches GPUI or the window.

/// A logical shell shape: the island's width, height, and corner radius.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlassShape {
    /// Interpolated width, logical pixels.
    pub width: f32,
    /// Interpolated height, logical pixels.
    pub height: f32,
    /// Corner radius, logical pixels.
    pub radius: f32,
}

impl GlassShape {
    /// "No warp baked": `lens` treats a backdrop with this shape as an
    /// unwarped crop.
    pub const ZERO: Self = Self { width: 0.0, height: 0.0, radius: 0.0 };
}

impl From<rustedge::island::Morph> for GlassShape {
    fn from(morph: rustedge::island::Morph) -> Self {
        Self { width: morph.width, height: morph.height, radius: morph.radius }
    }
}

/// A shell rect in image pixels, relative to the crop's top-left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shell {
    /// Left edge, image pixels from the crop's left.
    pub x: f32,
    /// Top edge, image pixels from the crop's top.
    pub y: f32,
    /// Shell width in image pixels.
    pub width: f32,
    /// Shell height in image pixels.
    pub height: f32,
    /// Corner radius in image pixels.
    pub radius: f32,
}

impl Shell {
    fn center(&self) -> (f32, f32) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    fn half(&self) -> (f32, f32) {
        (self.width / 2.0, self.height / 2.0)
    }
}

/// Available sampling distance from the shell edge to the image edge, in
/// image pixels. `warp` derives it from the shell and the image bounds, so a
/// centered capsule may still sample the stage interior beside it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Margins {
    /// Distance from the shell's left edge to the image's left edge.
    pub left: f32,
    /// Distance from the shell's top edge to the image's top edge.
    pub top: f32,
    /// Distance from the shell's right edge to the image's right edge.
    pub right: f32,
    /// Distance from the shell's bottom edge to the image's bottom edge.
    pub bottom: f32,
}

/// An axis-aligned rect in whatever frame the caller uses: image pixels for
/// crops, logical pixels for the window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Extent along x.
    pub width: f32,
    /// Extent along y.
    pub height: f32,
}

impl Rect {
    /// The all-zero rect, for `CropSpec::EMPTY` and "no warp" sentinels.
    pub const ZERO: Self = Self { x: 0.0, y: 0.0, width: 0.0, height: 0.0 };
}

/// Signed distance to a rounded rectangle: negative inside, zero on the
/// edge, positive outside.
#[must_use]
pub fn sd_round_rect(p: (f32, f32), half: (f32, f32), radius: f32) -> f32 {
    let qx = p.0.abs() - half.0 + radius;
    let qy = p.1.abs() - half.1 + radius;
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - radius
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests assert the precondition themselves")]
mod tests {
    use super::*;

    const HALF: (f32, f32) = (210.0, 144.0); // the 420x288 card
    const RADIUS: f32 = 24.0;

    fn near(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-3, "{a} != {b}");
    }

    #[test]
    fn the_shape_takes_only_morph_geometry() {
        let morph = rustedge::island::Morph {
            width: 320.0,
            height: 56.0,
            radius: 28.0,
            shadow: 12.0,
            detail: 0.5,
        };
        let shape = GlassShape::from(morph);
        near(shape.width, 320.0);
        near(shape.height, 56.0);
        near(shape.radius, 28.0);
    }

    #[test]
    fn sdf_is_zero_on_the_edge_negative_inside_positive_outside() {
        near(sd_round_rect((0.0, -144.0), HALF, RADIUS), 0.0);
        near(sd_round_rect((210.0, 0.0), HALF, RADIUS), 0.0);
        assert!(sd_round_rect((0.0, -100.0), HALF, RADIUS) < 0.0);
        assert!(sd_round_rect((0.0, -200.0), HALF, RADIUS) > 0.0);
    }

    #[test]
    fn sdf_corner_distance_is_radial_from_the_corner_center() {
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let cx = HALF.0 - RADIUS;
        let cy = HALF.1 - RADIUS;
        let on_arc = (cx + RADIUS * s, -(cy + RADIUS * s));
        assert!(sd_round_rect(on_arc, HALF, RADIUS).abs() < 1e-3);
        let out = (on_arc.0 + 8.0 * s, on_arc.1 - 8.0 * s);
        near(sd_round_rect(out, HALF, RADIUS), 8.0);
    }
}
