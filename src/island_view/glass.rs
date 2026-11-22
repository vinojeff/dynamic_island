//! The liquid-glass optics: everything the shell paints that reads as glass.
//!
//! GPUI has no backdrop blur and no shader hooks here, so each Flutter
//! `liquid_glass` idea maps to a painter primitive:
//!
//! | Flutter vocabulary | Here |
//! | --- | --- |
//! | blur (frost) | none: the crisp wallpaper crop [`lens`] bends |
//! | refraction (index, width) | one CPU SDF warp baked into the crop, [`lens`] paints it once |
//! | dispersion | R/G/B sample offsets inside the warp — [`glass_optics::DISPERSION`] |
//! | specular (angle, spread) | the diagonal white streak, two stacked two-stop gradients |
//! | saturation | boosted while cropping, [`crate::wallpaper::SATURATION`] |
//! | adaptivity | the scrim follows the crop's luma, [`veil_alpha`] |
//!
//! [`lens`] spans the root and paints once; [`layers`] are the shell's own
//! underlays, painted before the body so content stays above every optic.

use std::sync::Arc;

use gpui_kit::{
    App, Bounds, BoxShadow, Corners, ParentElement, Pixels, RenderImage, Styled, Window, canvas,
    div, hsla, linear_color_stop, linear_gradient, point, px, size,
};

use rustedge::island;

use crate::glass_optics::{self, GlassShape, Rect};
use crate::wallpaper;

/// Width of the refraction rim, physical pixels. Flutter's `refractionWidth`.
///
/// Three bands wide: a bright edge decomposing inward, which is what makes
/// the silhouette read as a lens rather than a colored rectangle.
pub(super) const REFRACTION_WIDTH: f32 = 4.0;

/// How long one backdrop cross-fades into the next after a re-warp lands.
pub(super) const GLASS_FADE_MS: u64 = 120;

/// Below this shell height the wallpaper lens is fully faded out; the line
/// glass covers the crossing.
const LENS_FADE_LOW: f32 = 7.0;

/// Above this shell height the wallpaper lens is fully opaque.
const LENS_FADE_HIGH: f32 = 13.0;

/// Diagonal direction of the specular streak, degrees clockwise from top.
pub(super) const SPECULAR_ANGLE: f32 = 130.0;

/// The glass hue. The same family as the tucked line so both read as one
/// material (`GLASS_HUE` in `chrome.rs`).
pub(super) const GLASS_HUE: f32 = 0.54;

/// The scrim's luma while no crop has landed yet — a mid-gray wallpaper.
pub(super) const IDLE_LUMA: f32 = 0.35;

/// How many concentric bands the rim renders, bright rim inward.
const RIM_BANDS: usize = 3;

/// The milky veil a luma calls for. A dark wallpaper needs more milk so the
/// dark ink stays readable; a bright wallpaper needs less so its color shows.
///
/// The face is now a sharp crop with a thin bent band, so the old 0.16..0.26
/// veil read as fog over a clear image; these values keep only a tint. If
/// dark ink ever struggles on a dark wallpaper, raise the clamp floor first.
#[must_use]
pub(super) fn veil_alpha(luma: f32) -> f32 {
    (0.15 - 0.09 * luma).clamp(0.06, 0.15)
}

/// Rim brightness by band: brightest flush against the edge, falling off
/// inward, so the rim reads as a lens edge pressed on the wall.
#[must_use]
pub(super) fn rim_alpha(band: usize) -> f32 {
    match band {
        0 => 0.55,
        1 => 0.20,
        2 => 0.08,
        _ => 0.0,
    }
}

/// One shell shadow value list: the drop shadow that existed before glass
/// (kept — rims do not separate a card from a busy wallpaper), the dark inner
/// hem on the lower rim, and the cyan glow that ties the shell to the tucked
/// line.
#[must_use]
pub(super) fn shell_shadows() -> Vec<BoxShadow> {
    vec![
        BoxShadow {
            color: hsla(0.0, 0.0, 0.0, 0.30),
            offset: point(px(0.0), px(14.0)),
            blur_radius: px(44.0),
            spread_radius: px(0.0),
            inset: false,
        },
        BoxShadow {
            // The hem. Real glass is weightier along the bottom rim.
            color: hsla(0.55, 0.08, 0.18, 0.16),
            offset: point(px(0.0), px(2.0)),
            blur_radius: px(6.0),
            spread_radius: px(0.0),
            inset: true,
        },
        BoxShadow {
            // The glow. Same hue family as the tucked line's bloom.
            color: hsla(GLASS_HUE, 0.60, 0.70, 0.10),
            offset: point(px(0.0), px(0.0)),
            blur_radius: px(24.0),
            spread_radius: px(0.0),
            inset: false,
        },
    ]
}

