//! Process entry, the overlay window, and the tray thread.
//!
//! Startup, in order:
//!
//! 0. [`claim_single_instance`] takes `Local\rustedge.overlay`. A second
//!    launch sees `ERROR_ALREADY_EXISTS` and returns before any window exists.
//!    The handle stays alive until the process exits, which is what releases it.
//! 1. [`tray::spawn`] starts the tray thread and blocks until the icon exists,
//!    so a fast quit cannot race it. The command `Receiver` comes back here.
//! 2. [`open_island`] asks [`rustedge::island::stage_frame`] for the 420 by 360
//!    stage, centered on the primary display, top flush with the screen.
//! 3. `gpui_kit::open_window` builds the window. Before it is shown,
//!    [`hide_system_border`] strips the DWM caption and sets
//!    `WS_EX_TOOLWINDOW` so the shell does not grow a taskbar button. One
//!    `SetWindowPos(HWND_TOPMOST)` runs here. Later raises belong to the tray.
//! 4. The deferred init calls [`IslandView::arm`] and [`IslandView::take_tray`].
//!    Arm starts the hook, the volume STA, and the three poll loops.
//! 5. Once the window is on screen, [`show_without_taskbar`] hides and shows
//!    with `SW_SHOWNOACTIVATE`. A `Normal` window that has already been shown
//!    keeps its taskbar button unless it is hidden once after
//!    `WS_EX_APPWINDOW` is cleared. [`tray::watch_layer`] publishes the hwnd.
//!
//! The tray thread is the rest of this file. It pumps `GetMessage`, owns the
//! notify icon, and runs [`tray::maintain_layer`] from
//! `EVENT_SYSTEM_FOREGROUND` plus a 2s timer. That pass raises only when
//! [`rustedge::island::needs_topmost_raise`] says the dock is no longer the
//! top of the topmost band, and hides only after [`rustedge::island::HideLatch`]
//! accepts exclusive fullscreen. User toggles and quit are `Command`s; the view
//! applies them. The tray does not call `ShowWindow` for those.
//!
//! Release builds are a GUI subsystem so double-clicking the exe does not
//! open a console. Debug builds keep the console for the process log.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod island_view;
mod perf_view;
mod pointer;
mod toasts;

use anyhow::Context as _;
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DwmSetWindowAttribute,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GWL_STYLE, GetWindowLongPtrW, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SetWindowLongPtrW, SetWindowPos, WS_CAPTION,
    WS_EX_APPWINDOW, WS_EX_TOOLWINDOW, WS_SYSMENU, WS_THICKFRAME,
};
use gpui_kit::{
    App, AppContext, Bounds, Fill, Pixels, Styled, Window, WindowBackgroundAppearance,
    WindowBounds, WindowKind, WindowOptions, point, px, size, transparent_black,
};
use gpui_kit::base::Root;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, CloseHandle};
use windows::Win32::System::Threading::CreateMutexExW;
use windows::core::PCWSTR;

use crate::island_view::IslandView;
use rustedge::island::stage_frame;

/// Local name. `Local\` keeps the lock inside this logon session.
const INSTANCE_MUTEX: &str = "Local\\rustedge.overlay\0";

/// Who won the named mutex. The handle is the lock.
enum InstanceClaim {
    /// This process created the name. Hold the handle until exit.
    First(InstanceLock),
    /// Another process already owns the name, or the lock could not be created.
    AlreadyRunning,
}

/// An owned mutex handle. Drop closes it, which releases the name.
struct InstanceLock(windows::Win32::Foundation::HANDLE);

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // SAFETY: the handle came from `CreateMutexExW` and has not been closed.
        unsafe {
            let _closed = CloseHandle(self.0);
        }
    }
}

/// Create the named mutex, or report that another instance already holds it.
///
/// A null name would be an unnamed mutex and would not exclude anything.
/// If the call fails, this process does not start: it cannot prove it is alone.
fn claim_single_instance() -> InstanceClaim {
    let name: Vec<u16> = INSTANCE_MUTEX.encode_utf16().collect();
    // SAFETY: the name is a null-terminated UTF-16 string that outlives the call.
    // A null security attributes pointer is the default DACL. `MUTEX_ALL_ACCESS`
    // is the same access `CreateMutexW` requests.
    let created = unsafe { CreateMutexExW(None, PCWSTR(name.as_ptr()), 0, 0x001F_0001) };
    let Ok(handle) = created else {
        return InstanceClaim::AlreadyRunning;
    };
    // SAFETY: `GetLastError` reads this thread's last error. `CreateMutexExW`
    // sets `ERROR_ALREADY_EXISTS` before returning the existing handle.
    let already = unsafe { windows::Win32::Foundation::GetLastError() } == ERROR_ALREADY_EXISTS;
    let lock = InstanceLock(handle);
    if already {
        InstanceClaim::AlreadyRunning
    } else {
        InstanceClaim::First(lock)
    }
}

