//! Real-time backdrop capture for the liquid glass.
//!
//! The wallpaper builder samples a file once per change; this module samples
//! the composited desktop instead. Windows Graphics Capture delivers a frame
//! only when the desktop changes, and the handler crops the stage plus the lens
//! margin before reading anything back.
//!
//! Everything except the capture handler is pure: crop math, downsampling,
//! luma smoothing, environment parsing, and the session state machine. The view
//! feeds events and runs the returned [`Action`]; it never touches the capture
//! thread.

use crate::wallpaper::REFRACTION_MARGIN;

/// How the glass gets its pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Live Windows Graphics Capture.
    Live,
    /// The static wallpaper crop (fallback; also the recording-visible mode).
    Static,
}

/// `RUSTEDGE_GLASS=static` turns the live path off for this launch.
pub(crate) fn mode_from_env(value: Option<&str>) -> Mode {
    if value.is_some_and(|text| text.trim().eq_ignore_ascii_case("static")) {
        Mode::Static
    } else {
        Mode::Live
    }
}

const LUMA_TAU_MS: f32 = 200.0;
const MAX_FAILURES: u32 = 3;
const RETRY_BACKOFF_MS: [u64; 3] = [1_000, 2_000, 4_000];
const CLEAN_RETRY_MS: u64 = 1_000;
const PREWARM_MS: u64 = 2_000;

/// One physical crop of the primary monitor, shared with the capture thread.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CropSpec {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
    /// Integer downsampling factor, 1..=3.
    pub factor: u32,
    /// Logical margin around the stage, `(left, top, right, bottom)`.
    pub pad: (f32, f32, f32, f32),
}

/// The stage clipped to the primary monitor, in physical pixels.
fn stage_on_screen(
    stage: (i32, i32, u32, u32),
    screen: (u32, u32),
) -> Option<(u32, u32, u32, u32)> {
    let left = i64::from(stage.0).max(0);
    let top = i64::from(stage.1).max(0);
    let right = (i64::from(stage.0) + i64::from(stage.2)).min(i64::from(screen.0));
    let bottom = (i64::from(stage.1) + i64::from(stage.3)).min(i64::from(screen.1));
    if right <= left || bottom <= top {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to the u32 screen bounds"
    )]
    Some((left as u32, top as u32, (right - left) as u32, (bottom - top) as u32))
}

/// Physical crop for one stage rect, plus the logical margin the lens uses.
///
/// Width and height are rounded down to a multiple of the downsampling factor
/// so the output has no leftover pixels. Rounding trims the right/bottom edge;
/// on a screen-clipped side that shaves at most `factor - 1` physical pixels of
/// an edge the lens cannot sample past anyway.
pub(crate) fn crop_spec(
    stage: (i32, i32, u32, u32),
    scale: f32,
    screen: (u32, u32),
) -> Option<CropSpec> {
    if screen.0 == 0 || screen.1 == 0 || !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "scale is a small positive DPI factor"
    )]
    let factor = (scale.round() as u32).clamp(1, 3);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "40 logical pixels times a small DPI factor"
    )]
    let margin = (REFRACTION_MARGIN * scale).round().max(0.0) as u32;
    let (sx, sy, sw, sh) = stage_on_screen(stage, screen)?;
    let left = sx.saturating_sub(margin);
    let top = sy.saturating_sub(margin);
    let right = (sx + sw + margin).min(screen.0);
    let bottom = (sy + sh + margin).min(screen.1);
    let right = left + ((right - left) / factor) * factor;
    let bottom = top + ((bottom - top) / factor) * factor;
    if right <= left || bottom <= top {
        return None;
    }
    #[allow(clippy::cast_precision_loss, reason = "pixel counts are far under 2^24")]
    let pad = (
        (sx - left) as f32 / scale,
        (sy - top) as f32 / scale,
        right.saturating_sub(sx + sw) as f32 / scale,
        bottom.saturating_sub(sy + sh) as f32 / scale,
    );
    Some(CropSpec { left, top, right, bottom, factor, pad })
}

