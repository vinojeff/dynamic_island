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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use windows_capture::capture::{
    CaptureControl, Context, GraphicsCaptureApiError, GraphicsCaptureApiHandler,
};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE,
};

use crate::glass_optics::{GlassShape, Rect};
use crate::wallpaper::{self, GlassBackdrop, RawGlass, REFRACTION_MARGIN};

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
const CAPTURE_INTERVAL_MS: u64 = 33;

/// Backoff after the Nth consecutive fault, capped at the last step.
fn backoff_after(failures: u32) -> u64 {
    let index = usize::try_from(failures).unwrap_or(0).saturating_sub(1);
    RETRY_BACKOFF_MS
        .get(index.min(RETRY_BACKOFF_MS.len() - 1))
        .copied()
        .unwrap_or(CLEAN_RETRY_MS)
}

/// The physical crop plus the geometry the warp and lens need.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CropGeometry {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
    /// Integer downsampling factor, 1..=3.
    pub factor: u32,
    /// Crop rect in logical px relative to the window's top-left.
    pub crop: Rect,
    /// Image pixels per logical pixel: `scale / factor`.
    pub image_scale: f32,
    /// Unclipped stage width in logical pixels.
    pub stage_width: f32,
    /// Logical desktop size; the reference scales its intensity by it.
    pub page: (f32, f32),
}

/// One physical crop of the primary monitor, plus the shape to bake.
///
/// `shape` changes every morph tick while `geometry` only moves on a slide or
/// a DPI change; only a geometry change invalidates the last raw frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CropSpec {
    pub geometry: CropGeometry,
    pub shape: GlassShape,
    /// Whether the capture thread should bake a warp for this frame. False
    /// while a morph is still moving: a warp baked now would be painted as the
    /// plain crop anyway, so it would only burn a core the animation needs.
    pub warp: bool,
}

impl CropSpec {
    const EMPTY: Self = Self {
        geometry: CropGeometry {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
            factor: 1,
            crop: Rect::ZERO,
            image_scale: 1.0,
            stage_width: 0.0,
            page: (0.0, 0.0),
        },
        shape: GlassShape::ZERO,
        warp: false,
    };
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
    shape: GlassShape,
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
        reason = "128 logical pixels times a small DPI factor"
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
    let crop = wallpaper::crop_rect(left, top, right, bottom, (stage.0, stage.1), scale);
    #[allow(
        clippy::cast_precision_loss,
        reason = "pixel counts are far under f32's 24-bit mantissa"
    )]
    let image_scale = scale / factor as f32;
    #[allow(
        clippy::cast_precision_loss,
        reason = "pixel counts are far under f32's 24-bit mantissa"
    )]
    let stage_width = stage.2 as f32 / scale;
    #[allow(
        clippy::cast_precision_loss,
        reason = "screen pixel counts are far under f32's 24-bit mantissa"
    )]
    let page = (screen.0 as f32 / scale, screen.1 as f32 / scale);
    Some(CropSpec {
        geometry: CropGeometry {
            left,
            top,
            right,
            bottom,
            factor,
            crop,
            image_scale,
            stage_width,
            page,
        },
        shape,
        warp: true,
    })
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
            let target = out.get_mut(offset..offset + 4)?;
            for (channel, value) in sum.iter().zip(target) {
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
    /// Cursor capture could not be disabled: retry with the default.
    Cursor,
    /// Minimum update interval is unsupported: retry with the default.
    Interval,
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

    #[allow(clippy::too_many_lines)]
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
                        } else if self.prewarm {
                            // The prewarm session is warming the cache while the
                            // island is collapsed; the view feeds `wanted ==
                            // false` on every tick. Only the first frame (or the
                            // deadline) ends this session.
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
                    eprintln!("live glass: capture failed {MAX_FAILURES} times, falling back");
                    self.force_off();
                    return Action::None;
                }
                let wait = backoff_after(self.failures);
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
                        eprintln!(
                            "live glass: capture failed {MAX_FAILURES} times, falling back"
                        );
                        self.force_off();
                        return Action::None;
                    }
                    let wait = backoff_after(self.failures);
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

/// One captured frame: the raw crop (kept for shape-only re-warps) and the
/// backdrop the capture thread already warped. Named `LiveFrame` to stay
/// clear of `windows_capture::frame::Frame`.
pub(crate) struct LiveFrame {
    /// The unrefracted crop.
    pub raw: Arc<RawGlass>,
    /// The warped backdrop, or `None` when the warp failed.
    pub warped: Option<Arc<GlassBackdrop>>,
    /// Monotonic per-session frame counter.
    pub generation: u64,
    /// Raw lightness; `take_frame` replaces it with the smoothed value.
    pub luma: f32,
}

