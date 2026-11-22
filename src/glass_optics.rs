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
/// Extra bend at the 45° corner, on top of the base Snell bend. The
/// reference demo boosts corners to roughly the edge pull's magnitude.
pub(crate) const CORNER_BOOST: f32 = 0.50;
/// `depth / bevel`: how deep into the band the pull reaches.
const DEPTH_FACTOR: f32 = 0.8;
/// Max growth of the sample offset between adjacent pixels, in image pixels.
///
/// The face blur (σ2.5, matching the reference's `blurRadius 5`) suppresses
/// minification aliasing, so the profile can ride a steeper cap line and
/// keep the punchy edge the reference has. Without the blur this must stay
/// ≤ 2 or the edge aliases into jaggies.
const MAX_SAMPLE_STEP: f32 = 4.0;
/// Gaussian-approximating blur over the whole face before the warp, in
/// pixels — the reference demo's `blurRadius 5` translates to σ2.5.
pub const FACE_BLUR_SIGMA: f32 = 2.5;

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

/// The un-capped normalized Snell curve — the shape the LUT integrates.
fn raw_bend(e: f32) -> f32 {
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
///
/// Integrated from `raw_bend` with the per-pixel sample step capped at
/// [`MAX_SAMPLE_STEP`]; outside the cap the curve is the raw Snell shape, at
/// the silhouette it rides the cap line. Reaches zero before the inner edge
/// and stays there, which blends seamlessly into the copied center.
#[must_use]
pub fn bend_lut() -> [f32; BEND_STEPS] {
    #[allow(
        clippy::cast_precision_loss,
        reason = "BEND_STEPS is 64; the quotients are exact in f32"
    )]
    let dt = 1.0 / (BEND_STEPS - 1) as f32;
    // Δoffset per pixel = depth·Δlut and Δd = bevel·dt, so capping the step
    // at MAX_SAMPLE_STEP caps Δlut per entry at the quotient below.
    #[allow(
        clippy::cast_precision_loss,
        reason = "BEND_STEPS is 64; the quotient is exact in f32"
    )]
    let max_lut_step = MAX_SAMPLE_STEP / (DEPTH_FACTOR * (BEND_STEPS - 1) as f32);
    let mut lut = [0.0; BEND_STEPS];
    lut[0] = 1.0;
    let mut remaining = 1.0;
    for step in 1..BEND_STEPS {
        #[allow(
            clippy::cast_precision_loss,
            reason = "BEND_STEPS is 64; the conversions are exact in f32"
        )]
        let e = (step as f32 - 0.5) * dt;
        // raw_bend decreases inward, so the drop between the neighbours is
        // the positive quantity. Beyond the cap the profile rides the cap
        // line; near the end it accelerates along the even split so the
        // curve lands exactly on zero at the inner edge.
        let raw_drop = (raw_bend(e - dt) - raw_bend(e + dt)) / (2.0 * dt) * dt;
        #[allow(
            clippy::cast_precision_loss,
            reason = "BEND_STEPS is 64; the quotient is exact in f32"
        )]
        let steps_left = (BEND_STEPS - step) as f32;
        let descent = (raw_drop.max(remaining / steps_left)).min(max_lut_step);
        let previous = lut.get(step - 1).copied().unwrap_or(1.0);
        let value = (previous - descent).max(0.0);
        if let Some(slot) = lut.get_mut(step) {
            *slot = value;
        }
        remaining = (remaining - descent).max(0.0);
    }
    lut
}

/// Corner emphasis: `sin²(2θ)` — 0 on straight edges, 1 at the 45° point.
#[must_use]
pub fn corner_term(n: (f32, f32)) -> f32 {
    4.0 * n.0 * n.0 * n.1 * n.1
}

/// Three clamped-edge box passes approximate a Gaussian of `sigma` — the
/// reference demo blurs the whole face (σ2.5) before anything else, which is
/// where its softness lives. In-place, O(1) per pixel via running sums.
pub fn blur(rgba: &mut [u8], width: usize, height: usize, sigma: f32) {
    if width == 0 || height == 0 || rgba.len() != width * height * 4 || sigma <= 0.0 || sigma.is_nan()
    {
        return;
    }
    // Three box passes of radius r approximate sigma ≈ sqrt(r(r+1)).
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "sigma is a small constant; the radius rounds to 2"
    )]
    let radius = ((sigma * 0.577 + 0.5) as usize).max(1);
    let mut temp = vec![0u8; rgba.len()];
    for _ in 0..3 {
        box_pass(rgba, &mut temp, width, height, radius, true);
        box_pass(&temp, rgba, width, height, radius, false);
    }
}