fn main() {
    // The guard must stay alive until `run` returns. Dropping it early releases
    // the name, and a second click can open another window.
    let InstanceClaim::First(_instance) = claim_single_instance() else {
        return;
    };
    let commands = tray::spawn();
    gpui_kit::application().run(move |app| {
        gpui_kit::init(app);
        if open_island(app, commands).is_err() {
            app.quit();
        }
    });
}

fn open_island(app: &mut App, commands: std::sync::mpsc::Receiver<tray::Command>) -> anyhow::Result<()> {
    let display = app
        .primary_display()
        .context("no primary display is available")?;
    let display_width = logical_pixels(display.bounds().size.width)?;
    let frame = stage_frame(display_width);
    if frame.width == 0 || frame.height == 0 {
        anyhow::bail!("primary display has no usable size");
    }

    let origin = display.bounds().origin;
    let bounds = Bounds::new(
        point(origin.x + px(logical_f32_i32(frame.x)), origin.y + px(logical_f32_i32(frame.y))),
        size(px(logical_f32(frame.width)), px(logical_f32(frame.height))),
    );
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        focus: false,
        show: true,
        kind: WindowKind::Normal,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        display_id: Some(display.id()),
        window_background: WindowBackgroundAppearance::Transparent,
        window_min_size: Some(bounds.size),
        ..Default::default()
    };

    let (window, _view) = gpui_kit::open_window(options, app, |window, cx| {
        // Before the window is shown the DWM frame may not exist yet.
        hide_system_border(window);
        cx.new(|cx| {
            let view = IslandView::new();
            cx.defer_in(window, |view: &mut IslandView, _window, cx| {
                view.arm(cx);
                view.take_tray(commands);
            });
            view
        })
    })
    .context("gpui-kit refused to open the overlay window")?;
    // gpui-component's WindowState plugin paints the root with the theme background.
    // Root styles refine after that hook, so this clears the stage without removing the plugin.
    window
        .update(app, |root, window, cx| {
            if let Some(root) = window.root::<Root>().flatten() {
                root.update(cx, |root, _cx| {
                    root.style().background = Some(Fill::Color(transparent_black().into()));
                });
            }
            cx.notify(root.entity_id());
            // The window is on screen now, so DWM will honor the border color.
            // Hide then show drops the taskbar button the first show registered.
            hide_system_border(window);
            show_without_taskbar(window);
            tray::watch_layer(window_hwnd(window));
        })
        .ok();
    Ok(())
}

fn window_hwnd(window: &Window) -> windows_sys::Win32::Foundation::HWND {
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return std::ptr::null_mut();
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return std::ptr::null_mut();
    };
    #[allow(clippy::cast_possible_truncation)]
    {
        handle.hwnd.get() as windows_sys::Win32::Foundation::HWND
    }
}

