//! The media half of an open card. Paint only; the clicks already close over
//! `IslandView`.
//!
//! [`media_page`] lays a 96px cover, then the seek column, then the optional
//! session mark and mode marks. The cover and the seek column both call
//! `stop_propagation` before their click: the body listener in `paint_motion`
//! would otherwise fold the card. `claim` on that same path sets
//! `gesture_handled`, so a press that hit a control is not also a body click.
//!
//! The seek column paints the title, the album line, and the progress bar.
//! Pointer-down calls `begin_seek` with [`rustedge::island::seek_fraction`].
//! The hook thread then updates a local fraction; pointer-up is the single
//! `media::seek`. A refused or missing timeline paints the bar read-only and
//! the transport buttons that the session will not accept are dimmed.

use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::{
    App, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Styled,
    StyledImage, Window, canvas, div, hsla, point, prelude::FluentBuilder, px, rgb,
};

use rustedge::island::{self};
use rustedge::media;

use super::{Click, millis_now};

/// Card controls. Each closure updates `IslandView` directly.
pub(super) struct CardClicks {
    pub(super) open_player: Click,
    pub(super) seek: Click,
    pub(super) sessions: Click,
    pub(super) session: SessionClick,
    pub(super) shuffle: Click,
    pub(super) repeat: Click,
}

type SessionClick = Rc<dyn Fn(&usize, &mut Window, &mut App)>;

#[allow(clippy::struct_excessive_bools)]
pub(super) struct MediaPaint<'a> {
    pub(super) title: &'a str,
    pub(super) cover: Option<Arc<gpui_kit::RenderImage>>,
    pub(super) progress: f32,
    pub(super) position_secs: u32,
    pub(super) duration_secs: u32,
    pub(super) track_hot: bool,
    pub(super) album_line: Option<String>,
    pub(super) modes: media::ModeCaps,
    pub(super) sessions: Vec<media::MediaSession>,
    pub(super) seekable: bool,
    pub(super) sessions_open: bool,
    pub(super) clicks: &'a CardClicks,
}

pub(super) fn media_page(paint: MediaPaint<'_>) -> gpui_kit::Div {
    let MediaPaint {
        title, cover, progress, position_secs, duration_secs, track_hot, album_line, modes,
        sessions, seekable, sessions_open, clicks,
    } = paint;
    let many = sessions.len() > 1;
    let title = title.to_owned();
    let art = div()
        .w(px(96.0))
        .h(px(96.0))
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, {
            let open = Rc::clone(&clicks.open_player);
            move |event, window, app| {
                app.stop_propagation();
                open(event, window, app);
            }
        })
        .child(match cover {
            Some(image) => gpui_kit::img(gpui_kit::ImageSource::Render(image))
                .size(gpui_kit::px(96.0))
                .rounded(gpui_kit::px(12.0))
                .object_fit(gpui_kit::ObjectFit::Cover)
                .into_any_element(),
            None => div()
                .w(px(96.0))
                .h(px(96.0))
                .rounded(px(12.0))
                .bg(rgb(0x003a_3a3c))
                .into_any_element(),
        });
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(16.0))
        .child(art)
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .child(seek_column(SeekPaint {
                    title: &title,
                    album_line,
                    progress,
                    position_secs,
                    duration_secs,
                    track_hot,
                    seekable,
                    clicks,
                })),
        )
        .when(many, |page| page.child(session_mark(sessions_open, clicks)))
        .when(sessions_open && many, |page| page.child(session_list(&sessions, clicks)))
        .when(modes.shuffle.is_some() || modes.repeat.is_some(), |page| page.child(mode_marks(modes, clicks)))
}

fn session_mark(open: bool, clicks: &CardClicks) -> gpui_kit::Div {
    div()
        .absolute()
        .left(px(island::SESSION_MARK_X - 20.0))
        .top(px(island::SESSION_MARK_Y - 20.0))
        .w(px(island::SESSION_MARK_W))
        .h(px(island::SESSION_MARK_H))
        .rounded(px(6.0))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .bg(hsla(0.0, 0.0, 1.0, if open { 0.28 } else { 0.12 }))
        .on_mouse_down(MouseButton::Left, {
            let sessions = Rc::clone(&clicks.sessions);
            move |event, window, app| {
                app.stop_propagation();
                sessions(event, window, app);
            }
        })
        .child(div().w(px(8.0)).h(px(8.0)).rounded(px(2.0)).bg(rgb(0x00ff_ffff)))
}