/// 0.0 → 1.0 across [`GLASS_FADE_MS`] of elapsed time.
#[must_use]
pub(crate) fn fade_progress(elapsed_ms: u64) -> f32 {
    #[allow(
        clippy::cast_precision_loss,
        reason = "milliseconds fit in f32 for this use"
    )]
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
/// clips the shell out of the crop's natural rect. `fading` is the previous
/// backdrop plus the cross-fade progress — the new warp blends over it so a
/// landing never snaps. Empty when the opacity ramp or the fade says so; the
/// fallback wash in [`layers`] takes over.
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
    div()
        .absolute()
        .inset_0()
        .opacity(opacity)
        .child(
            canvas(
                |_, _, _| (),
                move |bounds: Bounds<Pixels>, (): (), window: &mut Window, _: &mut App| {
                    paint_backdrop(bounds, shape, &backdrop, window);
                },
            )
            .size_full(),
        )
}

/// One clipped draw: the current shell out of the (possibly stale-scaled)
/// crop rect. A backdrop baked for an older shape is mapped from its own
/// shell onto the current one so the old bevel rides the new edge instead of
/// ghosting inside the panel.
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
    let mut image_rect = natural;
    if backdrop.shape != shape && backdrop.shape.width > 0.0 && backdrop.shape.height > 0.0 {
        let baked = glass_optics::crop_rects(stage_width, backdrop.shape, backdrop.crop);
        image_rect = glass_optics::stale_image_bounds(natural, baked.0, shell);
    }
    let origin = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
    let _painted = window.paint_image(
        to_bounds(shell, origin),
        to_bounds(image_rect, origin),
        Corners::all(px(shape.radius)),
        Arc::clone(&backdrop.image),
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

/// The shell's underlay layers: scrim, specular sheen, rim, dispersion.
///
/// Painted ahead of the body so text and controls sit on top of the glass.
/// Each layer carries the shell's own corner radius: gpui's overflow mask is
/// a plain rectangle (`ContentMask { bounds }`), so the silhouette has to
/// come from every layer's own rounded background — same as [`lens`] clips
/// through `paint_image`'s radii.
///
/// With no backdrop yet, the scrim is the fallback wash — a light two-stop
/// glass gradient standing in until the wallpaper crop arrives.
pub(super) fn layers(radius: f32, glass: Option<(&Arc<RenderImage>, f32)>) -> Vec<gpui_kit::Div> {
    let luma = glass.map_or(IDLE_LUMA, |(_, luma)| luma);
    let seeded = glass.is_some();
    let shape = div().absolute().inset_0().rounded(px(radius));
    let scrim = if seeded {
        let top = veil_alpha(luma);
        let bottom = (top + 0.04).min(0.21);
        shape.bg(linear_gradient(
            180.0,
            linear_color_stop(hsla(GLASS_HUE, 0.14, 0.98, top), 0.0),
            linear_color_stop(hsla(GLASS_HUE, 0.09, 0.92, bottom), 1.0),
        ))
    } else {
        shape.bg(linear_gradient(
            180.0,
            linear_color_stop(hsla(GLASS_HUE, 0.14, 0.97, 0.60), 0.0),
            linear_color_stop(hsla(GLASS_HUE, 0.09, 0.90, 0.68), 1.0),
        ))
    };
    // The sheen. GPUI gradients take exactly two stops, so the three-stop
    // streak is two stacked divs meeting at the transparent midpoint.
    // Faint on purpose: a 0.28 wash combined with the scrim turned the face
    // into flat milk and erased the lens band underneath it.
    let sheen_high = div()
        .absolute()
        .inset_0()
        .rounded(px(radius))
        .bg(linear_gradient(
            SPECULAR_ANGLE,
            linear_color_stop(hsla(0.0, 1.0, 1.0, 0.07), 0.0),
            linear_color_stop(hsla(0.0, 1.0, 1.0, 0.0), 0.5),
        ));
    let sheen_low = div()
        .absolute()
        .inset_0()
        .rounded(px(radius))
        .bg(linear_gradient(
            SPECULAR_ANGLE,
            linear_color_stop(hsla(0.0, 1.0, 1.0, 0.0), 0.5),
            linear_color_stop(hsla(0.0, 1.0, 1.0, 0.03), 1.0),
        ));
    // Same auto-size trap as the lens: without `size_full` the canvas is
    // zero-high and every `paint_ring` band fails its size guard.
    let rim = div().absolute().inset_0().child(
        canvas(
            |_, _, _| (),
            move |bounds: Bounds<Pixels>, (): (), window: &mut Window, _: &mut App| {
                paint_rim(bounds, radius, window);
            },
        )
        .size_full(),
    );
    vec![scrim, sheen_high, sheen_low, rim]
}

/// Paints the refraction rim and its chromatic fringe inside one canvas.
///
/// Each band is a ring between two rounded contours; the inner contour is
/// traced backwards so the hole stays a hole instead of a second fill.
fn paint_rim(bounds: Bounds<Pixels>, radius: f32, window: &mut Window) {
    // The Fresnel falloff: three white bands, bright at the very edge.
    #[allow(
        clippy::cast_precision_loss,
        reason = "RIM_BANDS is 3; the quotient is exact in f32"
    )]
    let band_width = REFRACTION_WIDTH / RIM_BANDS as f32;
    for band in 0..RIM_BANDS {
        #[allow(
            clippy::cast_precision_loss,
            reason = "the band index is a small integer"
        )]
        let inset = band as f32 * band_width;
        paint_ring(
            bounds,
            inset,
            band_width,
            radius,
            hsla(0.0, 0.0, 1.0, rim_alpha(band)),
            window,
        );
    }
}

