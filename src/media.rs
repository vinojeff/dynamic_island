//! SMTC snapshot, transport, seek, and the cover cache.
//!
//! The view's 1s loop calls [`current`] on `background_executor` and copies
//! the [`NowPlaying`] back through `view.update`. A seek drag in progress
//! drops that copy so the clock keeps following the pointer.
//!
//! `RequestAsync().join()` runs a `WinRT` operation to completion on the
//! calling thread. [`send`] must not be joined that way: a player can leave
//! `TrySkipNextAsync` unfinished and freeze the caller. Transport is started
//! and a later poll reads the session. [`seek`] is the one call that does
//! wait, and it runs only on pointer-up (or a 5s arrow nudge), exactly once.
//! A refusal stores [`SEEK_REFUSED`]; the bar then paints read-only.
//!
//! [`CoverCache`] is an `OnceLock<Mutex<_>>` shared by the poll threads. It
//! is a 20-entry LRU keyed by title and artist. A hit moves the key to the
//! back; past twenty, the front entry is dropped.

use std::collections::HashMap;
use std::sync::Mutex;

/// What the media tab shows. Empty strings mean the session had no value.
#[derive(Clone, Debug, PartialEq)]
pub struct NowPlaying {
    /// Track title. Empty when nothing is playing.
    pub title: String,
    /// Artist. Empty when the session did not report one.
    pub artist: String,
    /// Playback is moving.
    pub playing: bool,
    /// Decoded album art, or `None` when the session has no thumbnail.
    pub cover: Option<std::sync::Arc<gpui_kit::RenderImage>>,
    /// Playback position as a fraction of the track, `0.0..=1.0`.
    pub progress: f32,
    /// Position in seconds.
    pub position_secs: u32,
    /// Duration in seconds. Zero when the timeline is missing.
    pub duration_secs: u32,
    /// Album title. Empty when the session did not report one.
    pub album: String,
    /// Album year. `None` when the session did not report one.
    pub album_year: Option<u32>,
    /// Previous, play/pause, and next, in that order.
    pub transport: TransportCaps,
    /// Shuffle and repeat, when the session exposes them.
    pub modes: ModeCaps,
    /// SMTC sessions currently open. One means the switcher stays hidden.
    pub sessions: Vec<MediaSession>,
    /// The session accepts `TryChangePlaybackPositionAsync`.
    pub seekable: bool,
}

impl NowPlaying {
    /// Nothing is playing.
    #[must_use]
    pub fn idle() -> Self {
        Self {
            title: String::new(),
            artist: String::new(),
            playing: false,
            cover: None,
            progress: 0.0,
            position_secs: 0,
            duration_secs: 0,
            album: String::new(),
            album_year: None,
            transport: TransportCaps::all_enabled(),
            modes: ModeCaps::hidden(),
            sessions: Vec::new(),
            seekable: false,
        }
    }

    /// Album line under the title. Empty album renders nothing, so the row is omitted.
    #[must_use]
    pub fn album_line(&self) -> Option<String> {
        album_line(&self.album, self.album_year)
    }

    /// Capsule line. Idle is a fixed label; a missing artist is just the title.
    #[must_use]
    pub fn line(&self) -> String {
        if self.title.is_empty() {
            return "未在播放".to_owned();
        }
        if self.artist.is_empty() {
            return self.title.clone();
        }
        let mut line = String::new();
        line.push_str(&self.title);
        line.push_str(" - ");
        line.push_str(&self.artist);
        line
    }
}

/// Which transport buttons the session will accept.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportCaps {
    /// Previous track is offered.
    pub previous: bool,
    /// Play and pause are offered.
    pub play_pause: bool,
    /// Next track is offered.
    pub next: bool,
}

impl TransportCaps {
    /// Every button accepts a click. Used when the session reports no controls.
    #[must_use]
    pub const fn all_enabled() -> Self {
        Self { previous: true, play_pause: true, next: true }
    }

    /// No button accepts a click. The row still draws, at full opacity.
    #[must_use]
    pub const fn all_disabled() -> Self {
        Self { previous: false, play_pause: false, next: false }
    }

    /// True when every button is refused. The panel then keeps the current look.
    #[must_use]
    pub const fn none_enabled(self) -> bool {
        !self.previous && !self.play_pause && !self.next
    }
}

