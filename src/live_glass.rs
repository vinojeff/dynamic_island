//! Real-time backdrop capture for the liquid glass.
//!
//! The wallpaper builder samples a file once per change; this module samples
//! the composited desktop instead. Windows Graphics Capture delivers a frame
//! only when the desktop changes, and the handler crops the stage plus the lens
//! margin before reading anything back.
//!
//! Everything except the capture handler is pure: crop math, downsampling,
//! luma smoothing, environment parsing, and the session state machine. The view
//! feeds events and runs the returned [`Action`]; it never touches the capture
//! thread.

use crate::wallpaper::REFRACTION_MARGIN;

/// How the glass gets its pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Live Windows Graphics Capture.
    Live,
    /// The static wallpaper crop (fallback; also the recording-visible mode).
    Static,
}

/// `RUSTEDGE_GLASS=static` turns the live path off for this launch.
pub(crate) fn mode_from_env(value: Option<&str>) -> Mode {
    if value.is_some_and(|text| text.trim().eq_ignore_ascii_case("static")) {
        Mode::Static
    } else {
        Mode::Live
    }
}

const LUMA_TAU_MS: f32 = 200.0;

/// One physical crop of the primary monitor, shared with the capture thread.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CropSpec {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
    /// Integer downsampling factor, 1..=3.
    pub factor: u32,
    /// Logical margin around the stage, `(left, top, right, bottom)`.
    pub pad: (f32, f32, f32, f32),
}

/// The stage clipped to the primary monitor, in physical pixels.
fn stage_on_screen(
    stage: (i32, i32, u32, u32),
    screen: (u32, u32),
) -> Option<(u32, u32, u32, u32)> {
    let left = i64::from(stage.0).max(0);
    let top = i64::from(stage.1).max(0);
    let right = (i64::from(stage.0) + i64::from(stage.2)).min(i64::from(screen.0));
    let bottom = (i64::from(stage.1) + i64::from(stage.3)).min(i64::from(screen.1));
    if right <= left || bottom <= top {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to the u32 screen bounds"
    )]
    Some((left as u32, top as u32, (right - left) as u32, (bottom - top) as u32))
}

/// Physical crop for one stage rect, plus the logical margin the lens uses.
///
/// Width and height are rounded down to a multiple of the downsampling factor
/// so the output has no leftover pixels. Rounding trims the right/bottom edge;
/// on a screen-clipped side that shaves at most `factor - 1` physical pixels of
/// an edge the lens cannot sample past anyway.
pub(crate) fn crop_spec(
    stage: (i32, i32, u32, u32),
    scale: f32,
    screen: (u32, u32),
) -> Option<CropSpec> {
    if screen.0 == 0 || screen.1 == 0 || !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "scale is a small positive DPI factor"
    )]
    let factor = (scale.round() as u32).clamp(1, 3);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "40 logical pixels times a small DPI factor"
    )]
    let margin = (REFRACTION_MARGIN * scale).round().max(0.0) as u32;
    let (sx, sy, sw, sh) = stage_on_screen(stage, screen)?;
    let left = sx.saturating_sub(margin);
    let top = sy.saturating_sub(margin);
    let right = (sx + sw + margin).min(screen.0);
    let bottom = (sy + sh + margin).min(screen.1);
    let right = left + ((right - left) / factor) * factor;
    let bottom = top + ((bottom - top) / factor) * factor;
    if right <= left || bottom <= top {
        return None;
    }
    #[allow(clippy::cast_precision_loss, reason = "pixel counts are far under 2^24")]
    let pad = (
        (sx - left) as f32 / scale,
        (sy - top) as f32 / scale,
        right.saturating_sub(sx + sw) as f32 / scale,
        bottom.saturating_sub(sy + sh) as f32 / scale,
    );
    Some(CropSpec { left, top, right, bottom, factor, pad })
}

/// Box-averages `factor x factor` blocks of an RGBA frame.
///
/// `pitch` is the source row stride in bytes; it may exceed `width * 4` because
/// D3D staging textures pad rows. Leftover rows/columns are dropped.
fn downsample(
    src: &[u8],
    width: u32,
    height: u32,
    pitch: u32,
    factor: u32,
) -> Option<(Vec<u8>, u32, u32)> {
    if width == 0 || height == 0 || factor == 0 {
        return None;
    }
    let out_width = width / factor;
    let out_height = height / factor;
    if out_width == 0 || out_height == 0 {
        return None;
    }
    let block = factor as usize;
    let row_bytes = width as usize * 4;
    let pitch = pitch as usize;
    if pitch < row_bytes || src.len() < pitch * height as usize {
        return None;
    }
    let mut out = vec![0_u8; out_width as usize * out_height as usize * 4];
    let block_area = block * block;
    for oy in 0..out_height as usize {
        for ox in 0..out_width as usize {
            let mut sum = [0_usize; 4];
            for by in 0..block {
                let start = (oy * block + by) * pitch + ox * block * 4;
                let pixels = src.get(start..start + block * 4)?;
                for pixel in pixels.chunks_exact(4) {
                    for (channel, value) in sum.iter_mut().zip(pixel) {
                        *channel += usize::from(*value);
                    }
                }
            }
            let offset = (oy * out_width as usize + ox) * 4;
            for (channel, value) in sum.iter().zip(&mut out[offset..offset + 4]) {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "mean of u8 stays in u8"
                )]
                let mean = (*channel / block_area) as u8;
                *value = mean;
            }
        }
    }
    Some((out, out_width, out_height))
}

