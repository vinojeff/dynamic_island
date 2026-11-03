//! The liquid-glass optics: everything the shell paints that reads as glass.
//!
//! GPUI has no backdrop blur and no shader hooks here, so each Flutter
//! `liquid_glass` idea maps to a painter primitive:
//!
//! | Flutter vocabulary | Here |
//! | --- | --- |
//! | blur (frost) | the frosted wallpaper crop [`frost`] paints below everything |
//! | refraction (index, width) | the Fresnel rim bands, [`rim_alpha`] |
//! | dispersion | warm/cool fringe halves riding the rim, [`DISPERSION`] |
//! | specular (angle, spread) | the diagonal white streak, two stacked two-stop gradients |
//! | saturation | boosted while cropping, [`crate::wallpaper::SATURATION`] |
//! | adaptivity | the scrim follows the crop's luma, [`plate_alpha`] |
//!
//! [`frost`] spans the root and paints once; [`layers`] are the shell's own
//! underlays, painted before the body so content stays above every optic.

use std::sync::Arc;

use gpui_kit::{
    App, Bounds, BoxShadow, Corners, ParentElement, Pixels, RenderImage, Styled, Window, canvas,
    div, hsla, linear_color_stop, linear_gradient, point, px, size,
};

use rustedge::island::{self, IslandState};

use crate::wallpaper;

/// Width of the refraction rim, physical pixels. Flutter's `refractionWidth`.
///
/// Three bands wide: a bright edge decomposing inward, which is what makes
/// the silhouette read as a lens rather than a colored rectangle.
pub(super) const REFRACTION_WIDTH: f32 = 4.0;

/// Horizontal split of the chromatic fringe, physical pixels. Flutter's
/// `dispersion`. The warm fringe rides the outer half of the rim, the cool
/// fringe the inner half.
pub(super) const DISPERSION: f32 = 2.0;

/// Diagonal direction of the specular streak, degrees clockwise from top.
pub(super) const SPECULAR_ANGLE: f32 = 130.0;

/// The glass hue. The same family as the tucked line so both read as one
/// material (`GLASS_HUE` in `chrome.rs`).
pub(super) const GLASS_HUE: f32 = 0.54;

/// The scrim's luma while no crop has landed yet — a mid-gray wallpaper.
pub(super) const IDLE_LUMA: f32 = 0.35;

/// How many concentric bands the rim renders, bright rim inward.
const RIM_BANDS: usize = 3;

/// Depth of the lens band inside the silhouette, logical pixels.
pub(super) const REFRACTION_DEPTH: f32 = 18.0;

/// Rings used to approximate the lens falloff. Twenty-four 0.75px bands put
/// the seam step at `REFRACTION_PULL / REFRACTION_STEPS`, small enough that no
/// ring reads as a jump.
const REFRACTION_STEPS: usize = 24;

/// How far the wallpaper is pulled outward at the silhouette, logical pixels.
///
/// The band is [`REFRACTION_DEPTH`] deep, so the rim reads a 34px strip of
/// desktop compressed into 18px — that squeeze is what makes a lens read as
/// thick glass rather than as a plain blur. Must stay under
/// [`crate::wallpaper::REFRACTION_MARGIN`].
const REFRACTION_PULL: f32 = 34.0;

/// The milky veil a luma calls for. A dark wallpaper needs more milk so the
/// dark ink stays readable; a bright wallpaper needs less so its color shows.
#[must_use]
pub(super) fn veil_alpha(luma: f32) -> f32 {
    (0.26 - 0.10 * luma).clamp(0.16, 0.26)
}

/// Outward pull of the lens `inset` logical pixels inside the silhouette.
///
/// Maximal flush against the edge, zero at [`REFRACTION_DEPTH`]. The falloff
/// is linear so neighbouring rings always differ by the same step and no band
/// reads as a jump.
#[must_use]
pub(super) fn refraction_pull(inset: f32) -> f32 {
    let t = (1.0 - inset / REFRACTION_DEPTH).clamp(0.0, 1.0);
    REFRACTION_PULL * t
}