/// Shuffle and repeat, only when the session publishes them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModeCaps {
    /// Draw the shuffle button.
    pub shuffle: Option<bool>,
    /// Draw the repeat button. `None` hides it.
    pub repeat: Option<RepeatMode>,
}

impl ModeCaps {
    /// Neither control is published.
    #[must_use]
    pub const fn hidden() -> Self {
        Self { shuffle: None, repeat: None }
    }
}

/// SMTC auto-repeat. Track is the single-track loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepeatMode {
    /// Playback stops at the end.
    None,
    /// The queue loops.
    List,
    /// The current track loops.
    Track,
}

/// One open SMTC session, for the panel switcher.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaSession {
    /// App user model id. Passed back to select this session.
    pub id: String,
    /// Short label drawn in the list. The executable name when the id has none.
    pub label: String,
    /// This session is the one the panel is showing.
    pub current: bool,
}

/// Local scrub. Commands are not sent until [`SeekGesture::release`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeekGesture {
    fraction: f32,
    duration_secs: u32,
    dragging: bool,
}

impl SeekGesture {
    /// A press on the track. `fraction` is clamped to `0.0..=1.0`.
    #[must_use]
    pub fn press(fraction: f32, duration_secs: u32) -> Self {
        Self {
            fraction: fraction.clamp(0.0, 1.0),
            duration_secs,
            dragging: true,
        }
    }

    /// Pointer moved while the button is held. Still no command.
    pub fn drag(&mut self, fraction: f32) {
        self.fraction = fraction.clamp(0.0, 1.0);
    }

    /// Button released. Returns the one seek to send.
    #[must_use]
    pub const fn release(self) -> (f32, u32) {
        (self.fraction, self.duration_secs)
    }

    /// Position the clock should show while the pointer is down.
    #[must_use]
    pub fn shown(self, reported: f32, reported_secs: u32) -> (f32, u32) {
        if self.dragging {
            (self.fraction, fraction_secs(self.fraction, self.duration_secs))
        } else {
            (reported, reported_secs)
        }
    }
}

/// Seconds for a fraction of a track. The last second is included.
#[must_use]
pub fn fraction_secs(fraction: f32, duration_secs: u32) -> u32 {
    let fraction = if fraction.is_finite() { fraction.clamp(0.0, 1.0) } else { 0.0 };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    let seconds = (fraction * duration_secs as f32).round() as u32;
    seconds.min(duration_secs)
}

/// ±5 seconds, clamped to the track. A zero duration stays at zero.
#[must_use]
pub fn nudge_secs(position_secs: u32, duration_secs: u32, forward: bool) -> u32 {
    if duration_secs == 0 {
        return 0;
    }
    let next = if forward {
        position_secs.saturating_add(5)
    } else {
        position_secs.saturating_sub(5)
    };
    next.min(duration_secs)
}

/// Fraction of a track a nudged position occupies.
#[must_use]
pub fn secs_fraction(position_secs: u32, duration_secs: u32) -> f32 {
    if duration_secs == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let fraction = position_secs.min(duration_secs) as f32 / duration_secs as f32;
    fraction.clamp(0.0, 1.0)
}

/// "album · 2024", or just the album. A blank album is omitted.
#[must_use]
pub fn album_line(album: &str, year: Option<u32>) -> Option<String> {
    let album = album.trim();
    if album.is_empty() {
        return None;
    }
    let mut line = String::from(album);
    if let Some(year) = year {
        line.push_str(" · ");
        line.push_str(&year.to_string());
    }
    Some(line)
}

/// A transport button.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Transport {
    /// Previous track.
    Previous,
    /// Toggle play and pause.
    PlayPause,
    /// Next track.
    Next,
}

/// Shuffle or repeat, sent only when the session publishes the control.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModeCommand {
    /// Set shuffle. The panel flips the value it last read.
    Shuffle(bool),
    /// Set repeat. The panel advances None → List → Track → None.
    Repeat(RepeatMode),
}

/// The next repeat mode. Hidden controls never reach this.
#[must_use]
pub const fn next_repeat(mode: RepeatMode) -> RepeatMode {
    match mode {
        RepeatMode::None => RepeatMode::List,
        RepeatMode::List => RepeatMode::Track,
        RepeatMode::Track => RepeatMode::None,
    }
}

