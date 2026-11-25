//! The liquid-glass paint side: everything the shell draws that reads as glass.
//!
//! The material itself is the reference shader, ported in [`crate::glass_optics`]
//! and baked into the wallpaper crop before it reaches GPUI. That leaves this
//! module deliberately thin:
//!
//! - [`lens`] paints the already-warped crop once, clipped to the shell's
//!   rounded silhouette. That single image *is* the glass: the refracted face,
//!   the softened interior, and the bright/dark edge are all in the warp.
//! - [`layers`] adds nothing over a landed backdrop. Before the first warp it
//!   paints a dark neutral wash so the white ink has something to sit on.
//! - [`shell_shadows`] is the reference's one drop shadow.
//!
//! There is no painted rim, scrim, sheen, or cyan glow. The reference has
//! none of those; its bright edge is refracted content.

use std::sync::Arc;

use gpui_kit::{
    App, Bounds, BoxShadow, Corners, ParentElement, Pixels, RenderImage, Styled, Window, canvas,
    div, hsla, linear_color_stop, linear_gradient, point, px, size,
};

use rustedge::island;

use crate::glass_optics::{self, GlassShape, Rect};
use crate::wallpaper;

/// How long a matching warp takes to blend in over the plain fallback it
/// replaces.
pub(super) const GLASS_FADE_MS: u64 = 120;

/// Below this shell height the wallpaper lens is fully faded out; the line
/// glass covers the crossing.
const LENS_FADE_LOW: f32 = 7.0;

/// Above this shell height the wallpaper lens is fully opaque.
const LENS_FADE_HIGH: f32 = 13.0;

/// The cool glass hue shared by the boot wash and the tucked line.
pub(super) const GLASS_HUE: f32 = 0.54;

/// The boot wash's lightness while no crop has landed yet.
pub(super) const IDLE_LUMA: f32 = 0.35;

/// 0.0 → 1.0 across [`GLASS_FADE_MS`] of elapsed time.
#[must_use]
pub(crate) fn fade_progress(elapsed_ms: u64) -> f32 {
    #[allow(clippy::cast_precision_loss, reason = "milliseconds fit in f32 for this use")]
    (elapsed_ms as f32 / GLASS_FADE_MS as f32).clamp(0.0, 1.0)
}

/// The lens fades out as the island collapses into the line: the neon line
/// glass takes over below [`LENS_FADE_HIGH`] and the wallpaper lens is gone
/// by [`LENS_FADE_LOW`], so the mode switch stops being a hard cut.
#[must_use]
fn lens_opacity_for_height(height: f32) -> f32 {
    ((height - LENS_FADE_LOW) / (LENS_FADE_HIGH - LENS_FADE_LOW)).clamp(0.0, 1.0)
}

/// The refracted wallpaper under the shell, painted once at the root.
///
/// The crop is warped on the producer side (`RawGlass::warp`); this only
/// clips the shell out of the crop's natural rect. A backdrop baked for a
/// different shape paints its plain twin instead (see [`lens_image`]), so a
/// morph never shows the previous shape's refraction. `fading` is the plain
/// backdrop that a newly matching warp has just replaced, plus the cross-fade
/// progress — the refraction blends in so the landing never snaps. Empty when
/// the opacity ramp or the fade says so; the fallback wash in [`layers`] takes
/// over.
pub(super) fn lens(
    shape: island::Morph,
    backdrop: Option<&Arc<wallpaper::GlassBackdrop>>,
    fading: Option<(&Arc<wallpaper::GlassBackdrop>, f32)>,
) -> gpui_kit::Div {
    let height_opacity = lens_opacity_for_height(shape.height);
    // `chrome::paint` hands the tucked line to `line_glass`, which paints
    // *over* the lens through the fade band below; fully tucked (height 6)
    // the opacity is zero and nothing paints.
    if height_opacity <= 0.0 {
        return div().absolute().inset_0();
    }
    let mut base = div().absolute().inset_0().opacity(height_opacity);
    let (fading_backdrop, t) = match fading {
        Some((old, t)) if t < 1.0 => (Some(Arc::clone(old)), t),
        _ => (None, 1.0),
    };
    if let Some(old) = fading_backdrop {
        // Old under, new over: a proper cross-fade instead of a hard swap.
        base = base.child(lens_canvas(old, shape, 1.0 - t));
    }
    let Some(backdrop) = backdrop else {
        return base;
    };
    base.child(lens_canvas(Arc::clone(backdrop), shape, t))
}