/// Window rect that samples `pull` logical pixels outside the silhouette.
///
/// The stage rect is the crop's extent, so drawing it into a rect shrunk about
/// the shell centre maps window `p` to wallpaper
/// `anchor + (p - anchor) * w / (w - 2 * pull)`: the rim reads the desktop
/// outside the panel and compresses it inward, which is how a thick glass edge
/// bends the world. `pull` must stay under the crop's margin or the lens would
/// run off the edge of the image.
#[must_use]
pub(super) fn lens_bounds(
    stage: (f32, f32, f32, f32),
    anchor: (f32, f32),
    pull: (f32, f32),
) -> (f32, f32, f32, f32) {
    let (left, top, width, height) = stage;
    if width <= 2.0 * pull.0 || height <= 2.0 * pull.1 {
        return stage;
    }
    let fx = (width - 2.0 * pull.0) / width;
    let fy = (height - 2.0 * pull.1) / height;
    (
        anchor.0 + (left - anchor.0) * fx,
        anchor.1 + (top - anchor.1) * fy,
        width * fx,
        height * fy,
    )
}

/// Horizontal amount the shell sits right of the window's left edge.
///
/// The root centers the shell, and the card spans the full stage, so the card
/// lands at zero offset — exactly where the wallpaper behind it is.
#[must_use]
pub(super) fn shell_offset(shape_width: f32) -> f32 {
    // Widening the u32 stage width into f32 loses nothing.
    #[allow(clippy::cast_precision_loss)]
    let offset = (island::STAGE_WIDTH as f32 - shape_width) / 2.0;
    offset
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

/// The frosted wallpaper under the shell, painted once at the root.
///
/// The canvas spans the whole window so its bounds match the stage one-to-one;
/// `paint_image` then clips to the shell's rounded rect, which is what carries
/// the silhouette (gpui's overflow mask is a plain rectangle). Empty when
/// tucked as the line or before the first build lands — the fallback wash in
/// [`layers`] takes over.
pub(super) fn frost(
    shape: island::Morph,
    state: IslandState,
    backdrop: Option<&Arc<wallpaper::GlassBackdrop>>,
) -> gpui_kit::Div {
    let base = div().absolute().inset_0();
    // `chrome::paint` hands the tucked line to `line_glass` below 12px, so the
    // frost must not paint a blurred sliver under it during the tuck morph.
    if state == IslandState::Line || shape.height < 12.0 {
        return base;
    }
    let Some(backdrop) = backdrop else {
        return base;
    };
    let backdrop = Arc::clone(backdrop);
    let width = shape.width;
    let height = shape.height;
    let radius = px(shape.radius);
    // `canvas()` defaults to an auto-sized box: in this block parent its
    // height collapses to zero and `paint_image` bails on an empty rect.
    // `size_full` is load-bearing, not decoration.
    base.child(
        canvas(
            |_, _, _| (),
            move |bounds: Bounds<Pixels>, (): (), window: &mut Window, _: &mut App| {
                paint_frost(bounds, (width, height), radius, &backdrop, window);
            },
        )
        .size_full(),
    )
}

/// Paints the crop as one ring per [`REFRACTION_STEPS`] plus the flat centre.
///
/// Ring `k` clips the shell inset by `k * step` and draws the crop shrunk
/// about the shell centre by that ring's [`refraction_pull`], so the rim reads
/// the wallpaper *outside* the panel and compresses it inward. The rings use
/// the sharp `rim` copy — the frosted `face` would blur the displacement away
/// — and the centre is the untouched frosted crop.
fn paint_frost(
    bounds: Bounds<Pixels>,
    (width, height): (f32, f32),
    radius: Pixels,
    backdrop: &wallpaper::GlassBackdrop,
    window: &mut Window,
) {
    let pad = backdrop.pad;
    let shell = Bounds::new(
        point(bounds.origin.x + px(shell_offset(width)), bounds.origin.y),
        size(px(width), px(height)),
    );
    // The crop covers the stage plus the lens margin the builder kept.
    let natural = Bounds::new(
        point(bounds.origin.x - px(pad.0), bounds.origin.y - px(pad.1)),
        size(
            px(f32::from(bounds.size.width) + pad.0 + pad.2),
            px(f32::from(bounds.size.height) + pad.1 + pad.3),
        ),
    );
    let stage = (
        f32::from(natural.origin.x),
        f32::from(natural.origin.y),
        f32::from(natural.size.width),
        f32::from(natural.size.height),
    );
    let anchor = (
        f32::from(shell.origin.x) + width / 2.0,
        f32::from(shell.origin.y) + height / 2.0,
    );
    // A screen edge leaves no margin on that side, so the lens cannot reach
    // past it; cap each axis by the least margin it actually has.
    let reach = (
        pad.0.min(pad.2).min(REFRACTION_PULL),
        pad.1.min(pad.3).min(REFRACTION_PULL),
    );
    #[allow(
        clippy::cast_precision_loss,
        reason = "REFRACTION_STEPS is 24; the quotient is exact in f32"
    )]
    let step = REFRACTION_DEPTH / REFRACTION_STEPS as f32;
    for ring in 0..REFRACTION_STEPS {
        #[allow(
            clippy::cast_precision_loss,
            reason = "the ring index is a small integer"
        )]
        let inset = ring as f32 * step;
        // `refraction_pull` is the full-strength figure; scale it by how far
        // this panel may actually reach past its own edge.
        let fade = refraction_pull(inset) / REFRACTION_PULL;
        let lean = (reach.0 * fade, reach.1 * fade);
        paint_crop(
            inset_shell(shell, inset),
            bounds_from(lens_bounds(stage, anchor, lean)),
            (radius - px(inset)).max(px(0.0)),
            &backdrop.lens,
            window,
        );
    }
    // The centre keeps the frosted crop exactly as sampled: no lens, no pull.
    paint_crop(
        inset_shell(shell, REFRACTION_DEPTH),
        natural,
        (radius - px(REFRACTION_DEPTH)).max(px(0.0)),
        &backdrop.image,
        window,
    );
}