/// Whether the last seek was refused by the session.
///
/// The drag happens on the GPUI thread and the seek itself runs on a
/// background thread. Both read this flag. A plain `bool` shared across
/// threads is a data race. An atomic is the smallest sharing that is defined.
static SEEK_REFUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Marks seek unavailable, for a session that reports no timeline.
pub fn mark_seek_unavailable() {
    SEEK_REFUSED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// The session refused the previous seek, so the bar should be read-only.
#[must_use]
pub fn seek_refused() -> bool {
    SEEK_REFUSED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Seeks the current session to `fraction` of `duration_secs`. No session is a no-op.
///
/// A `false` result from the session marks seek as unsupported.
pub fn seek(fraction: f32, duration_secs: u32) {
    if seek_inner(fraction.clamp(0.0, 1.0), duration_secs) == Ok(true) {
        SEEK_REFUSED.store(false, std::sync::atomic::Ordering::SeqCst);
    } else {
        SEEK_REFUSED.store(true, std::sync::atomic::Ordering::SeqCst);
        warn_seek_once();
    }
}

fn warn_seek_once() {
    use std::sync::atomic::AtomicBool;
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        eprintln!("warn: smtc seek refused");
    }
}

fn seek_inner(fraction: f32, duration_secs: u32) -> windows::core::Result<bool> {
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager;
    if duration_secs == 0 {
        return Ok(false);
    }
    let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()?.join()?;
    let session = manager.GetCurrentSession()?;
    if !position_enabled(&session) {
        return Ok(false);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let ticks = (duration_secs as f32 * fraction * 10_000_000.0) as i64;
    session.TryChangePlaybackPositionAsync(ticks)?.join()
}

fn position_enabled(session: &windows::Media::Control::GlobalSystemMediaTransportControlsSession) -> bool {
    session
        .GetPlaybackInfo()
        .and_then(|info| info.Controls())
        .and_then(|controls| controls.IsPlaybackPositionEnabled())
        .unwrap_or(false)
}

/// Activates the app that owns the current session. No session is a no-op.
pub fn open_player() {
    let _opened = open_player_inner();
}

fn open_player_inner() -> windows::core::Result<()> {
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager;
    let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()?.join()?;
    let session = manager.GetCurrentSession()?;
    let app_id = session.SourceAppUserModelId()?.to_string_lossy();
    let needle = app_id.split('!').next().unwrap_or("").to_ascii_lowercase();
    if needle.is_empty() {
        return Ok(());
    }
    raise_matching_window(&needle);
    Ok(())
}

fn raise_matching_window(needle: &str) {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::System::ProcessStatus::GetModuleBaseNameW;
    use windows_sys::Win32::System::Threading::OpenProcess;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindowVisible, SetForegroundWindow,
    };
    struct Search {
        needle: String,
        found: HWND,
    }
    unsafe extern "system" fn visit(hwnd: HWND, param: LPARAM) -> i32 {
        // SAFETY: `param` is the `Search` passed to `EnumWindows` below, and the
        // enumeration runs on this thread until it returns.
        let search = unsafe { &mut *(param as *mut Search) };
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }
        let mut pid = 0_u32;
        let _thread = unsafe { GetWindowThreadProcessId(hwnd, &raw mut pid) };
        let process = unsafe { OpenProcess(0x0410, 0, pid) };
        if process.is_null() {
            return 1;
        }
        let mut name = [0_u16; 260];
        #[allow(clippy::cast_possible_truncation)]
        let read = unsafe { GetModuleBaseNameW(process, std::ptr::null_mut(), name.as_mut_ptr(), name.len() as u32) };
        let _closed = unsafe { windows_sys::Win32::Foundation::CloseHandle(process) };
        if read == 0 {
            return 1;
        }
        let module = name.get(..read as usize).map_or_else(String::new, |units| String::from_utf16_lossy(units).to_ascii_lowercase());
        if module.contains(&search.needle) {
            search.found = hwnd;
            return 0;
        }
        1
    }
    let mut search = Search { needle: needle.to_owned(), found: std::ptr::null_mut() };
    unsafe {
        let _listed = EnumWindows(Some(visit), (&raw mut search) as LPARAM);
        if !search.found.is_null() {
            let _raised = SetForegroundWindow(search.found);
        }
    }
}

/// Flips shuffle on the current session. A missing control is a no-op.
pub fn toggle_shuffle(active: bool) {
    let _changed = toggle_shuffle_inner(!active);
}

