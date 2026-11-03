//! The frosted-wallpaper backdrop the liquid-glass shell sits on.
//!
//! GPUI has no backdrop blur and the dock's window cannot sample what sits
//! behind it, so the frost is built once from the wallpaper file itself: a
//! crop of exactly the region the stage covers, blurred and saturated on the
//! background executor, then painted one-to-one under the shell. The crop is
//! rebuilt when the fingerprint moves — the wallpaper file (path, size,
//! mtime) or the stage rect (the dock slides across monitors with different
//! origins and DPI scales).
//!
//! Scope notes: `WallpaperStyle` and `TileWallpaper` from the desktop registry
//! select the Windows placement (fill, fit, center, stretch, tile). `Span`
//! across multiple monitors is approximated as fill on the primary display.
//! Per-monitor wallpapers are not honored; [`SystemParametersInfoW`] reports
//! the shared path and the dock lives on the primary display. Live video
//! wallpapers (Wallpaper Engine) never change a file, so the glass keeps the
//! last sampled frame — under [`BLUR_RADIUS`] pixels of blur that reads the
//! same.

use std::sync::Arc;
use std::time::UNIX_EPOCH;

use gpui_kit::RenderImage;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN, SPI_GETDESKWALLPAPER, SystemParametersInfoW,
};

/// Box-blur radius, physical pixels. Three passes of this approximate a
/// Gaussian wide enough to read as frost, and wide enough that a stale frame
/// of a video wallpaper is indistinguishable.
pub const BLUR_RADIUS: usize = 9;

/// Blur radius of the lens copy, physical pixels.
///
/// The rim bends the desktop, so it has to keep the structure it bends:
/// painting the frosted face into the lens band would hide the displacement
/// entirely. Two pixels of blur still reads as polished glass.
pub const LENS_BLUR_RADIUS: usize = 2;

/// Saturation boost. iOS glass reads slightly richer than the raw wallpaper.
pub const SATURATION: f32 = 1.15;

/// How Windows places the wallpaper on the primary display.
///
/// Mirrors `HKCU\Control Panel\Desktop\WallpaperStyle` plus `TileWallpaper`.
/// `Span` is accepted but approximated as [`Self::Fill`] on the primary
/// display; a true virtual-desktop span needs per-monitor geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WallpaperPosition {
    /// Native size, centered; anything outside the display is cropped.
    Center,
    /// Native size, repeated across the display.
    Tile,
    /// Non-uniform stretch to the display size.
    Stretch,
    /// Uniform scale to fit, with transparent letterbox margins.
    Fit,
    /// Uniform scale to cover, centered and cropped.
    Fill,
    /// Virtual-desktop span, approximated as [`Self::Fill`] on the primary.
    Span,
}

impl WallpaperPosition {
    /// Parses the two registry strings. Tile wins over the style value.
    ///
    /// Missing or unknown styles use `Fill`, the modern Windows default.
    #[must_use]
    pub fn from_registry(style: &str, tile: &str) -> Self {
        if tile.trim() == "1" {
            return Self::Tile;
        }
        match style.trim() {
            "0" => Self::Center,
            "2" => Self::Stretch,
            "6" => Self::Fit,
            "22" => Self::Span,
            _ => Self::Fill,
        }
    }
}

/// How much wallpaper the crop keeps around the stage, logical pixels.
///
/// The edge lens samples the desktop *outside* the panel; the margin is the
/// distance it can reach. Must exceed the lens pull in `island_view::glass`.
pub const REFRACTION_MARGIN: f32 = 40.0;

/// A frosted wallpaper crop ready to paint. Built on the background executor
/// and swapped into the view as one value.
#[derive(Clone)]
pub struct GlassBackdrop {
    /// The blurred crop, one-to-one with the stage plus a lens margin.
    pub image: Arc<RenderImage>,
    /// The same crop with only [`LENS_BLUR_RADIUS`], for the refracting rim.
    ///
    /// The rim has to keep the structure it displaces, so it cannot borrow
    /// the frosted face; the two are the same geometry, so a ring can swap
    /// between them without shifting.
    pub lens: Arc<RenderImage>,
    /// Mean lightness of the crop, 0.0 dark .. 1.0 bright. Drives the scrim.
    pub luma: f32,
    /// Logical margin around the stage, `(left, top, right, bottom)`.
    ///
    /// Zero on a side where the stage touches a display edge, so the lens
    /// knows it cannot sample past the screen.
    pub pad: (f32, f32, f32, f32),
}