/// Fills one ring band: `inset` from the edge, `band` thick, corner radii
/// shrunk by exactly the inset so the band hugs the silhouette.
#[allow(clippy::too_many_lines)]
fn paint_ring(
    bounds: Bounds<Pixels>,
    inset: f32,
    band: f32,
    radius: f32,
    color: gpui_kit::Hsla,
    window: &mut Window,
) {
    let left = f32::from(bounds.origin.x);
    let top = f32::from(bounds.origin.y);
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    let outer = (
        left + inset,
        top + inset,
        width - 2.0 * inset,
        height - 2.0 * inset,
    );
    let deep = inset + band;
    let inner = (
        left + deep,
        top + deep,
        width - 2.0 * deep,
        height - 2.0 * deep,
    );
    if outer.2 <= 0.0 || outer.3 <= 0.0 || inner.2 <= 0.0 || inner.3 <= 0.0 {
        return;
    }
    let outer_radius = (radius - inset).max(0.0);
    let inner_radius = (radius - deep).max(0.0);
    let mut ring = gpui_kit::PathBuilder::fill();
    let out_walk = walk_rounded(outer, outer_radius, false);
    let in_walk = walk_rounded(inner, inner_radius, true);
    let Some(first) = out_walk.first().copied() else {
        return;
    };
    ring.move_to(first);
    for at in out_walk.iter().skip(1) {
        ring.line_to(*at);
    }
    for at in &in_walk {
        ring.line_to(*at);
    }
    ring.close();
    if let Ok(path) = ring.build() {
        window.paint_path(path, color);
    }
}

/// Samples one rounded-rect contour clockwise (or backwards) at four arcs.
#[allow(
    clippy::cast_precision_loss,
    reason = "geometry floats: the values are window-sized, far under f32's \
              24-bit mantissa"
)]
fn walk_rounded(
    rect: (f32, f32, f32, f32),
    radius: f32,
    reverse: bool,
) -> Vec<gpui_kit::Point<Pixels>> {
    const ARC_STEPS: usize = 8;
    let (left, top, width, height) = rect;
    let edge = radius.min(width / 2.0).min(height / 2.0);
    let mut points = Vec::with_capacity(4 * ARC_STEPS + 4);
    // Corner centers, clockwise from the top-right.
    let centers = [
        (left + width - edge, top + edge),
        (left + width - edge, top + height - edge),
        (left + edge, top + height - edge),
        (left + edge, top + edge),
    ];
    for (index, (corner_x, corner_y)) in centers.iter().enumerate() {
        for step in 0..=ARC_STEPS {
            let angle = -std::f32::consts::FRAC_PI_2
                + (index as f32 + step as f32 / ARC_STEPS as f32) * std::f32::consts::FRAC_PI_2;
            points.push(point(
                px(corner_x + edge * angle.cos()),
                px(corner_y + edge * angle.sin()),
            ));
        }
    }
    if reverse {
        points.reverse();
    }
    points
}