/// One clamped-edge box pass over rows (`horizontal`) or columns. The running
/// window sum keeps every pixel O(1); out-of-range steps clamp to the border.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "the window division is 0..=255 by construction; window itself is a 5-tap count; pixel offsets are tiny"
)]
fn box_pass(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    height: usize,
    radius: usize,
    horizontal: bool,
) {
    fn add_pixel(src: &[u8], base: usize, sums: &mut [u32; 4]) {
        if let Some(tap) = src.get(base..base + 4) {
            for (sum, value) in sums.iter_mut().zip(tap) {
                *sum += u32::from(*value);
            }
        }
    }
    fn sub_pixel(src: &[u8], base: usize, sums: &mut [u32; 4]) {
        if let Some(tap) = src.get(base..base + 4) {
            for (sum, value) in sums.iter_mut().zip(tap) {
                *sum -= u32::from(*value);
            }
        }
    }
    let (lines, line_len) = if horizontal { (height, width) } else { (width, height) };
    if lines == 0 || line_len == 0 {
        return;
    }
    let last = line_len - 1;
    #[allow(
        clippy::cast_precision_loss,
        reason = "the window is 5 taps; the conversion is exact in u32 range"
    )]
    let window = (2 * radius + 1) as u32;
    for line in 0..lines {
        let base_of = |i: usize| {
            if horizontal {
                (line * width + i) * 4
            } else {
                (i * width + line) * 4
            }
        };
        let mut sums = [0u32; 4];
        // Seed the clamped window at i = 0: pixel 0 stands in for every
        // out-of-range step, pixels 1..=radius follow.
        for _ in 0..=radius {
            add_pixel(src, base_of(0), &mut sums);
        }
        for j in 1..=radius.min(last) {
            add_pixel(src, base_of(j), &mut sums);
        }
        for i in 0..line_len {
            let pixel = [
                (sums[0] / window) as u8,
                (sums[1] / window) as u8,
                (sums[2] / window) as u8,
                (sums[3] / window) as u8,
            ];
            let out_base = base_of(i);
            for (offset, value) in pixel.iter().enumerate() {
                if let Some(slot) = dst.get_mut(out_base + offset) {
                    *slot = *value;
                }
            }
            // Slide the window toward i + 1: enter the right edge, leave the left.
            if i + 1 < line_len {
                add_pixel(src, base_of((i + 1 + radius).min(last)), &mut sums);
                let leaving = (i as i32 - radius as i32).clamp(0, last as i32) as usize;
                sub_pixel(src, base_of(leaving), &mut sums);
            }
        }
    }
}

/// Sample offset for one output pixel, in image pixels. `None` copies the
/// source pixel: clearly outside the shell (more than a pixel), past the
/// bevel, or on the medial axis.
#[must_use]
pub fn displacement(
    lut: &[f32; BEND_STEPS],
    pixel: (f32, f32),
    shell: Shell,
    bevel_px: f32,
    margins: Margins,
) -> Option<(f32, f32)> {
    if bevel_px <= 0.0 || bevel_px.is_nan() {
        return None;
    }
    let center = shell.center();
    let half = shell.half();
    let local = (pixel.0 - center.0, pixel.1 - center.1);
    let d = -sd_round_rect(local, half, shell.radius);
    // Pixel centers sit half a pixel off the geometry, and the exact arc
    // point carries float error, so centers up to a pixel outside the
    // silhouette are still edge pixels: clamp them onto the silhouette
    // instead of leaving a one-pixel gap in the band.
    if d <= -1.0 || d >= bevel_px {
        return None;
    }
    let d = d.max(0.0);
    let normal = sdf_normal(local, half, shell.radius)?;
    // Linear interpolation between LUT entries: nearest-neighbour lookup
    // quantized the bend into visible micro-rings exactly where the profile
    // is steepest.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "e is clamped to 0..=1 and BEND_STEPS is 64"
    )]
    let index = {
        let x = e_of(d, bevel_px) * (BEND_STEPS - 1) as f32;
        let index = x.floor() as usize;
        let t = x - index as f32;
        (index, t)
    };
    let (index, t) = index;
    let low = lut.get(index).copied().unwrap_or(0.0);
    let high = lut.get(index + 1).copied().unwrap_or(low);
    let depth = DEPTH_FACTOR * bevel_px;
    let bend = (low + (high - low) * t) * depth * (1.0 + CORNER_BOOST * corner_term(normal));
    Some((
        (normal.0 * bend).clamp(-margins.left, margins.right),
        (normal.1 * bend).clamp(-margins.top, margins.bottom),
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

/// Fraction of the bend used to spread R/G/B sample positions.
pub const DISPERSION: f32 = 0.03;

/// One CPU resampling pass: the whole face is blurred (σ2.5, the reference
/// demo's softness), then the bevel band is displaced along the SDF normal
/// with R/G/B sampling slightly different positions for dispersion; the rest
/// of the blurred face is kept verbatim. Returns `None` when `src` does not
/// match `width * height * 4`.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "pixel counts are far under f32's 24-bit mantissa"
)]
pub fn warp(
    src: &[u8],
    width: usize,
    height: usize,
    shell: Shell,
    bevel_px: f32,
    blur_sigma: f32,
) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || src.len() != width * height * 4 {
        return None;
    }
    let mut blurred = src.to_vec();
    blur(&mut blurred, width, height, blur_sigma);
    let mut out = blurred.clone();
    if bevel_px <= 0.0 || bevel_px.is_nan() {
        return Some(out);
    }
    let margins = Margins {
        left: shell.x.max(0.0),
        top: shell.y.max(0.0),
        right: (width as f32 - (shell.x + shell.width)).max(0.0),
        bottom: (height as f32 - (shell.y + shell.height)).max(0.0),
    };
    let lut = bend_lut();
    for (y, row) in out.chunks_exact_mut(width * 4).enumerate() {
        for (x, chunk) in row.chunks_exact_mut(4).enumerate() {
            let p = (x as f32 + 0.5, y as f32 + 0.5);
            let Some((bx, by)) = displacement(&lut, p, shell, bevel_px, margins) else {
                continue;
            };
            let red = sample_bilinear(
                &blurred,
                width,
                height,
                p.0 + bx * (1.0 - DISPERSION),
                p.1 + by * (1.0 - DISPERSION),
            );
            let green = sample_bilinear(&blurred, width, height, p.0 + bx, p.1 + by);
            let blue = sample_bilinear(
                &blurred,
                width,
                height,
                p.0 + bx * (1.0 + DISPERSION),
                p.1 + by * (1.0 + DISPERSION),
            );
            if let [r, g, b, a] = chunk {
                *r = red[0];
                *g = green[1];
                *b = blue[2];
                *a = green[3];
            }
        }
    }
    Some(out)
}