/// What makes the frosted crop stale: the wallpaper file AND the stage rect.
///
/// The rect is the overlay window's physical rect — origin, width, height —
/// in the resolution [`fingerprint_now`] was called with.
#[derive(Clone, Debug, PartialEq)]
pub struct Fingerprint {
    /// Wallpaper file path, empty for a solid-color desktop.
    path: String,
    /// File length, bytes.
    len: u64,
    /// Modification time, seconds since the Unix epoch.
    mtime: u64,
    /// The stage rect the crop was taken for.
    rect: (i32, i32, u32, u32),
    /// The Windows placement the crop was drawn with.
    position: WallpaperPosition,
}

/// Snapshots one fingerprint: wallpaper path + stats for the given stage rect.
///
/// `None` when the desktop is a solid color (empty path from SPI) or the file
/// vanished between the path call and the stat.
#[must_use]
pub fn fingerprint_now(rect: (i32, i32, u32, u32)) -> Option<Fingerprint> {
    let path = wallpaper_path()?;
    fingerprint_file(&path, rect, desktop_position())
}

/// Stats a specific path. Split from [`fingerprint_now`] so tests can pin a
/// temp file instead of whatever wallpaper the desktop currently shows.
fn fingerprint_file(
    path: &str,
    rect: (i32, i32, u32, u32),
    position: WallpaperPosition,
) -> Option<Fingerprint> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .map_or(0, |dur| dur.as_secs());
    Some(Fingerprint {
        path: path.to_owned(),
        len: meta.len(),
        mtime,
        rect,
        position,
    })
}

/// Queries the wallpaper path. An empty answer is a solid-color desktop.
fn wallpaper_path() -> Option<String> {
    let mut buffer = [0u16; 1024];
    // SAFETY: the buffer lives for the call and the action only writes to it.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "1024 is far below u32::MAX"
    )]
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETDESKWALLPAPER,
            buffer.len() as u32,
            buffer.as_mut_ptr().cast(),
            0,
        )
    };
    if ok == 0 {
        return None;
    }
    let end = buffer.iter().position(|&u| u == 0).unwrap_or(buffer.len());
    if end == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(buffer.get(..end)?))
}

/// Reads the active wallpaper placement from the desktop registry.
fn desktop_position() -> WallpaperPosition {
    let style = desktop_value("WallpaperStyle").unwrap_or_default();
    let tile = desktop_value("TileWallpaper").unwrap_or_default();
    WallpaperPosition::from_registry(&style, &tile)
}

/// Reads one string value under `HKCU\Control Panel\Desktop`.
fn desktop_value(name: &str) -> Option<String> {
    let subkey = wide("Control Panel\\Desktop");
    let value = wide(name);
    let mut buffer = [0u16; 64];
    let mut bytes = u32::try_from(buffer.len().checked_mul(2)?).ok()?;
    // SAFETY: both names are null-terminated, `buffer` lives for the call,
    // and `bytes` describes its byte capacity.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &raw mut bytes,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let len = usize::try_from(bytes)
        .ok()?
        .checked_div(2)?
        .min(buffer.len());
    let end = buffer
        .iter()
        .take(len)
        .position(|&unit| unit == 0)
        .unwrap_or(len);
    if end == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(buffer.get(..end)?))
}

/// Null-terminates a string for the wide Win32 APIs.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Physical display size the wallpaper currently covers. Primary monitor.
#[must_use]
pub fn screen_size() -> (u32, u32) {
    // SAFETY: GetSystemMetrics has no precondition beyond an initialized
    // desktop, which a windowed process has.
    #[allow(
        clippy::cast_sign_loss,
        reason = "screen metrics are non-negative; a negative reply reads as 0"
    )]
    {
        (
            unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(0) as u32,
            unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(0) as u32,
        )
    }
}