/// One clipped draw of the crop. The shell's own corner radius is reduced by
/// the inset so every ring hugs the silhouette.
fn paint_crop(
    clip: Bounds<Pixels>,
    image_bounds: Bounds<Pixels>,
    radius: Pixels,
    image: &Arc<RenderImage>,
    window: &mut Window,
) {
    let _painted = window.paint_image(
        clip,
        image_bounds,
        Corners::all(radius),
        Arc::clone(image),
        0,
        false,
    );
}

/// Shrinks the shell by `inset` on every side, never past zero.
fn inset_shell(shell: Bounds<Pixels>, inset: f32) -> Bounds<Pixels> {
    let width = (f32::from(shell.size.width) - 2.0 * inset).max(0.0);
    let height = (f32::from(shell.size.height) - 2.0 * inset).max(0.0);
    Bounds::new(
        point(shell.origin.x + px(inset), shell.origin.y + px(inset)),
        size(px(width), px(height)),
    )
}

/// Converts the float rect from [`scaled_about`] back into window bounds.
fn bounds_from(rect: (f32, f32, f32, f32)) -> Bounds<Pixels> {
    let (left, top, width, height) = rect;
    Bounds::new(
        point(px(left), px(top)),
        size(px(width.max(0.0)), px(height.max(0.0))),
    )
}