/// Drop the system frame GPUI leaves on a borderless Windows window.
///
/// `WindowKind::Normal` is created with `WS_SYSMENU`. DWM still paints that frame:
/// a light 1px border while inactive, and a dark line along the top while active.
/// `DWMWA_BORDER_COLOR = DWMWA_COLOR_NONE` only recolors it, and Windows restores
/// the color on activation (zed-industries/zed#61508). Clearing the frame styles
/// and sending `SWP_FRAMECHANGED` makes `WM_NCCALCSIZE` give the whole window to
/// the client area. Failures are ignored.
fn hide_system_border(window: &mut Window) {
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };

    #[allow(clippy::cast_possible_truncation)]
    let hwnd = handle.hwnd.get() as windows_sys::Win32::Foundation::HWND;
    let color = DWMWA_COLOR_NONE;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let attribute = DWMWA_BORDER_COLOR as u32;
    #[allow(clippy::cast_possible_truncation)]
    let size = std::mem::size_of_val(&color) as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_possible_wrap)]
    let frame = (WS_CAPTION | WS_THICKFRAME | WS_SYSMENU) as isize;
    #[allow(clippy::cast_possible_wrap)]
    let tool = WS_EX_TOOLWINDOW as isize;
    #[allow(clippy::cast_possible_wrap)]
    let app = WS_EX_APPWINDOW as isize;
    unsafe {
        let _status = DwmSetWindowAttribute(hwnd, attribute, std::ptr::from_ref(&color).cast(), size);
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        if style != 0 {
            let _previous = SetWindowLongPtrW(hwnd, GWL_STYLE, style & !frame);
        }
        // Read-modify-write. Click-through may already have set layered/transparent.
        // `WS_EX_APPWINDOW` forces a taskbar button. `WS_EX_TOOLWINDOW` keeps it off.
        let extended = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if extended != 0 {
            let _previous = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, (extended & !app) | tool);
        }
        // SWP_FRAMECHANGED is required or the taskbar keeps the old frame.
        // HWND_TOPMOST once at show. The tray thread reasserts it afterwards;
        // a single call here loses to the next fullscreen window.
        let _moved = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// Drops the taskbar button, then shows the overlay without activating it.