/// The newest captured frame, overwritten by the capture thread.
#[derive(Default)]
struct Sink {
    latest: Mutex<Option<LiveFrame>>,
    next_generation: AtomicU64,
}

impl Sink {
    fn next_generation(&self) -> u64 {
        self.next_generation.fetch_add(1, Ordering::Relaxed)
    }

    fn publish(&self, frame: LiveFrame) {
        let mut slot = match self.latest.lock() {
            Ok(slot) => slot,
            Err(poison) => PoisonError::into_inner(poison),
        };
        *slot = Some(frame);
    }

    fn take(&self) -> Option<LiveFrame> {
        match self.latest.lock() {
            Ok(mut slot) => slot.take(),
            Err(poison) => PoisonError::into_inner(poison).take(),
        }
    }
}

/// Frame-pipeline failure. Ends the session; the machine counts it.
#[derive(Debug)]
pub(crate) struct LiveError;

impl std::fmt::Display for LiveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("live glass frame failed")
    }
}

impl std::error::Error for LiveError {}

/// Values moved onto the capture thread at start.
pub(crate) struct HandlerFlags {
    spec: Arc<Mutex<CropSpec>>,
    sink: Arc<Sink>,
}

pub(crate) struct LiveHandler {
    spec: Arc<Mutex<CropSpec>>,
    sink: Arc<Sink>,
}

impl GraphicsCaptureApiHandler for LiveHandler {
    type Flags = HandlerFlags;
    type Error = LiveError;

    fn new(context: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let flags = context.flags;
        Ok(Self { spec: flags.spec, sink: flags.sink })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let spec = match self.spec.lock() {
            Ok(guard) => *guard,
            Err(poison) => *PoisonError::into_inner(poison),
        };
        if spec.geometry.right <= spec.geometry.left
            || spec.geometry.bottom <= spec.geometry.top
        {
            return Ok(());
        }
        if spec.geometry.right > frame.width() || spec.geometry.bottom > frame.height() {
            return Ok(());
        }
        let mut buffer = frame
            .buffer_crop(
                spec.geometry.left,
                spec.geometry.top,
                spec.geometry.right,
                spec.geometry.bottom,
            )
            .map_err(|_| LiveError)?;
        let (width, height, pitch) = (buffer.width(), buffer.height(), buffer.row_pitch());
        let (mut rgba, out_width, out_height) =
            downsample(buffer.as_raw_buffer(), width, height, pitch, spec.geometry.factor)
                .ok_or(LiveError)?;
        wallpaper::saturate(&mut rgba, wallpaper::SATURATION);
        let luma = wallpaper::mean_luma(&rgba);
        let generation = self.sink.next_generation();
        let raw = Arc::new(RawGlass {
            rgba,
            width: usize::try_from(out_width).map_err(|_| LiveError)?,
            height: usize::try_from(out_height).map_err(|_| LiveError)?,
            crop: spec.geometry.crop,
            image_scale: spec.geometry.image_scale,
            stage_width: spec.geometry.stage_width,
            page: spec.geometry.page,
            luma,
            generation,
            flat: OnceLock::new(),
        });
        // The capture thread warps with the newest shape; the view re-warps
        // the kept raw when only the shape moved. A moving morph is left
        // unwarped: the paint side would show the plain crop anyway.
        let warped = if spec.warp {
            raw.warp(spec.shape, None).map(Arc::new)
        } else {
            None
        };
        self.sink.publish(LiveFrame { raw, warped, generation, luma });
        Ok(())
    }
}

pub(crate) type Control = CaptureControl<LiveHandler, LiveError>;

/// Everything a start needs, cloned off the view before the blocking call.
pub(crate) struct StartRequest {
    spec: Arc<Mutex<CropSpec>>,
    sink: Arc<Sink>,
}

fn classify_start_error<E>(error: &GraphicsCaptureApiError<E>) -> StartFault {
    use windows_capture::graphics_capture_api::Error as CaptureError;
    let GraphicsCaptureApiError::GraphicsCaptureApiError(error) = error else {
        return StartFault::Other;
    };
    match error {
        CaptureError::CursorConfigUnsupported => StartFault::Cursor,
        CaptureError::BorderConfigUnsupported => StartFault::Border,
        CaptureError::MinimumUpdateIntervalUnsupported => StartFault::Interval,
        CaptureError::Unsupported => StartFault::Unsupported,
        _ => StartFault::Other,
    }
}

