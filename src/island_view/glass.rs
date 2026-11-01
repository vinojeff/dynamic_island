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

/// The scrim a luma calls for. Bright wallpaper deepens it so the ink stays
/// readable; dark wallpaper thins it so the frost shows.
#[must_use]
pub(super) fn scrim_alpha(luma: f32) -> f32 {
    (0.14 + 0.30 * luma).clamp(0.10, 0.44)
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
        0 => 0.28,
        1 => 0.10,
        2 => 0.04,
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
            color: hsla(0.0, 0.0, 0.0, 0.40),
            offset: point(px(0.0), px(16.0)),
            blur_radius: px(48.0),
            spread_radius: px(0.0),
            inset: false,
        },
        BoxShadow {
            // The hem. Real glass is weightier along the bottom rim.
            color: hsla(0.0, 0.0, 0.0, 0.25),
            offset: point(px(0.0), px(2.0)),
            blur_radius: px(6.0),
            spread_radius: px(0.0),
            inset: true,
        },
        BoxShadow {
            // The glow. Same hue family as the tucked line's bloom.
            color: hsla(GLASS_HUE, 0.60, 0.70, 0.12),
            offset: point(px(0.0), px(0.0)),
            blur_radius: px(24.0),
            spread_radius: px(0.0),
            inset: false,
        },
    ]
}

/// The frosted wallpaper under the shell, painted once at the root.
///
/// The canvas spans the whole window so its bounds match the stage crop
/// one-to-one; [`Window::paint_image`] then clips to the shell's rounded
/// rect, which is what carries the silhouette (gpui's overflow mask is a
/// plain rectangle). Empty when tucked as the line or before the first build
/// lands — the fallback wash in [`layers`] takes over.
pub(super) fn frost(
    shape: island::Morph,
    state: IslandState,
    backdrop: Option<&Arc<RenderImage>>,
) -> gpui_kit::Div {
    let base = div().absolute().inset_0();
    if state == IslandState::Line {
        return base;
    }
    let Some(image) = backdrop else {
        return base;
    };
    let image = Arc::clone(image);
    let width = shape.width;
    let height = shape.height;
    let radius = px(shape.radius);
    base.child(canvas(
        |_, _, _| (),
        move |bounds: Bounds<Pixels>, (): (), window: &mut Window, _: &mut App| {
            let shell = Bounds::new(
                point(bounds.origin.x + px(shell_offset(width)), bounds.origin.y),
                size(px(width), px(height)),
            );
            // Slide the wallpaper crop into window space, clipped to the shell.
            let _painted = window.paint_image(
                shell,
                bounds,
                Corners::all(radius),
                Arc::clone(&image),
                0,
                false,
            );
        },
    ))
}

/// The shell's underlay layers: scrim, specular sheen, rim, dispersion.
///
/// Painted ahead of the body so text and controls sit on top of the glass.
/// With no backdrop yet, the scrim is the fallback wash — a thin two-stop
/// gradient standing in until the frosted crop arrives.
pub(super) fn layers(radius: f32, glass: Option<(&Arc<RenderImage>, f32)>) -> Vec<gpui_kit::Div> {
    let luma = glass.map_or(IDLE_LUMA, |(_, luma)| luma);
    let seeded = glass.is_some();
    let scrim = div().absolute().inset_0();
    let scrim = if seeded {
        scrim.bg(linear_gradient(
            180.0,
            linear_color_stop(hsla(0.0, 0.0, 0.05, scrim_alpha(luma)), 0.0),
            linear_color_stop(hsla(0.0, 0.0, 0.04, scrim_alpha(luma)), 1.0),
        ))
    } else {
        scrim.bg(linear_gradient(
            180.0,
            linear_color_stop(hsla(0.0, 0.0, 0.05, 0.62), 0.0),
            linear_color_stop(hsla(0.0, 0.0, 0.04, 0.48), 1.0),
        ))
    };
    // The sheen. GPUI gradients take exactly two stops, so the three-stop
    // streak is two stacked divs meeting at the transparent midpoint.
    let sheen_high = div().absolute().inset_0().bg(linear_gradient(
        SPECULAR_ANGLE,
        linear_color_stop(hsla(0.0, 1.0, 1.0, 0.10), 0.0),
        linear_color_stop(hsla(0.0, 1.0, 1.0, 0.0), 0.5),
    ));
    let sheen_low = div().absolute().inset_0().bg(linear_gradient(
        SPECULAR_ANGLE,
        linear_color_stop(hsla(0.0, 1.0, 1.0, 0.0), 0.5),
        linear_color_stop(hsla(0.0, 1.0, 1.0, 0.03), 1.0),
    ));
    let rim = div().absolute().inset_0().child(canvas(
        |_, _, _| (),
        move |bounds: Bounds<Pixels>, (): (), window: &mut Window, _: &mut App| {
            paint_rim(bounds, radius, window);
        },
    ));
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
        hsla(0.98, 0.80, 0.60, 0.06),
        window,
    );
    paint_ring(
        bounds,
        DISPERSION,
        DISPERSION,
        radius,
        hsla(0.56, 0.80, 0.60, 0.06),
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

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use rustedge::island::{CARD_WIDTH, PILL_WIDTH};

    #[test]
    fn scrim_deepens_on_a_bright_wallpaper_and_thins_on_a_dark_one() {
        assert_eq!(scrim_alpha(0.0), 0.14, "dark wallpaper → thin scrim");
        assert_eq!(scrim_alpha(1.0), 0.44, "bright wallpaper → deep scrim");
        let mut luma = 0.0;
        while luma <= 1.0 {
            assert!(
                (0.10..=0.44).contains(&scrim_alpha(luma)),
                "luma {luma} escapes the clamp"
            );
            luma += 0.05;
        }
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
        assert_eq!(rim_alpha(0), 0.28, "the edge is the Fresnel peak");
        assert!(rim_alpha(0) > rim_alpha(1) && rim_alpha(1) > rim_alpha(2));
        assert_eq!(rim_alpha(RIM_BANDS), 0.0, "beyond the rim: nothing");
        // `(STAGE_WIDTH − width)/2` centers any morph width.
        let expected = (island::STAGE_WIDTH as f32 - 260.0) / 2.0;
        near(shell_offset(260.0), expected);
    }

    fn near(got: f32, want: f32) {
        assert!((got - want).abs() < 1e-4, "{got} vs {want}");
    }
}