#[allow(clippy::unwrap_used, clippy::cast_precision_loss)]
#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui_kit::{IntoElement, ParentElement, Styled, div, point, px, size};

    use rustedge::island::{IslandState, CARD_HEIGHT, CARD_RADIUS, CARD_WIDTH, STAGE_HEIGHT, STAGE_WIDTH};

    use super::*;

    fn backdrop() -> Arc<wallpaper::GlassBackdrop> {
        let image = Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(
            image::RgbaImage::new(2, 2)
        )]));
        Arc::new(wallpaper::GlassBackdrop {
            image,
            luma: 0.5,
            crop: Rect { x: -48.0, y: -48.0, width: 516.0, height: 456.0 },
            shape: GlassShape::ZERO,
            generation: 0,
        })
    }

    #[test]
    fn veil_thickens_on_a_dark_wallpaper_and_thins_on_a_bright_one() {
        assert_eq!(veil_alpha(0.0), 0.15, "dark wallpaper → more milk");
        assert!(
            (veil_alpha(1.0) - 0.06).abs() < 1e-6,
            "bright wallpaper → less milk"
        );
        let mut luma = 0.0;
        while luma <= 1.0 {
            assert!(
                (0.06..=0.15).contains(&veil_alpha(luma)),
                "luma {luma} escapes the clamp"
            );
            luma += 0.05;
        }
    }

    #[test]
    fn rim_alpha_falls_off_inward_and_exactly_bright_at_edge() {
        assert_eq!(rim_alpha(0), 0.55, "the edge is the Fresnel peak");
        assert!(rim_alpha(0) > rim_alpha(1) && rim_alpha(1) > rim_alpha(2));
        assert_eq!(rim_alpha(RIM_BANDS), 0.0, "beyond the rim: nothing");
    }

    #[test]
    fn fade_progress_runs_zero_to_one_across_the_window() {
        assert_eq!(fade_progress(0), 0.0);
        assert_eq!(fade_progress(GLASS_FADE_MS), 1.0);
        assert_eq!(fade_progress(GLASS_FADE_MS * 10), 1.0, "clamped past the end");
        let half = fade_progress(GLASS_FADE_MS / 2);
        assert!(half > 0.4 && half < 0.6);
        let mut t = 0.0;
        while t <= GLASS_FADE_MS as f32 {
            let next = fade_progress(t as u64 + 10);
            assert!(next >= t / GLASS_FADE_MS as f32, "fade must not go backwards");
            t += 10.0;
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

    /// Same trap for the rim canvas: zero height means every band fails
    /// `paint_ring`'s positive-size guard and the rim never paints either.
    #[gpui_kit::test]
    fn the_rim_canvas_fills_the_shell(cx: &mut gpui_kit::TestAppContext) {
        let measured = Rc::new(Cell::new((0.0f32, 0.0f32)));
        let image = backdrop();
        let mut layers = super::layers(CARD_RADIUS, Some((&image.image, 0.5)));
        let rim = layers.pop().unwrap();
        let slot = Rc::clone(&measured);
        let rim = rim.on_children_prepainted(move |children, _, _| {
            if let Some(bounds) = children.last() {
                slot.set((f32::from(bounds.size.width), f32::from(bounds.size.height)));
            }
        });
        cx.add_empty_window().draw(
            point(px(0.0), px(0.0)),
            size(px(CARD_WIDTH), px(CARD_HEIGHT)),
            move |_window, _app| div().size_full().relative().child(rim).into_any_element(),
        );
        let (width, height) = measured.get();
        assert!(
            (width - CARD_WIDTH).abs() < 0.01,
            "rim canvas width {width} does not fill the {CARD_WIDTH}px shell"
        );
        assert!(
            (height - CARD_HEIGHT).abs() < 0.01,
            "rim canvas height {height} does not fill the {CARD_HEIGHT}px shell"
        );
    }
}