/// Box-averages `factor x factor` blocks of an RGBA frame.
///
/// `pitch` is the source row stride in bytes; it may exceed `width * 4` because
/// D3D staging textures pad rows. Leftover rows/columns are dropped.
fn downsample(
    src: &[u8],
    width: u32,
    height: u32,
    pitch: u32,
    factor: u32,
) -> Option<(Vec<u8>, u32, u32)> {
    if width == 0 || height == 0 || factor == 0 {
        return None;
    }
    let out_width = width / factor;
    let out_height = height / factor;
    if out_width == 0 || out_height == 0 {
        return None;
    }
    let block = factor as usize;
    let row_bytes = width as usize * 4;
    let pitch = pitch as usize;
    if pitch < row_bytes || src.len() < pitch * height as usize {
        return None;
    }
    let mut out = vec![0_u8; out_width as usize * out_height as usize * 4];
    let block_area = block * block;
    for oy in 0..out_height as usize {
        for ox in 0..out_width as usize {
            let mut sum = [0_usize; 4];
            for by in 0..block {
                let start = (oy * block + by) * pitch + ox * block * 4;
                let pixels = src.get(start..start + block * 4)?;
                for pixel in pixels.chunks_exact(4) {
                    for (channel, value) in sum.iter_mut().zip(pixel) {
                        *channel += usize::from(*value);
                    }
                }
            }
            let offset = (oy * out_width as usize + ox) * 4;
            for (channel, value) in sum.iter().zip(&mut out[offset..offset + 4]) {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "mean of u8 stays in u8"
                )]
                let mean = (*channel / block_area) as u8;
                *value = mean;
            }
        }
    }
    Some((out, out_width, out_height))
}

/// Time-based exponential smoothing for the backdrop lightness.
pub(crate) struct LumaSmoother {
    value: Option<f32>,
}

impl LumaSmoother {
    pub(crate) const fn new() -> Self {
        Self { value: None }
    }

    /// Feeds one measured luma and returns the smoothed value.
    pub(crate) fn push(&mut self, target: f32, dt_ms: f32) -> f32 {
        let target = if target.is_finite() { target.clamp(0.0, 1.0) } else { 0.0 };
        let next = match self.value {
            None => target,
            Some(current) => {
                let dt = dt_ms.clamp(0.0, 1_000.0);
                let alpha = 1.0 - (-dt / LUMA_TAU_MS).exp();
                current + (target - current) * alpha
            }
        };
        let next = next.clamp(0.0, 1.0);
        self.value = Some(next);
        next
    }
}

/// Where the capture session is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Off,
    Idle,
    Starting,
    Running,
    Stopping,
}

/// What the view should do on the executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    None,
    Start,
    Stop,
}

/// How a failed start should be treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartFault {
    /// WGC or WDA is not available: fall back for this launch.
    Unsupported,
    /// The border cannot be disabled: a full-screen capture border is worse.
    Border,
    /// Transient or unknown: count against the failure budget.
    Other,
}

/// One input to the session machine.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Event {
    SetWanted(bool),
    Prewarm,
    StartOk,
    StartErr(StartFault),
    Frame,
    SessionEnded { error: bool },
    PrewarmDeadline,
    RetryDue,
    StopDone,
}

/// Capture lifecycle without any threading or Windows types.
pub(crate) struct Machine {
    phase: Phase,
    wanted: bool,
    /// Desired visibility to apply when the current transition finishes.
    after: Option<bool>,
    failures: u32,
    retry_at: Option<u64>,
    prewarm: bool,
    prewarm_deadline: Option<u64>,
}

impl Machine {
    pub(crate) const fn new() -> Self {
        Self {
            phase: Phase::Idle,
            wanted: false,
            after: None,
            failures: 0,
            retry_at: None,
            prewarm: false,
            prewarm_deadline: None,
        }
    }

    pub(crate) const fn phase(&self) -> Phase {
        self.phase
    }

    pub(crate) const fn is_live(&self) -> bool {
        !matches!(self.phase, Phase::Off)
    }

    #[cfg(test)]
    pub(crate) const fn failures(&self) -> u32 {
        self.failures
    }

    pub(crate) const fn prewarm_deadline(&self) -> Option<u64> {
        self.prewarm_deadline
    }

    pub(crate) fn rearm(&mut self) {
        self.phase = Phase::Idle;
        self.wanted = false;
        self.after = None;
        self.failures = 0;
        self.retry_at = None;
        self.prewarm = false;
        self.prewarm_deadline = None;
    }

    pub(crate) fn force_off(&mut self) {
        self.phase = Phase::Off;
        self.after = None;
        self.prewarm = false;
        self.prewarm_deadline = None;
    }

