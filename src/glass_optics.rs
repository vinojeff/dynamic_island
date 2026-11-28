//! Pure liquid-glass optics: a rounded-rect SDF, the reference shader's
//! exponential edge layers, and one CPU resampling pass that displaces the
//! wallpaper crop along the SDF normal.
//!
//! The model is a faithful CPU port of the fragment shader in
//! `dashersw/liquid-glass-js` (`container.js`) — the look this shell is
//! matched against:
//!
//! 1. `d` is the positive inward distance from the silhouette, in logical
//!    pixels.
//! 2. Three exponential layers stack along the outward normal:
//!    `edge = exp(-d * EDGE_DISTANCE) * EDGE_INTENSITY`,
//!    `rim  = exp(-d * RIM_DISTANCE) * RIM_INTENSITY`, and (only while
//!    [`CENTER_WARP`] is on) `base = (1 - exp(-d * BASE_DISTANCE)) *
//!    BASE_INTENSITY`.
//! 3. A corner term peaks within a few pixels of each rounded corner, and a
//!    tangential ripple rides the rim gain (zero at the target settings).
//! 4. The total intensity is added to *page* UV, so a wide page pulls farther
//!    horizontally than vertically; the crop is at page scale, so the
//!    displacement in crop pixels is `intensity * page * image_scale`.
//! 5. The crop is Gaussian-blurred before sampling. The reference blurs the
//!    same texture at the displaced coordinate; at its `blurRadius / 2` sigma
//!    the two orders are indistinguishable. There is no painted rim, scrim,
//!    sheen, or dispersion: the bright edge in the reference is refracted
//!    content, not a decoration.
//!
//! `wallpaper::RawGlass` warps here before the image reaches GPUI, so the
//! lens paints once and the UI thread never resamples. Nothing in this module
//! touches GPUI or the window.

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
    /// "No warp baked". No live shell has a zero width or height, so a
    /// backdrop carrying this shape never matches and always paints its plain
    /// crop.
    pub const ZERO: Self = Self { width: 0.0, height: 0.0, radius: 0.0 };
}

/// Largest per-axis difference at which a warp still counts as baked for the
/// live shell, logical pixels.
///
/// The edge band is a few pixels wide and its displacement decays
/// exponentially, so a sub-pixel difference moves the refraction by less than
/// a pixel. Anything larger must fall back to the plain crop: the baked bands
/// would sit visibly inside or outside the live edge.
pub const SHAPE_MATCH_EPSILON: f32 = 0.5;

/// Whether a warp baked for `baked` can be painted under `live` as-is.
///
/// Exact equality is too strict: the morph interpolates, so the frame that
/// finishes a morph can land a fraction of a pixel away from the resting
/// shape the warp was requested for.
#[must_use]
pub fn same_shape(baked: GlassShape, live: GlassShape) -> bool {
    (baked.width - live.width).abs() <= SHAPE_MATCH_EPSILON
        && (baked.height - live.height).abs() <= SHAPE_MATCH_EPSILON
        && (baked.radius - live.radius).abs() <= SHAPE_MATCH_EPSILON
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
    fn half(&self) -> (f32, f32) {
        (self.width / 2.0, self.height / 2.0)
    }
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
    let gx = sd_round_rect((p.0 + EPS, p.1), half, radius)
        - sd_round_rect((p.0 - EPS, p.1), half, radius);
    let gy = sd_round_rect((p.0, p.1 + EPS), half, radius)
        - sd_round_rect((p.0, p.1 - EPS), half, radius);
    let len = (gx * gx + gy * gy).sqrt();
    if len < 0.1 { None } else { Some((gx / len, gy / len)) }
}

