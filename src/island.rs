//! Geometry and policy for the pill. No window, no thread, no I/O.
//!
//! The view asks this module at each decision and then paints the answer:
//!
//! 1. [`stage_frame`] places the fixed 420 by 360 stage on the primary display.
//! 2. [`hover_at`] classifies a window-local pointer. A line uses the full
//!    width and an 8px band; a capsule or card uses the 220 by 80 band.
//!    [`over_card`] is the larger [`CARD_WIDTH`] by [`CARD_HEIGHT`] (420 by 288)
//!    hit, used only by the card-idle timer.
//! 3. [`after_hover`] and [`after_body_click`] name the next [`IslandState`].
//!    A card ignores hover. A line ignores clicks. The view adds the dwell
//!    (`DWELL_MS`) and the leave delay (`HOVER_LEAVE_MS`) around those answers.
//! 4. [`resting`] plus a lerp in the view turns a state pair into a [`Morph`].
//!    Anything involving [`IslandState::Line`] lasts [`TUCK_MS`]; capsule and
//!    card last [`MORPH_MS`]. `detail` stays 0 while the card is closed.
//! 5. [`wheel_of`] then [`step_tab`] swaps [`Tab::Media`] and [`Tab::Perf`].
//!    A vertical wheel without Alt is volume, decided in `volume`.
//! 6. [`needs_topmost_raise`] and [`wants_exclusive_hide`] plus [`HideLatch`]
//!    are the tray thread's whole z-order policy.
//!
//! Sizes are logical pixels. Integer frame fields stay exact; the morph uses
//! `f32` because it is interpolated.
//!
//! [`marquee_alphas`] is the capsule line's edge fade. The view passes each
//! glyph's real advance, in ems, and paints that alpha. A dark strip is not a fade.

/// Screen rectangle for the overlay window, in logical pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IslandFrame {
    /// Left edge, from the display origin.
    pub x: i32,
    /// Top edge, from the display origin.
    pub y: i32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

/// Fixed overlay window width. The pill morphs inside it.
pub const STAGE_WIDTH: u32 = 420;

/// Fixed overlay window height.
pub const STAGE_HEIGHT: u32 = 360;

/// Gap between the top of the display and the overlay window.
///
/// The line is the top of the window, so this stays 0. A positive inset paints
/// the line below the screen edge.
pub const STAGE_TOP: u32 = 0;

/// Collapsed pill width.
pub const PILL_WIDTH: f32 = 220.0;

/// Width of the tucked line. It spans the stage.
pub const LINE_WIDTH: f32 = 420.0;

/// Height of the tucked line.
pub const LINE_HEIGHT: f32 = 1.5;

/// Corner radius of the tucked line.
pub const LINE_RADIUS: f32 = 2.0;

/// How far below the tucked line the pointer still counts as hovering it.
///
/// The line is 1.5px tall. A deeper band opens the capsule before the pointer
/// reaches the line, around where the capsule itself would sit.
pub const LINE_HOVER_DEPTH: f32 = 8.0;

/// How far below the window's top edge the pointer still counts as hovering the capsule.
pub const HOVER_DEPTH: f32 = 80.0;

/// Half the capsule width, measured from the stage center.
pub const HOVER_HALF_WIDTH: f32 = PILL_WIDTH / 2.0;

/// How long a pointer must stay outside the hover zone before the capsule tucks.
pub const HOVER_LEAVE_MS: u64 = 100;

/// How long the pointer must stay off the card before it folds back into the capsule.
pub const CARD_IDLE_MS: u64 = 2_000;

/// Duration of the line and capsule exchange.
pub const TUCK_MS: u64 = 280;

/// The three presentations of the island. The card is sticky; the capsule is not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IslandState {
    /// A 4px line parked on the top edge.
    Line,
    /// The hover capsule.
    Capsule,
    /// The expanded card. Leaving the hover zone does not leave this state.
    Card,
}

/// Where the pointer is relative to the invisible hover rectangle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Hover {
    /// Inside the rectangle.
    Inside,
    /// Outside it.
    Outside,
}