fn toggle_shuffle_inner(next: bool) -> windows::core::Result<()> {
    let session = current_session()?;
    let _started = session.TryChangeShuffleActiveAsync(next)?;
    Ok(())
}

/// Cycles repeat on the current session. A missing control is a no-op.
pub fn cycle_repeat(mode: RepeatMode) {
    let _changed = cycle_repeat_inner(next_repeat(mode));
}

fn cycle_repeat_inner(mode: RepeatMode) -> windows::core::Result<()> {
    use windows::Media::MediaPlaybackAutoRepeatMode;
    let requested = match mode {
        RepeatMode::None => MediaPlaybackAutoRepeatMode::None,
        RepeatMode::List => MediaPlaybackAutoRepeatMode::List,
        RepeatMode::Track => MediaPlaybackAutoRepeatMode::Track,
    };
    let session = current_session()?;
    let _started = session.TryChangeAutoRepeatModeAsync(requested)?;
    Ok(())
}

/// Makes `id` the current SMTC session. An unknown id is a no-op.
///
/// Windows has no "set current" call. A paused session is started so the
/// next poll follows it. A session that is already playing is left alone.
pub fn select_session(id: &str) {
    let _selected = select_session_inner(id);
}

fn select_session_inner(id: &str) -> windows::core::Result<()> {
    use windows::Media::Control::{
        GlobalSystemMediaTransportControlsSessionManager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus,
    };
    let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()?.join()?;
    let sessions = manager.GetSessions()?;
    for session in sessions {
        let app = session.SourceAppUserModelId()?.to_string_lossy();
        if app != id {
            continue;
        }
        let playing = session
            .GetPlaybackInfo()
            .and_then(|info| info.PlaybackStatus())
            .is_ok_and(|status| status == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing);
        if !playing {
            let _started = session.TryPlayAsync()?;
        }
        break;
    }
    Ok(())
}

fn current_session() -> windows::core::Result<windows::Media::Control::GlobalSystemMediaTransportControlsSession> {
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager;
    let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()?.join()?;
    manager.GetCurrentSession()
}

/// Sends one transport command to the current session. No session is a no-op.
pub fn send(command: Transport) {
    let _sent = send_inner(command);
}

fn send_inner(command: Transport) -> windows::core::Result<()> {
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager;
    let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()?.join()?;
    let session = manager.GetCurrentSession()?;
    // Do not join the operation. A player that never completes TrySkipNextAsync
    // would freeze the thread that called `send`. The poll picks up the result.
    match command {
        Transport::Previous => {
            let _started = session.TrySkipPreviousAsync()?;
        }
        Transport::Next => {
            let _started = session.TrySkipNextAsync()?;
        }
        Transport::PlayPause => {
            let _started = session.TryTogglePlayPauseAsync()?;
        }
    }
    Ok(())
}

/// Reads the current SMTC session. Failure and no session are both idle.
#[must_use]
pub fn current() -> NowPlaying {
    read().unwrap_or_else(|_| NowPlaying::idle())
}

fn read() -> windows::core::Result<NowPlaying> {
    use windows::Media::Control::{
        GlobalSystemMediaTransportControlsSessionManager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus,
    };
    let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()?.join()?;
    let Some(session) = manager.GetCurrentSession().ok() else {
        return Ok(NowPlaying::idle());
    };
    let props = session.TryGetMediaPropertiesAsync()?.join()?;
    let status = session.GetPlaybackInfo()?.PlaybackStatus()?;
    let playing = status == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing;
    let title = hstring_to_string(&props.Title()?);
    let artist = hstring_to_string(&props.Artist()?);
    let cover = thumbnail_bytes(&props).and_then(|bytes| cached_cover(&title, &artist, &bytes));
    let (progress, position_secs, duration_secs) = timeline(&session);
    let album = hstring_to_string(&props.AlbumTitle()?);
    let album_year = None;
    let transport = transport_caps(&session);
    let modes = mode_caps(&session);
    let sessions = list_sessions(&manager, &session);
    let seekable = position_enabled(&session);
    Ok(NowPlaying {
        title,
        artist,
        playing,
        cover,
        progress,
        position_secs,
        duration_secs,
        album,
        album_year,
        transport,
        modes,
        sessions,
        seekable,
    })
}

fn control_flag(flag: &windows::core::Result<bool>, fallback: bool) -> bool {
    match flag {
        Ok(value) => *value,
        Err(_) => fallback,
    }
}

