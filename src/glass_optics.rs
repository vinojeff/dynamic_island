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

/// Unit *outward* normal of the SDF at `p`, or `None` on the medial axis
/// where the central difference collapses.
#[must_use]
pub fn sdf_normal(p: (f32, f32), half: (f32, f32), radius: f32) -> Option<(f32, f32)> {
    const EPS: f32 = 1.0;
    let gx =
        sd_round_rect((p.0 + EPS, p.1), half, radius) - sd_round_rect((p.0 - EPS, p.1), half, radius);
    let gy =
        sd_round_rect((p.0, p.1 + EPS), half, radius) - sd_round_rect((p.0, p.1 - EPS), half, radius);
    let len = (gx * gx + gy * gy).sqrt();
    if len < 0.1 {
        None
    } else {
        Some((gx / len, gy / len))
    }
}

/// Bevel band width for one shape, in logical pixels. Grows with the shorter
/// side, clamps to a usable band, and never crosses the midline.
#[must_use]
pub fn bevel_width(shape: GlassShape) -> f32 {
    let min = shape.width.min(shape.height);
    (min * 0.18).clamp(10.0, 36.0).min(min / 2.0)
}

/// LUT resolution for the bend profile. 64 entries are far denser than the
/// band reads.
const BEND_STEPS: usize = 64;
/// Superellipse exponent of the bevel height profile.
const BEVEL_POWER: f32 = 4.0;
/// Glass index of refraction, air → glass.
const REFRACTIVE_INDEX: f32 = 1.5;
/// Fraction of the bevel where the inner-edge taper starts.
const TAPER_START: f32 = 0.75;
/// Extra bend at the 45° corner, on top of the base Snell bend.
pub(crate) const CORNER_BOOST: f32 = 0.30;

/// `tan(θi - θt)` for a surface slope `dh/dd`, air → glass. A vertical wall
/// (`INFINITY`) saturates at `sqrt(n²-1)`.
fn snell_bend(slope: f32) -> f32 {
    let theta_i = slope.atan();
    let sin_t = (theta_i.sin() / REFRACTIVE_INDEX).clamp(-1.0, 1.0);
    (theta_i - sin_t.asin()).tan()
}

fn index_limit() -> f32 {
    (REFRACTIVE_INDEX * REFRACTIVE_INDEX - 1.0).sqrt()
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Normalized Snell bend at bevel progress `e` (0 at the silhouette, 1 at
/// the inner edge).
///
/// The height profile is `h(e) = bevel * (1-(1-e)^p)^(1/p)` with the height
/// equal to the bevel width, so `dh/dd` cancels the bevel and the curve is
/// shape-independent. The raw bend is divided by its edge limit and tapered
/// to zero at the inner edge for a C1 handoff to the flat center.
#[must_use]
pub fn bend_at(e: f32) -> f32 {
    let e = e.clamp(0.0, 1.0);
    if e >= 1.0 {
        return 0.0;
    }
    let inward = 1.0 - e;
    // dh/dd = (1-e)^(p-1) * (1-(1-e)^p)^(1/p - 1); e = 0 is a vertical wall.
    let base = 1.0 - inward.powf(BEVEL_POWER);
    let slope = if base <= 0.0 {
        f32::INFINITY
    } else {
        inward.powf(BEVEL_POWER - 1.0) * base.powf(1.0 / BEVEL_POWER - 1.0)
    };
    let bend = snell_bend(slope) / index_limit();
    bend * (1.0 - smoothstep(TAPER_START, 1.0, e))
}

/// The bend profile sampled once per warp pass.
#[must_use]
pub fn bend_lut() -> [f32; BEND_STEPS] {
    let mut lut = [0.0; BEND_STEPS];
    for (step, slot) in lut.iter_mut().enumerate() {
        #[allow(
            clippy::cast_precision_loss,
            reason = "BEND_STEPS is 64; the quotient is exact in f32"
        )]
        let e = step as f32 / (BEND_STEPS - 1) as f32;
        *slot = bend_at(e);
    }
    lut
}

/// Corner emphasis: `sin²(2θ)` — 0 on straight edges, 1 at the 45° point.
#[must_use]
pub fn corner_term(n: (f32, f32)) -> f32 {
    4.0 * n.0 * n.0 * n.1 * n.1
}

