//! The view. It subscribes to every channel and paints one morphing pill.
//!
//! `IslandView` lives on the GPUI thread. Nothing else mutates it. The hook,
//! the tray, and the volume STA copy a small value into a channel. Blocking
//! `WinRT` and sysinfo calls run on `background_executor` and come back through
//! `view.update`, which runs the closure on this thread (the same arrangement
//! as `SendMessage` back to a window thread).
//!
//! [`IslandView::arm`] starts five detached loops. After the view is dropped,
//! `view.update` fails and the loop ends:
//!
//! - 32ms: [`IslandView::poll_pointer`], [`IslandView::poll_tray`],
//!   [`IslandView::poll_volume`], and the card-idle check. Drain only. No I/O.
//! - 1s: `media::current`, then `toasts::poll`. A seek drag drops the fresh
//!   snapshot. Transport is started, not joined.
//! - 1s, separate: one `perf::Sampler` moved onto the executor and back.
//!   `cx.notify` only when the performance page is actually showing.
//! - hover tickets: [`IslandView::note_dwell`] (150ms) and
//!   [`IslandView::schedule_tuck`] (100ms). The ticket is a generation. Leave
//!   and re-enter both increment it; a timer holding an old ticket returns.
//! - notice hold: [`IslandView::arm_notice`] sleeps 6s, then removes that id
//!   only if it is still the head and not pinned.
//!
//! Paint is [`IslandView::render`] to [`IslandView::face`] to
//! [`IslandView::paint_motion`] to `chrome::paint`. `face` is a snapshot so
//! the render path does not re-borrow the view. `go` records `from`, `to`,
//! a generation, and `started_ms`. The render path eases with `ease_out_quint`
//! and asks for another frame 16ms later while the generation still matches.
//!
//! `Option<T>` is the explicit null: not armed yet, no drag, no notice.
//! Callers match before use.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::{
    App, Context, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Render, ScrollDelta, ScrollWheelEvent, Styled,
    Window, div, ease_out_quint,
};

use crate::pointer::{PointerWatch, ScreenPoint};
use crate::toasts;
use rustedge::media::{self, NowPlaying};
use rustedge::notice::{Content, Notice, Queue};
use rustedge::perf::{self, PerfHistory};
use rustedge::island::{
    self, Hover, IslandState, Morph, Tab, Wheel, resting,
};
use rustedge::volume::{self, Endpoint, Reading, VolumeCommand, VolumeState};

mod chrome;
mod media_page;

use chrome::{page_arrow_at, paint};
use media_page::CardClicks;

type Click = Rc<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>;

/// What the face paints. Kept together so the render path stays one argument.
#[derive(Clone)]
#[allow(clippy::struct_excessive_bools)]
struct Face {
    shape: Morph,
    playing: bool,
    tab: Tab,
    volume: VolumeState,
    volume_visible: bool,
    state: IslandState,
    notice: Option<Notice>,
    badge: bool,
    media_line: String,
    media_playing: bool,
    cover: Option<std::sync::Arc<gpui_kit::RenderImage>>,
    progress: f32,
    position_secs: u32,
    duration_secs: u32,
    track_hot: bool,
    album_line: Option<String>,
    transport: media::TransportCaps,
    modes: media::ModeCaps,
    sessions: Vec<media::MediaSession>,
    seekable: bool,
    sessions_open: bool,
    perf: PerfHistory,
    page_slide: f32,
}

/// One morph already on screen, so the next animation knows where it starts.
struct Shown {
    shape: Rc<Cell<Morph>>,
    /// `None` while the shape is at rest.
    motion: Option<Motion>,
}

/// A morph in flight, measured from when it started.
#[derive(Clone)]
struct Motion {
    from: IslandState,
    to: IslandState,
    generation: u64,
    started_ms: u64,
}

