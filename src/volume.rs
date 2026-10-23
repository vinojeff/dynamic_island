#![allow(clippy::ref_as_ptr, clippy::inline_always)]
//! Master volume for the hover capsule, split into a pure rule and an STA.
//!
//! The 32ms loop on the GPUI thread calls [`wheel_applies`] first. A card, a
//! missing endpoint, or a line swallows the wheel. A vertical wheel that
//! passes then goes through [`accumulate_wheel`]: touchpad pixels pile up
//! until 40px (`PIXEL_STEP`), which is one [`WHEEL_STEP`] (2%). A step that
//! would not move the endpoint (already 0% or 100%) produces no command.
//! Rolling up from mute is [`VolumeCommand::SetLevel`]; the STA clears mute
//! when the scalar rises from zero.
//!
//! [`Endpoint`] is the only owner of `IAudioEndpointVolume`. The interface is
//! created on its thread and is not `Send`, so the compiler refuses to move
//! it. The UI sends [`VolumeCommand`] and drains [`Reading`]. `OnNotify` on
//! that same thread copies the scalar and the mute bit into a `Reading`. It
//! does not call back into the view.
//!
//! Drop drops the command sender before joining. The STA is blocked in
//! `recv`; joining first deadlocks. The view keeps the overlay up for
//! [`OVERLAY_HOLD`] (1500ms) after the last accepted step. `VOLUME_HOLD_MS`
//! in `island` is a different constant and is not this timer.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
};
use windows::Win32::Media::Audio::{
    AUDIO_VOLUME_NOTIFICATION_DATA, IMMDeviceEnumerator, MMDeviceEnumerator, eMultimedia, eRender,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::core::BOOL;
use windows_implement::implement;

/// How long the wheel overlay stays up after the last accepted step.
pub const OVERLAY_HOLD: Duration = Duration::from_millis(1_500);

/// How long the pointer must rest on the capsule before the slider appears.
pub const SLIDER_SHOW_AFTER: Duration = Duration::from_millis(300);

/// How long the pointer must stay off the capsule and slider before it hides.
pub const SLIDER_HIDE_AFTER: Duration = Duration::from_millis(300);

/// Minimum gap between slider `Set` commands while the thumb is dragged.
pub const SLIDER_THROTTLE: Duration = Duration::from_millis(30);

/// One wheel notch, as a fraction of the master scalar.
pub const WHEEL_STEP: f32 = 0.02;

/// Touchpad pixels that accumulate into one [`WHEEL_STEP`].
const PIXEL_STEP: f32 = 40.0;

/// Master volume the capsule can show.
#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct VolumeState {
    /// `0.0..=1.0`. Values outside that range are not representable.
    pub level: f32,
    /// Endpoint is muted. A muted endpoint can still report a non-zero level.
    pub muted: bool,
    /// A render endpoint was opened. When false the capsule hides volume UI.
    pub available: bool,
    /// Wheel overlay is showing.
    pub overlay: bool,
    /// Vertical slider is showing.
    pub slider: bool,
}

impl VolumeState {
    /// No endpoint yet. The capsule must not offer volume controls.
    #[must_use]
    pub const fn unavailable() -> Self {
        Self {
            level: 0.0,
            muted: false,
            available: false,
            overlay: false,
            slider: false,
        }
    }

    /// Whole percent for the overlay label. `1.0` is 100.
    #[must_use]
    pub fn percent(self) -> u8 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let percent = (self.level.clamp(0.0, 1.0) * 100.0).round() as u8;
        percent.min(100)
    }

    /// Which speaker glyph to draw.
    #[must_use]
    pub const fn icon(self) -> VolumeIcon {
        if self.muted || self.level <= 0.0 {
            VolumeIcon::Muted
        } else if self.level < 0.5 {
            VolumeIcon::Low
        } else {
            VolumeIcon::High
        }
    }
}

/// Speaker glyph. Muted wins over the level.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VolumeIcon {
    /// Crossed speaker. Level is zero or the endpoint is muted.
    Muted,
    /// One wave. Level is below one half.
    Low,
    /// Two waves. Level is at least one half.
    High,
}