/// Sample offset for one output pixel, in image pixels. `None` copies the
/// source pixel: clearly outside the shell (more than a pixel), past the
/// bevel, or on the medial axis.
#[must_use]
pub fn displacement(
    lut: &[f32; BEND_STEPS],
    p: (f32, f32),
    shell: Shell,
    bevel_px: f32,
    margins: Margins,
) -> Option<(f32, f32)> {
    if !(bevel_px > 0.0) {
        return None;
    }
    let center = shell.center();
    let half = shell.half();
    let local = (p.0 - center.0, p.1 - center.1);
    let d = -sd_round_rect(local, half, shell.radius);
    // Pixel centers sit half a pixel off the geometry, and the exact arc
    // point carries float error, so centers up to a pixel outside the
    // silhouette are still edge pixels: clamp them onto the silhouette
    // instead of leaving a one-pixel gap in the band.
    if d <= -1.0 || d >= bevel_px {
        return None;
    }
    let d = d.max(0.0);
    let n = sdf_normal(local, half, shell.radius)?;
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "e is clamped to 0..=1 and BEND_STEPS is 64"
    )]
    let step = (e_of(d, bevel_px) * (BEND_STEPS - 1) as f32).round() as usize;
    let depth = 0.8 * bevel_px;
    let bend = lut.get(step).copied().unwrap_or(0.0) * depth
        * (1.0 + CORNER_BOOST * corner_term(n));
    Some((
        (n.0 * bend).clamp(-margins.left, margins.right),
        (n.1 * bend).clamp(-margins.top, margins.bottom),
    ))
}

fn e_of(d: f32, bevel_px: f32) -> f32 {
    (d / bevel_px).clamp(0.0, 1.0)
}

/// The shell rect inside one crop, in image pixels.
///
/// `stage_width` is the unclipped stage width in logical pixels — the same
/// measure `crop_rects` centers against — and `crop` is the crop rect
/// relative to the window's top-left, so a window hanging off the screen
/// yields a negative `shell.x`.
#[must_use]
pub fn shell_rect(stage_width: f32, shape: GlassShape, crop: Rect, image_scale: f32) -> Shell {
    let (shell, _) = crop_rects(stage_width, shape, crop);
    Shell {
        x: (shell.x - crop.x) * image_scale,
        y: (shell.y - crop.y) * image_scale,
        width: shape.width * image_scale,
        height: shape.height * image_scale,
        radius: shape.radius * image_scale,
    }
}

/// The shell rect and the crop's natural rect, in logical pixels relative to
/// the window's top-left. The shell's top sits on the window's top edge and
/// the shell is centered horizontally; `crop` is returned unchanged.
#[must_use]
pub fn crop_rects(stage_width: f32, shape: GlassShape, crop: Rect) -> (Rect, Rect) {
    let offset = (stage_width - shape.width) / 2.0;
    let shell = Rect { x: offset, y: 0.0, width: shape.width, height: shape.height };
    (shell, crop)
}