/// Classifies a window-local pointer against the shape that is actually showing.
///
/// A line is the full stage width and only `LINE_HOVER_DEPTH` tall, so the
/// capsule does not open until the pointer is on the line. A capsule uses the
/// wider, deeper band. Coordinates outside the stage are outside.
#[must_use]
pub fn hover_at(state: IslandState, x: f32, y: f32) -> Hover {
    #[allow(clippy::cast_precision_loss)]
    let center = (STAGE_WIDTH as f32) / 2.0;
    let (half_width, depth) = match state {
        IslandState::Line => (LINE_WIDTH / 2.0, LINE_HOVER_DEPTH),
        IslandState::Capsule | IslandState::Card => (HOVER_HALF_WIDTH, HOVER_DEPTH),
    };
    let inside =
        (0.0..=depth).contains(&y) && x >= center - half_width && x <= center + half_width;
    if inside { Hover::Inside } else { Hover::Outside }
}

/// Whether a window-local pointer is inside the card, which is larger than the hover band.
#[must_use]
pub fn over_card(x: f32, y: f32) -> bool {
    #[allow(clippy::cast_precision_loss)]
    let center = (STAGE_WIDTH as f32) / 2.0;
    (0.0..=CARD_HEIGHT).contains(&y)
        && x >= center - CARD_WIDTH / 2.0
        && x <= center + CARD_WIDTH / 2.0
}

/// The next state after the pointer enters or leaves the hover zone.
///
/// A card ignores the pointer. A line opens into a capsule, and a capsule tucks
/// back into a line.
#[must_use]
pub const fn after_hover(state: IslandState, hover: Hover) -> IslandState {
    match (state, hover) {
        (IslandState::Card, _) => IslandState::Card,
        (IslandState::Line, Hover::Inside) => IslandState::Capsule,
        (IslandState::Capsule, Hover::Outside) => IslandState::Line,
        (state, _) => state,
    }
}

/// The next state after a click on the island body.
#[must_use]
pub const fn after_body_click(state: IslandState) -> IslandState {
    match state {
        IslandState::Capsule => IslandState::Card,
        IslandState::Card => IslandState::Capsule,
        IslandState::Line => IslandState::Line,
    }
}

/// Pages the dock cycles through. Order is the wheel order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tab {
    /// Now playing.
    Media,
    /// CPU, memory, and network. Sampled in the background.
    Perf,
}

/// How far the wheel moved, and whether it should change the tab.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Wheel {
    /// Vertical wheel. On Media this is volume, not a tab change.
    Vertical {
        /// Positive is away from the user (next / volume up).
        up: bool,
    },
    /// Horizontal wheel, or vertical wheel with Alt held.
    Tab {
        /// Positive is the next tab.
        next: bool,
    },
}

/// One step around the tab ring. Media and performance swap places.
#[must_use]
pub const fn step_tab(tab: Tab, next: bool) -> Tab {
    let _ = next;
    match tab {
        Tab::Media => Tab::Perf,
        Tab::Perf => Tab::Media,
    }
}

/// How long a page slide takes, in milliseconds.
pub const PAGE_SLIDE_MS: u64 = 180;

/// Classifies a wheel delta. Horizontal movement wins over vertical.
/// `alt` turns a vertical wheel into a tab change.
#[must_use]
pub fn wheel_of(delta_x: f32, delta_y: f32, alt: bool) -> Option<Wheel> {
    let horizontal = delta_x.abs() > delta_y.abs() && delta_x.abs() >= 0.5;
    let vertical = delta_y.abs() >= 0.5;
    if horizontal {
        Some(Wheel::Tab { next: delta_x > 0.0 })
    } else if vertical && alt {
        Some(Wheel::Tab { next: delta_y > 0.0 })
    } else if vertical {
        Some(Wheel::Vertical { up: delta_y > 0.0 })
    } else {
        None
    }
}

/// Capsule width for a title, clamped to 200..=260 logical pixels.
#[must_use]
pub fn capsule_width(title: &str) -> f32 {
    let chars = title.chars().count().min(32);
    #[allow(clippy::cast_precision_loss)]
    let width = 96.0 + (chars as f32) * 8.0;
    width.clamp(CAPSULE_MIN, CAPSULE_MAX)
}

/// Whether a dwell timer that started at `entered_ms` has matured by `now_ms`.
#[must_use]
pub const fn dwell_ready(entered_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(entered_ms) >= 150
}

/// Master volume after one wheel step, in percent, clamped to 0..=100.
#[must_use]
pub const fn stepped_volume(percent: u8, up: bool) -> u8 {
    if up {
        let raised = percent.saturating_add(VOLUME_STEP);
        if raised > 100 { 100 } else { raised }
    } else {
        percent.saturating_sub(VOLUME_STEP)
    }
}

/// Percent added or removed by one volume wheel step.
pub const VOLUME_STEP: u8 = 2;

/// How long the volume bar stays up, in milliseconds.
pub const VOLUME_HOLD_MS: u64 = 800;

