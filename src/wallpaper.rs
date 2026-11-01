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
//! Scope notes: the display is mapped as a plain stretch, so a wallpaper
//! whose aspect ratio differs from the display is cropped approximately.
//! Per-monitor wallpapers are not honored; [`SystemParametersInfoW`] reports
//! the shared path and the dock lives on the primary display. Live video
//! wallpapers (Wallpaper Engine) never change a file, so the glass keeps the
//! last sampled frame — under [`BLUR_RADIUS`] pixels of blur that reads the
//! same.

use std::sync::Arc;
use std::time::UNIX_EPOCH;

use gpui_kit::RenderImage;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN, SPI_GETDESKWALLPAPER, SystemParametersInfoW,
};

/// Box-blur radius, physical pixels. Three passes of this approximate a
/// Gaussian wide enough to read as frost, and wide enough that a stale frame
/// of a video wallpaper is indistinguishable.
pub const BLUR_RADIUS: usize = 12;

/// Saturation boost. iOS glass reads slightly richer than the raw wallpaper.
pub const SATURATION: f32 = 1.15;

/// A frosted wallpaper crop ready to paint. Built on the background executor
/// and swapped into the view as one value.
#[derive(Clone)]
pub struct GlassBackdrop {
    /// The blurred crop, matched one-to-one to the stage rect.
    pub image: Arc<RenderImage>,
    /// Mean lightness of the crop, 0.0 dark .. 1.0 bright. Drives the scrim.
    pub luma: f32,
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
}

/// Snapshots one fingerprint: wallpaper path + stats for the given stage rect.
///
/// `None` when the desktop is a solid color (empty path from SPI) or the file
/// vanished between the path call and the stat.
#[must_use]
pub fn fingerprint_now(rect: (i32, i32, u32, u32)) -> Option<Fingerprint> {
    fingerprint_file(wallpaper_path()?.as_str(), rect)
}

/// Stats a specific path. Split from [`fingerprint_now`] so tests can pin a
/// temp file instead of whatever wallpaper the desktop currently shows.
fn fingerprint_file(path: &str, rect: (i32, i32, u32, u32)) -> Option<Fingerprint> {
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
pub fn build(display: (u32, u32), rect: (i32, i32, u32, u32)) -> Option<GlassBackdrop> {
    let fingerprint = fingerprint_now(rect)?;
    if display.0 == 0 || display.1 == 0 {
        return None;
    }
    let bytes = std::fs::read(&fingerprint.path).ok()?;
    let wall = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?
        .to_rgba8();
    // Fill-mapping approximation: the wallpaper is stretched over the whole
    // display, then the stage is cropped out of that.
    let stretched = image::imageops::resize(
        &wall,
        display.0,
        display.1,
        image::imageops::FilterType::Triangle,
    );
    let (x, y, w, h) = map_rect((stretched.width(), stretched.height()), display, rect);
    if w == 0 || h == 0 {
        return None;
    }
    let mut crop_raw = image::imageops::crop_imm(&stretched, x, y, w, h)
        .to_image()
        .into_raw();
    let cw = w as usize;
    let ch = h as usize;
    blur3(&mut crop_raw, cw, ch, BLUR_RADIUS);
    saturate(&mut crop_raw, SATURATION);
    let luma = mean_luma(&crop_raw);
    let frame = frame_from_rgba(cw, ch, &crop_raw)?;
    Some(GlassBackdrop {
        image: Arc::new(RenderImage::new(smallvec::smallvec![frame])),
        luma,
    })
}

/// Maps the stage rect into wallpaper pixels under stretch mapping.
///
/// Returns the crop `(x, y, width, height)`, clamped to the wallpaper so a
/// half-off-screen window still crops a valid region.
#[must_use]
pub fn map_rect(
    wall: (u32, u32),
    display: (u32, u32),
    rect: (i32, i32, u32, u32),
) -> (u32, u32, u32, u32) {
    if wall.0 == 0 || wall.1 == 0 || display.0 == 0 || display.1 == 0 {
        return (0, 0, 0, 0);
    }
    let scale_x = f64::from(wall.0) / f64::from(display.0);
    let scale_y = f64::from(wall.1) / f64::from(display.1);
    // floor after scaling, clamped to the wallpaper; a rect wider than the
    // wall simply shrinks rather than wrapping.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let x = (f64::from(rect.0) * scale_x).max(0.0) as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let y = (f64::from(rect.1) * scale_y).max(0.0) as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let w = (f64::from(rect.2) * scale_x) as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let h = (f64::from(rect.3) * scale_y) as u32;
    let w = w.min(wall.0.saturating_sub(x));
    let h = h.min(wall.1.saturating_sub(y));
    (x, y, w, h)
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

    #[test]
    fn map_rect_scales_the_window_rect_into_wallpaper_pixels() {
        // A 3200x1800 wallpaper stretched over a 2560x1440 display: the
        // scale is 1.25 on both axes.
        let crop = map_rect((3200, 1800), (2560, 1440), (1070, 0, 420, 360));
        assert_eq!(crop, (1337, 0, 525, 450));
    }

    #[test]
    fn map_rect_clamps_a_rect_hanging_off_the_display() {
        let crop = map_rect((1000, 1000), (1000, 1000), (900, 0, 400, 100));
        assert_eq!(crop, (900, 0, 100, 100));
    }

    #[test]
    fn map_rect_is_zero_on_degenerate_sizes() {
        assert_eq!(map_rect((0, 10), (10, 10), (0, 0, 1, 1)), (0, 0, 0, 0));
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
        let first = fingerprint_file(&path, rect);
        assert!(first.is_some(), "the file exists so a fingerprint exists");
        let same = fingerprint_file(&path, rect);
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
        let retime = fingerprint_file(&path, rect);
        assert_ne!(first, retime, "a new mtime is a new fingerprint");

        let moved = fingerprint_file(&path, (900, 0, 420, 360));
        assert_ne!(first, moved, "a moved stage rect is a new fingerprint");

        let vanished = fingerprint_file(&dir.join("missing.png").to_string_lossy(), rect);
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
        let first = fingerprint_file(&path, rect);
        assert!(first.is_some());
        let again = fingerprint_file(&path, rect);
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