/// Builds the frosted crop for one stage rect.
///
/// Runs on the background executor only: a full decode plus resize costs
/// hundreds of milliseconds. Any failure — unreadable file, undecodable
/// bytes, degenerate rect — yields `None`, and the caller still records the
/// fingerprint so a broken wallpaper does not re-trigger a decode every poll.
#[must_use]
pub fn build(
    display: (u32, u32),
    rect: (i32, i32, u32, u32),
    scale: f32,
) -> Option<GlassBackdrop> {
    let fingerprint = fingerprint_now(rect)?;
    if display.0 == 0 || display.1 == 0 || !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let bytes = std::fs::read(&fingerprint.path).ok()?;
    let wall = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?
        .to_rgba8();
    // Draw the wallpaper the way Windows does, then crop the stage *plus* a
    // margin out of that display-sized canvas. The lens reads the margin to
    // bend the desktop outside the panel into the rim.
    let composed = compose(&wall, display, fingerprint.position);
    let (sx, sy, sw, sh) = stage_crop((composed.width(), composed.height()), rect)?;
    // Widening display-sized integers into f32 loses nothing that matters.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    let margin = (REFRACTION_MARGIN * scale).round().max(0.0) as u32;
    let left = sx.saturating_sub(margin);
    let top = sy.saturating_sub(margin);
    let right = (sx + sw + margin).min(composed.width());
    let bottom = (sy + sh + margin).min(composed.height());
    let w = right.saturating_sub(left);
    let h = bottom.saturating_sub(top);
    if w == 0 || h == 0 {
        return None;
    }
    let mut crop_raw = image::imageops::crop_imm(&composed, left, top, w, h)
        .to_image()
        .into_raw();
    let cw = w as usize;
    let ch = h as usize;
    saturate(&mut crop_raw, SATURATION);
    // The rim keeps a lightly blurred copy so the lens has structure to bend.
    let mut lens_raw = crop_raw.clone();
    blur3(&mut lens_raw, cw, ch, LENS_BLUR_RADIUS);
    blur3(&mut crop_raw, cw, ch, BLUR_RADIUS);
    let luma = mean_luma(&crop_raw);
    let frame = frame_from_rgba(cw, ch, &crop_raw)?;
    let lens_frame = frame_from_rgba(cw, ch, &lens_raw)?;
    // The lens works in logical pixels; the pad is physical pixels / scale.
    #[allow(clippy::cast_precision_loss, reason = "pixel counts are far under 2^24")]
    let pad = (
        (sx - left) as f32 / scale,
        (sy - top) as f32 / scale,
        (right - (sx + sw)) as f32 / scale,
        (bottom - (sy + sh)) as f32 / scale,
    );
    Some(GlassBackdrop {
        image: Arc::new(RenderImage::new(smallvec::smallvec![frame])),
        lens: Arc::new(RenderImage::new(smallvec::smallvec![lens_frame])),
        luma,
        pad,
    })
}

/// Draws the wallpaper into a display-sized canvas using the Windows placement.
///
/// `Fill`, `Span`, and `Stretch` cover every pixel; `Fit` and `Center` leave
/// transparent margins so the real desktop background shows through; `Tile`
/// repeats the native image. `imageops::replace` clips negative placements,
/// so a covered image crops instead of failing.
#[must_use]
fn compose(
    wall: &image::RgbaImage,
    display: (u32, u32),
    position: WallpaperPosition,
) -> image::RgbaImage {
    let mut canvas =
        image::RgbaImage::from_pixel(display.0, display.1, image::Rgba([0, 0, 0, 0]));
    let (wall_w, wall_h) = wall.dimensions();
    if wall_w == 0 || wall_h == 0 || display.0 == 0 || display.1 == 0 {
        return canvas;
    }
    match position {
        WallpaperPosition::Stretch => {
            let resized = resize_wall(wall, display.0, display.1);
            image::imageops::replace(&mut canvas, &resized, 0, 0);
        }
        WallpaperPosition::Fill | WallpaperPosition::Span => {
            let scale = (f64::from(display.0) / f64::from(wall_w))
                .max(f64::from(display.1) / f64::from(wall_h));
            let (width, height) = scaled_dimensions(wall_w, wall_h, scale);
            let resized = resize_wall(wall, width, height);
            let x = (i64::from(display.0) - i64::from(width)) / 2;
            let y = (i64::from(display.1) - i64::from(height)) / 2;
            image::imageops::replace(&mut canvas, &resized, x, y);
        }
        WallpaperPosition::Fit => {
            let scale = (f64::from(display.0) / f64::from(wall_w))
                .min(f64::from(display.1) / f64::from(wall_h));
            let (width, height) = scaled_dimensions(wall_w, wall_h, scale);
            let resized = resize_wall(wall, width, height);
            let x = (i64::from(display.0) - i64::from(width)) / 2;
            let y = (i64::from(display.1) - i64::from(height)) / 2;
            image::imageops::replace(&mut canvas, &resized, x, y);
        }
        WallpaperPosition::Center => {
            let x = (i64::from(display.0) - i64::from(wall_w)) / 2;
            let y = (i64::from(display.1) - i64::from(wall_h)) / 2;
            image::imageops::replace(&mut canvas, wall, x, y);
        }
        WallpaperPosition::Tile => {
            let mut y = 0i64;
            while y < i64::from(display.1) {
                let mut x = 0i64;
                while x < i64::from(display.0) {
                    image::imageops::replace(&mut canvas, wall, x, y);
                    x += i64::from(wall_w);
                }
                y += i64::from(wall_h);
            }
        }
    }
    canvas
}