fn start_with(
    cursor: CursorCaptureSettings,
    interval: MinimumUpdateIntervalSettings,
    request: &StartRequest,
) -> Result<Control, StartFault> {
    let monitor = Monitor::primary().map_err(|_| StartFault::Other)?;
    let flags = HandlerFlags {
        spec: Arc::clone(&request.spec),
        sink: Arc::clone(&request.sink),
    };
    let settings = Settings::new(
        monitor,
        cursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        interval,
        DirtyRegionSettings::Default,
        ColorFormat::Rgba8,
        flags,
    );
    LiveHandler::start_free_threaded(settings).map_err(|error| classify_start_error(&error))
}

/// Starts a session, degrading optional settings the OS cannot honor.
///
/// `start_free_threaded` blocks until WinRT/D3D are up, so the caller runs this
/// on the background executor.
pub(crate) fn start_session(request: &StartRequest) -> Result<Control, StartFault> {
    if !GraphicsCaptureApi::is_supported().unwrap_or(false) {
        return Err(StartFault::Unsupported);
    }
    let mut cursor = CursorCaptureSettings::WithoutCursor;
    let mut interval =
        MinimumUpdateIntervalSettings::Custom(Duration::from_millis(CAPTURE_INTERVAL_MS));
    for _ in 0..4 {
        match start_with(cursor, interval, request) {
            Ok(control) => return Ok(control),
            Err(StartFault::Cursor) => cursor = CursorCaptureSettings::Default,
            Err(StartFault::Interval) => interval = MinimumUpdateIntervalSettings::Default,
            Err(fault) => return Err(fault),
        }
    }
    Err(StartFault::Other)
}

fn set_affinity(hwnd: HWND, excluded: bool) -> bool {
    let affinity = if excluded { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE };
    // SAFETY: `hwnd` came from the overlay window and stays alive for the
    // process; the call only changes capture affinity.
    unsafe { SetWindowDisplayAffinity(hwnd, affinity) != 0 }
}

/// View-side handle: owns the machine, the shared crop, and the control.
pub(crate) struct LiveGlass {
    machine: Machine,
    spec: Arc<Mutex<CropSpec>>,
    sink: Arc<Sink>,
    control: Option<Control>,
    luma: LumaSmoother,
    last_frame_ms: u64,
    hwnd: Option<HWND>,
    enabled: bool,
}

impl LiveGlass {
    pub(crate) fn new() -> Self {
        Self {
            machine: Machine::new(),
            spec: Arc::new(Mutex::new(CropSpec::EMPTY)),
            sink: Arc::new(Sink::default()),
            control: None,
            luma: LumaSmoother::new(),
            last_frame_ms: 0,
            hwnd: None,
            enabled: false,
        }
    }

    pub(crate) const fn is_live(&self) -> bool {
        self.enabled && self.machine.is_live()
    }

    pub(crate) const fn running(&self) -> bool {
        matches!(self.machine.phase(), Phase::Running)
    }

    pub(crate) fn prepare(&mut self, hwnd: HWND) {
        self.hwnd = Some(hwnd);
    }

    /// Enables or disables the live path; returns false when it cannot run.
    pub(crate) fn set_enabled(&mut self, on: bool) -> bool {
        if on {
            let Some(hwnd) = self.hwnd else {
                self.enabled = false;
                self.machine.force_off();
                return false;
            };
            if !set_affinity(hwnd, true) {
                self.enabled = false;
                self.machine.force_off();
                eprintln!("live glass: display affinity failed, falling back to the wallpaper");
                return false;
            }
            self.enabled = true;
            self.machine.rearm();
            true
        } else {
            if let Some(hwnd) = self.hwnd {
                let _ = set_affinity(hwnd, false);
            }
            self.enabled = false;
            self.machine.force_off();
            false
        }
    }

    pub(crate) fn prewarm(&mut self, now_ms: u64) -> Action {
        self.machine.handle(Event::Prewarm, now_ms)
    }

    pub(crate) fn update_crop(
        &mut self,
        stage: (i32, i32, u32, u32),
        scale: f32,
        shape: GlassShape,
        warp: bool,
    ) -> bool {
        let Some(mut spec) = crop_spec(stage, scale, wallpaper::screen_size(), shape) else {
            return false;
        };
        spec.warp = warp;
        let mut slot = match self.spec.lock() {
            Ok(slot) => slot,
            Err(poison) => PoisonError::into_inner(poison),
        };
        let geometry_changed = slot.geometry != spec.geometry;
        if *slot != spec {
            *slot = spec;
        }
        geometry_changed
    }