/// A change the UI asks the endpoint thread to apply.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VolumeCommand {
    /// Set the scalar. The endpoint thread unmutes when this crosses up from mute.
    SetLevel(f32),
    /// Flip mute without changing the scalar.
    ToggleMute,
}

/// What one wheel event did.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WheelStep {
    /// The command to send, if the accumulator crossed a step.
    pub command: Option<VolumeCommand>,
    /// Accumulator after this event.
    pub pending: f32,
}

/// Fold one wheel delta into the pending step.
///
/// `delta_y` is positive upward. Lines are already notches; pixels accumulate
/// until they reach [`PIXEL_STEP`]. A step that would not change the endpoint
/// returns no command, including rolling past 0% or 100%. Rolling upward from
/// mute or zero unmutes by setting the scalar.
#[must_use]
pub fn accumulate_wheel(level: f32, muted: bool, pending: f32, delta_y: f32, pixels: bool) -> WheelStep {
    let delta = if pixels { delta_y / PIXEL_STEP } else { delta_y };
    let pending = pending + delta;
    let steps = pending.trunc();
    if steps == 0.0 {
        return WheelStep { command: None, pending };
    }
    let next = (level + steps * WHEEL_STEP).clamp(0.0, 1.0);
    let pending = pending - steps;
    if (next - level).abs() < f32::EPSILON && !(muted && steps > 0.0) {
        return WheelStep { command: None, pending };
    }
    WheelStep {
        command: Some(VolumeCommand::SetLevel(next)),
        pending,
    }
}

/// Whether a slider drag may emit `Set` now.
///
/// A click always emits. A drag emits on the first move and then at most once
/// per [`SLIDER_THROTTLE`].
#[must_use]
pub const fn slider_due(last_emit_ms: Option<u64>, now_ms: u64, dragging: bool) -> bool {
    if !dragging {
        return true;
    }
    match last_emit_ms {
        None => true,
        Some(last) => now_ms.saturating_sub(last) >= 30,
    }
}

/// Map a click on a vertical track to a scalar. `0.0` is the bottom.
#[must_use]
pub fn slider_level(offset_from_top: f32, height: f32) -> f32 {
    if height <= f32::EPSILON {
        return 0.0;
    }
    (1.0 - offset_from_top / height).clamp(0.0, 1.0)
}

/// The wheel overlay covers the capsule, but not an open card.
#[must_use]
pub const fn wheel_applies(card_open: bool, available: bool) -> bool {
    available && !card_open
}

/// The slider is only offered while the capsule is showing media, not a notice.
#[must_use]
pub const fn slider_applies(card_open: bool, available: bool, notice: bool) -> bool {
    available && !card_open && !notice
}

/// A snapshot the endpoint thread can send across threads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reading {
    /// `0.0..=1.0` master scalar.
    pub level: f32,
    /// Endpoint mute flag.
    pub muted: bool,
    /// `false` when no render device could be opened.
    pub available: bool,
}

impl Reading {
    /// No device. The capsule hides volume UI.
    #[must_use]
    pub const fn unavailable() -> Self {
        Self { level: 0.0, muted: false, available: false }
    }
}

/// Owns the endpoint thread. Drop closes the command channel and joins.
pub struct Endpoint {
    commands: Option<Sender<VolumeCommand>>,
    readings: Receiver<Reading>,
    thread: Option<JoinHandle<()>>,
}

impl Endpoint {
    /// Starts the STA thread and asks it for the current master volume.
    ///
    /// The first [`Reading`] arrives on [`Self::poll`]. If the device is missing
    /// that reading has `available == false`.
    #[must_use]
    pub fn spawn() -> Self {
        let (commands_tx, commands_rx) = mpsc::channel();
        let (readings_tx, readings_rx) = mpsc::channel();
        let thread = thread::spawn(move || run_endpoint(&commands_rx, &readings_tx));
        Self {
            commands: Some(commands_tx),
            readings: readings_rx,
            thread: Some(thread),
        }
    }

    /// Asks the endpoint thread to apply one change. A dead thread is ignored.
    pub fn send(&self, command: VolumeCommand) {
        if let Some(commands) = &self.commands {
            let _sent = commands.send(command);
        }
    }