/// Root view. The line, capsule, and card are one state.
#[allow(clippy::struct_excessive_bools)]
pub struct IslandView {
    state: IslandState,
    shown: Shown,
    /// Bumped whenever a morph starts, so the animation element is recreated.
    generation: u64,
    /// Bumped when the pointer re-enters, cancelling a pending tuck.
    hover_ticket: u64,
    playing: bool,
    tab: Tab,
    /// Master volume. The wheel overlay is visible while `overlay_until` is set.
    volume: VolumeState,
    /// Endpoint thread. `None` until the window is armed.
    endpoint: Option<Endpoint>,
    /// Leftover touchpad delta, in wheel steps.
    wheel_pending: f32,
    /// Monotonic millisecond when the wheel overlay should hide.
    overlay_until: Option<u64>,
    /// Monotonic millisecond when the pointer entered the trigger. `None` outside it.
    dwell_since: Option<u64>,
    /// When the pointer left the card. `None` while it is over the card.
    card_left_ms: Option<u64>,
    /// Pointer is over the progress track, so it draws thicker.
    track_hot: bool,
    /// Scrub in progress. The clock follows this until the button is released.
    seek_drag: Option<media::SeekGesture>,
    /// A painted control handled the current press. The card body must not also fold.
    gesture_handled: bool,
    /// Session list is open on the card.
    sessions_open: bool,
    /// Toast ids seen on the previous poll.
    seen_toasts: std::collections::HashSet<u32>,
    /// Screen position of the pointer when a horizontal drag started.
    drag_origin: Option<i32>,
    /// Window left edge when that drag started.
    window_origin: Option<i32>,
    /// Global mouse hook. The line is click-through, so the window itself
    /// cannot see the pointer enter the trigger.
    pointer: Option<PointerWatch>,
    /// Window is parked off-screen. Alt+H toggles this.
    hidden: bool,
    /// Exclusive fullscreen hid the dock. Notices stay queued until it ends.
    exclusive: bool,
    /// Physical top-left of the stage, captured from a window callback.
    stage_origin: Option<ScreenPoint>,
    /// Toasts waiting to preempt media. Empty means the media source.
    notices: Queue,
    /// Last SMTC snapshot. Idle until the first poll returns.
    now_playing: NowPlaying,
    /// Tray clicks. `None` until the window is armed.
    tray: Option<std::sync::mpsc::Receiver<crate::tray::Command>>,
    /// CPU, memory, and network. Kept across page changes.
    perf: PerfHistory,
    /// `0.0` is media, `1.0` is performance. Slides for [`island::PAGE_SLIDE_MS`].
    page_slide: f32,
    /// Where the slide is going, if one is running.
    page_target: Option<f32>,
    /// Monotonic millisecond the current slide started.
    page_started_ms: u64,
    /// Arrow under the pointer. `None` means neither.
    arrow_hot: Option<bool>,
}

