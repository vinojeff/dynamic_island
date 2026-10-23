//! Paint the shell. No hook, no tray, no endpoint, no state change.
//!
//! [`paint`] is the only entry. `IslandView::paint_motion` has already built
//! the listeners (`toggle`, transport, mute, page chevrons, seek) and a
//! [`Face`] snapshot. From here:
//!
//! 1. The capsule title is the toast marquee, else [`media_title`] for the
//!    current tab (track line, or the performance capsule line).
//! 2. Line and capsule paint the compact row. Card paint goes to [`card_body`].
//! 3. [`card_body`] shows [`notice_page`] whenever a toast is the head. That
//!    page has no chevrons and no slide: [`IslandView::turn_page`] already
//!    refuses while a head exists.
//! 4. Otherwise [`expanded_body`] lays the media page and the performance page
//!    side by side and translates by `page_slide` (0 = media, 1 = performance)
//!    across [`rustedge::island::CARD_WIDTH`]. The two chevrons stay faint
//!    until the pointer is over them.
//!
//! The volume overlay is a sibling of the pill, shown only while
//! `volume_visible` (`volume::OVERLAY_HOLD`, 1500ms after the last accepted
//! wheel step).

use std::rc::Rc;
use std::time::Duration;

use gpui_kit::{
    Animation, AnimationExt, App, Bounds, BoxShadow, ContentMask, Font, FontStyle, FontWeight,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Pixels, Point,
    SharedString, Styled, StyledImage, TextRun, Window, div, hsla, point, prelude::FluentBuilder,
    px, rgb,
};

use rustedge::island::{self, IslandState, Tab};
use rustedge::media;
use rustedge::notice::{self, Notice};
use rustedge::perf::{self, PerfHistory};
use rustedge::volume::{self, VolumeState};

use crate::perf_view;

use super::media_page::{CardClicks, MediaPaint, bars, controls, media_page};
use super::{Click, Face, millis_now};