fn transport_caps(session: &windows::Media::Control::GlobalSystemMediaTransportControlsSession) -> TransportCaps {
    let Ok(info) = session.GetPlaybackInfo() else {
        return TransportCaps::all_enabled();
    };
    let Ok(controls) = info.Controls() else {
        return TransportCaps::all_enabled();
    };
    let caps = TransportCaps {
        previous: control_flag(&controls.IsPreviousEnabled(), true),
        play_pause: control_flag(&controls.IsPlayPauseToggleEnabled(), true)
            || control_flag(&controls.IsPlayEnabled(), false)
            || control_flag(&controls.IsPauseEnabled(), false),
        next: control_flag(&controls.IsNextEnabled(), true),
    };
    if caps.none_enabled() { TransportCaps::all_enabled() } else { caps }
}

fn mode_caps(session: &windows::Media::Control::GlobalSystemMediaTransportControlsSession) -> ModeCaps {
    let Ok(info) = session.GetPlaybackInfo() else {
        return ModeCaps::hidden();
    };
    let controls = info.Controls().ok();
    let shuffle_offered = controls.as_ref().and_then(|controls| controls.IsShuffleEnabled().ok()).unwrap_or(false);
    let repeat_offered = controls.as_ref().and_then(|controls| controls.IsRepeatEnabled().ok()).unwrap_or(false);
    ModeCaps {
        shuffle: if shuffle_offered {
            info.IsShuffleActive().ok().and_then(|value| value.Value().ok())
        } else {
            None
        },
        repeat: if repeat_offered {
            info.AutoRepeatMode().ok().and_then(|value| value.Value().ok()).map(repeat_of)
        } else {
            None
        },
    }
}

fn repeat_of(mode: windows::Media::MediaPlaybackAutoRepeatMode) -> RepeatMode {
    use windows::Media::MediaPlaybackAutoRepeatMode;
    if mode == MediaPlaybackAutoRepeatMode::Track {
        RepeatMode::Track
    } else if mode == MediaPlaybackAutoRepeatMode::List {
        RepeatMode::List
    } else {
        RepeatMode::None
    }
}

fn list_sessions(
    manager: &windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager,
    current: &windows::Media::Control::GlobalSystemMediaTransportControlsSession,
) -> Vec<MediaSession> {
    let Ok(current_id) = current.SourceAppUserModelId() else {
        return Vec::new();
    };
    let Ok(sessions) = manager.GetSessions() else {
        return Vec::new();
    };
    let mut listed = Vec::new();
    for session in sessions {
        let Ok(id) = session.SourceAppUserModelId() else {
            continue;
        };
        let id = id.to_string_lossy();
        if id.is_empty() {
            continue;
        }
        listed.push(MediaSession {
            label: session_label(&id),
            current: id == current_id.to_string_lossy(),
            id,
        });
    }
    listed
}

fn session_label(id: &str) -> String {
    let head = id.split('!').next().unwrap_or(id);
    let file = head.rsplit(['\\', '/']).next().unwrap_or(head);
    let stem = file.split('.').next().unwrap_or(file);
    if stem.is_empty() { id.to_owned() } else { stem.to_owned() }
}

/// Progress plus the two clock values, in seconds. A missing timeline is all zero.
fn timeline(
    session: &windows::Media::Control::GlobalSystemMediaTransportControlsSession,
) -> (f32, u32, u32) {
    let Ok(timeline) = session.GetTimelineProperties() else {
        return (0.0, 0, 0);
    };
    let Ok(end) = timeline.EndTime() else {
        return (0.0, 0, 0);
    };
    let Ok(position) = timeline.Position() else {
        return (0.0, 0, 0);
    };
    (
        progress_of(position.Duration, end.Duration),
        ticks_to_secs(position.Duration),
        ticks_to_secs(end.Duration),
    )
}

/// 100-nanosecond ticks to whole seconds. Negative values are zero.
#[must_use]
pub fn ticks_to_secs(ticks: i64) -> u32 {
    if ticks <= 0 {
        return 0;
    }
    u32::try_from(ticks / 10_000_000).unwrap_or(u32::MAX)
}

/// `m:ss` for a duration. Hours fold into minutes.
#[must_use]
pub fn clock(seconds: u32) -> String {
    let minutes = seconds / 60;
    let rest = seconds % 60;
    format!("{minutes}:{rest:02}")
}