impl IslandView {
    /// Tucked line, playing the first demo track.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: IslandState::Line,
            shown: Shown {
                shape: Rc::new(Cell::new(resting(IslandState::Line))),
                motion: None,
            },
            generation: 0,
            hover_ticket: 0,
            playing: true,
            tab: Tab::Media,
            volume: VolumeState::unavailable(),
            endpoint: None,
            wheel_pending: 0.0,
            overlay_until: None,
            dwell_since: None,
            card_left_ms: None,
            track_hot: false,
            seek_drag: None,
            gesture_handled: false,
            sessions_open: false,
            seen_toasts: std::collections::HashSet::new(),
            drag_origin: None,
            window_origin: None,
            pointer: None,
            hidden: false,
            exclusive: false,
            stage_origin: None,
            notices: Queue::new(),
            now_playing: NowPlaying::idle(),
            tray: None,
            perf: PerfHistory::new(),
            page_slide: 0.0,
            page_target: None,
            page_started_ms: 0,
            arrow_hot: None,
        }
    }

    /// Takes the tray channel. Quit asks GPUI to stop; show/hide is a window call.
    pub fn take_tray(&mut self, commands: std::sync::mpsc::Receiver<crate::tray::Command>) {
        self.tray = Some(commands);
    }

    fn absorb_toasts(&mut self, toasts: Vec<rustedge::notice::Notice>, cx: &mut Context<Self>) {
        let ids: Vec<u32> = toasts.iter().map(|toast| toast.id).collect();
        // The first poll is the Action Center backlog, not a new arrival.
        if self.seen_toasts.is_empty() && self.notices.head_id().is_none() && !self.exclusive {
            self.seen_toasts.extend(ids);
            return;
        }
        self.notices.retain_ids(&ids);
        for toast in toasts {
            if self.seen_toasts.insert(toast.id) {
                self.push_notice(toast, cx);
            }
        }
        self.seen_toasts.retain(|id| ids.contains(id));
    }

    /// A toast arrived. The line grows into a capsule on its own. An open card
    /// is not interrupted; the toast waits and the badge lights.
    #[allow(dead_code)]
    pub fn push_notice(&mut self, notice: Notice, cx: &mut Context<Self>) {
        let panel = self.state == IslandState::Card;
        let id = notice.id;
        self.notices.push(notice, panel);
        if self.exclusive {
            cx.notify();
            return;
        }
        if !panel && self.state == IslandState::Line {
            self.go(IslandState::Capsule);
        }
        if !panel {
            self.arm_notice(id, cx);
        }
        cx.notify();
    }

    #[allow(clippy::unused_self)]
    fn arm_notice(&mut self, id: u32, cx: &mut Context<Self>) {
        cx.spawn(async move |view, cx| {
            cx.background_executor().timer(Duration::from_millis(rustedge::notice::NOTICE_HOLD_MS)).await;
            view.update(cx, |view, cx| view.finish_notice(id, cx)).ok();
        })
        .detach();
    }

    fn finish_notice(&mut self, id: u32, cx: &mut Context<Self>) {
        if self.notices.pinned() || self.notices.head_id() != Some(id) {
            return;
        }
        if !self.notices.remove_id(id) {
            return;
        }
        let hovering = self.dwell_since.is_some() || self.state == IslandState::Capsule;
        if self.notices.head_id().is_some() {
            if let Some(next) = self.notices.head_id() {
                self.arm_notice(next, cx);
            }
        } else if !hovering {
            self.go(IslandState::Line);
        }
        cx.notify();
    }

    /// Installs the global pointer hook once the window exists.
    ///
    /// Both loops are detached: no `JoinHandle` is kept. After GPUI drops the
    /// view, `view.update` returns an error and the loop exits. The 32ms loop
    /// only drains messages that have already arrived. The 1s loop is the one
    /// that calls SMTC and the notification center. Those calls block, so they
    /// run on `background_executor` and never occupy the UI thread.
    pub fn arm(&mut self, cx: &mut Context<Self>) {
        if self.pointer.is_none() {
            self.pointer = Some(PointerWatch::spawn());
        }
        if self.endpoint.is_none() {
            self.endpoint = Some(Endpoint::spawn());
        }
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(32)).await;
                let continued = view
                    .update(cx, |view, cx| {
                        view.poll_pointer(cx);
                        view.poll_tray(cx);
                        view.poll_volume(cx);
                        if view.state == IslandState::Capsule {
                            cx.notify();
                        }
                        if view.card_left_ms.is_some() {
                            view.watch_card(false, cx);
                        }
                        true
                    })
                    .is_ok();
                if !continued {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |view, cx| {
            loop {
                let snapshot = cx.background_executor().spawn(async { media::current() }).await;
                let continued = view
                    .update(cx, |view, cx| {
                        if view.seek_drag.is_none() {
                            view.now_playing = snapshot;
                        }
                        if view.now_playing.duration_secs == 0 {
                            media::mark_seek_unavailable();
                        }
                        cx.notify();
                        true
                    })
                    .is_ok();
                if !continued {
                    break;
                }
                cx.background_executor().timer(Duration::from_millis(1000)).await;
                let toasts = cx.background_executor().spawn(async { toasts::poll() }).await;
                view.update(cx, |view, cx| view.absorb_toasts(toasts, cx)).ok();
            }
        })
        .detach();
        cx.spawn(async move |view, cx| {
            let mut sampler = cx
                .background_executor()
                .spawn(async { perf::Sampler::warm(millis_now()) })
                .await;
            cx.background_executor().timer(perf::SAMPLE_EVERY).await;
            loop {
                let taken = cx
                    .background_executor()
                    .spawn(async move {
                        let reading = sampler.sample(millis_now());
                        (sampler, reading)
                    })
                    .await;
                sampler = taken.0;
                let reading = taken.1;
                let continued = view
                    .update(cx, |view, cx| {
                        view.perf.push(reading);
                        if view.tab == Tab::Perf && view.state != IslandState::Line {
                            cx.notify();
                        }
                        true
                    })
                    .is_ok();
                if !continued {
                    break;
                }
                cx.background_executor().timer(perf::SAMPLE_EVERY).await;
            }
        })
        .detach();
    }

    fn remember_stage(&mut self, window: &Window) {
        self.stage_origin = Some(ScreenPoint {
            x: physical_origin_x(window),
            y: physical_origin_y(window),
            pressed: false,
            released: false,
        });
    }

    fn poll_tray(&mut self, cx: &mut Context<Self>) {
        let Some(tray) = self.tray.take() else {
            return;
        };
        let commands: Vec<crate::tray::Command> = tray.try_iter().collect();
        self.tray = Some(tray);
        for command in commands {
            match command {
                crate::tray::Command::Toggle => {
                    if !self.exclusive {
                        toggle_overlay(cx);
                    }
                }
                crate::tray::Command::Quit => cx.quit(),
                crate::tray::Command::ExclusiveHide => self.hold_for_exclusive(cx),
                crate::tray::Command::ExclusiveShow => self.release_exclusive(cx),
            }
        }
    }

    fn hold_for_exclusive(&mut self, cx: &mut Context<Self>) {
        self.exclusive = true;
        self.notices.set_suspended(true);
        cx.notify();
    }

    fn release_exclusive(&mut self, cx: &mut Context<Self>) {
        if !self.exclusive {
            return;
        }
        self.exclusive = false;
        self.notices.set_suspended(false);
        if self.notices.head_id().is_some() && self.state == IslandState::Line {
            self.go(IslandState::Capsule);
        }
        if let Some(id) = self.notices.head_id() {
            self.arm_notice(id, cx);
        }
        cx.notify();
    }

    fn poll_pointer(&mut self, cx: &mut Context<Self>) {
        let Some(pointer) = self.pointer.as_ref() else {
            return;
        };
        let points: Vec<ScreenPoint> = pointer.drain();
        let released = points.iter().any(|point| point.released);
        let Some(point) = points.last().copied() else {
            return;
        };
        if released {
            self.finish_seek(cx);
        }
        if self.hidden || self.exclusive {
            return;
        }
        let placed = cx.windows().first().and_then(|handle| {
            handle.update(cx, |_root, window, _cx| {
                let scale = window.scale_factor().max(1.0);
                (physical_origin_x(window), physical_origin_y(window), scale)
            }).ok()
        });
        let Some((origin_x, origin_y, scale)) = placed else {
            return;
        };
        #[allow(clippy::cast_precision_loss)]
        let x = point.x.saturating_sub(origin_x) as f32 / scale;
        #[allow(clippy::cast_precision_loss)]
        let y = point.y.saturating_sub(origin_y) as f32 / scale;
        let hover = island::hover_at(self.state, x, y);
        if self.seek_drag.is_some() {
            self.move_seek(x, cx);
        }
        self.watch_card(island::over_card(x, y), cx);
        // Tucked, the window still covers its 420 by 360 stage. Off the line the
        // window sets `WS_EX_TRANSPARENT`, so clicks fall through to whatever is
        // underneath. Entering the hot zone clears it. This is why the pointer
        // comes from a global hook instead of the window's own `WM_MOUSEMOVE`:
        // once the window is click-through it receives no moves.
        let through = hover == Hover::Outside && self.state == IslandState::Line;
        for handle in cx.windows() {
            handle
                .update(cx, |_root, window, _cx| {
                    crate::pointer::set_click_through(window_hwnd(window), through);
                })
                .ok();
        }
        self.apply_hover(hover, cx);
    }

    fn move_seek(&mut self, x: f32, cx: &mut Context<Self>) {
        let Some(gesture) = self.seek_drag.as_mut() else {
            return;
        };
        gesture.drag(island::seek_fraction(x));
        cx.notify();
    }

    fn finish_seek(&mut self, cx: &mut Context<Self>) {
        let Some(gesture) = self.seek_drag.take() else {
            return;
        };
        let (fraction, seconds) = gesture.release();
        cx.background_executor()
            .spawn(async move { media::seek(fraction, seconds) })
            .detach();
        cx.notify();
    }

    /// A painted control already took this press, so the card fold must not.
    fn claim(&mut self) {
        self.gesture_handled = true;
    }

    fn begin_seek(&mut self, fraction: f32, cx: &mut Context<Self>) {
        self.claim();
        if !self.now_playing.seekable || self.now_playing.duration_secs == 0 || media::seek_refused() {
            return;
        }
        self.seek_drag = Some(media::SeekGesture::press(fraction, self.now_playing.duration_secs));
        cx.notify();
    }

    fn toggle_sessions(&mut self, cx: &mut Context<Self>) {
        self.claim();
        self.sessions_open = !self.sessions_open;
        cx.notify();
    }

    fn pick_session(&mut self, index: usize, cx: &mut Context<Self>) {
        self.claim();
        let Some(session) = self.now_playing.sessions.get(index) else {
            return;
        };
        let id = session.id.clone();
        self.sessions_open = false;
        cx.background_executor().spawn(async move { media::select_session(&id) }).detach();
        cx.notify();
    }

    fn toggle_shuffle(&mut self, cx: &mut Context<Self>) {
        self.claim();
        let active = self.now_playing.modes.shuffle == Some(true);
        cx.background_executor().spawn(async move { media::toggle_shuffle(active) }).detach();
    }

    fn cycle_repeat(&mut self, cx: &mut Context<Self>) {
        self.claim();
        let mode = self.now_playing.modes.repeat.unwrap_or(media::RepeatMode::None);
        cx.background_executor().spawn(async move { media::cycle_repeat(mode) }).detach();
    }

    fn nudge(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.seek_drag.is_some() || !self.now_playing.seekable || media::seek_refused() {
            return;
        }
        let duration = self.now_playing.duration_secs;
        if duration == 0 {
            return;
        }
        let next = media::nudge_secs(self.now_playing.position_secs, duration, forward);
        let fraction = media::secs_fraction(next, duration);
        self.now_playing.position_secs = next;
        self.now_playing.progress = fraction;
        cx.background_executor()
            .spawn(async move { media::seek(fraction, duration) })
            .detach();
        cx.notify();
    }

    fn watch_card(&mut self, over: bool, cx: &mut Context<Self>) {
        if self.state != IslandState::Card {
            self.card_left_ms = None;
            return;
        }
        if over {
            self.card_left_ms = None;
            return;
        }
        let left = *self.card_left_ms.get_or_insert_with(millis_now);
        if millis_now().saturating_sub(left) >= island::CARD_IDLE_MS {
            self.card_left_ms = None;
            self.notices.dismiss();
            self.go(IslandState::Capsule);
            cx.notify();
        }
    }

    fn apply_hover(&mut self, hover: Hover, cx: &mut Context<Self>) {
        let Hover::Inside = hover else {
            self.dwell_since = None;
            self.schedule_tuck(cx);
            return;
        };
        self.hover_ticket = self.hover_ticket.saturating_add(1);
        self.note_dwell(cx);
    }

    fn go(&mut self, next: IslandState) {
        if next == self.state {
            return;
        }
        let from = self.state;
        self.generation = self.generation.saturating_add(1);
        self.shown.motion = Some(Motion {
            from,
            to: next,
            generation: self.generation,
            started_ms: millis_now(),
        });
        self.state = next;
    }
}