fn session_list(sessions: &[media::MediaSession], clicks: &CardClicks) -> gpui_kit::Div {
    let mut list = div()
        .absolute()
        .top(px(24.0))
        .right(px(8.0))
        .w(px(140.0))
        .flex()
        .flex_col()
        .bg(hsla(0.0, 0.0, 0.1, 0.9))
        .rounded(px(8.0))
        .p(px(6.0));
    for (index, session) in sessions.iter().enumerate() {
        let pick = Rc::clone(&clicks.session);
        list = list.child(
            div()
                .h(px(22.0))
                .text_size(px(12.0))
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, move |_event, window, app| {
                    app.stop_propagation();
                    pick(&index, window, app);
                })
                .text_color(hsla(0.0, 0.0, 1.0, if session.current { 1.0 } else { 0.55 }))
                .child(session.label.clone()),
        );
    }
    list
}

fn mode_marks(modes: media::ModeCaps, clicks: &CardClicks) -> gpui_kit::Div {
    div()
        .absolute()
        .bottom(px(8.0))
        .right(px(72.0))
        .flex()
        .gap(px(8.0))
        .when(modes.shuffle.is_some(), |row| {
            let on = modes.shuffle == Some(true);
            row.child(mode_dot(on).cursor_pointer().on_mouse_down(MouseButton::Left, {
                let shuffle = Rc::clone(&clicks.shuffle);
                move |event, window, app| {
                    app.stop_propagation();
                    shuffle(event, window, app);
                }
            }))
        })
        .when(modes.repeat.is_some(), |row| {
            let on = matches!(modes.repeat, Some(media::RepeatMode::List | media::RepeatMode::Track));
            row.child(mode_dot(on).cursor_pointer().on_mouse_down(MouseButton::Left, {
                let repeat = Rc::clone(&clicks.repeat);
                move |event, window, app| {
                    app.stop_propagation();
                    repeat(event, window, app);
                }
            }))
        })
}

fn mode_dot(on: bool) -> gpui_kit::Div {
    div()
        .w(px(16.0))
        .h(px(16.0))
        .rounded(px(8.0))
        .bg(hsla(0.0, 0.0, 1.0, if on { 0.9 } else { 0.35 }))
}

struct SeekPaint<'a> {
    title: &'a str,
    album_line: Option<String>,
    progress: f32,
    position_secs: u32,
    duration_secs: u32,
    track_hot: bool,
    seekable: bool,
    clicks: &'a CardClicks,
}

fn seek_column(paint: SeekPaint<'_>) -> gpui_kit::Div {
    let SeekPaint { title, album_line, progress, position_secs, duration_secs, track_hot, seekable, clicks } = paint;
    let title = title.to_owned();
    let column_width = island::seek_column_px();
    div()
        .w(px(column_width))
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(
            div()
                .w(px(column_width))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(17.0))
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, {
                    let open = Rc::clone(&clicks.open_player);
                    move |event, window, app| {
                        app.stop_propagation();
                        open(event, window, app);
                    }
                })
                .child(title),
        )
        .child(div().h(px(16.0)))
        .when(album_line.is_some(), |column| {
            column.child(
                div()
                    .text_size(px(11.0))
                    .text_color(hsla(0.0, 0.0, 1.0, 0.55))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(album_line.unwrap_or_default()),
            )
        })
        .child(
            div()
                .mt(px(8.0))
                .h(px(18.0))
                .flex()
                .items_center()
                .when(seekable, gpui_kit::Styled::cursor_pointer)
                .on_mouse_down(MouseButton::Left, {
                    let seek = Rc::clone(&clicks.seek);
                    move |event, window, app| {
                        app.stop_propagation();
                        seek(event, window, app);
                    }
                })
                .child(progress_track(progress, track_hot && seekable)),
        )
        .child(
            div()
                .mt(px(4.0))
                .w(px(column_width))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_right()
                .text_size(px(11.0))
                .child(format!("{} / {}", media::clock(position_secs), media::clock(duration_secs))),
        )
}

fn progress_track(progress: f32, hot: bool) -> gpui_kit::Div {
    let width = island::seek_column_px();
    let fill = island::seek_fill_px(progress);
    div()
        .relative()
        .w(px(width))
        .h(px(if hot { 6.0 } else { 4.0 }))
        .rounded(px(2.0))
        .bg(hsla(0.0, 0.0, 1.0, 0.15))
        .child(
            div()
                .w(px(fill))
                .h_full()
                .rounded(px(2.0))
                .bg(rgb(0x00ff_ffff)),
        )
        .when(hot, |track| {
            track.child(
                div()
                    .absolute()
                    .top(px(-1.0))
                    .left(px((fill - island::SEEK_THUMB / 2.0).max(0.0)))
                    .w(px(island::SEEK_THUMB))
                    .h(px(island::SEEK_THUMB))
                    .rounded(px(island::SEEK_THUMB / 2.0))
                    .bg(rgb(0x00ff_ffff)),
            )
        })
}

#[allow(dead_code)]
fn page_note(heading: &'static str, body: &'static str) -> gpui_kit::Div {
    div()
        .flex()
        .flex_col()
        .child(div().text_size(px(16.0)).child(heading))
        .child(div().mt(px(8.0)).text_size(px(13.0)).child(body))
}