/// Time-based exponential smoothing for the backdrop lightness.
pub(crate) struct LumaSmoother {
    value: Option<f32>,
}

impl LumaSmoother {
    pub(crate) const fn new() -> Self {
        Self { value: None }
    }

    /// Feeds one measured luma and returns the smoothed value.
    pub(crate) fn push(&mut self, target: f32, dt_ms: f32) -> f32 {
        let target = if target.is_finite() { target.clamp(0.0, 1.0) } else { 0.0 };
        let next = match self.value {
            None => target,
            Some(current) => {
                let dt = dt_ms.clamp(0.0, 1_000.0);
                let alpha = 1.0 - (-dt / LUMA_TAU_MS).exp();
                current + (target - current) * alpha
            }
        };
        let next = next.clamp(0.0, 1.0);
        self.value = Some(next);
        next
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_static_is_case_and_space_insensitive() {
        assert_eq!(mode_from_env(Some("static")), Mode::Static);
        assert_eq!(mode_from_env(Some("  STATIC ")), Mode::Static);
        assert_eq!(mode_from_env(Some("live")), Mode::Live);
        assert_eq!(mode_from_env(None), Mode::Live);
    }

    #[test]
    fn crop_keeps_the_lens_margin() {
        let spec = crop_spec((100, 0, 420, 360), 1.0, (1920, 1080)).unwrap();
        assert_eq!((spec.left, spec.top, spec.right, spec.bottom), (60, 0, 560, 400));
        assert_eq!(spec.factor, 1);
        assert_eq!(spec.pad, (40.0, 0.0, 40.0, 40.0));
    }

    #[test]
    fn crop_clamps_at_screen_edges() {
        let left = crop_spec((0, 0, 420, 360), 1.0, (1920, 1080)).unwrap();
        assert_eq!(left.left, 0);
        assert_eq!(left.pad.0, 0.0);
        assert_eq!(left.pad.1, 0.0);

        assert!(crop_spec((2000, 0, 420, 360), 1.0, (1920, 1080)).is_none());
    }

    #[test]
    fn crop_is_a_whole_number_of_blocks() {
        let spec = crop_spec((90, 0, 1260, 1080), 3.0, (1920, 1080)).unwrap();
        assert_eq!(spec.factor, 3);
        assert_eq!((spec.right - spec.left) % 3, 0);
        assert_eq!((spec.bottom - spec.top) % 3, 0);
        assert_eq!(spec.pad, (30.0, 0.0, 40.0, 0.0));
    }

    #[test]
    fn downsample_averages_two_by_two() {
        let src = [
            255, 0, 0, 255, 0, 255, 0, 255,
            0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let (out, width, height) = downsample(&src, 2, 2, 8, 2).unwrap();
        assert_eq!((width, height), (1, 1));
        assert_eq!(out, vec![127, 127, 127, 255]);
    }

    #[test]
    fn downsample_strips_row_padding_at_one_to_one() {
        let src = [
            1, 2, 3, 4, 5, 6, 7, 8,
            99, 99, 99, 99,
        ];
        let (out, width, height) = downsample(&src, 2, 1, 12, 1).unwrap();
        assert_eq!((width, height), (2, 1));
        assert_eq!(out, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn downsample_three_by_three() {
        let src = vec![30_u8; 3 * 3 * 4];
        let (out, width, height) = downsample(&src, 3, 3, 12, 3).unwrap();
        assert_eq!((width, height), (1, 1));
        assert_eq!(out, vec![30, 30, 30, 30]);
    }

    #[test]
    fn downsample_rejects_a_block_larger_than_the_image() {
        assert!(downsample(&[0; 16], 2, 2, 8, 3).is_none());
    }

    #[test]
    fn luma_snaps_then_eases() {
        let mut smoother = LumaSmoother::new();
        assert!((smoother.push(0.8, 33.0) - 0.8).abs() < f32::EPSILON);
        let next = smoother.push(0.0, 33.0);
        assert!(next > 0.0 && next < 0.8);
        assert!(smoother.push(2.0, 1_000.0) <= 1.0);
    }
}