/// Clamped bilinear RGBA sample.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped coordinates index the crop; the result rounds back to 0..=255"
)]
fn sample_bilinear(src: &[u8], width: usize, height: usize, x: f32, y: f32) -> [u8; 4] {
    let x = x.clamp(0.0, (width - 1) as f32);
    let y = y.clamp(0.0, (height - 1) as f32);
    let x0 = x.floor() as usize;
    let y0 = y.floor() as usize;
    let x1 = (x0 + 1).min(width - 1);
    let y1 = (y0 + 1).min(height - 1);
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let at = |px: usize, py: usize| {
        let base = (py * width + px) * 4;
        [
            f32::from(src.get(base).copied().unwrap_or(0)),
            f32::from(src.get(base + 1).copied().unwrap_or(0)),
            f32::from(src.get(base + 2).copied().unwrap_or(0)),
            f32::from(src.get(base + 3).copied().unwrap_or(0)),
        ]
    };
    let (tl, tr, bl, br) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
    let corners = tl.iter().zip(tr.iter()).zip(bl.iter().zip(br.iter()));
    let mut out = [0u8; 4];
    for (slot, ((&tl, &tr), (&bl, &br))) in out.iter_mut().zip(corners) {
        let top = tl + (tr - tl) * fx;
        let bottom = bl + (br - bl) * fx;
        let value = top + (bottom - top) * fy;
        *slot = value.round().clamp(0.0, 255.0) as u8;
    }
    out
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
        near(lut.first().copied().unwrap(), 1.0);
        near(lut.last().copied().unwrap(), 0.0);
        for pair in lut.windows(2) {
            if let [a, b] = pair {
                assert!(a >= b, "bend must not grow inward: {a} -> {b}");
            }
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
        near(len, 0.8 * 36.0 * 1.5);
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

    #[test]
    fn adjacent_rows_never_sample_more_than_two_pixels_apart() {
        // Regression: the raw Snell profile has a vertical wall at the
        // silhouette — the first LUT step moved the sample ~6px over half a
        // pixel of inward distance, an 11:1 minification that bilinear cannot
        // represent. The visible symptom was jaggies along the whole edge.
        let shell = card_shell();
        let lut = bend_lut();
        let mut prev =
            displacement(&lut, (shell.x + 210.0, shell.y), shell, 36.0, even_margins()).unwrap();
        for row in 1..=12 {
            #[allow(
                clippy::cast_precision_loss,
                reason = "row is 1..=12; the conversion is exact"
            )]
            let p = (shell.x + 210.0, shell.y + 0.5 * row as f32);
            let cur = displacement(&lut, p, shell, 36.0, even_margins()).unwrap();
            let step = ((cur.0 - prev.0).abs() + (cur.1 - prev.1).abs()).sqrt();
            assert!(
                step <= 2.2,
                "row {row}: sample moved {step:.2}px between adjacent pixels"
            );
            prev = cur;
        }
    }

    #[test]
    fn the_lut_never_steps_more_than_the_slope_cap() {
        let lut = bend_lut();
        for pair in lut.windows(2) {
            if let [a, b] = pair {
                assert!(a - b <= 0.082, "the profile cliff is back: {a} -> {b} in one step");
            }
        }
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

    fn solid(width: usize, height: usize, rgba: [u8; 4]) -> Vec<u8> {
        let mut out = Vec::with_capacity(width * height * 4);
        for _ in 0..width * height {
            out.extend_from_slice(&rgba);
        }
        out
    }

    fn pixel(buf: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
        let base = (y * width + x) * 4;
        [
            buf.get(base).copied().unwrap_or(0),
            buf.get(base + 1).copied().unwrap_or(0),
            buf.get(base + 2).copied().unwrap_or(0),
            buf.get(base + 3).copied().unwrap_or(0),
        ]
    }

    fn small_shell() -> Shell {
        Shell { x: 20.0, y: 20.0, width: 60.0, height: 60.0, radius: 10.0 }
    }

    #[test]
    fn warp_keeps_the_center_and_the_margin_untouched() {
        let src = solid(100, 100, [10, 20, 30, 255]);
        let out = warp(&src, 100, 100, small_shell(), 10.0, 0.0).unwrap();
        assert_eq!(pixel(&out, 100, 50, 50), [10, 20, 30, 255]);
        assert_eq!(pixel(&out, 100, 2, 2), [10, 20, 30, 255]);
    }

    #[test]
    fn warp_pulls_the_top_edge_from_above() {
        let mut src = solid(100, 100, [0, 0, 255, 255]);
        for row in src.chunks_exact_mut(100 * 4).take(20) {
            for chunk in row.chunks_exact_mut(4) {
                if let [r, g, b, a] = chunk {
                    *r = 255;
                    *g = 0;
                    *b = 0;
                    *a = 255;
                }
            }
        }
        let out = warp(&src, 100, 100, small_shell(), 10.0, 0.0).unwrap();
        let edge = pixel(&out, 100, 50, 20);
        assert!(edge[0] > 200, "the top edge must sample the red margin: {edge:?}");
        assert!(edge[2] < 100, "the top edge must not stay blue: {edge:?}");
    }

    #[test]
    fn warp_preserves_alpha() {
        let src = solid(100, 100, [10, 20, 30, 0]);
        let out = warp(&src, 100, 100, small_shell(), 10.0, 0.0).unwrap();
        assert_eq!(pixel(&out, 100, 50, 50)[3], 0);
        assert_eq!(pixel(&out, 100, 50, 20)[3], 0);
    }

    #[test]
    fn warp_rejects_a_size_mismatch_and_zero_bevel_copies() {
        assert!(warp(&[0, 0, 0, 0], 2, 2, small_shell(), 4.0, 0.0).is_none());
        let src = solid(100, 100, [1, 2, 3, 255]);
        let out = warp(&src, 100, 100, small_shell(), 0.0, 0.0).unwrap();
        assert_eq!(out, src);
    }

    #[test]
    fn blur_keeps_a_uniform_face_and_dimensions() {
        let mut buf = solid(10, 8, [40, 60, 80, 255]);
        blur(&mut buf, 10, 8, 2.5);
        assert_eq!(buf, solid(10, 8, [40, 60, 80, 255]));
    }

    #[test]
    fn blur_softens_a_hard_step_without_touching_the_far_field() {
        let mut buf = Vec::with_capacity(20 * 20 * 4);
        for _y in 0..20 {
            for x in 0..20 {
                let value = if x < 10 { 0 } else { 255 };
                buf.extend_from_slice(&[value, value, value, 255]);
            }
        }
        blur(&mut buf, 20, 20, 2.5);
        let left = pixel(&buf, 20, 1, 10)[0];
        let right = pixel(&buf, 20, 18, 10)[0];
        assert!(left < 10, "the far field keeps black: {left}");
        assert!(right > 245, "the far field keeps white: {right}");
        let at_edge = pixel(&buf, 20, 10, 10)[0];
        assert!(
            (100..=160).contains(&at_edge),
            "the step must soften toward the middle: {at_edge}"
        );
        let mut previous = pixel(&buf, 20, 4, 10)[0];
        for x in 5..16 {
            let value = pixel(&buf, 20, x, 10)[0];
            assert!(value >= previous, "the ramp must be monotone at x={x}");
            previous = value;
        }
    }
}