fn media_title(tab: Tab, media_line: &str, history: &PerfHistory) -> String {
    match tab {
        Tab::Media => media_line.to_owned(),
        Tab::Perf => perf::capsule_line(history),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn paint<T, P, L, N, M>(
    face: &Face,
    clicks: &CardClicks,
    window: &Window,
    toggle: T,
    previous: P,
    play: L,
    next: N,
    mute: M,
    page_prev: Click,
    page_next: Click,
    arrow_hot: Option<bool>,
) -> gpui_kit::Div
where
    T: Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    P: Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    L: Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    N: Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    M: Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
{
    let Face {
        shape, playing, tab, volume, volume_visible, state, notice, badge, media_line,
        media_playing, cover, progress, position_secs, duration_secs, track_hot, album_line,
        transport, modes, sessions, seekable, sessions_open, perf, page_slide,
    } = face.clone();
    let title = match notice.as_ref() {
        Some(item) => notice::marquee_text(item),
        None => media_title(tab, &media_line, &perf),
    };
    let page_prev = Rc::new(page_prev);
    let page_next = Rc::new(page_next);
    let playing = if tab == Tab::Media { media_playing } else { playing };
    let ink = rgb(0x00f5_f5f7);
    let body = div()
        .flex_1()
        .w_full()
        .relative()
        .when(state == IslandState::Capsule || state == IslandState::Card, |body| {
            body.cursor_pointer().on_mouse_down(MouseButton::Left, toggle)
        })
        .when(state == IslandState::Line && shape.height < 12.0, |body| body.child(edge_line()))
        .when(state != IslandState::Card && shape.height >= 12.0, |body| {
            if volume_visible && volume.available {
                body.child(volume_overlay(volume, mute))
            } else {
                body.child(compact_row(playing, &title, tab, cover.clone(), 1.0, CapsuleEnds {
                    hot: arrow_hot,
                    previous: Rc::clone(&page_prev),
                    next: Rc::clone(&page_next),
                }, window))
            }
        })
        .when(state == IslandState::Card, |body| {
            body.child(card_body(CardPaint {
                title: title.as_str(),
                tab,
                notice: notice.as_ref(),
                badge,
                cover: cover.clone(),
                progress,
                position_secs,
                duration_secs,
                track_hot,
                album_line: album_line.clone(),
                modes,
                sessions: sessions.clone(),
                seekable,
                sessions_open,
                opacity: shape.detail.clamp(0.0, 1.0),
                clicks,
                perf,
                page_slide,
                arrow_hot,
                page_prev: Rc::clone(&page_prev),
                page_next: Rc::clone(&page_next),
            }))
        });

    let shell = div()
        .w(px(shape.width))
        .h(px(shape.height))
        .flex()
        .flex_col()
        .overflow_hidden()
        .rounded(px(shape.radius))
        .bg(shell_fill(state))
        .text_color(ink)
        .when(state != IslandState::Line, |shell| {
            shell
                .border_1()
                .border_color(hsla(0.0, 0.0, 1.0, 0.12))
                .shadow(vec![BoxShadow {
                    color: hsla(0.0, 0.0, 0.0, 0.40),
                    offset: point(px(0.0), px(16.0)),
                    blur_radius: px(48.0),
                    spread_radius: px(0.0),
                    inset: false,
                }])
        })
        .child(body);

    if state == IslandState::Card && tab == Tab::Media {
        shell.child(controls(playing, transport, previous, play, next))
    } else {
        shell
    }
}

fn shell_fill(state: IslandState) -> gpui_kit::Hsla {
    match state {
        IslandState::Line => hsla(0.0, 0.0, 1.0, 0.35),
        IslandState::Capsule | IslandState::Card => hsla(0.0, 0.0, 0.078, 0.55),
    }
}

/// The 4px edge. A 1px hairline sits on its lower rim so it stays visible on a dark wallpaper.
fn thumb(image: std::sync::Arc<gpui_kit::RenderImage>) -> gpui_kit::Img {
    gpui_kit::img(gpui_kit::ImageSource::Render(image))
        .size(gpui_kit::px(40.0))
        .rounded(gpui_kit::px(8.0))
        .object_fit(gpui_kit::ObjectFit::Cover)
}

fn edge_line() -> gpui_kit::Div {
    div().size_full()
}

struct CapsuleEnds {
    hot: Option<bool>,
    previous: Click,
    next: Click,
}

fn compact_row(
    playing: bool,
    title: &str,
    tab: Tab,
    cover: Option<std::sync::Arc<gpui_kit::RenderImage>>,
    opacity: f32,
    ends: CapsuleEnds,
    window: &Window,
) -> gpui_kit::Div {
    let status = match (tab, cover) {
        (Tab::Media, Some(image)) => thumb(image).into_any_element(),
        (Tab::Media, None) => bars(playing).into_any_element(),
        (Tab::Perf, _) => perf_view::clock_face(millis_now()).into_any_element(),
    };
    div()
        .absolute()
        .size_full()
        .flex()
        .items_center()
        .pl(px(28.0))
        .pr(px(28.0))
        .opacity(opacity)
        .child(capsule_arrow(false, ends.hot == Some(false), ends.previous))
        .child(capsule_arrow(true, ends.hot == Some(true), ends.next))
        .child(status)
        .child(marquee(title, window))
}

/// Text size of the capsule line.
const TEXT_PX: f32 = 13.0;

/// Pixels of travel per second. A longer line takes longer; the speed does
/// not change with the mix of Latin and CJK.
const SCROLL_PX_PER_SEC: f32 = 39.0;

/// Pixels of the capsule line that stay on screen. The shell is the 220px
/// pill minus the two 28px gutters, the clock (five digits at this text size),
/// and the 12px gap before the line.
const VISIBLE_PX: f32 = 220.0 - 28.0 - 28.0 - TEXT_PX * 5.0 - 12.0;

/// One line of text. A line that fits is solid. A longer line is shaped once
/// and slides left; each glyph fades from the position that shaping gave it.
///
/// The animation lives on this shell, not on the glyphs. A performance sample
/// rewrites the line every second. An id on the inner element is then a new
/// element, and GPUI drops the fade with the old one. The period is the
/// shaped width at [`SCROLL_PX_PER_SEC`], not a fixed em estimate.
fn marquee(title: &str, window: &Window) -> gpui_kit::AnyElement {
    let shaped = shape_line(title, window);
    let shell = div().ml(px(12.0)).flex_1().h(px(20.0)).overflow_hidden().relative();
    if shaped.width <= VISIBLE_PX {
        return shell.child(solid_line(title)).into_any_element();
    }
    let seconds = (shaped.width / SCROLL_PX_PER_SEC).max(0.1);
    let line = shaped;
    shell
        .with_animation(
            "marquee",
            Animation::new(Duration::from_secs_f32(seconds)).repeat(),
            move |shell, delta| shell.child(faded_line(&line, delta * line.width)),
        )
        .into_any_element()
}

fn solid_line(title: &str) -> gpui_kit::Div {
    div().text_size(px(TEXT_PX)).text_color(rgb(0x00f5_f5f7)).child(title.to_owned())
}

/// One shaping of the whole line. Glyph positions come from the font, so
/// kerning stays in the advances and `%` / `GB` are not boxed at `0.55em`.
struct ShapedLine {
    glyphs: Vec<PlacedGlyph>,
    width: f32,
}

struct PlacedGlyph {
    font: gpui_kit::FontId,
    glyph: gpui_kit::GlyphId,
    x: f32,
    advance: f32,
}

fn shape_line(title: &str, window: &Window) -> ShapedLine {
    let font = Font {
        family: SharedString::from(".SystemUIFont"),
        features: gpui_kit::FontFeatures::default(),
        fallbacks: Some(gpui_kit::FontFallbacks::from_fonts(vec!["Inter".to_owned()])),
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    };
    let layout = window.text_system().layout_line(
        title,
        px(TEXT_PX),
        &[TextRun {
            len: title.len(),
            font,
            color: hsla(0.0, 0.0, 0.96, 1.0),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
    let width = f32::from(layout.width);
    let mut glyphs = Vec::new();
    for run in &layout.runs {
        for glyph in &run.glyphs {
            glyphs.push(PlacedGlyph {
                font: run.font_id,
                glyph: glyph.id,
                x: f32::from(glyph.position.x),
                advance: 0.0,
            });
        }
    }
    for index in 0..glyphs.len() {
        let next = glyphs.get(index + 1).map_or(width, |glyph| glyph.x);
        let current = glyphs.get(index).map_or(0.0, |glyph| glyph.x);
        if let Some(glyph) = glyphs.get_mut(index) {
            glyph.advance = (next - current).max(0.0);
        }
    }
    ShapedLine { glyphs, width }
}

/// The same shaping, scrolled. Alpha uses `position.x` minus the pixels
/// already traveled, then the center of that glyph's own advance.
fn faded_line(line: &ShapedLine, offset_px: f32) -> gpui_kit::Div {
    let positions: Vec<f32> = line.glyphs.iter().map(|glyph| glyph.x).collect();
    let advances: Vec<f32> = line.glyphs.iter().map(|glyph| glyph.advance).collect();
    let alphas = island::marquee_alphas(&positions, &advances, offset_px, VISIBLE_PX);
    let placed: Vec<PaintedGlyph> = line
        .glyphs
        .iter()
        .zip(alphas)
        .map(|(glyph, alpha)| PaintedGlyph {
            font: glyph.font,
            glyph: glyph.glyph,
            x: glyph.x - offset_px,
            alpha,
        })
        .collect();
    div().size_full().child(gpui_kit::canvas(
        move |_bounds, _window, _app| placed,
        move |bounds, placed, window, _app| paint_glyphs(bounds, &placed, window),
    ))
}

struct PaintedGlyph {
    font: gpui_kit::FontId,
    glyph: gpui_kit::GlyphId,
    x: f32,
    alpha: f32,
}

fn paint_glyphs(bounds: Bounds<Pixels>, glyphs: &[PaintedGlyph], window: &mut Window) {
    // Same vertical slot as the solid text element: a 20px line box holding a
    // 13px face. The shaper's ascent is the font metric, not that slot, and
    // using it pushed the glyphs below the clip.
    let baseline = f32::from(bounds.origin.y) + (20.0 + TEXT_PX) / 2.0;
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        for glyph in glyphs {
            let origin = Point { x: px(f32::from(bounds.origin.x) + glyph.x), y: px(baseline) };
            let color = hsla(0.0, 0.0, 0.96, glyph.alpha);
            let _painted = window.paint_glyph(origin, glyph.font, glyph.glyph, px(TEXT_PX), color);
        }
    });
}

#[allow(clippy::struct_excessive_bools)]
struct CardPaint<'a> {
    title: &'a str,
    #[allow(dead_code)]
    tab: Tab,
    notice: Option<&'a Notice>,
    badge: bool,
    cover: Option<std::sync::Arc<gpui_kit::RenderImage>>,
    progress: f32,
    position_secs: u32,
    duration_secs: u32,
    track_hot: bool,
    album_line: Option<String>,
    modes: media::ModeCaps,
    sessions: Vec<media::MediaSession>,
    seekable: bool,
    sessions_open: bool,
    opacity: f32,
    clicks: &'a CardClicks,
    perf: PerfHistory,
    page_slide: f32,
    arrow_hot: Option<bool>,
    page_prev: Click,
    page_next: Click,
}

fn card_body(paint: CardPaint<'_>) -> gpui_kit::Div {
    if let Some(notice) = paint.notice {
        return notice_page(notice, paint.badge, paint.opacity);
    }
    expanded_body(paint)
}

fn notice_page(notice: &Notice, badge: bool, opacity: f32) -> gpui_kit::Div {
    div()
        .absolute()
        .size_full()
        .flex()
        .flex_col()
        .p(px(20.0))
        .opacity(opacity)
        .child(div().text_size(px(13.0)).child(notice.app.clone()))
        .child(div().mt(px(12.0)).text_size(px(16.0)).child(notice.title.clone()))
        .child(div().mt(px(8.0)).text_size(px(14.0)).child(notice.body.clone()))
        .when(badge, |page| page.child(div().mt(px(8.0)).child("还有一条")))
}

fn expanded_body(paint: CardPaint<'_>) -> gpui_kit::Div {
    let CardPaint {
        title, tab: _, cover, progress, position_secs, duration_secs, track_hot, album_line, modes,
        sessions, seekable, sessions_open, opacity, clicks, perf, page_slide, arrow_hot, page_prev, page_next, ..
    } = paint;
    let media = media_page(MediaPaint {
        title,
        cover,
        progress,
        position_secs,
        duration_secs,
        track_hot,
        album_line,
        modes,
        sessions,
        seekable,
        sessions_open,
        clicks,
    });
    let slide = page_slide.clamp(0.0, 1.0);
    let travel = island::CARD_WIDTH;
    div()
        .absolute()
        .size_full()
        .overflow_hidden()
        .opacity(opacity)
        .child(perf_view::page_arrow(false, arrow_hot == Some(false), page_prev))
        .child(perf_view::page_arrow(true, arrow_hot == Some(true), page_next))
        .child(
            div()
                .absolute()
                .top(px(28.0))
                .left(px(-slide * travel))
                .w(px(travel))
                .h_full()
                .p(px(20.0))
                .opacity(1.0 - slide)
                .child(media),
        )
        .child(
            div()
                .absolute()
                .top(px(0.0))
                .left(px((1.0 - slide) * travel))
                .w(px(travel))
                .h_full()
                .opacity(slide)
                .child(perf_view::perf_page(&perf, millis_now())),
        )
}

/// Which title-row arrow contains a stage-local point. `Some(true)` is the right arrow.
///
/// `shell_width` is the shape on screen, so the capsule and the card share the test.
pub(super) fn page_arrow_at(x: f32, y: f32, state: IslandState, shell_width: f32) -> Option<bool> {
    if state == IslandState::Line || !(0.0..=28.0).contains(&y) {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let left = (island::STAGE_WIDTH as f32 - shell_width) / 2.0;
    let on_left = (left..=left + 28.0).contains(&x);
    let right = left + shell_width - 28.0;
    let on_right = (right..=right + 28.0).contains(&x);
    if on_left { Some(false) } else if on_right { Some(true) } else { None }
}

/// Capsule chevron, vertically centered. Faint until the pointer is over it.
fn capsule_arrow(forward: bool, hot: bool, listener: Click) -> gpui_kit::Div {
    div()
        .absolute()
        .top(px(16.0))
        .w(px(24.0))
        .h(px(24.0))
        .when(forward, |arrow| arrow.right(px(4.0)))
        .when(!forward, |arrow| arrow.left(px(4.0)))
        .flex()
        .items_center()
        .justify_center()
        .opacity(if hot { 0.9 } else { 0.16 })
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, move |event, window, app| listener(event, window, app))
        .child(div().text_size(px(14.0)).child(if forward { ">" } else { "<" }))
}

/// Capsule contents while the wheel overlay is up: icon, percent, thin bar.
fn volume_overlay<M>(volume: VolumeState, mute: M) -> gpui_kit::Div
where
    M: Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
{
    let fill = if volume.muted {
        hsla(0.0, 0.0, 1.0, 0.4)
    } else {
        rgb(0x00ff_ffff).into()
    };
    div()
        .absolute()
        .size_full()
        .flex()
        .items_center()
        .px(px(14.0))
        .gap(px(10.0))
        .child(
            div()
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, mute)
                .child(speaker(volume.icon())),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(px(13.0))
                        .child(format!("{}%", volume.percent())),
                )
                .child(thin_meter(volume.level, fill)),
        )
}