    pub(crate) fn handle(&mut self, event: Event, now_ms: u64) -> Action {
        match event {
            Event::SetWanted(on) => {
                self.wanted = on;
                if on {
                    // An expand supersedes the warm cache: the session should
                    // keep running, not stop after its first frame.
                    self.prewarm = false;
                    self.prewarm_deadline = None;
                }
                match self.phase {
                    Phase::Off => Action::None,
                    Phase::Idle => {
                        if on {
                            self.start(now_ms)
                        } else {
                            Action::None
                        }
                    }
                    Phase::Starting | Phase::Stopping => {
                        self.after = Some(on);
                        Action::None
                    }
                    Phase::Running => {
                        if on {
                            Action::None
                        } else {
                            self.phase = Phase::Stopping;
                            self.after = Some(false);
                            Action::Stop
                        }
                    }
                }
            }
            Event::Prewarm => {
                self.prewarm = true;
                if self.phase != Phase::Idle || self.wanted {
                    return Action::None;
                }
                let action = self.start(now_ms);
                if action == Action::Start {
                    self.prewarm_deadline = Some(now_ms.saturating_add(PREWARM_MS));
                }
                action
            }
            Event::StartOk => {
                if self.phase != Phase::Starting {
                    return Action::None;
                }
                self.phase = Phase::Running;
                if !self.prewarm {
                    self.prewarm_deadline = None;
                }
                let want = self.after.take().unwrap_or(self.wanted);
                if !want && !self.prewarm {
                    self.phase = Phase::Stopping;
                    Action::Stop
                } else {
                    Action::None
                }
            }
            Event::StartErr(fault) => {
                if matches!(fault, StartFault::Unsupported | StartFault::Border) {
                    self.force_off();
                    return Action::None;
                }
                self.failures = self.failures.saturating_add(1);
                self.phase = Phase::Idle;
                if self.failures >= MAX_FAILURES {
                    self.force_off();
                    return Action::None;
                }
                let wait =
                    RETRY_BACKOFF_MS[(self.failures as usize - 1).min(RETRY_BACKOFF_MS.len() - 1)];
                self.retry_at = Some(now_ms.saturating_add(wait));
                Action::None
            }
            Event::Frame => {
                self.failures = 0;
                self.retry_at = None;
                let was_warming = self.prewarm;
                self.prewarm = false;
                self.prewarm_deadline = None;
                if was_warming && self.phase == Phase::Running && !self.wanted {
                    self.phase = Phase::Stopping;
                    self.after = Some(false);
                    Action::Stop
                } else {
                    Action::None
                }
            }
            Event::SessionEnded { error } => {
                if !matches!(self.phase, Phase::Running | Phase::Starting) {
                    return Action::None;
                }
                self.phase = Phase::Idle;
                self.prewarm_deadline = None;
                if error {
                    self.failures = self.failures.saturating_add(1);
                    if self.failures >= MAX_FAILURES {
                        self.force_off();
                        return Action::None;
                    }
                    let wait =
                        RETRY_BACKOFF_MS
                            [(self.failures as usize - 1).min(RETRY_BACKOFF_MS.len() - 1)];
                    self.retry_at = Some(now_ms.saturating_add(wait));
                } else {
                    self.retry_at = Some(now_ms.saturating_add(CLEAN_RETRY_MS));
                }
                Action::None
            }
            Event::PrewarmDeadline => {
                if self.prewarm_deadline.is_none() {
                    return Action::None;
                }
                self.prewarm = false;
                self.prewarm_deadline = None;
                if self.phase == Phase::Running && !self.wanted {
                    self.phase = Phase::Stopping;
                    self.after = Some(false);
                    Action::Stop
                } else {
                    Action::None
                }
            }
            Event::RetryDue => {
                if self.phase != Phase::Idle || !(self.wanted || self.prewarm) {
                    return Action::None;
                }
                let action = self.start(now_ms);
                if action == Action::Start && self.prewarm && !self.wanted {
                    self.prewarm_deadline = Some(now_ms.saturating_add(PREWARM_MS));
                }
                action
            }
            Event::StopDone => {
                self.phase = Phase::Idle;
                if !self.prewarm {
                    self.prewarm_deadline = None;
                }
                let want = self.after.take().unwrap_or(self.wanted);
                if want {
                    self.start(now_ms)
                } else {
                    Action::None
                }
            }
        }
    }