pub(super) fn controls<P, L, N>(
    playing: bool,
    transport: media::TransportCaps,
    previous: P,
    play: L,
    next: N,
) -> gpui_kit::Div
where
    P: Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    L: Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    N: Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
{
    let shown = if transport.none_enabled() { media::TransportCaps::all_enabled() } else { transport };
    div()
        .h(px(64.0))
        .w_full()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(24.0))
        .child(transport_button(44.0, false, shown.previous, skip_mark(false), previous))
        .child(transport_button(56.0, true, shown.play_pause, play_mark(playing), play))
        .child(transport_button(44.0, false, shown.next, skip_mark(true), next))
}

fn transport_button<F>(size: f32, primary: bool, enabled: bool, mark: gpui_kit::Div, listener: F) -> gpui_kit::Div
where
    F: Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
{
    div()
        .w(px(size))
        .h(px(size))
        .rounded(px(size / 2.0))
        .flex()
        .items_center()
        .justify_center()
        .opacity(if enabled { 1.0 } else { island::REFUSED_OPACITY })
        .when(enabled, gpui_kit::Styled::cursor_pointer)
        .when(primary, |button| button.bg(hsla(0.0, 0.0, 1.0, 0.12)))
        .when(enabled, move |button| {
            button.hover(move |button| button.bg(hsla(0.0, 0.0, 1.0, if primary { 0.18 } else { 0.10 })))
        })
        .on_mouse_down(MouseButton::Left, listener)
        .child(mark)
}

/// Short bar on the outer side, triangle pointing the same way.
/// Previous: bar then left-pointing triangle. Next: right-pointing triangle then bar.
fn skip_mark(forward: bool) -> gpui_kit::Div {
    let mark = div().flex().items_center().gap(px(2.0));
    if forward { mark.child(triangle(true)).child(stem()) } else { mark.child(stem()).child(triangle(false)) }
}

/// Filled triangle, 10x12. `forward` puts the point on the right.
fn triangle(forward: bool) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let left = bounds.origin.x;
            let right = left + bounds.size.width;
            let top = bounds.origin.y;
            let bottom = top + bounds.size.height;
            let mid_y = top + bounds.size.height / 2.0;
            let (tip, base_top, base_bottom) = if forward {
                (point(right, mid_y), point(left, top), point(left, bottom))
            } else {
                (point(left, mid_y), point(right, top), point(right, bottom))
            };
            let mut path = gpui_kit::Path::new(tip);
            path.line_to(base_top);
            path.line_to(base_bottom);
            window.paint_path(path, rgb(0x00ff_ffff));
        },
    )
    .w(px(10.0))
    .h(px(12.0))
}

fn play_mark(playing: bool) -> gpui_kit::Div {
    if playing {
        div().flex().items_center().gap(px(3.0)).child(stem()).child(stem())
    } else {
        div().child(wedge())
    }
}

fn wedge() -> gpui_kit::Div {
    div().w(px(10.0)).h(px(12.0)).rounded(px(2.0)).bg(rgb(0x00ff_ffff))
}

fn stem() -> gpui_kit::Div {
    div().w(px(3.0)).h(px(12.0)).rounded(px(1.0)).bg(rgb(0x00ff_ffff))
}

#[allow(dead_code)]
fn control<F>(label: &'static str, listener: F) -> gpui_kit::Div
where
    F: Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
{
    div()
        .px(px(12.0))
        .py(px(8.0))
        .rounded(px(16.0))
        .bg(hsla(0.0, 0.0, 1.0, 0.12))
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, listener)
        .child(label)
}

pub(super) fn bars(playing: bool) -> gpui_kit::Div {
    div()
        .w(px(64.0))
        .h(px(32.0))
        .flex()
        .items_end()
        .justify_center()
        .gap(px(3.0))
        .child(bar(0, 280, playing))
        .child(bar(1, 360, playing))
        .child(bar(2, 310, playing))
        .child(bar(3, 420, playing))
}

fn bar(index: u64, period_ms: u64, playing: bool) -> gpui_kit::AnyElement {
    let color = rgb(0x00f5_f5f7);
    let height = if playing { bar_height(index, period_ms, millis_now()) } else { 8.0 };
    div()
        .w(px(3.0))
        .h(px(height))
        .rounded(px(2.0))
        .bg(color)
        .into_any_element()
}

/// Height of one bar at `now_ms`. Each bar has its own phase so they do not move together.
fn bar_height(index: u64, period_ms: u64, now_ms: u64) -> f32 {
    let period = period_ms.max(1);
    let phase = now_ms.wrapping_add(index.saturating_mul(period / 4));
    let pos = phase % period;
    let half = period / 2;
    let rise = if pos < half { pos } else { period - pos };
    #[allow(clippy::cast_precision_loss)]
    let delta = rise as f32 / half.max(1) as f32;
    8.0 + delta * 18.0
}