/// How long the pointer must rest in the trigger before the capsule opens.
pub const DWELL_MS: u64 = 150;

/// Narrowest capsule.
pub const CAPSULE_MIN: f32 = 200.0;

/// Widest capsule.
pub const CAPSULE_MAX: f32 = 260.0;

/// One pinned app. Paths come from `launcher.json`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchApp {
    /// Label shown on the capsule and the grid.
    pub name: String,
    /// Executable path passed to `ShellExecuteW`.
    pub path: String,
}

/// At most six apps. Empty names and paths are dropped.
#[must_use]
pub fn parse_launcher(text: &str) -> Vec<LaunchApp> {
    let mut apps = Vec::new();
    for line in text.lines() {
        if apps.len() == 6 {
            break;
        }
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, path)) = line.split_once('|') else {
            continue;
        };
        let name = name.trim();
        let path = path.trim();
        if name.is_empty() || path.is_empty() {
            continue;
        }
        apps.push(LaunchApp { name: name.to_owned(), path: path.to_owned() });
    }
    apps
}

/// Built-in pins written when `launcher.json` is missing.
pub const DEFAULT_LAUNCHER: &str = "\
记事本|notepad.exe
计算器|calc.exe
终端|wt.exe
";

/// How long an exclusive-fullscreen reading must hold before the dock hides.
///
/// A foreground switch flashes through intermediate windows. Hiding on the
/// first sample would flicker the dock.
pub const FULLSCREEN_DEBOUNCE_MS: u64 = 300;

/// What one z-order sample says about the dock window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LayerSample {
    /// `WS_EX_TOPMOST` is set on the dock.
    pub topmost: bool,
    /// `GetWindow(GW_HWNDPREV)` found a visible window above the dock.
    pub covered: bool,
    /// The foreground window is exclusive fullscreen (`QUNS_RUNNING_D3D_FULL_SCREEN`).
    /// Borderless fullscreen (browser F11, video players) is `false`.
    pub exclusive: bool,
}

/// Whether this sample should call `SetWindowPos(HWND_TOPMOST)`.
///
/// Already-topmost and uncovered windows are left alone. Repeating the call
/// reorders the topmost band and flickers.
#[must_use]
pub const fn needs_topmost_raise(sample: LayerSample) -> bool {
    !sample.topmost || sample.covered
}

/// Hide only for exclusive fullscreen. Borderless stays visible: a corner dock
/// does not cover a video, and hiding it looks like a bug.
#[must_use]
pub const fn wants_exclusive_hide(sample: LayerSample) -> bool {
    sample.exclusive
}

/// Card padding on each side of the media page.
const SEEK_PAD: f32 = 20.0;
/// Cover art width on the media page.
const SEEK_ART: f32 = 96.0;
/// Gap between the cover and the seek column.
const SEEK_GAP: f32 = 16.0;

/// Pixel width of the seek column: card minus padding, cover, and gap.
#[must_use]
pub fn seek_column_px() -> f32 {
    let track = CARD_WIDTH - SEEK_PAD * 2.0 - SEEK_ART - SEEK_GAP;
    track.max(0.0)
}

/// White scrub thumb, in logical pixels. Hidden when the session cannot seek.
pub const SEEK_THUMB: f32 = 8.0;

/// Session button, card-local. The painted mark owns the click.
pub const SESSION_MARK_X: f32 = 374.0;
/// Session button top. The media page starts at [`SEEK_PAD`].
pub const SESSION_MARK_Y: f32 = SEEK_PAD;
/// Session button size.
pub const SESSION_MARK_W: f32 = 28.0;
/// Session button size.
pub const SESSION_MARK_H: f32 = 22.0;

/// Card-local x of the seek column. The card is centered on the stage.
#[must_use]
pub fn seek_origin_x() -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let card_left = (STAGE_WIDTH as f32 - CARD_WIDTH) / 2.0;
    card_left + SEEK_PAD + SEEK_ART + SEEK_GAP
}

/// Fraction of the track under a stage-local pointer. Clamped to `0.0..=1.0`.
#[must_use]
pub fn seek_fraction(stage_x: f32) -> f32 {
    let width = seek_column_px();
    if width <= f32::EPSILON {
        return 0.0;
    }
    let fraction = (stage_x - seek_origin_x()) / width;
    if fraction.is_finite() { fraction.clamp(0.0, 1.0) } else { 0.0 }
}