/// Edge layer gain at the silhouette. The target screenshot's `edgeIntensity`.
pub const EDGE_INTENSITY: f32 = 0.035;
/// Edge layer falloff per logical pixel. `edgeDistance` in the reference.
pub const EDGE_DISTANCE: f32 = 0.240;
/// Rim layer gain. `0.0` in the target screenshot.
pub const RIM_INTENSITY: f32 = 0.0;
/// Rim layer falloff per logical pixel. `rimDistance` in the reference.
pub const RIM_DISTANCE: f32 = 0.5;
/// Center-warp gain. `baseIntensity` in the reference.
pub const BASE_INTENSITY: f32 = 0.009;
/// Center-warp growth per logical pixel. `baseDistance` in the reference.
pub const BASE_DISTANCE: f32 = 0.16;
/// Extra push within a few pixels of each corner. `cornerBoost`.
pub const CORNER_BOOST: f32 = 0.018;
/// Tangential ripple amplitude. `rippleEffect`.
pub const RIPPLE_EFFECT: f32 = 0.06;
/// The reference's "Enable Center Warp" is off in the target screenshot.
pub const CENTER_WARP: bool = false;
/// Gaussian sigma of the face blur, logical pixels.
///
/// The reference computing `sigma = blurRadius / 2`; the target screenshot's
/// `blurRadius` is 1.0, so the face stays crisp and the softness lives in the
/// edge displacement itself.
pub const FACE_BLUR_SIGMA: f32 = 0.5;

/// Longest inward distance the edge layers can still bend anything.
const REFRACTION_SPAN: f32 = 48.0;
/// Corner falloff per logical pixel (reference `exp(-c * 0.3)`).
const CORNER_DISTANCE: f32 = 0.3;
/// Ripple frequency (reference `sin(distFromEdge * 25)`).
const RIPPLE_FREQUENCY: f32 = 25.0;

/// The unweighted edge falloff, shared by the ripple and the edge layer.
fn edge_falloff(d: f32) -> f32 {
    (-d * EDGE_DISTANCE).exp()
}

/// The rim falloff, shared by the ripple and the rim layer.
fn rim_falloff(d: f32) -> f32 {
    (-d * RIM_DISTANCE).exp()
}

/// Sample displacement for one point, in **logical page pixels**.
///
/// `p` is relative to the shell's center in logical pixels. `None` copies the
/// source pixel: outside the shell (more than a pixel), past
/// [`REFRACTION_SPAN`], or on the medial axis.
#[must_use]
fn refraction_offset(
    p: (f32, f32),
    half: (f32, f32),
    radius: f32,
    page: (f32, f32),
) -> Option<(f32, f32)> {
    let width = half.0 * 2.0;
    let height = half.1 * 2.0;
    let min_dim = width.min(height);
    if min_dim <= 0.0 {
        return None;
    }
    let d = -sd_round_rect(p, half, radius);
    // Pixel centers sit half a pixel off the geometry: a center up to one
    // pixel outside the silhouette still gets the silhouette's bend instead
    // of leaving a gap under the antialiased mask.
    if d < -1.0 || (!CENTER_WARP && d >= REFRACTION_SPAN) {
        return None;
    }
    let d = d.max(0.0);
    let normal = sdf_normal(p, half, radius)?;
    let base = if CENTER_WARP { (1.0 - (-d * BASE_DISTANCE).exp()) * BASE_INTENSITY } else { 0.0 };
    let rim = rim_falloff(d) * RIM_INTENSITY;
    let push = edge_falloff(d) * EDGE_INTENSITY + rim + base;
    // The reference measures the corner boost from the *farther* pair of
    // edges: it is largest where both normalized distances are small.
    let nx = ((p.0 + half.0).min(width - p.0 - half.0) / width).clamp(0.0, 0.5);
    let ny = ((p.1 + half.1).min(height - p.1 - half.1) / height).clamp(0.0, 0.5);
    let corner = (-(nx.max(ny) * min_dim) * CORNER_DISTANCE).exp() * CORNER_BOOST;
    // The ripple rides the rim layer, so a zero rim gain means no ripple.
    let ripple = (d / min_dim * RIPPLE_FREQUENCY).sin() * RIPPLE_EFFECT * rim;
    // Outward normal for the layers, tangent for the ripple.
    Some((
        (normal.0 * (push + corner) - normal.1 * ripple) * page.0,
        (normal.1 * (push + corner) + normal.0 * ripple) * page.1,
    ))
}