    /// Feeds visibility and time, and returns the action to run.
    pub(crate) fn tick(&mut self, wanted: bool, now_ms: u64) -> Action {
        let mut action = self.machine.handle(Event::SetWanted(wanted), now_ms);
        if action == Action::None {
            action = self.machine.handle(Event::RetryDue, now_ms);
        }
        if let Some(deadline) = self.machine.prewarm_deadline() {
            if now_ms >= deadline {
                let next = self.machine.handle(Event::PrewarmDeadline, now_ms);
                if next != Action::None {
                    action = next;
                }
            }
        }
        action
    }

    pub(crate) fn start_request(&self) -> StartRequest {
        StartRequest { spec: Arc::clone(&self.spec), sink: Arc::clone(&self.sink) }
    }

    pub(crate) fn start_done(&mut self, result: Result<Control, StartFault>, now_ms: u64) -> Action {
        match result {
            Ok(control) => {
                self.control = Some(control);
                self.machine.handle(Event::StartOk, now_ms)
            }
            Err(fault) => {
                eprintln!("live glass: capture start failed ({fault:?})");
                self.machine.handle(Event::StartErr(fault), now_ms)
            }
        }
    }

    pub(crate) fn control_finished(&self) -> bool {
        match &self.control {
            Some(control) => control.is_finished(),
            None => false,
        }
    }

    pub(crate) fn take_control(&mut self) -> Option<Control> {
        self.control.take()
    }

    pub(crate) fn session_ended(&mut self, error: bool, now_ms: u64) -> Action {
        self.machine.handle(Event::SessionEnded { error }, now_ms)
    }

    pub(crate) fn stop_done(&mut self, now_ms: u64) -> Action {
        self.machine.handle(Event::StopDone, now_ms)
    }