/// The shell's underlay layers: scrim, specular sheen, rim, dispersion.
///
/// Painted ahead of the body so text and controls sit on top of the glass.
/// Each layer carries the shell's own corner radius: gpui's overflow mask is
/// a plain rectangle (`ContentMask { bounds }`), so the silhouette has to
/// come from every layer's own rounded background — same as [`frost`] clips
/// through `paint_image`'s radii.
///
/// With no backdrop yet, the scrim is the fallback wash — a light two-stop
/// glass gradient standing in until the frosted crop arrives.
pub(super) fn layers(radius: f32, glass: Option<(&Arc<RenderImage>, f32)>) -> Vec<gpui_kit::Div> {
    let luma = glass.map_or(IDLE_LUMA, |(_, luma)| luma);
    let seeded = glass.is_some();
    let shape = div().absolute().inset_0().rounded(px(radius));
    let scrim = if seeded {
        let top = veil_alpha(luma);
        let bottom = (top + 0.06).min(0.32);
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
            linear_color_stop(hsla(0.0, 1.0, 1.0, 0.12), 0.0),
            linear_color_stop(hsla(0.0, 1.0, 1.0, 0.0), 0.5),
        ));
    let sheen_low = div()
        .absolute()
        .inset_0()
        .rounded(px(radius))
        .bg(linear_gradient(
            SPECULAR_ANGLE,
            linear_color_stop(hsla(0.0, 1.0, 1.0, 0.0), 0.5),
            linear_color_stop(hsla(0.0, 1.0, 1.0, 0.05), 1.0),
        ));
    // Same auto-size trap as the frost: without `size_full` the canvas is
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
    // The dispersion: the red end of real glass rides the outer rim half, the
    // blue end the inner half. Faint by design.
    paint_ring(
        bounds,
        0.0,
        DISPERSION,
        radius,
        hsla(0.98, 0.80, 0.60, 0.09),
        window,
    );
    paint_ring(
        bounds,
        DISPERSION,
        DISPERSION,
        radius,
        hsla(0.56, 0.80, 0.60, 0.09),
        window,
    );
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

    use rustedge::island::{
        CARD_HEIGHT, CARD_RADIUS, CARD_WIDTH, PILL_WIDTH, STAGE_HEIGHT, STAGE_WIDTH,
    };

    use super::*;

    fn backdrop() -> Arc<wallpaper::GlassBackdrop> {
        let image = Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(
            image::RgbaImage::new(2, 2)
        )]));
        let margin = wallpaper::REFRACTION_MARGIN;
        Arc::new(wallpaper::GlassBackdrop {
            image: Arc::clone(&image),
            lens: image,
            luma: 0.5,
            pad: (margin, margin, margin, margin),
        })
    }

    #[test]
    fn veil_thickens_on_a_dark_wallpaper_and_thins_on_a_bright_one() {
        assert_eq!(veil_alpha(0.0), 0.26, "dark wallpaper → more milk");
        assert_eq!(veil_alpha(1.0), 0.16, "bright wallpaper → less milk");
        let mut luma = 0.0;
        while luma <= 1.0 {
            assert!(
                (0.16..=0.26).contains(&veil_alpha(luma)),
                "luma {luma} escapes the clamp"
            );
            luma += 0.05;
        }
    }

    #[test]
    fn the_lens_pulls_hardest_at_the_edge_and_stops_at_the_depth() {
        assert_eq!(refraction_pull(0.0), REFRACTION_PULL);
        assert_eq!(refraction_pull(REFRACTION_DEPTH), 0.0);
        assert_eq!(
            refraction_pull(REFRACTION_DEPTH * 2.0),
            0.0,
            "past the band the crop is untouched"
        );
        let mut inset = 0.0;
        while inset < REFRACTION_DEPTH {
            let next = (inset + 0.5).min(REFRACTION_DEPTH);
            assert!(
                refraction_pull(inset) > refraction_pull(next),
                "the pull must fall off inward at {inset}"
            );
            inset = next;
        }
    }

    #[test]
    fn the_lens_rect_samples_outside_the_silhouette() {
        // The crop covers the stage plus a 26px margin on every side.
        let stage = (-26.0, -26.0, 472.0, 412.0);
        let anchor = (210.0, 140.0);
        let pull = (20.0, 20.0);
        let (left, top, width, height) = lens_bounds(stage, anchor, pull);
        // Shrinking about the anchor walks the drawn rect inward, which makes
        // the rim read wallpaper from beyond the panel edge.
        assert!(left > stage.0 && top > stage.1);
        near(width, 432.0);
        near(height, 372.0);
        // The anchor maps onto itself.
        near(left + (anchor.0 - stage.0) * (width / stage.2), anchor.0);
        near(top + (anchor.1 - stage.1) * (height / stage.3), anchor.1);
        // No pull means identity.
        assert_eq!(lens_bounds(stage, anchor, (0.0, 0.0)), stage);
        // A pull that would eat the crop is refused rather than inverted.
        assert_eq!(lens_bounds(stage, anchor, (300.0, 300.0)), stage);
    }

    #[test]
    fn shell_offset_centers_the_capsule_and_zeros_the_card() {
        near(
            shell_offset(PILL_WIDTH),
            island::STAGE_WIDTH as f32 / 2.0 - PILL_WIDTH / 2.0,
        );
        near(shell_offset(CARD_WIDTH), 0.0);
        // A mid-morph width stays symmetric about the center.
        let mid = 320.0;
        near(
            island::STAGE_WIDTH as f32 / 2.0 - shell_offset(mid) - mid / 2.0,
            shell_offset(mid) + mid / 2.0 - island::STAGE_WIDTH as f32 / 2.0,
        );
    }

    #[test]
    fn rim_alpha_falls_off_inward_and_exactly_bright_at_edge() {
        assert_eq!(rim_alpha(0), 0.55, "the edge is the Fresnel peak");
        assert!(rim_alpha(0) > rim_alpha(1) && rim_alpha(1) > rim_alpha(2));
        assert_eq!(rim_alpha(RIM_BANDS), 0.0, "beyond the rim: nothing");
        // `(STAGE_WIDTH − width)/2` centers any morph width.
        let expected = (island::STAGE_WIDTH as f32 - 260.0) / 2.0;
        near(shell_offset(260.0), expected);
    }

    /// The frost is painted from a canvas. A bare `canvas()` has an auto
    /// height in a block parent and collapses to zero, which makes
    /// `paint_image` bail on its empty visible rect: the frost silently never
    /// appears. Its bounds must be the whole stage.
    #[gpui_kit::test]
    fn the_frost_canvas_fills_the_stage(cx: &mut gpui_kit::TestAppContext) {
        let measured = Rc::new(Cell::new((0.0f32, 0.0f32)));
        let image = backdrop();
        let slot = Rc::clone(&measured);
        let frost = super::frost(
            island::resting(IslandState::Card),
            IslandState::Card,
            Some(&image),
        )
        .on_children_prepainted(move |children, _, _| {
            if let Some(bounds) = children.last() {
                slot.set((f32::from(bounds.size.width), f32::from(bounds.size.height)));
            }
        });
        cx.add_empty_window().draw(
            point(px(0.0), px(0.0)),
            size(px(STAGE_WIDTH as f32), px(STAGE_HEIGHT as f32)),
            move |_window, _app| div().size_full().relative().child(frost).into_any_element(),
        );
        let (width, height) = measured.get();
        assert!(
            (width - STAGE_WIDTH as f32).abs() < 0.01,
            "frost canvas width {width} does not fill the {STAGE_WIDTH}px stage"
        );
        assert!(
            (height - STAGE_HEIGHT as f32).abs() < 0.01,
            "frost canvas height {height} does not fill the {STAGE_HEIGHT}px stage"
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

    fn near(got: f32, want: f32) {
        assert!((got - want).abs() < 1e-4, "{got} vs {want}");
    }
}