/// Gaussian blur of an RGBA buffer in place.
///
/// Separable and normalized; `sigma` is in image pixels. A gaussian this
/// narrow is what keeps the reference's face crisp while the edge band is
/// heavily displaced.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "the kernel is at most 49 taps and pixel counts are far under \
              f32's 24-bit mantissa"
)]
pub fn blur(rgba: &mut [u8], width: usize, height: usize, sigma: f32) {
    const SCALE: u32 = 1 << 14;
    if width == 0 || height == 0 || rgba.len() != width * height * 4 {
        return;
    }
    if !sigma.is_finite() || sigma <= 0.0 {
        return;
    }
    // Beyond sigma 8 the reference's 13x13 kernel has already saturated;
    // capping keeps the tap count (and this loop) bounded.
    let sigma = sigma.min(8.0);
    let radius = (sigma * 3.0).ceil() as usize;
    if radius == 0 {
        return;
    }
    let denom = 2.0 * sigma * sigma;
    let mut kernel = Vec::with_capacity(radius * 2 + 1);
    for step in -(radius as i64)..=(radius as i64) {
        let distance = step as f32;
        let weight = (-(distance * distance) / denom).exp();
        kernel.push((weight * SCALE as f32).round() as u32);
    }
    let total: u32 = kernel.iter().sum();
    if total == 0 {
        return;
    }
    let mut temp = vec![0u8; rgba.len()];
    blur_pass(rgba, &mut temp, width, height, &kernel, total, true);
    blur_pass(&temp, rgba, width, height, &kernel, total, false);
}

/// How many row chunks to split an image into.
///
/// Threading only pays for itself on a full-size crop; a small buffer (a test,
/// or the tucked line) stays on one thread.
fn worker_count(rows: usize) -> usize {
    const MIN_ROWS_PER_WORKER: usize = 32;
    if rows < 2 * MIN_ROWS_PER_WORKER {
        return 1;
    }
    let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    cpus.min(8).min(rows / MIN_ROWS_PER_WORKER).max(1)
}

/// Rounded division by the kernel total, via a reciprocal multiply-shift.
///
/// `(sum + total/2) / total` costs one integer division per channel per pixel
/// — millions on a live crop, and the blur was the bulk of a warp. `recip` is
/// `ceil(2^48 / total)`. The kernel always contains its 16384-weight centre
/// tap, so `total >= 16384`, `recip <= 2^34`, and `sum < 255.5 * total < 2^28`
/// keep the product inside `u64`. The rounding error is `sum * e / 2^48` with
/// `e < total <= 802816`, which stays below 1, so the shift is exact.
#[inline]
#[allow(clippy::cast_possible_truncation, reason = "the quotient is at most 255")]
fn div_round(sum: u32, total: u32, recip: u64) -> u8 {
    let rounded = u64::from(sum) + u64::from(total / 2);
    ((rounded * recip) >> 48) as u8
}

/// One separable pass: `horizontal` scans rows, else columns.
///
/// Both orientations produce one output row at a time, so the pass is split
/// into row chunks that threads can own outright. The vertical pass walks tap
/// rows contiguously, which also stops the old column-strided reads.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "the tap weight times u8 stays in u32; offsets are a few pixels"
)]
fn blur_pass(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    height: usize,
    kernel: &[u32],
    total: u32,
    horizontal: bool,
) {
    let recip = (1u64 << 48).div_ceil(u64::from(total));
    let workers = worker_count(height);
    let rows_per = height.div_ceil(workers);
    let row_bytes = width * 4;
    if workers == 1 {
        blur_rows(src, dst, width, height, 0, kernel, total, recip, horizontal);
        return;
    }
    std::thread::scope(|scope| {
        for (chunk, y0) in dst
            .chunks_mut(rows_per * row_bytes)
            .zip((0..).step_by(rows_per))
        {
            scope.spawn(move || {
                blur_rows(src, chunk, width, height, y0, kernel, total, recip, horizontal);
            });
        }
    });
}