/// One opacity-wrapped canvas painting `backdrop` mapped onto `shape`.
fn lens_canvas(
    backdrop: Arc<wallpaper::GlassBackdrop>,
    shape: island::Morph,
    opacity: f32,
) -> gpui_kit::Div {
    // `canvas()` defaults to an auto-sized box: in a block parent its height
    // collapses to zero and `paint_image` bails on an empty rect.
    // `size_full` is load-bearing, not decoration.
    div().absolute().inset_0().opacity(opacity).child(
        canvas(
            |_, _, _| (),
            move |bounds: Bounds<Pixels>, (): (), window: &mut Window, _: &mut App| {
                paint_backdrop(bounds, shape, &backdrop, window);
            },
        )
        .size_full(),
    )
}

/// The image a backdrop contributes under `shape`.
///
/// A warp baked for `shape` is painted as-is. Any other shape gets the plain
/// crop: the baked displacement would otherwise land at the old edge, and
/// scaling the old shell onto the new one smears those bands across the
/// panel — the ghost of the previous shape. The plain crop is the desktop, so
/// a morph looks like clear glass until the matching warp lands.
fn lens_image(backdrop: &wallpaper::GlassBackdrop, shape: GlassShape) -> &Arc<RenderImage> {
    if glass_optics::same_shape(backdrop.shape, shape) { &backdrop.image } else { &backdrop.flat }
}

/// One clipped draw: the current shell out of the crop, in stage pixels.
#[allow(
    clippy::cast_precision_loss,
    reason = "geometry floats: the values are window-sized, far under f32's 24-bit mantissa"
)]
fn paint_backdrop(
    bounds: Bounds<Pixels>,
    morph: island::Morph,
    backdrop: &wallpaper::GlassBackdrop,
    window: &mut Window,
) {
    let stage_width = f32::from(bounds.size.width);
    let shape = GlassShape::from(morph);
    let (shell, natural) = glass_optics::crop_rects(stage_width, shape, backdrop.crop);
    let origin = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
    let _painted = window.paint_image(
        to_bounds(shell, origin),
        to_bounds(natural, origin),
        Corners::all(px(shape.radius)),
        Arc::clone(lens_image(backdrop, shape)),
        0,
        false,
    );
}

fn to_bounds(rect: Rect, origin: (f32, f32)) -> Bounds<Pixels> {
    Bounds::new(
        point(px(origin.0 + rect.x), px(origin.1 + rect.y)),
        size(px(rect.width.max(0.0)), px(rect.height.max(0.0))),
    )
}

/// One drop shadow, straight from the reference stylesheet:
/// `0 25px 50px rgba(0, 0, 0, 0.25)`. Rims do not separate a clear card from
/// a busy wallpaper, so the shadow stays.
#[must_use]
pub(super) fn shell_shadows() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: hsla(0.0, 0.0, 0.0, 0.25),
        offset: point(px(0.0), px(25.0)),
        blur_radius: px(50.0),
        spread_radius: px(0.0),
        inset: false,
    }]
}

/// The shell's underlay layers.
///
/// A landed backdrop is the whole material, so there is nothing to paint over
/// it. Before the first warp lands, a dark cool wash keeps the white ink
/// readable on whatever wallpaper is behind the transparent window.
pub(super) fn layers(radius: f32, glass: Option<&Arc<RenderImage>>) -> Vec<gpui_kit::Div> {
    if glass.is_some() {
        return Vec::new();
    }
    let lift = (0.10 + 0.08 * IDLE_LUMA).clamp(0.10, 0.18);
    vec![div().absolute().inset_0().rounded(px(radius)).bg(linear_gradient(
        180.0,
        linear_color_stop(hsla(GLASS_HUE, 0.06, lift, 0.62), 0.0),
        linear_color_stop(hsla(GLASS_HUE, 0.05, lift + 0.05, 0.66), 1.0),
    ))]
}

