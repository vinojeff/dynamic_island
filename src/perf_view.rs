//! The performance half of an open card.
//!
//! [`perf_page`] is called from `chrome::expanded_body` with the history the
//! view already owns. It does not sample. Three meters stack under a title
//! row: CPU, memory, then network. Each meter is a label, the current value,
//! and a 36px area chart of up to 60 points. Network draws download as the
//! fill and upload as a second stroke; both share [`perf::network_ceiling`]
//! so a spike does not pin the chart at the top. The wall clock on the title
//! row is [`rustedge::clock`] in the local timezone, set in the system UI face
//! with tabular figures so a changing digit does not shift the line.
//!
//! [`page_arrow`] is the chevron `chrome` places over the card. It is faint
//! until the pointer is over it. The click is the `page_prev` / `page_next`
//! listener from `IslandView`, which calls `turn_page`.

use std::collections::VecDeque;

use gpui_kit::{
    App, Font, FontFallbacks, FontFeatures, FontStyle, FontWeight, InteractiveElement, MouseButton,
    MouseDownEvent, ParentElement, SharedString, Styled, Window, canvas, div, hsla, point,
    prelude::FluentBuilder, px, rgb,
};

use rustedge::perf::{self, PerfHistory};

/// Performance page: clock on the title row, then CPU, memory, and network.
pub(crate) fn perf_page(history: &PerfHistory, now_ms: u64) -> gpui_kit::Div {
    let net: Vec<f32> = history
        .net_up
        .iter()
        .zip(history.net_down.iter())
        .map(|(up, down)| up.max(*down))
        .collect();
    let ceiling = perf::network_ceiling(&net);
    div()
        .size_full()
        .flex()
        .flex_col()
        .px(px(12.0))
        .pt(px(4.0))
        .child(title_row(now_ms))
        .child(meter("CPU", perf::percent_label(history.cpu_now()), &copied(&history.cpu), 100.0, CPU, None))
        .child(meter("内存", perf::bytes_label(history.mem_bytes), &copied(&history.mem), 100.0, MEM, None))
        .child(meter(
            "网络",
            format!(
                "↑ {}  ↓ {}",
                perf::rate_label(history.net_up.back().copied().unwrap_or(0.0)),
                perf::rate_label(history.net_down.back().copied().unwrap_or(0.0)),
            ),
            &copied(&history.net_down),
            ceiling,
            NET_DOWN,
            Some((copied(&history.net_up), NET_UP)),
        ))
}

/// CPU stroke. Fill is the same color at 20% alpha.
const CPU: u32 = 0x0089_b4fa;
/// Memory stroke.
const MEM: u32 = 0x00a6_e3a1;
/// Network upload stroke.
const NET_UP: u32 = 0x00f9_e2af;
/// Network download stroke.
const NET_DOWN: u32 = 0x0089_dceb;

fn title_row(now_ms: u64) -> gpui_kit::Div {
    div()
        .h(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .child(div().text_size(px(13.0)).child("性能"))
        .child(
            div()
                .absolute()
                .right(px(28.0))
                .child(clock_face(now_ms)),
        )
}

/// Local `HH:MM`. Segoe UI Variable on Windows, Inter if that face is missing.
/// `tnum` keeps every digit the same width.
pub(crate) fn clock_face(now_ms: u64) -> gpui_kit::Div {
    div()
        .font(clock_font())
        .text_size(px(22.0))
        .line_height(px(22.0))
        .text_color(hsla(0.0, 0.0, 1.0, 0.72))
        .child(rustedge::clock::clock_label(now_ms))
}

fn clock_font() -> Font {
    Font {
        family: SharedString::from(".SystemUIFont"),
        features: FontFeatures(std::sync::Arc::new(vec![("tnum".to_owned(), 1)])),
        fallbacks: Some(FontFallbacks::from_fonts(vec!["Inter".to_owned()])),
        weight: FontWeight::MEDIUM,
        style: FontStyle::Normal,
    }
}

fn copied(series: &VecDeque<f32>) -> Vec<f32> {
    series.iter().copied().collect()
}

/// One metric: name, current value, and a 36px area chart.
fn meter(
    name: &'static str,
    value: String,
    series: &[f32],
    ceiling: f32,
    color: u32,
    extra: Option<(Vec<f32>, u32)>,
) -> gpui_kit::Div {
    let points = series.to_vec();
    div()
        .mt(px(4.0))
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .items_end()
                .justify_between()
                .child(div().text_size(px(11.0)).text_color(hsla(0.0, 0.0, 1.0, 0.6)).child(name))
                .child(div().text_size(px(14.0)).child(value)),
        )
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, (), window, _| paint_area(bounds, &points, ceiling, color, extra.as_ref(), window),
            )
            .w_full()
            .h(px(36.0))
            .mt(px(2.0)),
        )
}

/// Area under the series, plus a 2px stroke. `extra` is the upload line.
fn paint_area(
    bounds: gpui_kit::Bounds<gpui_kit::Pixels>,
    series: &[f32],
    ceiling: f32,
    color: u32,
    extra: Option<&(Vec<f32>, u32)>,
    window: &mut Window,
) {
    paint_series(bounds, series, ceiling, color, window);
    if let Some((upload, upload_color)) = extra {
        paint_series(bounds, upload, ceiling, *upload_color, window);
    }
}

fn paint_series(
    bounds: gpui_kit::Bounds<gpui_kit::Pixels>,
    series: &[f32],
    ceiling: f32,
    color: u32,
    window: &mut Window,
) {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    if width < 1.0 || height < 1.0 {
        return;
    }
    let slots = perf::chart_slots(series);
    #[allow(clippy::cast_precision_loss)]
    let span = (perf::HISTORY_CAP.saturating_sub(1)) as f32;
    let mut fill = gpui_kit::PathBuilder::fill();
    let mut stroke = gpui_kit::PathBuilder::stroke(px(2.0));
    let mut started = false;
    for (index, value) in slots.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let x = f32::from(bounds.origin.x) + (index as f32) / span * width;
        let ratio = if ceiling <= f32::EPSILON { 0.0 } else { (value / ceiling).clamp(0.0, 1.0) };
        let y = f32::from(bounds.origin.y) + height - ratio * height;
        let at = point(px(x), px(y));
        if started {
            fill.line_to(at);
            stroke.line_to(at);
        } else {
            fill.move_to(at);
            stroke.move_to(at);
            started = true;
        }
    }
    let floor_y = px(f32::from(bounds.origin.y) + height);
    fill.line_to(point(px(f32::from(bounds.origin.x) + width), floor_y));
    fill.line_to(point(px(f32::from(bounds.origin.x)), floor_y));
    fill.close();
    if let Ok(path) = fill.build() {
        window.paint_path(path, rgb(perf::tint(color, 0.2)));
    }
    if let Ok(path) = stroke.build() {
        window.paint_path(path, rgb(color));
    }
}

/// Title-row chevron. Faint until the pointer is over it.
type Click = std::rc::Rc<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>;

pub(crate) fn page_arrow(forward: bool, hot: bool, listener: Click) -> gpui_kit::Div {
    div()
        .absolute()
        .top(px(4.0))
        .w(px(24.0))
        .h(px(24.0))
        .when(forward, |arrow| arrow.right(px(8.0)))
        .when(!forward, |arrow| arrow.left(px(8.0)))
        .flex()
        .items_center()
        .justify_center()
        .opacity(if hot { 0.9 } else { 0.16 })
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, move |event, window, app| listener(event, window, app))
        .child(div().text_size(px(14.0)).child(if forward { ">" } else { "<" }))
}
