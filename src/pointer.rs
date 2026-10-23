//! Global pointer samples, because a click-through window sees no moves.
//!
//! While the island is a line and the pointer is outside the 8px band, the
//! view sets `WS_EX_TRANSPARENT` on the stage. The window then receives no
//! `WM_MOUSEMOVE`, so it cannot observe the pointer that would open it.
//! [`PointerWatch::spawn`] installs `WH_MOUSE_LL` on its own thread instead.
//!
//! That callback runs on the installing thread, and only while the thread
//! pumps. [`on_mouse`] copies one [`ScreenPoint`] (physical pixels, plus
//! press and release bits) into a channel and returns. It must not touch a
//! window: a low-level hook sits on the system input path, and a slow
//! callback stalls the mouse for every process. The hook procedure is a
//! function pointer, so the `Sender` lives in a thread local.
//!
//! The thread peeks, sleeps 30ms, and peeks again. The view drains the
//! channel every 32ms and converts the point into window-local logical
//! pixels before [`rustedge::island::hover_at`].
//!
//! Drop stores [`HOOK_STOP`] and joins. `PostThreadMessage(WM_QUIT)` would
//! not work: the idle peek discards the quit. [`set_click_through`] is called
//! from the GPUI thread, not from the hook.

use std::cell::Cell;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, PeekMessageW, SetWindowsHookExW, UnhookWindowsHookEx, PM_REMOVE, WH_MOUSE_LL,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
};

/// Screen point, physical pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScreenPoint {
    /// Horizontal.
    pub x: i32,
    /// Vertical.
    pub y: i32,
    /// The button went down. Moves and releases leave this false.
    pub pressed: bool,
    /// The button came up. Moves and presses leave this false.
    pub released: bool,
}

use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_LAYERED, WS_EX_TRANSPARENT,
};

/// Owns the hook thread. Drop posts `WM_QUIT` and joins.
pub struct PointerWatch {
    thread: Option<JoinHandle<()>>,
    points: Receiver<ScreenPoint>,
}

impl PointerWatch {
    /// Starts the hook. Move events arrive on [`Self::drain`].
    #[must_use]
    pub fn spawn() -> Self {
        let (sender, points) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::spawn(move || run_hook(&sender, &ready_tx));
        let _thread_id = ready_rx.recv().ok().unwrap_or(0);
        Self { thread: Some(thread), points }
    }

    /// Points posted since the last drain.
    #[must_use]
    pub fn drain(&self) -> Vec<ScreenPoint> {
        self.points.try_iter().collect()
    }

}

impl Drop for PointerWatch {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        HOOK_STOP.store(true, std::sync::atomic::Ordering::SeqCst);
        let _joined = thread.join();
    }
}

// The hook procedure is a function pointer, so it cannot carry a context
// argument. The sender is touched only by the hook thread, so it lives in a
// thread local rather than a static mutex: taking a lock inside the callback
// can deadlock against the UI thread.
thread_local! {
    static HOOK_SENDER: Cell<Option<Sender<ScreenPoint>>> = const { Cell::new(None) };
}

// The hook cannot be stopped with `PostThreadMessage(WM_QUIT)` from another
// thread. This thread drains with `PeekMessage`, not `GetMessage`, so a posted
// quit is discarded by the idle peek. Both sides load and store the flag.
// `SeqCst` only guarantees that a load of `true` sees every store that
// happened before the matching store.
static HOOK_STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn run_hook(sender: &Sender<ScreenPoint>, ready: &Sender<u32>) {
    // SAFETY: a null module and thread id of 0 installs a global low-level
    // mouse hook on this thread. The proc does not dereference foreign state.
    let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(on_mouse), std::ptr::null_mut(), 0) };
    // SAFETY: querying the current thread id has no precondition.
    let thread_id = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
    if hook.is_null() {
        let _sent = ready.send(0);
        return;
    }
    HOOK_SENDER.with(|slot| slot.set(Some(sender.clone())));
    let _sent = ready.send(thread_id);
    HOOK_STOP.store(false, std::sync::atomic::Ordering::SeqCst);
    let mut message = windows_sys::Win32::UI::WindowsAndMessaging::MSG::default();
    // The system delivers a low-level hook only while this thread pumps.
    // `PeekMessage` returns as soon as the queue is empty, so the thread has to
    // sleep or it busy-waits. 30ms is the sampling cap, not a timer resolution.
    while !HOOK_STOP.load(std::sync::atomic::Ordering::SeqCst) {
        while unsafe { PeekMessageW(&raw mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) } > 0 {}
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    // SAFETY: `hook` was returned by `SetWindowsHookExW` on this thread.
    unsafe {
        let _removed = UnhookWindowsHookEx(hook);
    }
    HOOK_SENDER.with(|slot| slot.set(None));
}

unsafe extern "system" fn on_mouse(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0
        && (wparam == WM_MOUSEMOVE as WPARAM
            || wparam == WM_LBUTTONDOWN as WPARAM
            || wparam == WM_LBUTTONUP as WPARAM)
    {
        // SAFETY: for `WH_MOUSE_LL`, `lparam` points at an `MSLLHOOKSTRUCT`
        // that lives for this call. Only `pt` is read.
        let point = unsafe { &*(lparam as *const Msll) }.pt;
        let pressed = wparam == WM_LBUTTONDOWN as WPARAM;
        let released = wparam == WM_LBUTTONUP as WPARAM;
        // `Cell` cannot lend the sender out and take it back. Take it, send,
        // then put it back. The send copies a 16-byte point and allocates
        // nothing. A failed send means the other end is gone; drop the sample.
        HOOK_SENDER.with(|slot| {
            let sender = slot.take();
            if let Some(sender) = sender.as_ref() {
                let _sent = sender.send(ScreenPoint { x: point.x, y: point.y, pressed, released });
            }
            slot.set(sender);
        });
    }
    // SAFETY: a null hook asks the system for the next hook in the chain.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

/// Lets clicks fall through the window while the dock is only the edge line.
pub fn set_click_through(hwnd: HWND, through: bool) {
    if hwnd.is_null() {
        return;
    }
    // SAFETY: `hwnd` is a live window owned by the caller. `GWL_EXSTYLE` is a
    // valid index. The returned style is a bitfield, not a pointer.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    if style == 0 {
        return;
    }
    #[allow(clippy::cast_possible_wrap)]
    let layered = WS_EX_LAYERED as isize;
    #[allow(clippy::cast_possible_wrap)]
    let transparent = WS_EX_TRANSPARENT as isize;
    let next = if through { style | layered | transparent } else { (style | layered) & !transparent };
    // SAFETY: same hwnd, and `next` is the previous style with two documented bits changed.
    unsafe {
        let _previous = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, next);
    }
}

#[repr(C)]
struct Msll {
    pt: POINT,
    mouse_data: u32,
    flags: u32,
    time: u32,
    extra: usize,
}