    /// Readings posted since the last poll. The newest one wins.
    #[must_use]
    pub fn poll(&self) -> Option<Reading> {
        self.readings.try_iter().last()
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        // Drop the sender first. The thread's `recv` then returns disconnected,
        // the same as closing the write end of a pipe. Join after that, so the
        // thread can leave the loop and call `CoUninitialize`. Joining first
        // waits on a thread that is waiting on this drop.
        self.commands.take();
        if let Some(thread) = self.thread.take() {
            let _joined = thread.join();
        }
    }
}

fn run_endpoint(commands: &Receiver<VolumeCommand>, readings: &Sender<Reading>) {
    // SAFETY: this thread has not initialized COM. STA matches the audio
    // endpoint, which is not `Send`. Uninitialize only when this call succeeded.
    let apartment = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let endpoint = open_endpoint();
    let Some(volume) = endpoint else {
        let _sent = readings.send(Reading::unavailable());
        return;
    };
    publish(&volume, readings);
    let callback = VolumeCallback { readings: readings.clone() };
    let callback: IAudioEndpointVolumeCallback = callback.into();
    // SAFETY: `callback` outlives the registration. It is unregistered before drop.
    let registered = unsafe { volume.RegisterControlChangeNotify(&callback) }.is_ok();
    while let Ok(command) = commands.recv() {
        apply(&volume, command);
        publish(&volume, readings);
    }
    if registered {
        // SAFETY: `callback` is the same interface that was registered.
        let _removed = unsafe { volume.UnregisterControlChangeNotify(&callback) };
    }
    drop(callback);
    drop(volume);
    // SAFETY: `CoInitializeEx` succeeded or returned a changed-mode that still
    // leaves this thread in an apartment. Uninitialize balances a successful init.
    // A failed init returns `RPC_E_CHANGED_MODE` only when COM is already up,
    // in which case uninitialize would unbalance the outer owner, so skip it.
    if apartment.is_ok() {
        unsafe { CoUninitialize() };
    }
}

fn open_endpoint() -> Option<IAudioEndpointVolume> {
    // SAFETY: COM is initialized on this thread. The enumerator is in-proc and
    // stays on this thread.
    let enumerator: IMMDeviceEnumerator = unsafe {
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)
    }.ok()?;
    // SAFETY: `eRender` + `eMultimedia` is the documented default playback endpoint.
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia) }.ok()?;
    // SAFETY: `IAudioEndpointVolume` is the activation contract for a render device.
    unsafe { device.Activate::<IAudioEndpointVolume>(CLSCTX_INPROC_SERVER, None) }.ok()
}

fn apply(volume: &IAudioEndpointVolume, command: VolumeCommand) {
    match command {
        VolumeCommand::SetLevel(level) => {
            let level = level.clamp(0.0, 1.0);
            // SAFETY: a null event context is the documented "no context" value.
            if unsafe { volume.GetMute() }.is_ok_and(BOOL::as_bool) && level > 0.0 {
                let _unmuted = unsafe { volume.SetMute(false, std::ptr::null()) };
            }
            let _set = unsafe { volume.SetMasterVolumeLevelScalar(level, std::ptr::null()) };
        }
        VolumeCommand::ToggleMute => {
            let Ok(muted) = (unsafe { volume.GetMute() }) else {
                return;
            };
            let _set = unsafe { volume.SetMute(!muted.as_bool(), std::ptr::null()) };
        }
    }
}

fn publish(volume: &IAudioEndpointVolume, readings: &Sender<Reading>) {
    let Ok(level) = (unsafe { volume.GetMasterVolumeLevelScalar() }) else {
        return;
    };
    let Ok(muted) = (unsafe { volume.GetMute() }) else {
        return;
    };
    let _sent = readings.send(Reading {
        level: level.clamp(0.0, 1.0),
        muted: muted.as_bool(),
        available: true,
    });
}

#[implement(IAudioEndpointVolumeCallback)]
struct VolumeCallback {
    readings: Sender<Reading>,
}

