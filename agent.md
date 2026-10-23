# Dock window layer

The dock stays visible over ordinary windows, including borderless fullscreen
(browser F11, windowed video players). Exclusive fullscreen hides it.

## Topmost

- `SetWindowPos(HWND_TOPMOST)` runs once when the overlay is shown, then again
  only when the dock is no longer the top of the topmost band.
- The tray thread owns the check. `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)`
  reasserts immediately. A 2s `SetTimer` on the same thread is the fallback.
- A pass calls `SetWindowPos` only when `WS_EX_TOPMOST` is missing or
  `GetWindow(GW_HWNDPREV)` finds a visible window above the dock. Repeating
  the call while already on top flickers.
- Flags are `SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE`. The dock never takes focus.

## Exclusive hide

- Hide only when `SHQueryUserNotificationState` returns
  `QUNS_RUNNING_D3D_FULL_SCREEN`. `QUNS_BUSY` also matches browser F11 and
  borderless video, so it does not hide.
- If that query fails, a window whose rect covers its monitor counts as
  fullscreen. That fallback cannot tell exclusive from borderless, so it hides.
- The hide waits 300ms (`HideLatch`). Leaving exclusive fullscreen shows the
  dock at once with `SW_SHOWNA`.
- While hidden, the notice queue does not expire and new toasts stay queued.
  Leaving fullscreen surfaces the head.
- A dock the user hid from the tray stays hidden when fullscreen ends.

## Forbidden

- Do not bounce `HWND_NOTOPMOST` then `HWND_TOPMOST`.
- Do not call `SetForegroundWindow`, and do not call `SetWindowPos` without
  `SWP_NOACTIVATE`.
- Do not parent the dock to the foreground window.