/// The pointer is over the progress column, and the session can seek.
///
/// `stage_y` is unused: the painted bar owns the vertical hit. This only
/// answers whether a window-local x is inside the column.
#[must_use]
pub fn over_seek(stage_x: f32, stage_y: f32, seekable: bool) -> bool {
    let _ = stage_y;
    if !seekable {
        return false;
    }
    let origin = seek_origin_x();
    (origin..=origin + seek_column_px()).contains(&stage_x)
}

/// Opacity of a transport button the session will not accept.
pub const REFUSED_OPACITY: f32 = 0.4;

/// Pixel width of the filled part of the seek track.
///
/// The track is the column left beside the cover inside the card. A fraction
/// of the parent width does not resolve here: the column's width is content
/// sized, so the fill has to be an absolute length or it paints at zero and
/// the old fixed 220px fill runs past the card on a long track.
#[must_use]
pub fn seek_fill_px(progress: f32) -> f32 {
    let progress = if progress.is_finite() { progress.clamp(0.0, 1.0) } else { 0.0 };
    seek_column_px() * progress
}

/// Debounced hide flag.
///
/// `pending_since` is the first millisecond the raw reading asked to hide.
/// `None` means the raw reading is "show". `hidden` is what the window does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HideLatch {
    /// First sample that asked to hide, if the latch is not yet committed.
    pub pending_since: Option<u64>,
    /// The dock is hidden for exclusive fullscreen.
    pub hidden: bool,
}

impl HideLatch {
    /// Visible, with no pending hide.
    #[must_use]
    pub const fn visible() -> Self {
        Self { pending_since: None, hidden: false }
    }

    /// Advance the latch. Returns whether the dock's show state must change.
    #[must_use]
    pub const fn advance(self, hide: bool, now_ms: u64) -> (Self, bool) {
        if !hide {
            let changed = self.hidden;
            return (Self { pending_since: None, hidden: false }, changed);
        }
        if self.hidden {
            return (self, false);
        }
        let since = match self.pending_since {
            Some(since) => since,
            None => now_ms,
        };
        if now_ms.saturating_sub(since) >= FULLSCREEN_DEBOUNCE_MS {
            return (Self { pending_since: None, hidden: true }, true);
        }
        (Self { pending_since: Some(since), hidden: false }, false)
    }
}

/// Collapsed pill height.
pub const PILL_HEIGHT: f32 = 56.0;

/// Expanded card width.
pub const CARD_WIDTH: f32 = 420.0;

/// Expanded card height. Media and performance share it, so a page change
/// never resizes the shell.
pub const CARD_HEIGHT: f32 = 288.0;

/// Expanded card corner radius.
pub const CARD_RADIUS: f32 = 24.0;

/// Shadow blur at rest.
pub const SHADOW_REST: f32 = 12.0;

/// Shadow blur when the card is open.
pub const SHADOW_RAISED: f32 = 40.0;

/// Morph duration in milliseconds. The easing curve is applied by the view.
pub const MORPH_MS: u64 = 520;

/// Demo tracks cycled by the previous and next controls.
pub const TRACKS: [&str; 3] = ["Midnight Drive", "Glass House", "North Shore"];

/// Width, height, radius, shadow blur, and detail opacity at one morph position.
///
/// `progress` is `0.0` for the pill and `1.0` for the card. Values outside that
/// range are clamped.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Morph {
    /// Interpolated width.
    pub width: f32,
    /// Interpolated height.
    pub height: f32,
    /// Corner radius. The pill uses half its height.
    pub radius: f32,
    /// Shadow blur radius.
    pub shadow: f32,
    /// Opacity of the expanded controls. The pill content uses `1.0 - detail`.
    pub detail: f32,
}

/// The resting shape of one state. `Card` is fully open; the others are closed.
#[must_use]
pub fn resting(state: IslandState) -> Morph {
    match state {
        IslandState::Line => line_morph(0.0),
        IslandState::Capsule => morph(0.0),
        IslandState::Card => morph(1.0),
    }
}

/// Line at `0.0`, capsule at `1.0`. The view supplies an eased progress.
///
/// Width moves from the full stage down to the capsule. Height grows down from
/// the top edge. Detail stays at `0.0` because this exchange never opens the card.
#[must_use]
pub fn line_morph(progress: f32) -> Morph {
    let progress = progress.clamp(0.0, 1.0);
    Morph {
        width: LINE_WIDTH + (PILL_WIDTH - LINE_WIDTH) * progress,
        height: LINE_HEIGHT + (PILL_HEIGHT - LINE_HEIGHT) * progress,
        radius: LINE_RADIUS + (PILL_HEIGHT / 2.0 - LINE_RADIUS) * progress,
        shadow: SHADOW_REST * progress,
        detail: 0.0,
    }
}