///
/// A `Normal` window that has already been shown keeps its taskbar button even
/// after `WS_EX_TOOLWINDOW` is set. Hiding it and showing it again, with
/// `WS_EX_APPWINDOW` clear, makes the shell forget that button.
fn show_without_taskbar(window: &mut Window) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNOACTIVATE, ShowWindow};

    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    #[allow(clippy::cast_possible_truncation)]
    let hwnd = handle.hwnd.get() as windows_sys::Win32::Foundation::HWND;
    unsafe {
        let _hidden = ShowWindow(hwnd, SW_HIDE);
        let _shown = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

/// Tray icon that never touches the volume STA.
///
/// The icon needs its own `HWND` and window procedure; shell callbacks arrive
/// as messages to that window. `IAudioEndpointVolume` is pinned to a different
/// STA thread. A COM object is not shared across apartments unless it was
/// marshaled, so the two threads each call `CoInitializeEx` and never share
/// the pointer. The tray posts a `Command` on a channel. The GPUI thread is
/// the only one that acts on it.
mod tray {
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
    use windows_sys::Win32::UI::Shell::{
        NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
    };
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, EVENT_SYSTEM_FOREGROUND, GW_HWNDPREV, GetCursorPos,
    GetForegroundWindow, GetMessageW, GetSystemMetrics, GetWindow, GetWindowLongPtrW, GetWindowRect,
    GWL_EXSTYLE,
    HICON, HMENU, HWND_TOPMOST, IMAGE_ICON, IsIconic, IsWindowVisible, KillTimer, LR_DEFAULTSIZE,
    LoadImageW, MF_STRING, MSG, PostQuitMessage, RegisterClassW, SM_CXSCREEN,
    SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNA, SetForegroundWindow,
    SetTimer, SetWindowPos, ShowWindow, TPM_BOTTOMALIGN, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTALIGN,     TrackPopupMenu, TranslateMessage, WM_APP, WM_COMMAND, WM_CONTEXTMENU,
    WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP, WM_TIMER, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WINEVENT_SKIPOWNTHREAD, WNDCLASSW, WS_EX_TOPMOST,
};

    /// What the GPUI loop should do. The tray thread only posts these.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum Command {
        /// Show the dock if it is hidden, otherwise hide it.
        Toggle,
        /// Ask GPUI to quit. Drop impls then tear down SMTC and the hook.
        Quit,
        /// Exclusive fullscreen started. Hide the dock and park the notice queue.
        ExclusiveHide,
        /// Exclusive fullscreen ended. Show the dock without taking focus.
        ExclusiveShow,
    }

    const SHOW_ID: u16 = 1;
    const QUIT_ID: u16 = 2;
    const TRAY_ID: u32 = 1;
    const TRAY_CALLBACK: u32 = WM_APP + 1;
    /// `SetTimer` id for the z-order fallback.
    const LAYER_TIMER: usize = 1;
    /// Fallback interval. The foreground hook is the fast path.
    const LAYER_INTERVAL_MS: u32 = 2_000;

    /// Starts the icon thread and returns the command channel.
    pub fn spawn() -> Receiver<Command> {
        let (sender, commands) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        thread::spawn(move || run(&sender, &ready_tx));
        // The icon must exist before the overlay, or a fast quit races the thread.
        let _ready = ready_rx.recv();
        commands
    }

    struct Tray {
        window: HWND,
        icon: HICON,
        hook: windows_sys::Win32::UI::Accessibility::HWINEVENTHOOK,
        /// Debounced exclusive-fullscreen hide. Only this thread writes it.
        hide: std::cell::Cell<rustedge::island::HideLatch>,
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            unsafe {
                let _removed = Shell_NotifyIconW(NIM_DELETE, &notify_data(self.window, self.icon, true));
                if !self.hook.is_null() {
                    let _unhooked = windows_sys::Win32::UI::Accessibility::UnhookWinEvent(self.hook);
                }
                let _killed = KillTimer(self.window, LAYER_TIMER);
                if !self.icon.is_null() {
                    let _destroyed = DestroyIcon(self.icon);
                }
                if !self.window.is_null() {
                    let _destroyed = DestroyWindow(self.window);
                }
            }
        }
    }

    fn run(commands: &Sender<Command>, ready: &Sender<()>) {
        // SAFETY: this thread has not called CoInitializeEx. Apartment matches
        // the volume endpoint, but that endpoint lives on a different thread,
        // so the two STA initializations do not nest.
        // SAFETY: this thread has not initialized COM. The volume endpoint uses
        // its own STA thread, so the two `CoInitializeEx` calls do not nest.
        #[allow(clippy::cast_sign_loss)]
        let com = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
        if com < 0 {
            let _sent = ready.send(());
            return;
        }
        let tray = install(commands);
        let _sent = ready.send(());
        let Some(tray) = tray else {
            unsafe { CoUninitialize() };
            return;
        };
        // The stack `Tray` is stored as a raw pointer in a thread local. The
        // window procedure has no context argument, so this is how it finds its
        // object again. The hook and the timer both fire on this thread, so no
        // other thread reads the pointer. Clear it before `tray` drops or it
        // dangles.
        TRAY_STATE.with(|slot| slot.set(std::ptr::from_ref(&tray)));
        let mut message = MSG::default();
        // SAFETY: `message` is a stack MSG. A null hwnd drains this thread's queue.
        // `GetMessage` blocks while the queue is empty. The hook thread instead
        // peeks and sleeps.
        while unsafe { GetMessageW(&raw mut message, std::ptr::null_mut(), 0, 0) } > 0 {
            unsafe {
                let _translated = TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
        }
        TRAY_STATE.with(|slot| slot.set(std::ptr::null()));
        drop(tray);
        unsafe { CoUninitialize() };
    }

    fn install(commands: &Sender<Command>) -> Option<Tray> {
        let class_name: Vec<u16> = "RustedgeTray\0".encode_utf16().collect();
        let title: Vec<u16> = "rustedge\0".encode_utf16().collect();
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        if instance.is_null() {
            return None;
        }
        let class = WNDCLASSW {
            lpfnWndProc: Some(tray_proc),
            hInstance: instance,
            lpszClassName: class_name.as_ptr(),
            ..unsafe { std::mem::zeroed() }
        };
        let atom = unsafe { RegisterClassW(&raw const class) };
        if atom == 0 {
            return None;
        }
        let window = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                instance,
                std::ptr::null_mut(),
            )
        };
        if window.is_null() {
            return None;
        }
        let icon = load_icon();
        let added = unsafe { Shell_NotifyIconW(NIM_ADD, &notify_data(window, icon, false)) };
        if added == 0 {
            unsafe {
                let _destroyed = DestroyWindow(window);
            }
            return None;
        }
        if TRAY_SLOT.set(commands.clone()).is_err() {
            return None;
        }
        // Out of context: the callback runs on this thread, which already pumps
        // messages for the tray. In-context hooks need the target thread's queue.
        let hook = unsafe {
            windows_sys::Win32::UI::Accessibility::SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                std::ptr::null_mut(),
                Some(on_foreground),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS | WINEVENT_SKIPOWNTHREAD,
            )
        };
        // SAFETY: `window` is the message-only window created above. A 2s timer
        // covers a foreground change the hook misses.
        let _timer = unsafe { SetTimer(window, LAYER_TIMER, LAYER_INTERVAL_MS, None) };
        Some(Tray {
            window,
            icon,
            hook,
            hide: std::cell::Cell::new(rustedge::island::HideLatch::visible()),
        })
    }

    /// `winresource::WindowsResource::set_icon` embeds `assets/app.ico` as icon
    /// resource id `1`. Loading it from this module means a copied release exe
    /// still has a tray image; the source-tree `assets/` path does not travel
    /// with the binary.
    const EMBEDDED_ICON_ID: u16 = 1;

    fn load_icon() -> HICON {
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        if instance.is_null() {
            return std::ptr::null_mut();
        }
        // MAKEINTRESOURCEW(1): the low word is the ordinal, the high word is zero.
        let name = EMBEDDED_ICON_ID as usize as *const u16;
        // SAFETY: `instance` is this executable. `name` is an integer resource id,
        // not a string pointer. `LR_SHARED` is omitted so `Drop` may `DestroyIcon`.
        unsafe { LoadImageW(instance, name, IMAGE_ICON, 0, 0, LR_DEFAULTSIZE) as HICON }
    }

    fn notify_data(window: HWND, icon: HICON, delete: bool) -> NOTIFYICONDATAW {
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        #[allow(clippy::cast_possible_truncation)]
        {
            data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        }
        data.hWnd = window;
        data.uID = TRAY_ID;
        if !delete {
            data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
            data.uCallbackMessage = TRAY_CALLBACK;
            data.hIcon = icon;
            let tip = "rustedge";
            let tip_slots = data.szTip.split_last_mut().map_or(&mut [][..], |(_, slots)| slots);
            for (slot, unit) in tip_slots.iter_mut().zip(tip.encode_utf16()) {
                *slot = unit;
            }
        }
        data
    }

    // The window procedure is a function pointer and cannot close over the
    // sender. `OnceLock` is written once; every later read takes no lock.
    static TRAY_SLOT: std::sync::OnceLock<Sender<Command>> = std::sync::OnceLock::new();
    thread_local! {
        static TRAY_STATE: std::cell::Cell<*const Tray> = const { std::cell::Cell::new(std::ptr::null()) };
    }
    /// The dock hwnd. The tray thread reads it; the GPUI thread writes it once.
    ///
    /// An `HWND` is a pointer-sized integer. Atomics store integers, so the
    /// handle is kept as `isize`. The release store pairs with the acquire
    /// load: the tray thread observes the handle the GPUI thread published, not
    /// a torn write.
    static DOCK_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
    /// The user hid the dock from the tray. The tray thread must not show it again.
    static USER_HIDDEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// Hands the overlay hwnd to the tray thread. The hook and the 2s timer read it.
    pub fn watch_layer(dock: HWND) {
        if dock.is_null() {
            return;
        }
        #[allow(clippy::cast_possible_wrap)]
        DOCK_HWND.store(dock as isize, std::sync::atomic::Ordering::Release);
    }

    fn dock_hwnd() -> HWND {
        #[allow(clippy::cast_possible_wrap)]
        let raw = DOCK_HWND.load(std::sync::atomic::Ordering::Acquire);
        raw as HWND
    }

    const HWND_MESSAGE: HWND = -3isize as HWND;

    #[allow(clippy::too_many_arguments)]
    unsafe extern "system" fn on_foreground(
        _hook: windows_sys::Win32::UI::Accessibility::HWINEVENTHOOK,
        _event: u32,
        _hwnd: HWND,
        _object: i32,
        _child: i32,
        _thread: u32,
        _time: u32,
    ) {
        // Out-of-context hooks run on this thread, which owns `Tray`.
        // The callback arguments name the foreground window; the z-order pass
        // reads it itself through `GetForegroundWindow`.
        if let Some(sender) = TRAY_SLOT.get() {
            with_tray(|tray| maintain_layer(tray, sender));
        }
    }

    /// The tray menu asked to show or hide. Exclusive-fullscreen recovery must
    /// not pop a dock the user hid.
    pub fn note_user_visibility(visible: bool) {
        USER_HIDDEN.store(!visible, std::sync::atomic::Ordering::Release);
    }

    fn with_tray(apply: impl FnOnce(&Tray)) {
        TRAY_STATE.with(|slot| {
            let tray = slot.get();
            if tray.is_null() {
                return;
            }
            // SAFETY: the pointer is set to the stack `Tray` before the message
            // loop and cleared before that `Tray` drops. The hook and the timer
            // both run on this thread, so the reference does not escape.
            apply(unsafe { &*tray });
        });
    }

    /// One z-order pass. Raises only when the dock is not already the top of
    /// the topmost band, and hides only for exclusive fullscreen.
    fn maintain_layer(tray: &Tray, commands: &Sender<Command>) {
        let hide = &tray.hide;
        let dock = dock_hwnd();
        if dock.is_null() {
            return;
        }
        let sample = layer_sample(dock);
        if rustedge::island::needs_topmost_raise(sample) {
            // SWP_NOACTIVATE: never take focus from the foreground app.
            // Do not bounce through HWND_NOTOPMOST; that flickers.
            unsafe {
                let _raised = SetWindowPos(
                    dock,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
        let now = millis_now();
        let (next, changed) = hide.get().advance(rustedge::island::wants_exclusive_hide(sample), now);
        hide.set(next);
        if !changed {
            return;
        }
        let user_hidden = USER_HIDDEN.load(std::sync::atomic::Ordering::Acquire);
        let show = !next.hidden && !user_hidden;
        let command = if show { SW_SHOWNA } else { SW_HIDE };
        unsafe {
            let _shown = ShowWindow(dock, command);
        }
        let posted = if next.hidden { Command::ExclusiveHide } else { Command::ExclusiveShow };
        let _sent = commands.send(posted);
    }

    fn layer_sample(dock: HWND) -> rustedge::island::LayerSample {
        rustedge::island::LayerSample {
            topmost: has_topmost(dock),
            covered: covered_by_visible(dock),
            exclusive: foreground_is_exclusive(dock),
        }
    }

    fn has_topmost(dock: HWND) -> bool {
        // SAFETY: `dock` is the overlay hwnd stored by the GPUI thread.
        // `GWL_EXSTYLE` returns a bitfield. Zero means the call failed.
        let style = unsafe { GetWindowLongPtrW(dock, GWL_EXSTYLE) };
        #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
        let topmost = WS_EX_TOPMOST as isize;
        style & topmost != 0
    }

    fn covered_by_visible(dock: HWND) -> bool {
        // SAFETY: `GW_HWNDPREV` walks toward the top of the z-order. A null
        // result means the dock is already the topmost window.
        let above = unsafe { GetWindow(dock, GW_HWNDPREV) };
        if above.is_null() {
            return false;
        }
        // SAFETY: `above` came from `GetWindow` for this call.
        unsafe { IsWindowVisible(above) != 0 }
    }

    fn foreground_is_exclusive(dock: HWND) -> bool {
        use windows_sys::Win32::UI::Shell::{
            QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState,
        };
        let mut state = 0;
        // SAFETY: `state` is a writable QUERY_USER_NOTIFICATION_STATE.
        let status = unsafe { SHQueryUserNotificationState(&raw mut state) };
        // Direct3D exclusive fullscreen is the only quiet state that hides the
        // dock. `QUNS_BUSY` also matches browser F11 and borderless video, which
        // must stay visible. The rect fallback runs only when the query fails.
        if status >= 0 {
            return state == QUNS_RUNNING_D3D_FULL_SCREEN;
        }
        covers_its_monitor(dock)
    }

    /// Traditional fallback used only when `SHQueryUserNotificationState` fails.
    ///
    /// A window filling its monitor counts as fullscreen. This cannot tell
    /// exclusive from borderless, so a failed query prefers hiding over letting
    /// a real exclusive game cover the dock. The quiet-state path is the one
    /// that keeps browser F11 visible.
    fn covers_its_monitor(dock: HWND) -> bool {
        use windows_sys::Win32::Graphics::Gdi::{
            MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
        };
        // SAFETY: a null foreground window means the desktop. Nothing to hide for.
        let foreground = unsafe { GetForegroundWindow() };
        if foreground.is_null() || foreground == dock {
            return false;
        }
        // SAFETY: `foreground` is the foreground window for this call. A minimized
        // window does not cover the display, so it is not exclusive fullscreen.
        if unsafe { IsIconic(foreground) != 0 } {
            return false;
        }
        // SAFETY: `MONITOR_DEFAULTTONEAREST` returns the monitor under the window.
        let monitor = unsafe { MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST) };
        if monitor.is_null() {
            return screen_sized(foreground);
        }
        let mut info = MONITORINFO {
            cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>()).unwrap_or(0),
            ..MONITORINFO::default()
        };
        // SAFETY: `info.cbSize` is set. `monitor` came from `MonitorFromWindow`.
        let ok = unsafe { windows_sys::Win32::Graphics::Gdi::GetMonitorInfoW(monitor, &raw mut info) };
        if ok == 0 {
            return screen_sized(foreground);
        }
        let mut rect = windows_sys::Win32::Foundation::RECT::default();
        // SAFETY: `foreground` is the foreground window for this call.
        if unsafe { GetWindowRect(foreground, &raw mut rect) } == 0 {
            return false;
        }
        rect.left <= info.rcMonitor.left
            && rect.top <= info.rcMonitor.top
            && rect.right >= info.rcMonitor.right
            && rect.bottom >= info.rcMonitor.bottom
    }

    fn screen_sized(hwnd: HWND) -> bool {
        let mut rect = windows_sys::Win32::Foundation::RECT::default();
        // SAFETY: `hwnd` is the foreground window. `GetSystemMetrics` for the
        // primary screen size has no pointer argument.
        if unsafe { GetWindowRect(hwnd, &raw mut rect) } == 0 {
            return false;
        }
        let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        rect.left <= 0 && rect.top <= 0 && rect.right >= width && rect.bottom >= height
    }

    fn millis_now() -> u64 {
        u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_millis()),
        )
        .unwrap_or(0)
    }

    unsafe extern "system" fn tray_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if message == WM_TIMER && wparam == LAYER_TIMER {
            if let Some(sender) = TRAY_SLOT.get() {
                with_tray(|tray| maintain_layer(tray, sender));
            }
            return 0;
        }
        if message == TRAY_CALLBACK {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let event = lparam as u32;
            if event == WM_LBUTTONUP || event == WM_LBUTTONDBLCLK {
                let _sent = TRAY_SLOT.get().map(|sender| sender.send(Command::Toggle));
            }
            if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                show_menu(window);
            }
            return 0;
        }
        if message == WM_COMMAND {
            #[allow(clippy::cast_possible_truncation)]
            let id = (wparam as u32) & 0xffff;
            let command = match id as u16 {
                SHOW_ID => Some(Command::Toggle),
                QUIT_ID => Some(Command::Quit),
                _ => None,
            };
            if let Some(command) = command {
                let _sent = TRAY_SLOT.get().map(|sender| sender.send(command));
                if command == Command::Quit {
                    unsafe { PostQuitMessage(0) };
                }
            }
            return 0;
        }
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }

    fn show_menu(window: HWND) {
        let menu = unsafe { CreatePopupMenu() };
        if menu.is_null() {
            return;
        }
        let hidden = USER_HIDDEN.load(std::sync::atomic::Ordering::Acquire);
        let label = if hidden { "Show dock" } else { "Hide dock" };
        append(menu, SHOW_ID, label);
        append(menu, QUIT_ID, "Quit");
        let mut point = POINT::default();
        // TPM_RETURNCMD makes the choice the return value. Destroying the menu
        // before TrackPopupMenu returns used to drop the click onto the icon,
        // which the left-button handler treated as hide.
        let chosen = unsafe {
            let _cursor = GetCursorPos(&raw mut point);
            let _foreground = SetForegroundWindow(window);
            let id = TrackPopupMenu(
                menu,
                TPM_RIGHTALIGN | TPM_BOTTOMALIGN | TPM_RETURNCMD | TPM_NONOTIFY,
                point.x,
                point.y,
                0,
                window,
                std::ptr::null(),
            );
            let _destroyed = DestroyMenu(menu);
            id
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let id = chosen as u16;
        let command = match id {
            SHOW_ID => Some(Command::Toggle),
            QUIT_ID => Some(Command::Quit),
            _ => None,
        };
        let Some(command) = command else {
            return;
        };
        let _sent = TRAY_SLOT.get().map(|sender| sender.send(command));
        if command == Command::Quit {
            unsafe { PostQuitMessage(0) };
        }
    }

    fn append(menu: HMENU, id: u16, label: &str) {
        let wide: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let _added = AppendMenuW(menu, MF_STRING, id as usize, wide.as_ptr());
        }
    }
}

fn logical_pixels(pixels: Pixels) -> anyhow::Result<u32> {
    let value = f32::from(pixels);
    if !value.is_finite() || value < 0.0 {
        anyhow::bail!("display size {value} is not a usable logical width");
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(value as u32)
}

#[allow(clippy::cast_precision_loss)]
fn logical_f32(value: u32) -> f32 {
    value as f32
}

#[allow(clippy::cast_precision_loss)]
fn logical_f32_i32(value: i32) -> f32 {
    value as f32
}