    fn start(&mut self, now_ms: u64) -> Action {
        if self.phase != Phase::Idle || self.retry_at.is_some_and(|at| now_ms < at) {
            return Action::None;
        }
        self.phase = Phase::Starting;
        self.retry_at = None;
        Action::Start
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_static_is_case_and_space_insensitive() {
        assert_eq!(mode_from_env(Some("static")), Mode::Static);
        assert_eq!(mode_from_env(Some("  STATIC ")), Mode::Static);
        assert_eq!(mode_from_env(Some("live")), Mode::Live);
        assert_eq!(mode_from_env(None), Mode::Live);
    }

    #[test]
    fn crop_keeps_the_lens_margin() {
        let spec = crop_spec((100, 0, 420, 360), 1.0, (1920, 1080)).unwrap();
        assert_eq!((spec.left, spec.top, spec.right, spec.bottom), (60, 0, 560, 400));
        assert_eq!(spec.factor, 1);
        assert_eq!(spec.pad, (40.0, 0.0, 40.0, 40.0));
    }

    #[test]
    fn crop_clamps_at_screen_edges() {
        let left = crop_spec((0, 0, 420, 360), 1.0, (1920, 1080)).unwrap();
        assert_eq!(left.left, 0);
        assert_eq!(left.pad.0, 0.0);
        assert_eq!(left.pad.1, 0.0);

        assert!(crop_spec((2000, 0, 420, 360), 1.0, (1920, 1080)).is_none());
    }

    #[test]
    fn crop_is_a_whole_number_of_blocks() {
        let spec = crop_spec((90, 0, 1260, 1080), 3.0, (1920, 1080)).unwrap();
        assert_eq!(spec.factor, 3);
        assert_eq!((spec.right - spec.left) % 3, 0);
        assert_eq!((spec.bottom - spec.top) % 3, 0);
        assert_eq!(spec.pad, (30.0, 0.0, 40.0, 0.0));
    }

    #[test]
    fn downsample_averages_two_by_two() {
        let src = [
            255, 0, 0, 255, 0, 255, 0, 255,
            0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let (out, width, height) = downsample(&src, 2, 2, 8, 2).unwrap();
        assert_eq!((width, height), (1, 1));
        assert_eq!(out, vec![127, 127, 127, 255]);
    }

    #[test]
    fn downsample_strips_row_padding_at_one_to_one() {
        let src = [
            1, 2, 3, 4, 5, 6, 7, 8,
            99, 99, 99, 99,
        ];
        let (out, width, height) = downsample(&src, 2, 1, 12, 1).unwrap();
        assert_eq!((width, height), (2, 1));
        assert_eq!(out, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn downsample_three_by_three() {
        let src = vec![30_u8; 3 * 3 * 4];
        let (out, width, height) = downsample(&src, 3, 3, 12, 3).unwrap();
        assert_eq!((width, height), (1, 1));
        assert_eq!(out, vec![30, 30, 30, 30]);
    }

    #[test]
    fn downsample_rejects_a_block_larger_than_the_image() {
        assert!(downsample(&[0; 16], 2, 2, 8, 3).is_none());
    }

    #[test]
    fn luma_snaps_then_eases() {
        let mut smoother = LumaSmoother::new();
        assert!((smoother.push(0.8, 33.0) - 0.8).abs() < f32::EPSILON);
        let next = smoother.push(0.0, 33.0);
        assert!(next > 0.0 && next < 0.8);
        assert!(smoother.push(2.0, 1_000.0) <= 1.0);
    }

    fn running(machine: &mut Machine) {
        assert_eq!(machine.handle(Event::SetWanted(true), 0), Action::Start);
        assert_eq!(machine.handle(Event::StartOk, 0), Action::None);
    }

    fn fail_once(machine: &mut Machine, now: u64) {
        assert_eq!(machine.handle(Event::SetWanted(true), now), Action::Start);
        assert_eq!(machine.handle(Event::StartErr(StartFault::Other), now), Action::None);
        assert_eq!(machine.handle(Event::RetryDue, now + 10_000), Action::Start);
    }

    #[test]
    fn expand_starts_and_collapse_stops() {
        let mut machine = Machine::new();
        running(&mut machine);
        assert_eq!(machine.handle(Event::SetWanted(false), 0), Action::Stop);
        assert_eq!(machine.handle(Event::StopDone, 0), Action::None);
        assert_eq!(machine.phase(), Phase::Idle);
    }

    #[test]
    fn collapse_during_start_stops_after_ok() {
        let mut machine = Machine::new();
        assert_eq!(machine.handle(Event::SetWanted(true), 0), Action::Start);
        assert_eq!(machine.handle(Event::SetWanted(false), 0), Action::None);
        assert_eq!(machine.handle(Event::StartOk, 0), Action::Stop);
    }

    #[test]
    fn expand_during_stop_restarts_after_done() {
        let mut machine = Machine::new();
        running(&mut machine);
        assert_eq!(machine.handle(Event::SetWanted(false), 0), Action::Stop);
        assert_eq!(machine.handle(Event::SetWanted(true), 0), Action::None);
        assert_eq!(machine.handle(Event::StopDone, 0), Action::Start);
    }

    #[test]
    fn three_faults_turn_the_live_path_off() {
        let mut machine = Machine::new();
        fail_once(&mut machine, 0);
        assert_eq!(machine.handle(Event::StartErr(StartFault::Other), 1), Action::None);
        assert_eq!(machine.handle(Event::RetryDue, 20_000), Action::Start);
        assert_eq!(machine.handle(Event::StartErr(StartFault::Other), 20_000), Action::None);
        assert_eq!(machine.phase(), Phase::Off);
    }

    #[test]
    fn a_frame_clears_the_failure_budget() {
        let mut machine = Machine::new();
        fail_once(&mut machine, 0);
        assert_eq!(machine.handle(Event::StartOk, 1), Action::None);
        assert_eq!(machine.handle(Event::Frame, 1), Action::None);
        assert_eq!(machine.failures(), 0);
    }

    #[test]
    fn unsupported_and_border_faults_fall_back_immediately() {
        for fault in [StartFault::Unsupported, StartFault::Border] {
            let mut machine = Machine::new();
            assert_eq!(machine.handle(Event::SetWanted(true), 0), Action::Start);
            assert_eq!(machine.handle(Event::StartErr(fault), 0), Action::None);
            assert_eq!(machine.phase(), Phase::Off);
        }
    }

    #[test]
    fn clean_session_end_retries_without_charging() {
        let mut machine = Machine::new();
        running(&mut machine);
        assert_eq!(machine.handle(Event::SessionEnded { error: false }, 0), Action::None);
        assert_eq!(machine.failures(), 0);
        assert_eq!(machine.handle(Event::RetryDue, 999), Action::None);
        assert_eq!(machine.handle(Event::RetryDue, 1_000), Action::Start);
    }

    #[test]
    fn error_session_end_charges_once() {
        let mut machine = Machine::new();
        running(&mut machine);
        assert_eq!(machine.handle(Event::SessionEnded { error: true }, 0), Action::None);
        assert_eq!(machine.failures(), 1);
        assert_eq!(machine.handle(Event::RetryDue, 999), Action::None);
    }

    #[test]
    fn prewarm_caches_one_frame_then_stops() {
        let mut machine = Machine::new();
        assert_eq!(machine.handle(Event::Prewarm, 0), Action::Start);
        assert_eq!(machine.handle(Event::StartOk, 0), Action::None);
        assert_eq!(machine.handle(Event::Frame, 100), Action::Stop);
        assert_eq!(machine.handle(Event::StopDone, 100), Action::None);
    }

    #[test]
    fn prewarm_deadline_only_drops_the_marker() {
        let mut machine = Machine::new();
        assert_eq!(machine.handle(Event::Prewarm, 0), Action::Start);
        assert_eq!(machine.handle(Event::StartOk, 0), Action::None);
        assert_eq!(machine.handle(Event::PrewarmDeadline, 2_000), Action::Stop);
        assert_eq!(machine.failures(), 0);
    }

    #[test]
    fn hidden_means_collapsed() {
        let mut machine = Machine::new();
        running(&mut machine);
        assert_eq!(machine.handle(Event::SetWanted(false), 0), Action::Stop);
    }

    #[test]
    fn expand_during_prewarm_does_not_restart_after_collapse() {
        let mut machine = Machine::new();
        assert_eq!(machine.handle(Event::Prewarm, 0), Action::Start);
        assert_eq!(machine.handle(Event::StartOk, 0), Action::None);
        // The user expands before the warm frame arrives: the session simply
        // continues, and no stop-on-first-frame is pending any more.
        assert_eq!(machine.handle(Event::SetWanted(true), 10), Action::None);
        assert_eq!(machine.handle(Event::Frame, 20), Action::None);
        assert_eq!(machine.handle(Event::SetWanted(false), 30), Action::Stop);
        assert_eq!(machine.handle(Event::StopDone, 40), Action::None);
        // A stale prewarm marker must not start a session while collapsed.
        assert_eq!(machine.handle(Event::RetryDue, 10_000), Action::None);
        assert_eq!(machine.phase(), Phase::Idle);
    }
}