fn thin_meter(level: f32, fill: gpui_kit::Hsla) -> gpui_kit::Div {
    let level = level.clamp(0.0, 1.0);
    div()
        .w_full()
        .h(px(3.0))
        .rounded(px(2.0))
        .bg(hsla(0.0, 0.0, 1.0, 0.15))
        .child(
            div()
                .w(gpui_kit::relative(level))
                .h_full()
                .rounded(px(2.0))
                .bg(fill),
        )
}

fn speaker(icon: volume::VolumeIcon) -> gpui_kit::Div {
    let waves = match icon {
        volume::VolumeIcon::Muted => 0,
        volume::VolumeIcon::Low => 1,
        volume::VolumeIcon::High => 2,
    };
    div()
        .w(px(18.0))
        .h(px(16.0))
        .flex()
        .items_center()
        .gap(px(2.0))
        .child(div().w(px(4.0)).h(px(8.0)).rounded(px(1.0)).bg(rgb(0x00f5_f5f7)))
        .child(div().w(px(3.0)).h(px(12.0)).rounded(px(1.0)).bg(rgb(0x00f5_f5f7)))
        .when(waves >= 1, |row| row.child(div().w(px(2.0)).h(px(8.0)).rounded(px(1.0)).bg(hsla(0.0, 0.0, 1.0, 0.7))))
        .when(waves >= 2, |row| row.child(div().w(px(2.0)).h(px(12.0)).rounded(px(1.0)).bg(hsla(0.0, 0.0, 1.0, 0.7))))
        .when(icon == volume::VolumeIcon::Muted, |row| {
            row.child(div().w(px(2.0)).h(px(12.0)).bg(rgb(0x00ff_5a5a)))
        })
}