impl IAudioEndpointVolumeCallback_Impl for VolumeCallback_Impl {
    fn OnNotify(&self, notify: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows::core::Result<()> {
        if notify.is_null() {
            return Ok(());
        }
        // SAFETY: WASAPI passes a live `AUDIO_VOLUME_NOTIFICATION_DATA` for this
        // call. Only the master scalar and mute flag are read; the flexible
        // channel array is not touched.
        let data = unsafe { &*notify };
        let _sent = self.readings.send(Reading {
            level: data.fMasterVolume.clamp(0.0, 1.0),
            muted: data.bMuted.as_bool(),
            available: true,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        VolumeCommand, VolumeIcon, VolumeState, accumulate_wheel, slider_applies, slider_due,
        slider_level, wheel_applies,
    };

    #[test]
    fn a_line_up_raises_the_scalar_by_two_percent() {
        let step = accumulate_wheel(0.50, false, 0.0, 1.0, false);
        assert_eq!(step.command, Some(VolumeCommand::SetLevel(0.52)));
        assert!((step.pending).abs() < f32::EPSILON);
    }

    #[test]
    fn a_line_down_lowers_the_scalar_by_two_percent() {
        let step = accumulate_wheel(0.50, false, 0.0, -1.0, false);
        assert_eq!(step.command, Some(VolumeCommand::SetLevel(0.48)));
    }

    #[test]
    fn pixels_wait_until_forty_accumulate() {
        let first = accumulate_wheel(0.50, false, 0.0, 20.0, true);
        assert_eq!(first.command, None);
        let second = accumulate_wheel(0.50, false, first.pending, 20.0, true);
        assert_eq!(second.command, Some(VolumeCommand::SetLevel(0.52)));
    }

    #[test]
    fn rolling_past_the_ends_sends_nothing() {
        let top = accumulate_wheel(1.0, false, 0.0, 1.0, false);
        let bottom = accumulate_wheel(0.0, false, 0.0, -1.0, false);
        assert_eq!(top.command, None);
        assert_eq!(bottom.command, None);
    }

    #[test]
    fn rolling_up_from_mute_sets_the_level_and_clears_mute() {
        let step = accumulate_wheel(0.0, true, 0.0, 1.0, false);
        assert_eq!(step.command, Some(VolumeCommand::SetLevel(0.02)));
    }

    #[test]
    fn rolling_up_from_a_muted_nonzero_level_raises_it() {
        let step = accumulate_wheel(0.40, true, 0.0, 1.0, false);
        let VolumeCommand::SetLevel(level) = step.command.expect("a muted level still steps") else {
            panic!("mute toggle is not a wheel step");
        };
        assert!((level - 0.42).abs() < 0.001, "{level}");
    }

    #[test]
    fn the_slider_maps_the_top_to_full_and_the_bottom_to_silent() {
        assert!((slider_level(0.0, 160.0) - 1.0).abs() < f32::EPSILON);
        assert!(slider_level(160.0, 160.0).abs() < f32::EPSILON);
        assert!((slider_level(50.0, 100.0) - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn a_drag_waits_thirty_milliseconds_between_sets() {
        assert!(slider_due(None, 0, true));
        assert!(!slider_due(Some(1_000), 1_029, true));
        assert!(slider_due(Some(1_000), 1_030, true));
        assert!(slider_due(Some(1_000), 1_000, false));
    }

    #[test]
    fn percent_rounds_and_mute_picks_the_crossed_icon() {
        let loud = VolumeState {
            level: 0.655,
            muted: false,
            available: true,
            overlay: true,
            slider: false,
        };
        assert_eq!(loud.percent(), 66);
        assert_eq!(loud.icon(), VolumeIcon::High);
        let silent = VolumeState { level: 0.2, muted: true, ..loud };
        assert_eq!(silent.icon(), VolumeIcon::Muted);
        let quiet = VolumeState { muted: false, level: 0.49, ..loud };
        assert_eq!(quiet.icon(), VolumeIcon::Low);
    }

    #[test]
    fn the_card_and_a_missing_endpoint_refuse_the_wheel() {
        assert!(!wheel_applies(true, true));
        assert!(!wheel_applies(false, false));
        assert!(wheel_applies(false, true));
    }

    #[test]
    fn the_slider_stays_down_during_a_notice() {
        assert!(!slider_applies(false, true, true));
        assert!(slider_applies(false, true, false));
        assert!(!slider_applies(true, true, false));
    }
}