/// Fraction of `position` within `duration`. Both are 100-nanosecond ticks.
#[must_use]
pub fn progress_of(position: i64, duration: i64) -> f32 {
    if duration <= 0 || position <= 0 {
        return 0.0;
    }
    let position = position.min(duration);
    #[allow(clippy::cast_precision_loss)]
    let fraction = position as f32 / duration as f32;
    fraction.clamp(0.0, 1.0)
}

fn thumbnail_bytes(
    props: &windows::Media::Control::GlobalSystemMediaTransportControlsSessionMediaProperties,
) -> Option<Vec<u8>> {
    use windows::Storage::Streams::{Buffer, DataReader, InputStreamOptions};
    let reference = props.Thumbnail().ok()?;
    let stream = reference.OpenReadAsync().ok()?.join().ok()?;
    let size = u32::try_from(stream.Size().ok()?).ok()?;
    if size == 0 {
        return None;
    }
    let buffer = Buffer::Create(size).ok()?;
    let filled = stream.ReadAsync(&buffer, size, InputStreamOptions::None).ok()?.join().ok()?;
    let length = filled.Length().ok()?;
    let reader = DataReader::FromBuffer(&filled).ok()?;
    let mut bytes = vec![0_u8; length as usize];
    reader.ReadBytes(&mut bytes).ok()?;
    Some(bytes)
}

/// Decode PNG or JPEG bytes into the BGRA frame GPUI renders. `None` on bad input.
#[must_use]
pub fn decode_cover(bytes: &[u8]) -> Option<std::sync::Arc<gpui_kit::RenderImage>> {
    let image = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    let raw = image.into_raw();
    let mut bgra = Vec::with_capacity(raw.len());
    for chunk in raw.chunks_exact(4) {
        let Some(&[red, green, blue, alpha]) = chunk.get(..4) else {
            break;
        };
        bgra.extend_from_slice(&[blue, green, red, alpha]);
    }
    let buffer = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(width, height, bgra)?;
    let frame = image::Frame::new(buffer);
    Some(std::sync::Arc::new(gpui_kit::RenderImage::new(
        smallvec::smallvec![frame],
    )))
}

/// Remembers covers by `title + artist` so a repeated poll does not decode again.
///
/// `slots` is the map and `order` is the insertion order. A hit moves the key
/// to the back. Past twenty entries the front is dropped. Hand-rolled LRU, no
/// cache crate.
pub struct CoverCache {
    slots: HashMap<u64, std::sync::Arc<gpui_kit::RenderImage>>,
    order: Vec<u64>,
}

impl CoverCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self { slots: HashMap::new(), order: Vec::new() }
    }

    /// The cached cover, if this track was decoded before.
    #[must_use]
    pub fn get(&mut self, key: u64) -> Option<std::sync::Arc<gpui_kit::RenderImage>> {
        let found = self.slots.get(&key).cloned();
        if found.is_some() {
            self.order.retain(|slot| *slot != key);
            self.order.push(key);
        }
        found
    }

    /// Store a cover, dropping the oldest once twenty are kept.
    pub fn insert(&mut self, key: u64, cover: std::sync::Arc<gpui_kit::RenderImage>) {
        if self.slots.contains_key(&key) {
            return;
        }
        self.slots.insert(key, cover);
        self.order.push(key);
        while self.order.len() > 20 {
            if let Some(oldest) = self.order.first().copied() {
                let _removed = self.order.remove(0);
                let _cover = self.slots.remove(&oldest);
            }
        }
    }
}

impl Default for CoverCache {
    fn default() -> Self {
        Self::new()
    }
}

fn track_key(title: &str, artist: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    title.hash(&mut hasher);
    artist.hash(&mut hasher);
    hasher.finish()
}

fn cached_cover(title: &str, artist: &str, bytes: &[u8]) -> Option<std::sync::Arc<gpui_kit::RenderImage>> {
    let key = track_key(title, artist);
    let mut cache = cover_cache().lock().ok()?;
    if let Some(hit) = cache.get(key) {
        return Some(hit);
    }
    let decoded = decode_cover(bytes)?;
    cache.insert(key, std::sync::Arc::clone(&decoded));
    Some(decoded)
}