#[allow(clippy::unwrap_used, clippy::cast_precision_loss)]
#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui_kit::{IntoElement, ParentElement, Styled, div, point, px, size};

    use rustedge::island::{
        CARD_HEIGHT, CARD_RADIUS, CARD_WIDTH, IslandState, STAGE_HEIGHT, STAGE_WIDTH,
    };

    use super::*;

    fn backdrop() -> Arc<wallpaper::GlassBackdrop> {
        let image = Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(
            image::RgbaImage::new(2, 2)
        )]));
        let flat = Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(
            image::RgbaImage::from_pixel(2, 2, image::Rgba([9, 9, 9, 255]))
        )]));
        Arc::new(wallpaper::GlassBackdrop {
            image,
            flat,
            luma: 0.5,
            crop: Rect { x: -128.0, y: 0.0, width: 676.0, height: 616.0 },
            shape: GlassShape::ZERO,
            generation: 0,
        })
    }

    #[test]
    fn fade_progress_runs_zero_to_one_across_the_window() {
        assert_eq!(fade_progress(0), 0.0);
        assert_eq!(fade_progress(GLASS_FADE_MS), 1.0);
        assert_eq!(fade_progress(GLASS_FADE_MS * 10), 1.0, "clamped past the end");
        let half = fade_progress(GLASS_FADE_MS / 2);
        assert!(half > 0.4 && half < 0.6);
        let mut previous = 0.0;
        let mut elapsed = 0;
        while elapsed <= GLASS_FADE_MS {
            let t = fade_progress(elapsed);
            assert!(t >= previous, "fade must not go backwards at {elapsed}ms");
            previous = t;
            elapsed += 10;
        }
    }

    #[test]
    fn lens_opacity_fades_out_across_the_line_crossing() {
        assert_eq!(lens_opacity_for_height(6.0), 0.0, "tucked line: no lens");
        assert_eq!(lens_opacity_for_height(7.0), 0.0);
        assert_eq!(lens_opacity_for_height(13.0), 1.0);
        assert_eq!(lens_opacity_for_height(56.0), 1.0, "capsule: full lens");
        let mid = lens_opacity_for_height(10.0);
        assert!(mid > 0.3 && mid < 0.7, "the crossing blends, not cuts");
        assert!(lens_opacity_for_height(9.0) < lens_opacity_for_height(11.0));
    }

    /// Regression: a warp's displacement lives *inside* the shell it was baked
    /// for. Painting that warp under a morphing shell used to scale the baked
    /// panel onto the live one, which stretched the previous shape's edge
    /// bands into a ghost across the panel. Every mismatch must fall back to
    /// the plain crop, which sits pixel-exact on the desktop.
    #[test]
    fn a_warp_baked_for_another_shape_paints_the_plain_crop() {
        let card = GlassShape::from(island::resting(IslandState::Card));
        let capsule = GlassShape::from(island::resting(IslandState::Capsule));
        assert_ne!(card, capsule, "the two states must differ for this test");
        let mut baked = (*backdrop()).clone();
        baked.shape = capsule;
        assert!(
            Arc::ptr_eq(lens_image(&baked, capsule), &baked.image),
            "the baked shape paints its refraction"
        );
        assert!(
            Arc::ptr_eq(lens_image(&baked, card), &baked.flat),
            "a stale shape must paint the plain crop, never a scaled warp"
        );
        // An in-between morph frame is a mismatch too: any scaling moves the
        // bands, so only a (sub-pixel) shape match may paint the warp.
        let growing = GlassShape { width: 400.0, height: 200.0, radius: 22.0 };
        assert!(Arc::ptr_eq(lens_image(&baked, growing), &baked.flat));
        let nudged = GlassShape { width: capsule.width + 0.2, ..capsule };
        assert!(
            Arc::ptr_eq(lens_image(&baked, nudged), &baked.image),
            "a sub-pixel difference is still this warp"
        );
    }

    /// The lens is painted from a canvas. A bare `canvas()` has an auto
    /// height in a block parent and collapses to zero, which makes
    /// `paint_image` bail on its empty visible rect: the lens silently never
    /// appears. Its bounds must be the whole stage.
    #[gpui_kit::test]
    fn the_lens_canvas_fills_the_stage(cx: &mut gpui_kit::TestAppContext) {
        let measured = Rc::new(Cell::new((0.0f32, 0.0f32)));
        let image = backdrop();
        let slot = Rc::clone(&measured);
        let lens = super::lens(island::resting(IslandState::Card), Some(&image), None)
            .on_children_prepainted(move |children, _, _| {
                if let Some(bounds) = children.last() {
                    slot.set((f32::from(bounds.size.width), f32::from(bounds.size.height)));
                }
            });
        cx.add_empty_window().draw(
            point(px(0.0), px(0.0)),
            size(px(STAGE_WIDTH as f32), px(STAGE_HEIGHT as f32)),
            move |_window, _app| div().size_full().relative().child(lens).into_any_element(),
        );
        let (width, height) = measured.get();
        assert!(
            (width - STAGE_WIDTH as f32).abs() < 0.01,
            "lens canvas width {width} does not fill the {STAGE_WIDTH}px stage"
        );
        assert!(
            (height - STAGE_HEIGHT as f32).abs() < 0.01,
            "lens canvas height {height} does not fill the {STAGE_HEIGHT}px stage"
        );
    }

    #[test]
    fn a_landed_backdrop_needs_no_layers_over_it() {
        let image = backdrop();
        let layers = super::layers(CARD_RADIUS, Some(&image.image));
        assert!(layers.is_empty(), "the warp is the whole material");
    }

    #[test]
    fn the_boot_wash_covers_the_shell_before_a_backdrop_lands() {
        let layers = super::layers(CARD_RADIUS, None);
        assert_eq!(layers.len(), 1, "exactly one fallback wash");
        assert!(
            (CARD_WIDTH - 420.0).abs() < 0.01 && (CARD_HEIGHT - 288.0).abs() < 0.01,
            "the card constants moved"
        );
    }
}