/// Scales a wallpaper without changing its aspect ratio. The result is at
/// least one pixel on each axis.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "positive dimensions scaled by a finite positive factor round to \
              a small u32; the sanitiser keeps the result above zero"
)]
fn scaled_dimensions(width: u32, height: u32, scale: f64) -> (u32, u32) {
    let scaled_w = (f64::from(width) * scale).round().max(1.0) as u32;
    let scaled_h = (f64::from(height) * scale).round().max(1.0) as u32;
    (scaled_w, scaled_h)
}

/// One triangle-filter resize, the same filter the old stretch path used.
fn resize_wall(wall: &image::RgbaImage, width: u32, height: u32) -> image::RgbaImage {
    image::imageops::resize(wall, width, height, image::imageops::FilterType::Triangle)
}

/// Clips the stage rect to the display, in display pixels.
///
/// Returns `None` when the stage is entirely off-screen or the display has no
/// usable size.
#[must_use]
pub fn stage_crop(
    display: (u32, u32),
    rect: (i32, i32, u32, u32),
) -> Option<(u32, u32, u32, u32)> {
    if display.0 == 0 || display.1 == 0 {
        return None;
    }
    let x = u32::try_from(rect.0.max(0)).ok()?;
    let y = u32::try_from(rect.1.max(0)).ok()?;
    let width = rect.2.min(display.0.saturating_sub(x));
    let height = rect.3.min(display.1.saturating_sub(y));
    if width == 0 || height == 0 {
        return None;
    }
    Some((x, y, width, height))
}

/// One box-blur pass over one axis. RGBA, alpha passed through.
#[allow(
    clippy::indexing_slicing,
    reason = "every index is provably in bounds: `base + m*step + c` spans \
              exactly one row (horizontal: base scales the row, step 4) or one \
              column (vertical: base the column, step `w*4`), because `line` \
              runs over the OTHER axis and `m`/`head`/`tail` are clamped below \
              `major`; the tests pin both passes end-to-end"
)]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "accumulators stay small and well below f32's 24-bit mantissa, \
              averages are 0..=255 before rounding off negative values"
)]
fn blur_pass(src: &[u8], dst: &mut [u8], w: usize, h: usize, radius: usize, horizontal: bool) {
    let (major, minor) = if horizontal { (w, h) } else { (h, w) };
    let line_stride = w * 4;
    let step = if horizontal { 4 } else { w * 4 };
    for line in 0..minor {
        // Sliding window per channel; RGB only, alpha copies through. A line
        // is a row for horizontal passes and a column for vertical ones.
        let base = if horizontal {
            line * line_stride
        } else {
            line * 4
        };
        let mut sums = [0f32; 3];
        let initial = radius.min(major - 1);
        for off in 0..=initial {
            for c in 0..3 {
                sums[c] += f32::from(src[base + off * step + c]);
            }
        }
        let mut count = (initial + 1) as f32;
        for m in 0..major {
            let center = base + m * step;
            for c in 0..3 {
                dst[center + c] = (sums[c] / count).round() as u8;
            }
            dst[center + 3] = src[center + 3];
            // Window slides: add the incoming tail, drop the outgoing head.
            if m + radius + 1 < major {
                for c in 0..3 {
                    sums[c] += f32::from(src[base + (m + radius + 1) * step + c]);
                }
                count += 1.0;
            }
            if m >= radius {
                for c in 0..3 {
                    sums[c] -= f32::from(src[base + (m - radius) * step + c]);
                }
                count -= 1.0;
            }
        }
    }
}