impl Render for IslandView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let generation = self.generation;
        let face_now = self.face();
        self.paint_motion(generation, &face_now, window, cx)
    }
}

impl IslandView {
    fn face(&self) -> Face {
        Face {
            shape: self.shown.shape.get(),
            playing: self.playing,
            tab: self.tab,
            volume: VolumeState {
                overlay: self.overlay_until.is_some_and(|until| millis_now() < until),
                slider: false,
                ..self.volume
            },
            volume_visible: self.overlay_until.is_some_and(|until| millis_now() < until),
            state: self.state,
            notice: if self.exclusive {
                None
            } else {
                match self.notices.content() {
                    Content::Notice(notice) => Some(notice),
                    Content::Media => None,
                }
            },
            badge: self.notices.badge(),
            media_line: self.now_playing.line(),
            media_playing: self.now_playing.playing,
            cover: self.now_playing.cover.clone(),
            progress: self.seek_drag.map_or(self.now_playing.progress, |gesture| gesture.shown(self.now_playing.progress, self.now_playing.position_secs).0),
            position_secs: self.seek_drag.map_or(self.now_playing.position_secs, |gesture| gesture.shown(self.now_playing.progress, self.now_playing.position_secs).1),
            duration_secs: self.now_playing.duration_secs,
            track_hot: self.track_hot || self.seek_drag.is_some(),
            album_line: self.now_playing.album_line(),
            transport: self.now_playing.transport,
            modes: self.now_playing.modes,
            sessions: self.now_playing.sessions.clone(),
            seekable: self.now_playing.seekable && self.now_playing.duration_secs > 0 && !media::seek_refused(),
            sessions_open: self.sessions_open,
            perf: self.perf.clone(),
            page_slide: self.page_slide,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn paint_motion(&mut self, generation: u64, face_now: &Face, window: &Window, cx: &mut Context<Self>) -> gpui_kit::Div {
        let motion = self.shown.motion.clone();
        let toggle: Click = Rc::new(cx.listener(|view, _event: &MouseDownEvent, window, cx| {
            if view.gesture_handled {
                view.gesture_handled = false;
                return;
            }
            let pointer = window.mouse_position();
            if view.state == IslandState::Card && f32::from(pointer.x) < 116.0 && f32::from(pointer.y) < 116.0 {
                cx.background_executor().spawn(async { media::open_player() }).detach();
                return;
            }
            if matches!(view.notices.content(), Content::Notice(_)) && view.state == IslandState::Capsule {
                view.notices.pin();
            }
            if view.state == IslandState::Card {
                view.notices.dismiss();
            }
            view.go(island::after_body_click(view.state));
            cx.notify();
        }));
        let previous: Click = Rc::new(cx.listener(|view, _event, _window, cx| {
            view.claim();
            cx.stop_propagation();
            if view.now_playing.transport.previous {
                dispatch_transport(media::Transport::Previous, cx);
            }
        }));
        let play: Click = Rc::new(cx.listener(|view, _event, _window, cx| {
            view.claim();
            cx.stop_propagation();
            if view.now_playing.transport.play_pause {
                dispatch_transport(media::Transport::PlayPause, cx);
            }
        }));
        let next: Click = Rc::new(cx.listener(|view, _event, _window, cx| {
            view.claim();
            cx.stop_propagation();
            if view.now_playing.transport.next {
                dispatch_transport(media::Transport::Next, cx);
            }
        }));
        let mute: Click = Rc::new(cx.listener(|view, _event, _window, cx| {
            view.apply_volume(VolumeCommand::ToggleMute, cx);
        }));
        let page_prev: Click = Rc::new(cx.listener(|view, _event: &MouseDownEvent, _window, cx| {
            view.claim();
            cx.stop_propagation();
            if view.state != IslandState::Line {
                view.turn_page(false, cx);
            }
        }));
        let page_next: Click = Rc::new(cx.listener(|view, _event: &MouseDownEvent, _window, cx| {
            view.claim();
            cx.stop_propagation();
            if view.state != IslandState::Line {
                view.turn_page(true, cx);
            }
        }));


        let card_clicks = CardClicks {
            open_player: Rc::new(cx.listener(|view, _event: &MouseDownEvent, _window, cx| {
                view.claim();
                cx.background_executor().spawn(async { media::open_player() }).detach();
            })),
            seek: Rc::new(cx.listener(|view, _event: &MouseDownEvent, window, cx| {
                view.begin_seek(island::seek_fraction(f32::from(window.mouse_position().x)), cx);
            })),
            sessions: Rc::new(cx.listener(|view, _event: &MouseDownEvent, _window, cx| view.toggle_sessions(cx))),
            session: Rc::new(cx.listener(|view, index: &usize, _window, cx| view.pick_session(*index, cx))),
            shuffle: Rc::new(cx.listener(|view, _event: &MouseDownEvent, _window, cx| view.toggle_shuffle(cx))),
            repeat: Rc::new(cx.listener(|view, _event: &MouseDownEvent, _window, cx| view.cycle_repeat(cx))),
        };
        let _ = &card_clicks;

        let island = if let Some(motion) = motion.filter(|motion| motion.generation == generation) {
            let (shape, raw) = frame_shape(&motion);
            if raw < 1.0 {
                let ticket = generation;
                cx.spawn(async move |view, cx| {
                    cx.background_executor().timer(Duration::from_millis(16)).await;
                    view.update(cx, |view, cx| {
                        if view.generation == ticket {
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .detach();
            } else {
                self.shown.motion = None;
                self.shown.shape.set(shape);
            }
            paint(
                &Face { shape, state: motion.to, ..face_now.clone() },
                &card_clicks,
                window,
                press(&toggle),
                press(&previous),
                press(&play),
                press(&next),
                press(&mute),
                Rc::clone(&page_prev),
                Rc::clone(&page_next),
                self.arrow_hot,
            )
            .into_any_element()
        } else {
            paint(
                &Face { shape: island::resting(self.state), ..face_now.clone() },
                &card_clicks,
                window,
                press(&toggle),
                press(&previous),
                press(&play),
                press(&next),
                press(&mute),
                Rc::clone(&page_prev),
                Rc::clone(&page_next),
                self.arrow_hot,
            )
            .into_any_element()
        };

        let hover = cx.listener(|view, event: &MouseMoveEvent, window, cx| {
            view.remember_stage(window);
            let hover = island::hover_at(
                view.state,
                f32::from(event.position.x),
                f32::from(event.position.y),
            );
            let through = view.state == IslandState::Line && hover == Hover::Outside;
            crate::pointer::set_click_through(window_hwnd(window), through);
            if view.drag_origin.is_some() && view.seek_drag.is_none() {
                slide_horizontally(view, window);
            }
            view.apply_hover(hover, cx);
            let seekable = view.now_playing.seekable && view.now_playing.duration_secs > 0 && !media::seek_refused();
            let hot = view.state == IslandState::Card
                && island::over_seek(f32::from(event.position.x), 0.0, seekable);
            let arrow = page_arrow_at(
                f32::from(event.position.x),
                f32::from(event.position.y),
                view.state,
                view.shown.shape.get().width,
            );
            let changed = hot != view.track_hot || arrow != view.arrow_hot;
            view.track_hot = hot;
            view.arrow_hot = arrow;
            if changed {
                cx.notify();
            }
        });
        let press = cx.listener(|view, event: &MouseDownEvent, window, _cx| {
            if event.button != MouseButton::Left || view.state == IslandState::Line || view.seek_drag.is_some() {
                return;
            }
            view.drag_origin = cursor_x();
            view.window_origin = Some(physical_origin_x(window));
        });
        let release = cx.listener(|view, _: &MouseUpEvent, _window, _cx| {
            view.drag_origin = None;
            view.window_origin = None;
        });

        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .on_mouse_down(MouseButton::Left, press)
            .on_mouse_up(MouseButton::Left, release)
            .on_mouse_move(hover)
            .on_scroll_wheel(cx.listener(|view, event: &ScrollWheelEvent, _window, cx| {
                view.on_wheel(event, cx);
            }))
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, _window, cx| {
                view.on_arrow(event, cx);
            }))
            .child(island)
    }
}

impl IslandView {
    fn schedule_tuck(&mut self, cx: &mut Context<Self>) {
        if self.state != IslandState::Capsule || !matches!(self.notices.content(), Content::Media) {
            return;
        }
        self.hover_ticket = self.hover_ticket.saturating_add(1);
        // A ticket, not a cancel token. Leaving increments it; coming back
        // increments it again. The timer compares the number it captured. A
        // mismatch means the pointer re-entered, so this tuck is stale. Same
        // pattern as remembering a generation and returning when it changed.
        let ticket = self.hover_ticket;
        cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(island::HOVER_LEAVE_MS))
                .await;
            view.update(cx, |view, cx| {
                if view.hover_ticket == ticket && view.state == IslandState::Capsule {
                    view.go(IslandState::Line);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn note_dwell(&mut self, cx: &mut Context<Self>) {
        if self.state != IslandState::Line {
            self.dwell_since = None;
            return;
        }
        let now = millis_now();
        let started = self.dwell_since.get_or_insert(now);
        if !island::dwell_ready(*started, now) {
            let ticket = self.hover_ticket;
            cx.spawn(async move |view, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(island::DWELL_MS))
                    .await;
                view.update(cx, |view, cx| {
                    let ready = view.hover_ticket == ticket
                        && view.state == IslandState::Line
                        && view.dwell_since.is_some_and(|since| island::dwell_ready(since, millis_now()));
                    if ready {
                        view.dwell_since = None;
                        view.go(IslandState::Capsule);
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
            return;
        }
        self.dwell_since = None;
        self.go(IslandState::Capsule);
        cx.notify();
    }

    fn on_arrow(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.state != IslandState::Card || !self.now_playing.seekable || media::seek_refused() {
            return;
        }
        let forward = match event.keystroke.key.as_str() {
            "right" => true,
            "left" => false,
            _ => return,
        };
        self.nudge(forward, cx);
    }

    fn on_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        if self.state == IslandState::Line {
            return;
        }
        let (x, y, pixels) = match event.delta {
            ScrollDelta::Pixels(point) => (f32::from(point.x), f32::from(point.y), true),
            ScrollDelta::Lines(point) => (point.x, point.y, false),
        };
        let Some(wheel) = island::wheel_of(x, y, event.modifiers.alt) else {
            return;
        };
        match wheel {
            Wheel::Tab { next } => self.turn_page(next, cx),
            Wheel::Vertical { .. } if volume::wheel_applies(self.state == IslandState::Card, self.volume.available) => {
                let step = volume::accumulate_wheel(
                    self.volume.level,
                    self.volume.muted,
                    self.wheel_pending,
                    y,
                    pixels,
                );
                self.wheel_pending = step.pending;
                if let Some(command) = step.command {
                    self.apply_volume(command, cx);
                    self.flash_volume(cx);
                }
            }
            Wheel::Vertical { .. } => {}
        }
    }

    /// Slide to the other card page. The performance history is not cleared.
    fn turn_page(&mut self, next: bool, cx: &mut Context<Self>) {
        if self.notices.head_id().is_some() {
            return;
        }
        self.tab = island::step_tab(self.tab, next);
        let target = match self.tab {
            Tab::Media => 0.0,
            Tab::Perf => 1.0,
        };
        self.page_target = Some(target);
        self.page_started_ms = millis_now();
        cx.notify();
        cx.spawn(async move |view, cx| {
            cx.background_executor().timer(Duration::from_millis(16)).await;
            view.update(cx, IslandView::advance_page).ok();
        })
        .detach();
    }

    fn advance_page(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.page_target else {
            return;
        };
        let elapsed = millis_now().saturating_sub(self.page_started_ms);
        let span = island::PAGE_SLIDE_MS.max(1);
        #[allow(clippy::cast_precision_loss)]
        let raw = (elapsed as f32 / span as f32).clamp(0.0, 1.0);
        let eased = ease_out_quint()(raw);
        let origin = 1.0 - target;
        self.page_slide = origin + (target - origin) * eased;
        if raw < 1.0 {
            cx.spawn(async move |view, cx| {
                cx.background_executor().timer(Duration::from_millis(16)).await;
                view.update(cx, IslandView::advance_page).ok();
            })
            .detach();
        } else {
            self.page_slide = target;
            self.page_target = None;
        }
        cx.notify();
    }

    fn apply_volume(&mut self, command: VolumeCommand, cx: &mut Context<Self>) {
        match command {
            VolumeCommand::SetLevel(level) => {
                self.volume.level = level;
                self.volume.muted = false;
            }
            VolumeCommand::ToggleMute => {
                self.volume.muted = !self.volume.muted;
            }
        }
        if let Some(endpoint) = &self.endpoint {
            endpoint.send(command);
        }
        cx.notify();
    }

    fn flash_volume(&mut self, cx: &mut Context<Self>) {
        self.overlay_until = Some(millis_now().saturating_add(hold_ms(volume::OVERLAY_HOLD)));
        cx.notify();
    }

    fn poll_volume(&mut self, cx: &mut Context<Self>) {
        let reading = self.endpoint.as_ref().and_then(Endpoint::poll);
        if let Some(Reading { level, muted, available }) = reading {
            let changed = (self.volume.level - level).abs() > 0.001
                || self.volume.muted != muted
                || self.volume.available != available;
            self.volume.level = level;
            self.volume.muted = muted;
            self.volume.available = available;
            if changed {
                cx.notify();
            }
        }
        let expired = self.overlay_until.is_some_and(|until| millis_now() >= until);
        if expired {
            self.overlay_until = None;
            cx.notify();
        }
    }

}

fn dispatch_transport(command: media::Transport, cx: &mut gpui_kit::Context<IslandView>) {
    cx.spawn(async move |view, cx| {
        cx.background_executor()
            .spawn(async move { media::send(command) })
            .await;
        let snapshot = cx.background_executor().spawn(async { media::current() }).await;
        view.update(cx, |view, cx| {
            view.now_playing = snapshot;
            cx.notify();
        })
        .ok();
    })
    .detach();
}

pub(super) fn millis_now() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis()),
    )
    .unwrap_or(0)
}

/// Current shape and un-eased progress. `1.0` means the morph has finished.
fn frame_shape(motion: &Motion) -> (Morph, f32) {
    let duration = if motion.from == IslandState::Line || motion.to == IslandState::Line {
        island::TUCK_MS
    } else {
        island::MORPH_MS
    };
    let elapsed = millis_now().saturating_sub(motion.started_ms);
    #[allow(clippy::cast_precision_loss)]
    let raw = if duration == 0 { 1.0 } else { elapsed as f32 / duration as f32 };
    let progress = ease_out_quint()(raw.clamp(0.0, 1.0));
    (blend(motion.from, motion.to, progress), raw)
}

/// `progress` is `0.0` at `from` and `1.0` at `to`.
fn blend(from: IslandState, to: IslandState, progress: f32) -> Morph {
    lerp(island::resting(from), island::resting(to), progress)
}

fn lerp(start: Morph, end: Morph, progress: f32) -> Morph {
    Morph {
        width: start.width + (end.width - start.width) * progress,
        height: start.height + (end.height - start.height) * progress,
        radius: start.radius + (end.radius - start.radius) * progress,
        shadow: start.shadow + (end.shadow - start.shadow) * progress,
        detail: start.detail + (end.detail - start.detail) * progress,
    }
}

/// Slides the window sideways. The top edge stays where the window was opened.
///
/// The new left is the press-time cursor and window origin, not the sum of
/// event deltas. Deltas drift when a move message is dropped. An absolute
/// position is recomputed every move and stops when the button comes up.
/// `SWP_NOZORDER` keeps this move out of the z-order. The tray thread owns
/// topmost.
fn slide_horizontally(view: &IslandView, window: &mut Window) {
    let (Some(start), Some(origin), Some(cursor)) = (view.drag_origin, view.window_origin, cursor_x()) else {
        return;
    };
    let hwnd = window_hwnd(window);
    let top = physical_origin_y(window);
    unsafe {
        let _moved = windows_sys::Win32::UI::WindowsAndMessaging::SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            origin.saturating_add(cursor.saturating_sub(start)),
            top,
            0,
            0,
            windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOSIZE
                | windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOZORDER
                | windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
        );
    }
}

fn cursor_x() -> Option<i32> {
    let mut point = windows_sys::Win32::Foundation::POINT::default();
    let found = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&raw mut point) };
    if found == 0 { None } else { Some(point.x) }
}

fn physical_origin_x(window: &Window) -> i32 {
    logical_to_device(f32::from(window.bounds().origin.x), window.scale_factor())
}

fn physical_origin_y(window: &Window) -> i32 {
    logical_to_device(f32::from(window.bounds().origin.y), window.scale_factor())
}

fn logical_to_device(logical: f32, scale: f32) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    { (logical * scale).round() as i32 }
}

fn toggle_overlay(cx: &mut Context<IslandView>) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IsWindowVisible, SW_HIDE, SW_SHOWNOACTIVATE, ShowWindow,
    };
    for handle in cx.windows() {
        handle
            .update(cx, |_root, window, _cx| {
                let hwnd = window_hwnd(window);
                if hwnd.is_null() {
                    return;
                }
                // SAFETY: `hwnd` is this GPUI window. `IsWindowVisible` reads the
                // show state. `SW_SHOWNOACTIVATE` restores the overlay without
                // taking focus, which would break click-through.
                let visible = unsafe { IsWindowVisible(hwnd) } != 0;
                let command = if visible { SW_HIDE } else { SW_SHOWNOACTIVATE };
                unsafe {
                    let _shown = ShowWindow(hwnd, command);
                }
                crate::tray::note_user_visibility(!visible);
            })
            .ok();
    }
}

fn window_hwnd(window: &Window) -> windows_sys::Win32::Foundation::HWND {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return std::ptr::null_mut();
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return std::ptr::null_mut();
    };
    #[allow(clippy::cast_possible_truncation)]
    { handle.hwnd.get() as windows_sys::Win32::Foundation::HWND }
}

fn hold_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn press(click: &Click) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
    let click = Rc::clone(click);
    move |event, window, app| click(event, window, app)
}