#[allow(
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "the row needs the full frame geometry; the tap weight times u8 \
              stays in u32 and offsets are a few pixels"
)]
fn blur_rows(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    height: usize,
    y0: usize,
    kernel: &[u32],
    total: u32,
    recip: u64,
    horizontal: bool,
) {
    let radius = kernel.len() / 2;
    let last_x = width.saturating_sub(1) as i64;
    let last_y = height.saturating_sub(1) as i64;
    if horizontal {
        for (row_index, row) in dst.chunks_exact_mut(width * 4).enumerate() {
            let y = y0 + row_index;
            for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
                let mut sums = [0u32; 4];
                for (tap, weight) in kernel.iter().enumerate() {
                    let offset = tap as i64 - radius as i64;
                    let sx = (x as i64 + offset).clamp(0, last_x) as usize;
                    let base = (y * width + sx) * 4;
                    if let Some(src_pixel) = src.get(base..base + 4) {
                        for (sum, value) in sums.iter_mut().zip(src_pixel) {
                            *sum += weight * u32::from(*value);
                        }
                    }
                }
                for (channel, sum) in pixel.iter_mut().zip(sums) {
                    *channel = div_round(sum, total, recip);
                }
            }
        }
    } else {
        let mut sums = vec![[0u32; 4]; width];
        for (row_index, row) in dst.chunks_exact_mut(width * 4).enumerate() {
            let y = y0 + row_index;
            sums.fill([0u32; 4]);
            for (tap, weight) in kernel.iter().enumerate() {
                let offset = tap as i64 - radius as i64;
                let sy = (y as i64 + offset).clamp(0, last_y) as usize;
                let base = sy * width * 4;
                if let Some(src_row) = src.get(base..base + width * 4) {
                    for (sum, pixel) in sums.iter_mut().zip(src_row.chunks_exact(4)) {
                        for (channel, value) in sum.iter_mut().zip(pixel) {
                            *channel += weight * u32::from(*value);
                        }
                    }
                }
            }
            for (pixel, sum) in row.chunks_exact_mut(4).zip(&sums) {
                for (channel, &sum) in pixel.iter_mut().zip(sum) {
                    *channel = div_round(sum, total, recip);
                }
            }
        }
    }
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

/// One CPU resampling pass: Gaussian-blur the whole crop (the reference blurs
/// after displacement), then bend the edge band along the SDF normal. The
/// rest of the blurred face is kept verbatim.
///
/// `shell` is in image pixels relative to the crop; `image_scale` is image
/// pixels per logical pixel; `page` is the logical desktop size the reference
/// multiplies its intensity by; `blur_sigma` is logical pixels. Returns `None`
/// when the buffer or the scale is unusable.
#[must_use]
#[allow(clippy::cast_precision_loss, reason = "pixel counts are far under f32's 24-bit mantissa")]
pub fn warp(
    src: &[u8],
    width: usize,
    height: usize,
    shell: Shell,
    image_scale: f32,
    page: (f32, f32),
    blur_sigma: f32,
) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || src.len() != width * height * 4 {
        return None;
    }
    if !image_scale.is_finite() || image_scale <= 0.0 {
        return None;
    }
    let mut blurred = src.to_vec();
    blur(&mut blurred, width, height, blur_sigma * image_scale);
    let mut out = blurred.clone();
    let half = shell.half();
    if half.0 <= 0.0 || half.1 <= 0.0 {
        return Some(out);
    }
    let workers = worker_count(height);
    let rows_per = height.div_ceil(workers);
    let row_bytes = width * 4;
    if workers == 1 {
        warp_rows(&blurred, &mut out, width, height, 0, shell, image_scale, page);
    } else {
        let src: &[u8] = &blurred;
        std::thread::scope(|scope| {
            for (chunk, y0) in out
                .chunks_mut(rows_per * row_bytes)
                .zip((0..).step_by(rows_per))
            {
                scope.spawn(move || {
                    warp_rows(src, chunk, width, height, y0, shell, image_scale, page);
                });
            }
        });
    }
    Some(out)
}