/// Three box-blur passes, horizontal then vertical each, over an RGBA buffer.
///
/// Approaches a Gaussian far more cheaply than a float kernel. On a constant
/// input every pass is the identity, so frost never invents color.
pub fn blur3(buffer: &mut [u8], w: usize, h: usize, radius: usize) {
    if w == 0 || h == 0 || radius == 0 {
        return;
    }
    let mut scratch = vec![0u8; buffer.len()];
    for _ in 0..3 {
        blur_pass(buffer, &mut scratch, w, h, radius, true);
        blur_pass(&scratch, buffer, w, h, radius, false);
    }
}

/// Scales RGB saturation by `factor` around each pixel's luma, clamped.
///
/// 1.0 changes nothing; extremes clamp to their own channel range.
/// Scales RGB saturation by `factor` around each pixel's luma, clamped.
///
/// 1.0 changes nothing; extremes clamp to their own channel range.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "inputs are 0..=255 and every result clamps to that range \
              before the narrowing cast"
)]
pub fn saturate(buffer: &mut [u8], factor: f32) {
    for chunk in buffer.chunks_exact_mut(4) {
        let [r, g, b, ..] = *chunk else {
            continue;
        };
        let (r, g, b) = (f32::from(r), f32::from(g), f32::from(b));
        let luma = 0.299 * r + 0.587 * g + 0.114 * b;
        for (slot, value) in chunk.iter_mut().take(3).zip([r, g, b]) {
            *slot = (luma + (value - luma) * factor).round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// Mean per-pixel lightness over an RGBA buffer, 0.0 dark .. 1.0 bright.
#[must_use]
pub fn mean_luma(buffer: &[u8]) -> f32 {
    let chunks = buffer.len() / 4;
    if chunks == 0 {
        return 0.0;
    }
    let total: f64 = buffer
        .chunks_exact(4)
        .map(|chunk| {
            let [r, g, b, ..] = *chunk else {
                return 0.0;
            };
            0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b)
        })
        .sum();
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation, // a 0..1 mean fits f32 easily
        reason = "a 0..1 mean loses nothing to f32"
    )]
    let luma = (total / 255.0 / chunks as f64) as f32;
    luma
}