    /// Takes the newest frame, smooths its luma, and reports the machine action.
    ///
    /// Frames are only installed while the session is `Running`; a frame that
    /// lands during `Stopping` stays in the slot as the next cache but is not
    /// painted.
    pub(crate) fn take_frame(&mut self, now_ms: u64) -> (Option<LiveFrame>, Action) {
        if !self.running() {
            return (None, Action::None);
        }
        let Some(mut frame) = self.sink.take() else {
            return (None, Action::None);
        };
        #[allow(clippy::cast_precision_loss, reason = "milliseconds fit in f32 for this use")]
        let dt = now_ms.saturating_sub(self.last_frame_ms) as f32;
        self.last_frame_ms = now_ms;
        frame.luma = self.luma.push(frame.luma, dt);
        if let Some(warped) = frame.warped.take() {
            frame.warped = Some(Arc::new(GlassBackdrop {
                image: Arc::clone(&warped.image),
                flat: Arc::clone(&warped.flat),
                luma: frame.luma,
                crop: warped.crop,
                shape: warped.shape,
                generation: warped.generation,
            }));
        }
        let action = self.machine.handle(Event::Frame, now_ms);
        (Some(frame), action)
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use windows_capture::graphics_capture_api::Error as CaptureError;

    #[test]
    fn env_static_is_case_and_space_insensitive() {
        assert_eq!(mode_from_env(Some("static")), Mode::Static);
        assert_eq!(mode_from_env(Some("  STATIC ")), Mode::Static);
        assert_eq!(mode_from_env(Some("live")), Mode::Live);
        assert_eq!(mode_from_env(None), Mode::Live);
    }

    fn card_shape() -> GlassShape {
        GlassShape { width: 420.0, height: 288.0, radius: 24.0 }
    }

    #[test]
    fn crop_spec_carries_shape_geometry_and_image_scale() {
        let spec = crop_spec((100, 0, 420, 360), 2.0, (1920, 1080), card_shape()).unwrap();
        assert_eq!(spec.geometry.factor, 2);
        assert!((spec.geometry.image_scale - 1.0).abs() < 1e-6);
        assert!((spec.geometry.stage_width - 210.0).abs() < 1e-4);
        assert!((spec.geometry.page.0 - 960.0).abs() < 1e-4);
        assert!((spec.geometry.page.1 - 540.0).abs() < 1e-4);
        assert_eq!(spec.shape, card_shape());
    }

    #[test]
    fn update_crop_only_reports_geometry_changes() {
        let mut live = LiveGlass::new();
        assert!(live.update_crop((100, 0, 420, 360), 1.0, card_shape(), true));
        assert!(!live.update_crop((100, 0, 420, 360), 1.0, card_shape(), true));
        // Shape-only: the raw stays valid, so this is not a geometry change.
        let morphed = GlassShape { width: 320.0, height: 56.0, radius: 28.0 };
        assert!(!live.update_crop((100, 0, 420, 360), 1.0, morphed, true));
        assert!(live.update_crop((140, 0, 420, 360), 1.0, morphed, true));
    }

    #[test]
    fn update_crop_carries_the_warp_flag_without_reporting_geometry() {
        let mut live = LiveGlass::new();
        assert!(live.update_crop((100, 0, 420, 360), 1.0, card_shape(), false));
        assert!(!live.spec.lock().unwrap().warp, "a moving morph does not warp");
        assert!(!live.update_crop((100, 0, 420, 360), 1.0, card_shape(), true));
        assert!(live.spec.lock().unwrap().warp, "a settled shape warps again");
    }

    #[test]
    fn crop_keeps_the_lens_margin() {
        let spec = crop_spec((100, 0, 420, 360), 1.0, (1920, 1080), card_shape()).unwrap();
        assert_eq!(
            (
                spec.geometry.left,
                spec.geometry.top,
                spec.geometry.right,
                spec.geometry.bottom
            ),
            (0, 0, 648, 488)
        );
        assert_eq!(spec.geometry.factor, 1);
        assert!((spec.geometry.crop.x - -100.0).abs() < 1e-4);
        assert!((spec.geometry.crop.y - 0.0).abs() < 1e-4);
        assert!((spec.geometry.crop.width - 648.0).abs() < 1e-4);
        assert!((spec.geometry.crop.height - 488.0).abs() < 1e-4);
    }

    #[test]
    fn crop_clamps_at_screen_edges() {
        let left = crop_spec((0, 0, 420, 360), 1.0, (1920, 1080), card_shape()).unwrap();
        assert_eq!(left.geometry.left, 0);
        assert!(left.geometry.crop.x.abs() < 1e-6);
        assert!(left.geometry.crop.y.abs() < 1e-6);

        assert!(crop_spec((2000, 0, 420, 360), 1.0, (1920, 1080), card_shape()).is_none());
    }

    #[test]
    fn crop_is_a_whole_number_of_blocks() {
        let spec = crop_spec((90, 0, 1260, 1080), 3.0, (1920, 1080), card_shape()).unwrap();
        assert_eq!(spec.geometry.factor, 3);
        assert_eq!((spec.geometry.right - spec.geometry.left) % 3, 0);
        assert_eq!((spec.geometry.bottom - spec.geometry.top) % 3, 0);
        // The crop is clamped at the screen edge, 30 logical px left of the
        // window; the margin would reach 128 px past the stage if it fit.
        assert!((spec.geometry.crop.x - -30.0).abs() < 1e-4);
        assert!((spec.geometry.crop.width - 578.0).abs() < 1e-4);
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
    fn collapse_feed_does_not_stop_the_warm_session() {
        let mut machine = Machine::new();
        assert_eq!(machine.handle(Event::Prewarm, 0), Action::Start);
        assert_eq!(machine.handle(Event::StartOk, 0), Action::None);
        // Collapsed, the view feeds `wanted == false` on every 32ms tick. That
        // must not tear down the session before it has cached its first frame,
        // or the machine would start and stop forever.
        for tick in 0..5 {
            assert_eq!(machine.handle(Event::SetWanted(false), 10 + tick), Action::None);
        }
        assert_eq!(machine.handle(Event::Frame, 40), Action::Stop);
        assert_eq!(machine.handle(Event::StopDone, 50), Action::None);
        assert_eq!(machine.phase(), Phase::Idle);
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

    #[test]
    fn start_errors_degrade_by_kind() {
        let wrap = |error| GraphicsCaptureApiError::<()>::GraphicsCaptureApiError(error);
        assert_eq!(
            classify_start_error(&wrap(CaptureError::CursorConfigUnsupported)),
            StartFault::Cursor
        );
        assert_eq!(
            classify_start_error(&wrap(CaptureError::BorderConfigUnsupported)),
            StartFault::Border
        );
        assert_eq!(
            classify_start_error(&wrap(CaptureError::MinimumUpdateIntervalUnsupported)),
            StartFault::Interval
        );
        assert_eq!(
            classify_start_error(&wrap(CaptureError::Unsupported)),
            StartFault::Unsupported
        );
        assert_eq!(
            classify_start_error(&GraphicsCaptureApiError::<()>::ItemConvertFailed),
            StartFault::Other
        );
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff_after(1), 1_000);
        assert_eq!(backoff_after(2), 2_000);
        assert_eq!(backoff_after(3), 4_000);
        assert_eq!(backoff_after(9), 4_000);
    }
}