/// Maps the crop's natural rect from the baked shell's frame onto the current
/// shell's frame, axially. `lens` uses this while a warp for the new shape is
/// still in flight, so the old bevel rides the new edge instead of ghosting
/// inside the panel. Callers must pass a non-degenerate `baked_shell`.
#[must_use]
pub fn stale_image_bounds(natural: Rect, baked_shell: Rect, current_shell: Rect) -> Rect {
    let sx = current_shell.width / baked_shell.width;
    let sy = current_shell.height / baked_shell.height;
    Rect {
        x: current_shell.x + (natural.x - baked_shell.x) * sx,
        y: current_shell.y + (natural.y - baked_shell.y) * sy,
        width: natural.width * sx,
        height: natural.height * sy,
    }
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

    #[test]
    fn normals_point_straight_out_on_edges_and_diagonally_at_corners() {
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let top = sdf_normal((0.0, -144.0), HALF, RADIUS).unwrap();
        near(top.0, 0.0);
        near(top.1, -1.0);
        let right = sdf_normal((210.0, 0.0), HALF, RADIUS).unwrap();
        near(right.0, 1.0);
        near(right.1, 0.0);
        let cx = HALF.0 - RADIUS;
        let cy = HALF.1 - RADIUS;
        let corner = sdf_normal((cx + RADIUS * s, -(cy + RADIUS * s)), HALF, RADIUS).unwrap();
        near(corner.0, s);
        near(corner.1, -s);
    }

    #[test]
    fn the_top_tangent_keeps_a_vertical_normal() {
        // Regression: the old radial lens tilted this point ~52°.
        let tangent = sdf_normal((HALF.0 - RADIUS, -HALF.1), HALF, RADIUS).unwrap();
        let tilt = tangent.0.abs().atan2(tangent.1.abs()).to_degrees();
        assert!(tilt < 5.0, "tangent normal tilted {tilt}° from vertical");
    }

    #[test]
    fn a_thin_capsule_medial_axis_has_no_normal() {
        let half = (160.0, 6.0);
        assert!(sdf_normal((0.0, 0.0), half, 6.0).is_none());
    }

    #[test]
    fn bevel_clamps_by_shape_size() {
        let card = GlassShape { width: 420.0, height: 288.0, radius: 24.0 };
        near(bevel_width(card), 36.0);
        let capsule = GlassShape { width: 320.0, height: 56.0, radius: 28.0 };
        assert!((bevel_width(capsule) - 10.08).abs() < 1e-3);
        let thin = GlassShape { width: 100.0, height: 12.0, radius: 6.0 };
        near(bevel_width(thin), 6.0);
        let tiny = GlassShape { width: 8.0, height: 8.0, radius: 4.0 };
        near(bevel_width(tiny), 4.0);
    }

    #[test]
    fn snell_bend_matches_known_slopes() {
        near(snell_bend(0.0), 0.0);
        // slope 1: tan(45° - asin(sin45°/1.5)) ≈ 0.3034
        assert!((snell_bend(1.0) - 0.3034).abs() < 2e-3);
        // a vertical wall bends hardest: sqrt(n²-1)
        near(snell_bend(f32::INFINITY), (1.5f32 * 1.5 - 1.0).sqrt());
    }

    #[test]
    fn bend_runs_from_one_to_zero_and_falls_monotonically() {
        let lut = bend_lut();
        near(lut[0], 1.0);
        near(lut[63], 0.0);
        for pair in lut.windows(2) {
            assert!(pair[0] >= pair[1], "bend must not grow inward: {pair:?}");
        }
    }

    #[test]
    fn corner_term_peaks_at_45_degrees() {
        near(corner_term((0.0, -1.0)), 0.0);
        near(corner_term((1.0, 0.0)), 0.0);
        let s = std::f32::consts::FRAC_1_SQRT_2;
        near(corner_term((s, -s)), 1.0);
        near(corner_term((0.5, -0.866_025_4)), 0.75);
    }

    fn card_shell() -> Shell {
        Shell { x: 48.0, y: 48.0, width: 420.0, height: 288.0, radius: 24.0 }
    }

    fn even_margins() -> Margins {
        Margins { left: 48.0, top: 48.0, right: 48.0, bottom: 48.0 }
    }

    fn top_left_45(shell: Shell) -> (f32, f32) {
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let cx = shell.x + shell.width / 2.0;
        let cy = shell.y + shell.height / 2.0;
        let kx = cx - shell.width / 2.0 + shell.radius;
        let ky = cy - shell.height / 2.0 + shell.radius;
        (kx - shell.radius * s, ky - shell.radius * s)
    }

    #[test]
    fn the_top_edge_pulls_straight_up_by_the_depth() {
        let shell = card_shell();
        let offset =
            displacement(&bend_lut(), (shell.x + 210.0, shell.y), shell, 36.0, even_margins())
                .unwrap();
        near(offset.0, 0.0);
        near(offset.1, -(0.8 * 36.0));
    }

    #[test]
    fn the_45_degree_corner_pulls_along_the_normal_and_is_boosted() {
        let shell = card_shell();
        let (bx, by) =
            displacement(&bend_lut(), top_left_45(shell), shell, 36.0, even_margins()).unwrap();
        let len = (bx * bx + by * by).sqrt();
        near(len, 0.8 * 36.0 * 1.3);
        let s = std::f32::consts::FRAC_1_SQRT_2;
        near(bx / len, -s);
        near(by / len, -s);
    }

    #[test]
    fn the_axis_clamp_spares_the_other_component() {
        let shell = card_shell();
        let margins = Margins { left: 0.0, top: 48.0, right: 48.0, bottom: 48.0 };
        let (bx, by) = displacement(&bend_lut(), top_left_45(shell), shell, 36.0, margins).unwrap();
        near(bx, 0.0);
        assert!(by < -20.0, "the vertical bend must survive the x clamp: {by}");
    }

    #[test]
    fn outside_and_center_pixels_are_copies() {
        let shell = card_shell();
        let lut = bend_lut();
        let above = (shell.x + 210.0, shell.y - 10.0);
        assert!(displacement(&lut, above, shell, 36.0, even_margins()).is_none());
        let center = (shell.x + 210.0, shell.y + 144.0);
        assert!(displacement(&lut, center, shell, 36.0, even_margins()).is_none());
    }

    #[test]
    fn the_bend_never_reaches_past_the_margin() {
        let shell = card_shell();
        let lut = bend_lut();
        let (bx, by) =
            displacement(&lut, top_left_45(shell), shell, 36.0, even_margins()).unwrap();
        assert!(bx.abs() <= 48.0 && by.abs() <= 48.0);
    }

    #[test]
    fn tangent_and_midpoint_bend_alike_and_fall_off_inward() {
        let shell = card_shell();
        let lut = bend_lut();
        let tangent =
            displacement(&lut, (shell.x + 186.0, shell.y), shell, 36.0, even_margins()).unwrap();
        let midpoint =
            displacement(&lut, (shell.x + 210.0, shell.y), shell, 36.0, even_margins()).unwrap();
        let tangent_len = (tangent.0 * tangent.0 + tangent.1 * tangent.1).sqrt();
        let mid_len = (midpoint.0 * midpoint.0 + midpoint.1 * midpoint.1).sqrt();
        assert!((tangent_len - mid_len).abs() < 0.1 * mid_len);
        let inner =
            displacement(&lut, (shell.x + 210.0, shell.y + 4.0), shell, 36.0, even_margins())
                .unwrap();
        let inner_len = (inner.0 * inner.0 + inner.1 * inner.1).sqrt();
        assert!(inner_len < mid_len, "the bend must fall off inward");
    }

    fn card_shape() -> GlassShape {
        GlassShape { width: 420.0, height: 288.0, radius: 24.0 }
    }

    fn capsule_shape() -> GlassShape {
        GlassShape { width: 320.0, height: 56.0, radius: 28.0 }
    }

    fn stage_crop_rect() -> Rect {
        Rect { x: -48.0, y: -48.0, width: 516.0, height: 456.0 }
    }

    #[test]
    fn shell_rect_centers_the_card_and_offsets_the_capsule() {
        let shell = shell_rect(420.0, card_shape(), stage_crop_rect(), 1.0);
        near(shell.x, 48.0);
        near(shell.y, 48.0);
        near(shell.width, 420.0);
        near(shell.radius, 24.0);
        let shell = shell_rect(420.0, capsule_shape(), stage_crop_rect(), 1.25);
        near(shell.x, (50.0 + 48.0) * 1.25);
        near(shell.width, 400.0);
    }

    #[test]
    fn an_offscreen_window_shifts_the_shell_negative() {
        // The window hangs 10 physical px off the left edge (scale 1): the
        // crop starts 10 px right of the window origin.
        let crop = Rect { x: 10.0, y: 0.0, width: 458.0, height: 408.0 };
        let shell = shell_rect(420.0, card_shape(), crop, 1.0);
        near(shell.x, -10.0);
        near(shell.y, 0.0);
    }

    #[test]
    fn crop_rects_centers_the_shell_and_returns_the_natural_crop() {
        let (shell, natural) = crop_rects(420.0, card_shape(), stage_crop_rect());
        near(shell.x, 0.0);
        near(shell.y, 0.0);
        near(shell.width, 420.0);
        near(natural.x, -48.0);
        near(natural.width, 516.0);
        let (shell, _) = crop_rects(420.0, capsule_shape(), stage_crop_rect());
        near(shell.x, 50.0);
        near(shell.y, 0.0);
    }

    #[test]
    fn stale_image_bounds_maps_the_baked_shell_onto_the_current_one() {
        let natural = stage_crop_rect();
        let baked = Rect { x: 50.0, y: 0.0, width: 320.0, height: 56.0 };
        let current = Rect { x: 0.0, y: 0.0, width: 420.0, height: 288.0 };
        let image = stale_image_bounds(natural, baked, current);
        let sx = 420.0 / 320.0;
        let sy = 288.0 / 56.0;
        near(image.x, (-48.0 - 50.0) * sx);
        assert!((image.y - (-48.0 * sy)).abs() < 0.01);
        near(image.width, 516.0 * sx);
        assert!((image.height - 456.0 * sy).abs() < 0.01);
        // Same shape: identity.
        let same = stale_image_bounds(natural, current, current);
        near(same.x, natural.x);
        near(same.width, natural.width);
    }
}