/// Warps the rows in `dst`, whose first row is `y0` in the full image.
#[allow(
    clippy::too_many_arguments,
    clippy::cast_precision_loss,
    reason = "the optics need the full frame geometry; pixel counts fit f32"
)]
fn warp_rows(
    blurred: &[u8],
    dst: &mut [u8],
    width: usize,
    height: usize,
    y0: usize,
    shell: Shell,
    image_scale: f32,
    page: (f32, f32),
) {
    let half = shell.half();
    let inv = 1.0 / image_scale;
    let half_logical = (half.0 * inv, half.1 * inv);
    let radius = shell.radius * inv;
    // The shell's center, not its top-left: `local` is centre-relative.
    let center = (shell.x * inv + half_logical.0, shell.y * inv + half_logical.1);
    let page = (page.0.abs(), page.1.abs());
    for (row_index, row) in dst.chunks_exact_mut(width * 4).enumerate() {
        let y = y0 + row_index;
        for (x, chunk) in row.chunks_exact_mut(4).enumerate() {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let local = (px * inv - center.0, py * inv - center.1);
            let Some((dx, dy)) = refraction_offset(local, half_logical, radius, page) else {
                continue;
            };
            let sample = sample_bilinear(
                blurred,
                width,
                height,
                px + dx * image_scale,
                py + dy * image_scale,
            );
            chunk.copy_from_slice(&sample);
        }
    }
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

    fn card_shell_offset(p: (f32, f32), page: (f32, f32)) -> Option<(f32, f32)> {
        refraction_offset(p, HALF, RADIUS, page)
    }

    #[test]
    fn the_top_edge_pulls_up_by_the_page_height() {
        let (dx, dy) = card_shell_offset((0.0, -HALF.1), (1600.0, 900.0)).unwrap();
        near(dx, 0.0);
        near(dy, -(EDGE_INTENSITY * 900.0));
    }

    #[test]
    fn the_side_edges_pull_by_the_page_width() {
        let (dx, dy) = card_shell_offset((-HALF.0, 0.0), (1600.0, 900.0)).unwrap();
        near(dx, -(EDGE_INTENSITY * 1600.0));
        near(dy, 0.0);
        let (dx, dy) = card_shell_offset((HALF.0, 0.0), (1600.0, 900.0)).unwrap();
        near(dx, EDGE_INTENSITY * 1600.0);
        near(dy, 0.0);
    }

    #[test]
    fn a_zero_rim_gain_turns_the_ripple_off() {
        // The reference weights the ripple by the rim layer, so at the target
        // settings (Rim 0.000) the tangential term must vanish and the side
        // edge pulls straight out.
        assert!(RIM_INTENSITY.abs() < f32::EPSILON);
        let page = (1600.0, 900.0);
        let (dx, dy) = card_shell_offset((-HALF.0, 0.0), page).unwrap();
        assert!(dx.abs() > 1.0, "the edge still pulls");
        near(dy, 0.0);
    }

    #[test]
    fn the_edge_layer_falls_off_by_one_e_fold_every_edge_distance() {
        let page = (1000.0, 1000.0);
        let edge = card_shell_offset((0.0, -HALF.1), page).unwrap().1;
        let one = card_shell_offset((0.0, -HALF.1 + 1.0 / EDGE_DISTANCE), page).unwrap().1;
        assert!(one.abs() < edge.abs());
        near(one / edge, (-1.0f32).exp());
    }

    #[test]
    fn the_45_degree_corner_pushes_along_the_normal_and_is_boosted() {
        let page = (1600.0, 900.0);
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let cx = HALF.0 - RADIUS;
        let cy = HALF.1 - RADIUS;
        let p = (-(cx + RADIUS * s), -(cy + RADIUS * s));
        let (dx, dy) = card_shell_offset(p, page).unwrap();
        // The reference adds the same intensity to page UV, so undo the
        // per-axis page scale before checking the direction.
        near(dx / page.0, dy / page.1);
        assert!(dx / page.0 < 0.0);
        // The corner term adds to the plain edge push.
        let magnitude = (dx / page.0).hypot(dy / page.1);
        assert!(magnitude > EDGE_INTENSITY, "corner boost is missing");
    }

    #[test]
    fn the_center_and_the_far_field_are_copies() {
        let page = (1600.0, 900.0);
        assert!(card_shell_offset((0.0, 0.0), page).is_none());
        let beyond = -HALF.1 + REFRACTION_SPAN + 0.5;
        assert!(card_shell_offset((0.0, beyond), page).is_none());
        assert!(card_shell_offset((0.0, -HALF.1 - 10.0), page).is_none());
    }

    #[test]
    fn a_wider_page_pulls_farther() {
        let narrow = card_shell_offset((-HALF.0, 0.0), (800.0, 900.0)).unwrap();
        let wide = card_shell_offset((-HALF.0, 0.0), (1600.0, 900.0)).unwrap();
        near(wide.0, narrow.0 * 2.0);
    }

    #[test]
    fn crop_rects_centers_the_shell_and_returns_the_natural_crop() {
        let shape = GlassShape { width: 420.0, height: 288.0, radius: 24.0 };
        let crop = Rect { x: -48.0, y: 0.0, width: 516.0, height: 456.0 };
        let (shell, natural) = crop_rects(420.0, shape, crop);
        near(shell.x, 0.0);
        near(shell.y, 0.0);
        near(natural.width, 516.0);
        let capsule = GlassShape { width: 320.0, height: 56.0, radius: 28.0 };
        let (shell, _) = crop_rects(420.0, capsule, crop);
        near(shell.x, 50.0);
    }

    #[test]
    fn shell_rect_centers_the_card_and_offsets_the_capsule() {
        let crop = Rect { x: -48.0, y: 0.0, width: 516.0, height: 456.0 };
        let card = GlassShape { width: 420.0, height: 288.0, radius: 24.0 };
        let shell = shell_rect(420.0, card, crop, 1.0);
        near(shell.x, 48.0);
        near(shell.y, 0.0);
        near(shell.width, 420.0);
        near(shell.radius, 24.0);
        let capsule = GlassShape { width: 320.0, height: 56.0, radius: 28.0 };
        let shell = shell_rect(420.0, capsule, crop, 1.25);
        near(shell.x, (50.0 + 48.0) * 1.25);
        near(shell.width, 400.0);
    }

    #[test]
    fn an_offscreen_window_shifts_the_shell_negative() {
        let crop = Rect { x: 10.0, y: 0.0, width: 458.0, height: 408.0 };
        let card = GlassShape { width: 420.0, height: 288.0, radius: 24.0 };
        let shell = shell_rect(420.0, card, crop, 1.0);
        near(shell.x, -10.0);
        near(shell.y, 0.0);
    }

    #[test]
    fn a_sub_pixel_shape_difference_still_matches_the_warp() {
        let card = GlassShape { width: 420.0, height: 288.0, radius: 24.0 };
        assert!(same_shape(card, card));
        let nudged = GlassShape { width: 420.4, height: 288.0, radius: 24.0 };
        assert!(same_shape(card, nudged), "a fraction of a pixel is invisible");
        let moved = GlassShape { width: 424.0, height: 288.0, radius: 24.0 };
        assert!(!same_shape(card, moved), "4px moves the baked bands");
        assert!(!same_shape(GlassShape::ZERO, card), "no warp never matches");
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

    fn test_shell() -> Shell {
        Shell { x: 20.0, y: 20.0, width: 80.0, height: 80.0, radius: 18.0 }
    }

    const TEST_PAGE: (f32, f32) = (400.0, 400.0);

    #[test]
    fn warp_keeps_the_center_and_the_margin_untouched() {
        let src = solid(120, 120, [10, 20, 30, 255]);
        let out = warp(&src, 120, 120, test_shell(), 1.0, TEST_PAGE, 0.0).unwrap();
        assert_eq!(pixel(&out, 120, 60, 60), [10, 20, 30, 255]);
        assert_eq!(pixel(&out, 120, 2, 2), [10, 20, 30, 255]);
    }

    #[test]
    fn warp_pulls_the_top_edge_from_above() {
        let mut src = solid(120, 120, [0, 0, 255, 255]);
        for row in src.chunks_exact_mut(120 * 4).take(20) {
            for chunk in row.chunks_exact_mut(4) {
                if let [r, _g, b, _a] = chunk {
                    *r = 255;
                    *b = 0;
                }
            }
        }
        let out = warp(&src, 120, 120, test_shell(), 1.0, TEST_PAGE, 0.0).unwrap();
        let edge = pixel(&out, 120, 60, 20);
        assert!(edge[0] > 200, "the top edge must sample the red margin: {edge:?}");
        assert!(edge[2] < 100, "the top edge must not stay blue: {edge:?}");
    }

    #[test]
    fn warp_pulls_the_left_edge_from_the_left() {
        let mut src = solid(120, 120, [0, 0, 255, 255]);
        for (index, chunk) in src.chunks_exact_mut(4).enumerate() {
            if index % 120 < 20 {
                if let [r, _g, b, _a] = chunk {
                    *r = 255;
                    *b = 0;
                }
            }
        }
        let out = warp(&src, 120, 120, test_shell(), 1.0, TEST_PAGE, 0.0).unwrap();
        let edge = pixel(&out, 120, 20, 60);
        assert!(edge[0] > 200, "the left edge must sample the red margin: {edge:?}");
        assert!(edge[2] < 100, "the left edge must not stay blue: {edge:?}");
    }

    #[test]
    fn warp_clamps_at_the_crop_edge_instead_of_reading_out_of_bounds() {
        // The shell sits flush against the crop's top, the screen-top case.
        let shell = Shell { x: 0.0, y: 0.0, width: 80.0, height: 80.0, radius: 18.0 };
        let mut src = solid(100, 100, [0, 0, 255, 255]);
        for chunk in src.chunks_exact_mut(4).take(100 * 10) {
            if let [r, _g, b, _a] = chunk {
                *r = 255;
                *b = 0;
            }
        }
        let out = warp(&src, 100, 100, shell, 1.0, TEST_PAGE, 0.0).unwrap();
        assert_eq!(pixel(&out, 100, 40, 0), [255, 0, 0, 255]);
    }

    #[test]
    fn warp_preserves_alpha() {
        let src = solid(120, 120, [10, 20, 30, 0]);
        let out = warp(&src, 120, 120, test_shell(), 1.0, TEST_PAGE, 0.0).unwrap();
        assert_eq!(pixel(&out, 120, 60, 60)[3], 0);
        assert_eq!(pixel(&out, 120, 60, 20)[3], 0);
    }

    #[test]
    fn warp_scales_the_pull_with_the_image_scale() {
        // Same logical geometry at 2x: the crop is twice the pixels, so the
        // displacement in image pixels must double as well.
        let shell = Shell { x: 40.0, y: 40.0, width: 160.0, height: 160.0, radius: 36.0 };
        let mut src = solid(240, 240, [0, 0, 255, 255]);
        for row in src.chunks_exact_mut(240 * 4).take(40) {
            for chunk in row.chunks_exact_mut(4) {
                if let [r, _g, b, _a] = chunk {
                    *r = 255;
                    *b = 0;
                }
            }
        }
        let out = warp(&src, 240, 240, shell, 2.0, TEST_PAGE, 0.0).unwrap();
        assert_eq!(pixel(&out, 240, 120, 40), [255, 0, 0, 255]);
    }

    #[test]
    fn warp_rejects_a_size_mismatch_or_a_bad_scale() {
        assert!(warp(&[0, 0, 0, 0], 2, 2, test_shell(), 1.0, TEST_PAGE, 0.0).is_none());
        let src = solid(120, 120, [1, 2, 3, 255]);
        assert!(warp(&src, 120, 120, test_shell(), 0.0, TEST_PAGE, 0.0).is_none());
    }

    #[test]
    fn blur_keeps_a_uniform_face_and_dimensions() {
        let mut buf = solid(10, 8, [40, 60, 80, 255]);
        blur(&mut buf, 10, 8, 0.5);
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
        blur(&mut buf, 20, 20, 2.0);
        let left = pixel(&buf, 20, 1, 10)[0];
        let right = pixel(&buf, 20, 18, 10)[0];
        assert!(left < 10, "the far field keeps black: {left}");
        assert!(right > 245, "the far field keeps white: {right}");
        let at_edge = pixel(&buf, 20, 10, 10)[0];
        assert!(
            (140..=170).contains(&at_edge),
            "the step must soften toward the middle: {at_edge}"
        );
        let mut previous = pixel(&buf, 20, 4, 10)[0];
        for x in 5..16 {
            let value = pixel(&buf, 20, x, 10)[0];
            assert!(value >= previous, "the ramp must be monotone at x={x}");
            previous = value;
        }
    }

    #[test]
    fn blur_ignores_a_zero_sigma_and_a_size_mismatch() {
        let mut buf = solid(4, 4, [9, 9, 9, 255]);
        let copy = buf.clone();
        blur(&mut buf, 4, 4, 0.0);
        assert_eq!(buf, copy);
        let mut short = vec![0u8; 3];
        blur(&mut short, 4, 4, 1.0);
        assert_eq!(short, vec![0u8; 3]);
    }

    /// The pre-change per-pixel division, kept as an oracle for the reciprocal
    /// path. Written as a different code shape on purpose: if the two agree,
    /// the shift is exact and the row chunking changed nothing.
    #[allow(
        clippy::indexing_slicing,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        reason = "a fixed-size test frame; the offsets are a few pixels"
    )]
    fn reference_pass(
        src: &[u8],
        dst: &mut [u8],
        width: usize,
        height: usize,
        kernel: &[u32],
        total: u32,
        horizontal: bool,
    ) {
        let radius = kernel.len() / 2;
        let (lines, line_len) = if horizontal { (height, width) } else { (width, height) };
        let last = line_len.saturating_sub(1) as i64;
        for line in 0..lines {
            for index in 0..line_len {
                let mut sums = [0u32; 4];
                for (tap, weight) in kernel.iter().enumerate() {
                    let offset = tap as i64 - radius as i64;
                    let pos = (index as i64 + offset).clamp(0, last) as usize;
                    let (x, y) = if horizontal { (pos, line) } else { (line, pos) };
                    let base = (y * width + x) * 4;
                    for (sum, value) in sums.iter_mut().zip(&src[base..base + 4]) {
                        *sum += weight * u32::from(*value);
                    }
                }
                let out_base = if horizontal {
                    (line * width + index) * 4
                } else {
                    (index * width + line) * 4
                };
                for (channel, sum) in dst[out_base..out_base + 4].iter_mut().zip(sums) {
                    *channel = ((sum + total / 2) / total) as u8;
                }
            }
        }
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        reason = "the kernel weights are small and non-negative"
    )]
    fn reference_blur(rgba: &[u8], width: usize, height: usize, sigma: f32) -> Vec<u8> {
        let sigma = sigma.min(8.0);
        let radius = (sigma * 3.0).ceil() as i64;
        let denom = 2.0 * sigma * sigma;
        let mut kernel = Vec::new();
        for step in -radius..=radius {
            let distance = step as f32;
            let weight = (-(distance * distance) / denom).exp();
            kernel.push((weight * 16_384.0).round() as u32);
        }
        let total: u32 = kernel.iter().sum();
        let mut out = rgba.to_vec();
        let mut temp = vec![0u8; rgba.len()];
        reference_pass(rgba, &mut temp, width, height, &kernel, total, true);
        reference_pass(&temp, &mut out, width, height, &kernel, total, false);
        out
    }

    #[allow(clippy::cast_possible_truncation, reason = "the modulo keeps the value in u8")]
    fn noisy(width: usize, height: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                out.extend_from_slice(&[
                    (x * 7 % 256) as u8,
                    (y * 11 % 256) as u8,
                    ((x + y) * 3 % 256) as u8,
                    255,
                ]);
            }
        }
        out
    }

    #[test]
    fn blur_through_a_reciprocal_and_row_chunks_matches_the_division_oracle() {
        let (width, height) = (160, 120);
        let src = noisy(width, height);
        let mut actual = src.clone();
        blur(&mut actual, width, height, 1.5);
        let expected = reference_blur(&src, width, height, 1.5);
        assert_eq!(actual, expected, "the reciprocal shift must equal the division");
    }

    #[test]
    #[allow(clippy::cast_precision_loss, reason = "a 160x120 test frame fits f32")]
    fn warp_through_row_chunks_matches_the_serial_loop() {
        let (width, height) = (160, 120);
        let src = noisy(width, height);
        let shell = Shell { x: 10.0, y: 6.0, width: 140.0, height: 108.0, radius: 20.0 };
        let actual = warp(&src, width, height, shell, 1.0, TEST_PAGE, 1.0).unwrap();

        // The serial reference: the same optics, one pixel at a time.
        let blurred = reference_blur(&src, width, height, 1.0);
        let mut expected = blurred.clone();
        let half = shell.half();
        let radius = shell.radius;
        let center = (shell.x + half.0, shell.y + half.1);
        for (y, row) in expected.chunks_exact_mut(width * 4).enumerate() {
            for (x, chunk) in row.chunks_exact_mut(4).enumerate() {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let local = (px - center.0, py - center.1);
                let Some((dx, dy)) = refraction_offset(local, half, radius, TEST_PAGE) else {
                    continue;
                };
                let sample = sample_bilinear(&blurred, width, height, px + dx, py + dy);
                chunk.copy_from_slice(&sample);
            }
        }
        assert_eq!(actual, expected, "row chunks must not shift a sample");
    }
}