/// Linear interpolation used by the morph. The view supplies an eased progress.
#[must_use]
pub fn morph(progress: f32) -> Morph {
    let progress = progress.clamp(0.0, 1.0);
    let width = PILL_WIDTH + (CARD_WIDTH - PILL_WIDTH) * progress;
    let height = PILL_HEIGHT + (CARD_HEIGHT - PILL_HEIGHT) * progress;
    let radius = (PILL_HEIGHT / 2.0) + (CARD_RADIUS - PILL_HEIGHT / 2.0) * progress;
    let shadow = SHADOW_REST + (SHADOW_RAISED - SHADOW_REST) * progress;
    Morph {
        width,
        height,
        radius,
        shadow,
        detail: progress,
    }
}

/// Top-center frame of the fixed overlay on a display of `display_width`.
///
/// A zero-width display returns an empty frame.
#[must_use]
pub const fn stage_frame(display_width: u32) -> IslandFrame {
    if display_width == 0 {
        return IslandFrame {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
    }
    let x = (display_width as i64 - STAGE_WIDTH as i64) / 2;
    #[allow(clippy::cast_possible_truncation)]
    let x = x as i32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let y = STAGE_TOP as i32;
    IslandFrame {
        x,
        y,
        width: STAGE_WIDTH,
        height: STAGE_HEIGHT,
    }
}

/// How many pixels a marquee edge takes to go from clear to solid.
/// One point six ems at the 13px capsule face.
const MARQUEE_FADE_PX: f32 = 1.6 * 13.0;

/// Per-glyph opacity for one frame, each in `0.0..=1.0`.
///
/// `positions` is each glyph's shaped `position.x`, in pixels, from one layout
/// of the whole line. `advances` is the distance from that glyph to the next,
/// or to the shaped line width for the last glyph. `offset_px` is how far the
/// line has moved left this frame. `visible_px` is the content box.
///
/// A line that fits the box is solid. A longer line fades wherever a glyph's
/// center sits inside either edge band. The center is `position.x + advance /
/// 2 - offset_px`: the same x the shaper gave the glyph, minus the scrolled
/// pixels. The offset and the glyph's index never color a glyph on their own.
/// Both bands stay on for the whole pass. There is no "near the start" or
/// "near the end" branch.
#[must_use]
pub fn marquee_alphas(positions: &[f32], advances: &[f32], offset_px: f32, visible_px: f32) -> Vec<f32> {
    let last = positions.last().copied().unwrap_or(0.0);
    let tail = advances.last().copied().unwrap_or(0.0);
    let width = if last.is_finite() && tail.is_finite() { last + tail } else { 0.0 };
    if positions.len() != advances.len() || !width.is_finite() || width <= visible_px {
        return vec![1.0; positions.len()];
    }
    positions
        .iter()
        .zip(advances)
        .map(|(position, advance)| {
            let position = if position.is_finite() { *position } else { 0.0 };
            let advance = if advance.is_finite() { advance.max(0.0) } else { 0.0 };
            let center = position + advance / 2.0 - offset_px;
            let from_left = (center / MARQUEE_FADE_PX).clamp(0.0, 1.0);
            let from_right = ((visible_px - center) / MARQUEE_FADE_PX).clamp(0.0, 1.0);
            from_left.min(from_right)
        })
        .collect()
}

/// Sum of finite, non-negative advances. A bad width contributes nothing.
#[must_use]
pub fn marquee_width(advances: &[f32]) -> f32 {
    advances
        .iter()
        .map(|advance| if advance.is_finite() { advance.max(0.0) } else { 0.0 })
        .sum()
}

/// The next demo track, wrapping after the last one.
#[must_use]
pub const fn next_track(index: usize) -> usize {
    if TRACKS.is_empty() {
        return 0;
    }
    (index.saturating_add(1)) % TRACKS.len()
}

/// The previous demo track, wrapping before the first one.
#[must_use]
pub const fn previous_track(index: usize) -> usize {
    if TRACKS.is_empty() {
        return 0;
    }
    if index == 0 {
        TRACKS.len() - 1
    } else {
        index - 1
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CARD_HEIGHT, CARD_RADIUS, CARD_WIDTH, HOVER_DEPTH, LINE_HOVER_DEPTH, Hover,
        IslandState, LINE_HEIGHT, LINE_WIDTH, PILL_HEIGHT, PILL_WIDTH,
        SHADOW_RAISED, SHADOW_REST, STAGE_HEIGHT, STAGE_WIDTH, TRACKS, Tab, Wheel,
        after_body_click, after_hover, capsule_width, dwell_ready, hover_at, marquee_alphas,
        marquee_width, over_card,
        line_morph, morph, next_track, previous_track, resting,
        stage_frame, step_tab, stepped_volume, wheel_of, DWELL_MS,
        CAPSULE_MAX, CAPSULE_MIN, parse_launcher,
        FULLSCREEN_DEBOUNCE_MS, HideLatch, LayerSample, needs_topmost_raise, wants_exclusive_hide,
        seek_fill_px, seek_column_px, seek_fraction, seek_origin_x, over_seek,
    };

    fn near(actual: f32, expected: f32) {
        let delta = (actual - expected).abs();
        assert!(delta < 0.01, "{actual} was not near {expected}");
    }

    #[test]
    fn morph_is_a_pill_at_zero_and_a_card_at_one() {
        let rest = morph(0.0);
        near(rest.width, PILL_WIDTH);
        near(rest.height, PILL_HEIGHT);
        near(rest.radius, PILL_HEIGHT / 2.0);
        near(rest.shadow, SHADOW_REST);
        near(rest.detail, 0.0);

        let open = morph(1.0);
        near(open.width, CARD_WIDTH);
        near(open.height, CARD_HEIGHT);
        near(open.radius, CARD_RADIUS);
        near(open.shadow, SHADOW_RAISED);
        near(open.detail, 1.0);
    }

    #[test]
    fn morph_midpoint_is_the_average_of_the_two_sizes() {
        let mid = morph(0.5);
        near(mid.width, 320.0);
        near(mid.height, (PILL_HEIGHT + CARD_HEIGHT) / 2.0);
        near(mid.shadow, 26.0);
    }

    #[test]
    fn hover_zone_is_the_top_center_band() {
        #[allow(clippy::cast_precision_loss)]
        let center = (STAGE_WIDTH as f32) / 2.0;
        assert_eq!(hover_at(IslandState::Capsule, center, 0.0), Hover::Inside);
        assert_eq!(hover_at(IslandState::Capsule, center, HOVER_DEPTH), Hover::Inside);
        assert_eq!(hover_at(IslandState::Capsule, center - 110.0, 40.0), Hover::Inside);
        assert_eq!(hover_at(IslandState::Capsule, center + 110.0, 40.0), Hover::Inside);
        assert_eq!(hover_at(IslandState::Capsule, center, HOVER_DEPTH + 1.0), Hover::Outside);
        assert_eq!(hover_at(IslandState::Capsule, center - 111.0, 10.0), Hover::Outside);
        assert_eq!(hover_at(IslandState::Capsule, -1.0, 10.0), Hover::Outside);
    }

    #[test]
    fn a_line_only_responds_along_itself() {
        #[allow(clippy::cast_precision_loss)]
        let center = (STAGE_WIDTH as f32) / 2.0;
        assert_eq!(hover_at(IslandState::Line, center, LINE_HOVER_DEPTH), Hover::Inside);
        assert_eq!(hover_at(IslandState::Line, 0.0, 2.0), Hover::Inside);
        assert_eq!(hover_at(IslandState::Line, LINE_WIDTH, 2.0), Hover::Inside);
        assert_eq!(hover_at(IslandState::Line, center, LINE_HOVER_DEPTH + 1.0), Hover::Outside);
        assert_eq!(hover_at(IslandState::Line, center, 40.0), Hover::Outside);
        assert_eq!(hover_at(IslandState::Line, -1.0, 2.0), Hover::Outside);
    }

    #[test]
    fn hover_opens_the_line_and_tucks_the_capsule_but_not_the_card() {
        assert_eq!(
            after_hover(IslandState::Line, Hover::Inside),
            IslandState::Capsule
        );
        assert_eq!(
            after_hover(IslandState::Capsule, Hover::Outside),
            IslandState::Line
        );
        assert_eq!(
            after_hover(IslandState::Card, Hover::Outside),
            IslandState::Card
        );
        assert_eq!(
            after_hover(IslandState::Line, Hover::Outside),
            IslandState::Line
        );
    }

    #[test]
    fn body_click_toggles_only_between_capsule_and_card() {
        assert_eq!(after_body_click(IslandState::Capsule), IslandState::Card);
        assert_eq!(after_body_click(IslandState::Card), IslandState::Capsule);
        assert_eq!(after_body_click(IslandState::Line), IslandState::Line);
    }

    #[test]
    fn the_card_hit_area_covers_the_panel_but_not_the_stage_corners() {
        #[allow(clippy::cast_precision_loss)]
        let center = (STAGE_WIDTH as f32) / 2.0;
        assert!(over_card(center, 10.0));
        assert!(over_card(center, CARD_HEIGHT));
        assert!(!over_card(center, CARD_HEIGHT + 1.0));
        assert!(!over_card(center - CARD_WIDTH / 2.0 - 1.0, 10.0));
    }

    #[test]
    fn wheel_steps_tabs_in_a_ring() {
        assert_eq!(step_tab(Tab::Media, true), Tab::Perf);
        assert_eq!(step_tab(Tab::Perf, true), Tab::Media);
        assert_eq!(step_tab(Tab::Media, false), Tab::Perf);
    }

    #[test]
    fn horizontal_wheel_changes_tab_and_alt_vertical_does_too() {
        assert_eq!(wheel_of(2.0, 0.2, false), Some(Wheel::Tab { next: true }));
        assert_eq!(wheel_of(0.0, -1.0, true), Some(Wheel::Tab { next: false }));
        assert_eq!(wheel_of(0.0, 1.0, false), Some(Wheel::Vertical { up: true }));
        assert_eq!(wheel_of(0.1, 0.1, false), None);
    }

    #[test]
    fn capsule_width_tracks_the_title_inside_the_band() {
        near(capsule_width("ab"), CAPSULE_MIN);
        near(capsule_width(&"曲".repeat(30)), CAPSULE_MAX);
    }

    #[test]
    fn marquee_fades_from_each_shaped_position() {
        // 24 glyphs at the old 0.55em stand-in, now in pixels at 13px.
        let advance = 0.55 * 13.0;
        let positions: Vec<f32> = (0..24).map(|index| index as f32 * advance).collect();
        let advances = vec![advance; 24];
        let still = marquee_alphas(&positions, &advances, 0.0, 10.0 * 13.0);
        // Center is half an advance, 3.575px, inside the 20.8px band.
        near(still.first().copied().unwrap_or(1.0), (advance / 2.0) / (1.6 * 13.0));
        near(still.get(4).copied().unwrap_or(0.0), 1.0);
        let tail = still.get(17).copied().unwrap_or(1.0);
        assert!(tail < 1.0, "right edge stayed solid");

        // Scrolled well past the left edge: that glyph stays clear, and a
        // glyph whose center is back inside the box is solid. No branch
        // turns the left band off because the pass is underway.
        let slid = marquee_alphas(&positions, &advances, 2.2 * 13.0, 10.0 * 13.0);
        near(slid.first().copied().unwrap_or(1.0), 0.0);
        near(slid.get(8).copied().unwrap_or(0.0), 1.0);

        // 0.8em before the line has fully left, the last glyph's center is
        // inside the left band. A branch that shut the band off at the end
        // of the pass would paint it solid or clear.
        let line = 24.0 * advance;
        let ending = marquee_alphas(&positions, &advances, line - 0.8 * 13.0 - advance / 2.0, 10.0 * 13.0);
        let tail = ending.last().copied().unwrap_or(0.0);
        near(tail, (0.8 * 13.0) / (1.6 * 13.0));
    }

    #[test]
    fn marquee_that_fits_is_solid() {
        let positions = [0.0, 7.0, 14.0, 21.0];
        let advances = [7.0, 7.0, 7.0, 7.0];
        let short = marquee_alphas(&positions, &advances, 0.0, 10.0 * 13.0);
        assert!(short.iter().all(|alpha| (*alpha - 1.0).abs() < 0.01));
    }

    #[test]
    fn marquee_uses_the_shaped_position_not_a_fixed_em() {
        // A wide glyph whose shaped origin is already past the fade band.
        let alphas = marquee_alphas(&[40.0], &[20.0], 0.0, 30.0);
        near(alphas.first().copied().unwrap_or(1.0), 0.0);
    }

    #[test]
    fn marquee_width_ignores_a_non_finite_advance() {
        near(marquee_width(&[1.0, f32::NAN, 0.5]), 1.5);
    }

    #[test]
    fn dwell_needs_a_full_rest_and_volume_stays_in_range() {
        assert!(!dwell_ready(0, DWELL_MS - 1));
        assert!(dwell_ready(10, 10 + DWELL_MS));
        assert_eq!(stepped_volume(99, true), 100);
        assert_eq!(stepped_volume(1, false), 0);
        assert_eq!(stepped_volume(40, true), 42);
    }

    #[test]
    fn launcher_keeps_six_named_apps_and_skips_blanks() {
        let text = "# comment\n\n记事本|notepad.exe\n |missing\nbad\n计算器|calc.exe\n";
        let apps = parse_launcher(text);
        assert_eq!(apps.len(), 2);
        assert_eq!(apps.first().map(|app| app.name.as_str()), Some("记事本"));
        assert_eq!(apps.get(1).map(|app| app.path.as_str()), Some("calc.exe"));
        assert!(parse_launcher(&"a|b\n".repeat(8)).len() <= 6);
    }

    fn sample(topmost: bool, covered: bool, exclusive: bool) -> LayerSample {
        LayerSample { topmost, covered, exclusive }
    }

    #[test]
    fn topmost_is_reasserted_only_when_the_dock_is_not_already_on_top() {
        assert!(!needs_topmost_raise(sample(true, false, false)));
        assert!(needs_topmost_raise(sample(false, false, false)));
        assert!(needs_topmost_raise(sample(true, true, false)));
    }

    #[test]
    fn only_exclusive_fullscreen_asks_the_dock_to_hide() {
        assert!(!wants_exclusive_hide(sample(true, false, false)));
        assert!(wants_exclusive_hide(sample(true, true, true)));
    }

    #[test]
    fn a_seek_press_is_only_the_column_and_only_when_seeking_is_offered() {
        let on_track = seek_origin_x() + 134.0;
        assert!(over_seek(on_track, 0.0, true));
        assert!(!over_seek(on_track, 0.0, false));
        assert!(!over_seek(seek_origin_x() - 1.0, 80.0, true));
        near(seek_fraction(on_track), 0.5);
        near(seek_fraction(seek_origin_x() - 20.0), 0.0);
        near(seek_fraction(seek_origin_x() + 400.0), 1.0);
    }

    #[test]
    fn seek_fill_stays_inside_the_column_beside_the_cover() {
        // Card 420, padding 20+20, art 96, gap 16. The column is 268px.
        near(seek_fill_px(0.0), 0.0);
        near(seek_column_px(), 268.0);
        near(seek_fill_px(1.0), 268.0);
        near(seek_fill_px(0.5), 134.0);
        near(seek_fill_px(1.4), 268.0);
        near(seek_fill_px(f32::NAN), 0.0);
        assert!(seek_fill_px(1.0) < CARD_WIDTH);
    }

    #[test]
    fn exclusive_hide_waits_out_the_debounce_then_shows_immediately() {
        let start = HideLatch::visible();
        let (pending, changed) = start.advance(true, 1_000);
        assert!(!changed);
        assert!(!pending.hidden);
        let (still, changed) = pending.advance(true, 1_000 + FULLSCREEN_DEBOUNCE_MS - 1);
        assert!(!changed);
        assert!(!still.hidden);
        let (hidden, changed) = still.advance(true, 1_000 + FULLSCREEN_DEBOUNCE_MS);
        assert!(changed);
        assert!(hidden.hidden);
        let (again, changed) = hidden.advance(true, 9_000);
        assert!(!changed);
        assert!(again.hidden);
        let (shown, changed) = again.advance(false, 9_001);
        assert!(changed);
        assert!(!shown.hidden);
        assert!(shown.pending_since.is_none());
    }

    #[test]
    fn line_morph_grows_from_the_line_into_the_capsule() {
        let line = line_morph(0.0);
        near(line.width, LINE_WIDTH);
        near(line.height, LINE_HEIGHT);
        near(line.detail, 0.0);

        let capsule = line_morph(1.0);
        near(capsule.width, PILL_WIDTH);
        near(capsule.height, PILL_HEIGHT);
        near(capsule.detail, 0.0);

        let card = resting(IslandState::Card);
        near(card.width, CARD_WIDTH);
        near(card.height, CARD_HEIGHT);
    }

    #[test]
    fn stage_is_centered_on_a_2560_display() {
        let frame = stage_frame(2560);
        assert_eq!(frame.x, 1070);
        assert_eq!(frame.y, 0);
        assert_eq!(frame.width, STAGE_WIDTH);
        assert_eq!(frame.height, STAGE_HEIGHT);
    }

    #[test]
    fn track_controls_wrap() {
        assert_eq!(next_track(2), 0);
        assert_eq!(previous_track(0), 2);
        assert_eq!(TRACKS.len(), 3);
    }

}