fn cover_cache() -> &'static Mutex<CoverCache> {
    // Process-global and lazy. `OnceLock` runs the initializer once even if
    // several poll threads arrive together (`pthread_once` /
    // `InitOnceExecuteOnce`). The mutex is inside because the map is mutated
    // after that one-time init.
    static CACHE: std::sync::OnceLock<Mutex<CoverCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(CoverCache::new()))
}

fn hstring_to_string(value: &windows::core::HSTRING) -> String {
    value.to_string_lossy()
}

#[cfg(test)]
mod tests {
    use super::{
        NowPlaying, RepeatMode, SeekGesture, TransportCaps, album_line, clock, decode_cover,
        fraction_secs, next_repeat, nudge_secs, progress_of, secs_fraction, ticks_to_secs,
    };

    fn titled(title: &str) -> NowPlaying {
        NowPlaying { title: title.to_owned(), ..NowPlaying::idle() }
    }

    #[test]
    fn idle_uses_the_empty_label() {
        assert_eq!(NowPlaying::idle().line(), "未在播放");
    }

    #[test]
    fn a_title_without_an_artist_stands_alone() {
        let now = NowPlaying { artist: String::new(), ..titled("夜航") };
        assert_eq!(now.line(), "夜航");
    }

    #[test]
    fn a_title_and_artist_join_with_a_dash() {
        let now = NowPlaying { artist: "南岸".to_owned(), ..titled("夜航") };
        assert_eq!(now.line(), "夜航 - 南岸");
    }

    #[test]
    fn a_png_decodes_and_garbage_does_not() {
        let mut bytes = Vec::new();
        let image = image::RgbaImage::from_pixel(2, 2, image::Rgba([9, 8, 7, 255]));
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("encode");
        assert!(decode_cover(&bytes).is_some());
        assert!(decode_cover(&[0, 1, 2]).is_none());
    }

    #[test]
    fn progress_is_the_fraction_of_the_track_and_stays_inside_the_bar() {
        let quarter = progress_of(25, 100);
        assert!((quarter - 0.25).abs() < 0.001, "{quarter}");
        assert!((progress_of(0, 100)).abs() < 0.001);
        assert!((progress_of(10, 0)).abs() < 0.001);
        assert!((progress_of(150, 100) - 1.0).abs() < 0.001);
        assert_eq!(ticks_to_secs(840_000_000), 84);
        assert_eq!(clock(84), "1:24");
        assert_eq!(clock(242), "4:02");
    }

    #[test]
    fn a_drag_moves_the_clock_and_sends_one_seek_on_release() {
        let mut gesture = SeekGesture::press(0.25, 200);
        let (shown, secs) = gesture.shown(0.1, 20);
        assert!((shown - 0.25).abs() < 0.001);
        assert_eq!(secs, 50);
        gesture.drag(0.5);
        assert_eq!(gesture.release(), (0.5, 200));
    }

    #[test]
    fn arrow_seek_steps_five_seconds_and_stops_at_the_ends() {
        assert_eq!(nudge_secs(10, 200, true), 15);
        assert_eq!(nudge_secs(3, 200, false), 0);
        assert_eq!(nudge_secs(198, 200, true), 200);
        assert_eq!(nudge_secs(10, 0, true), 0);
        assert!((secs_fraction(50, 200) - 0.25).abs() < 0.001);
        assert_eq!(fraction_secs(1.0, 84), 84);
    }

    #[test]
    fn album_line_joins_the_year_and_hides_when_blank() {
        assert_eq!(album_line("  ", Some(2024)), None);
        assert_eq!(album_line("北岸", None).as_deref(), Some("北岸"));
        assert_eq!(album_line("北岸", Some(2024)).as_deref(), Some("北岸 · 2024"));
        let now = NowPlaying { album: "北岸".to_owned(), album_year: Some(2024), ..NowPlaying::idle() };
        assert_eq!(now.album_line().as_deref(), Some("北岸 · 2024"));
    }

    #[test]
    fn repeat_cycles_none_list_track() {
        assert_eq!(next_repeat(RepeatMode::None), RepeatMode::List);
        assert_eq!(next_repeat(RepeatMode::List), RepeatMode::Track);
        assert_eq!(next_repeat(RepeatMode::Track), RepeatMode::None);
    }

    #[test]
    fn disabled_transport_is_kept_only_when_some_button_still_works() {
        assert!(TransportCaps::all_disabled().none_enabled());
        assert!(!TransportCaps { previous: false, ..TransportCaps::all_enabled() }.none_enabled());
    }
}