/// Swaps RGB to BGRA and wraps one opaque frame, the exact construction
/// `media::decode_cover` uses so color order stays consistent app-wide. The
/// caller wraps the frame in a [`RenderImage`].
fn frame_from_rgba(width: usize, height: usize, rgba: &[u8]) -> Option<image::Frame> {
    if width == 0 || height == 0 {
        return None;
    }
    let mut bgra = Vec::with_capacity(rgba.len());
    for chunk in rgba.chunks_exact(4) {
        let Some(&[red, green, blue, alpha]) = chunk.get(..4) else {
            break;
        };
        bgra.extend_from_slice(&[blue, green, red, alpha]);
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "dimensions came from a decoded image's u32 fields"
    )]
    let buffer =
        image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(width as u32, height as u32, bgra)?;
    Some(image::Frame::new(buffer))
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn gray(value: u8, w: usize, h: usize) -> Vec<u8> {
        let mut buffer = Vec::with_capacity(w * h * 4);
        for _ in 0..w * h {
            buffer.extend_from_slice(&[value, value, value, 255]);
        }
        buffer
    }

    fn tiny_png(colors: [[u8; 4]; 4]) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(2, 2, |x, y| image::Rgba(colors[(y * 2 + x) as usize]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        img.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> image::RgbaImage {
        image::RgbaImage::from_pixel(width, height, image::Rgba(rgba))
    }

    #[test]
    fn registry_values_map_to_the_windows_placement() {
        use WallpaperPosition::{Center, Fill, Fit, Span, Stretch, Tile};
        assert_eq!(WallpaperPosition::from_registry("0", "0"), Center);
        assert_eq!(WallpaperPosition::from_registry("2", "0"), Stretch);
        assert_eq!(WallpaperPosition::from_registry("6", "0"), Fit);
        assert_eq!(WallpaperPosition::from_registry("10", "0"), Fill);
        assert_eq!(WallpaperPosition::from_registry("22", "0"), Span);
        assert_eq!(
            WallpaperPosition::from_registry("10", "1"),
            Tile,
            "tile wins over any style"
        );
        assert_eq!(
            WallpaperPosition::from_registry("", ""),
            Fill,
            "missing values use the modern default"
        );
        assert_eq!(WallpaperPosition::from_registry("99", "0"), Fill);
    }

    #[test]
    fn fill_covers_the_display_and_keeps_the_center() {
        // 4x2, left half red and right half blue, covered onto 2x2: scale 1,
        // placed at x = -1, so the canvas shows source columns 1 and 2.
        let wall = image::RgbaImage::from_fn(4, 2, |x, _| {
            if x < 2 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 0, 255, 255])
            }
        });
        let canvas = compose(&wall, (2, 2), WallpaperPosition::Fill);
        assert_eq!(canvas.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(canvas.get_pixel(1, 0).0, [0, 0, 255, 255]);
    }

    #[test]
    fn fit_letterboxes_instead_of_cropping() {
        let wall = solid(4, 2, [10, 20, 30, 255]);
        let canvas = compose(&wall, (4, 4), WallpaperPosition::Fit);
        assert_eq!(
            canvas.get_pixel(0, 0).0[3],
            0,
            "top letterbox is transparent"
        );
        assert_eq!(canvas.get_pixel(0, 1).0, [10, 20, 30, 255]);
        assert_eq!(canvas.get_pixel(3, 2).0, [10, 20, 30, 255]);
        assert_eq!(
            canvas.get_pixel(0, 3).0[3],
            0,
            "bottom letterbox is transparent"
        );
    }

    #[test]
    fn stretch_fills_every_pixel_without_preserving_aspect() {
        let wall = solid(4, 2, [1, 2, 3, 255]);
        let canvas = compose(&wall, (4, 4), WallpaperPosition::Stretch);
        assert_eq!(canvas.get_pixel(0, 0).0, [1, 2, 3, 255]);
        assert_eq!(canvas.get_pixel(3, 3).0, [1, 2, 3, 255]);
    }

    #[test]
    fn center_keeps_native_size_and_leaves_margins() {
        let wall = solid(2, 2, [200, 10, 10, 255]);
        let canvas = compose(&wall, (4, 4), WallpaperPosition::Center);
        assert_eq!(canvas.get_pixel(0, 0).0[3], 0);
        assert_eq!(canvas.get_pixel(1, 1).0, [200, 10, 10, 255]);
        assert_eq!(canvas.get_pixel(2, 2).0, [200, 10, 10, 255]);
        assert_eq!(canvas.get_pixel(3, 3).0[3], 0);
    }

    #[test]
    fn tile_repeats_the_native_wallpaper() {
        let wall = image::RgbaImage::from_fn(2, 2, |x, _| {
            if x == 0 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 0, 255, 255])
            }
        });
        let canvas = compose(&wall, (4, 4), WallpaperPosition::Tile);
        assert_eq!(canvas.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(canvas.get_pixel(1, 0).0, [0, 0, 255, 255]);
        assert_eq!(canvas.get_pixel(2, 0).0, [255, 0, 0, 255]);
        assert_eq!(canvas.get_pixel(3, 0).0, [0, 0, 255, 255]);
        assert_eq!(canvas.get_pixel(0, 2).0, [255, 0, 0, 255]);
    }

    #[test]
    fn stage_crop_clamps_a_rect_hanging_off_the_display() {
        assert_eq!(
            stage_crop((1000, 1000), (900, 0, 400, 100)),
            Some((900, 0, 100, 100))
        );
        assert_eq!(stage_crop((0, 10), (0, 0, 1, 1)), None);
    }

    #[test]
    fn box_blur_of_a_constant_image_is_the_same_constant() {
        let mut buffer = gray(200, 8, 8);
        blur3(&mut buffer, 8, 8, 2);
        for chunk in buffer.chunks_exact(4) {
            assert_eq!(&chunk[..3], &[200, 200, 200]);
            assert_eq!(chunk[3], 255);
        }
    }

    #[test]
    fn box_blur_softens_a_hard_edge() {
        let mut buffer = gray(0, 8, 8);
        for y in 0..8 {
            for x in 4..8 {
                let at = (y * 8 + x) * 4;
                buffer[at..at + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
        blur3(&mut buffer, 8, 8, 1);
        // The straddling column must sit strictly between the two sides.
        let at = 3 * 4;
        assert!(buffer[at] > 0 && buffer[at] < 255);
    }

    #[test]
    fn mean_luma_matches_a_hand_computation() {
        // Two pixels: pure white and pure black → mean luma 0.5.
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&[255, 255, 255, 255]);
        buffer.extend_from_slice(&[0, 0, 0, 255]);
        let luma = mean_luma(&buffer);
        assert!((luma - 0.5).abs() < 0.001, "luma {luma}");
    }

    #[test]
    fn zero_len_buffer_means_dark() {
        assert_eq!(mean_luma(&[]), 0.0);
    }

    #[test]
    fn saturation_boost_pushes_chroma_extremes_and_keeps_gray() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&[128, 128, 128, 255]); // gray unchanged
        buffer.extend_from_slice(&[128, 64, 64, 255]); // reddish dark
        saturate(&mut buffer, 1.2);
        assert_eq!(&buffer[0..3], &[128, 128, 128], "gray stays gray");
        assert!(buffer[4] >= 128, "red grows to {}", buffer[4]);
    }

    #[test]
    fn the_fingerprint_moves_when_the_file_or_rect_moves() {
        let dir = std::env::temp_dir().join(format!("rustedge-fingerprint-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("wall.png");
        std::fs::write(&file, tiny_png([[1, 2, 3, 255]; 4])).unwrap();
        let path = file.to_string_lossy().into_owned();
        let rect = (1070, 0, 420, 360);
        let first = fingerprint_file(&path, rect, WallpaperPosition::Fill);
        assert!(first.is_some(), "the file exists so a fingerprint exists");
        let same = fingerprint_file(&path, rect, WallpaperPosition::Fill);
        assert_eq!(first, same, "nothing changed");

        // Force the mtime so a rewrite is observable at second resolution.
        let old = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let handle = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        handle
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        drop(handle);
        let retime = fingerprint_file(&path, rect, WallpaperPosition::Fill);
        assert_ne!(first, retime, "a new mtime is a new fingerprint");

        let moved = fingerprint_file(&path, (900, 0, 420, 360), WallpaperPosition::Fill);
        assert_ne!(first, moved, "a moved stage rect is a new fingerprint");

        let restyled = fingerprint_file(&path, rect, WallpaperPosition::Fit);
        assert_ne!(first, restyled, "a new placement is a new fingerprint");

        let vanished = fingerprint_file(
            &dir.join("missing.png").to_string_lossy(),
            rect,
            WallpaperPosition::Fill,
        );
        assert!(vanished.is_none(), "a vanished file has no fingerprint");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_build_still_records_the_fingerprint_it_tried() {
        // The poller compares fingerprints, not build results: a wallpaper
        // file that fails to decode must yield the same fingerprint on every
        // pass whether or not the build succeeded, or the poll would re-decode
        // the broken file every second forever.
        let dir = std::env::temp_dir().join(format!("rustedge-failbuild-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let file = dir.join("garbage.png");
        std::fs::write(&file, b"not an image at all").unwrap();
        let path = file.to_string_lossy().into_owned();
        let rect = (1070, 0, 420, 360);
        let first = fingerprint_file(&path, rect, WallpaperPosition::Fill);
        assert!(first.is_some());
        let again = fingerprint_file(&path, rect, WallpaperPosition::Fill);
        assert_eq!(first, again, "fingerprint stability across failures");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_pipeline_holds_shape_on_a_constant_crop() {
        // `build` reaches the wallpaper only through the real SPI path, which
        // a unit test cannot point at a temp file. The pure pipeline pieces
        // behind it get the same inputs here: constant gray must survive blur
        // and saturation untouched, and the frame must carry the swap.
        let mut raw = gray(10, 42, 42);
        blur3(&mut raw, 42, 42, 3);
        saturate(&mut raw, 1.15);
        assert_eq!(raw[0], 10, "constant gray survives both passes");
        let frame = frame_from_rgba(42, 42, &raw).unwrap();
        assert_eq!(frame.buffer().width(), 42);
        assert_eq!(frame.buffer().height(), 42);
    }

    #[test]
    fn frame_from_rgba_swaps_to_bgra() {
        let frame = frame_from_rgba(1, 1, &[9, 8, 7, 255]).unwrap();
        // The typed slot reads [b, g, r] after the swap: blue=7 first.
        let pixel = frame.buffer().get_pixel(0, 0).0;
        assert_eq!(pixel[0], 7, "first slot is B after the swap");
        assert_eq!(pixel[2], 9, "third slot is R after the swap");
    }
}
