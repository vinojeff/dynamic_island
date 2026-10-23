# Architecture

Dynamic Edge is a top-center Windows overlay. One fixed window morphs between a
tucked line, a hover capsule, and an expanded card. Geometry and policy live in
a library so they can be tested without opening a window. The GPUI binary owns
the window, the threads, and every side effect.

Read this top to bottom and you are walking the same path the process walks:
startup, the three loops, a pointer sample becoming a shape, a toast becoming
the head, then paint.

## Threads

`IslandView` lives on the GPUI thread. Nothing else mutates it. The other
threads copy a small value into a channel, or come back through
`view.update`, which runs the closure on the thread that owns the view (the
same arrangement as `SendMessage` back to a window thread).

```
hook thread --ScreenPoint-->  GPUI thread (IslandView)  --VolumeCommand--> volume STA
tray thread --Command------>       |                          |
       ^                            |-- media::send / seek --> background executor
       |                            |-- media::current, toasts::poll (1s)
       |                            |-- perf::Sampler::sample (1s, its own loop)
       +------ DOCK_HWND (atomic) --+
```

| Thread | Pumps | Owns | Hands the GPUI thread |
|---|---|---|---|
| GPUI | GPUI runtime | `IslandView`, window styles, z-order requests | nothing; it is the owner |
| Mouse hook | `PeekMessage` + 30ms sleep | `WH_MOUSE_LL` | `ScreenPoint` over `mpsc` |
| Tray | `GetMessage` | notify icon, foreground hook, 2s topmost timer | `Command` over `mpsc` |
| Volume STA | blocking `recv` | `IAudioEndpointVolume`, COM apartment | `Reading` over `mpsc` |
| Background executor | GPUI pool | one-shot `WinRT` and `sysinfo` calls | results through `view.update` |

The volume interface is created on the STA thread and is not `Send`. The
compiler rejects moving it. The tray thread initializes its own apartment and
never touches that pointer. `Sampler` is also not shared: the perf loop moves
it onto the executor for one `sample` call and moves it back. Rebuilding a
`System` each tick would freeze CPU at 0, because usage is a diff.

`Option<T>` is the explicit null on the view: not armed yet, no drag, no
notice. Callers match before use.

## Modules

| File | Crate | Role |
|---|---|---|
| `src/lib.rs` | lib | Re-exports the pure modules. `missing_docs` is on. |
| `src/island.rs` | lib | Shapes, hit testing, morph math, the two-page swap, fullscreen latch. No I/O. |
| `src/notice.rs` | lib | Toast queue and preemption. No thread of its own. |
| `src/volume.rs` | lib | Wheel and slider rules, plus the `Endpoint` thread. |
| `src/perf.rs` | lib | CPU, memory, and network samples. One `System` for the process. |
| `src/media.rs` | lib | SMTC snapshot, seek, transport, cover cache. |
| `src/main.rs` | bin | Window, DWM frame, taskbar hiding, tray thread. |
| `src/island_view/mod.rs` | bin | The view. Subscribes to every channel, owns the loops, paints. |
| `src/island_view/chrome.rs` | bin | Shell paint: capsule row, card body, notice page, volume overlay. |
| `src/island_view/media_page.rs` | bin | Media half of the card: cover, seek, sessions, transport. |
| `src/perf_view.rs` | bin | Performance half of the card, and the page chevrons. |
| `src/pointer.rs` | bin | Low-level mouse hook and click-through. |
| `src/toasts.rs` | bin | One poll of `UserNotificationListener`. |

`island.rs` decides. `island_view` applies. A rule that can be stated without
a window belongs in the library, next to the test that locks it. `chrome`,
`media_page`, and `perf_view` only paint; the listeners they receive already
close over the view.

## What crosses a thread

| Value | From | To | Why it is safe |
|---|---|---|---|
| `ScreenPoint` | hook | GPUI | 16-byte copy. The hook callback does nothing else. |
| `Command` | tray | GPUI | enum copy. The tray never calls `ShowWindow` for user actions; the view does. |
| `HWND` as `AtomicIsize` | GPUI | tray | release store, acquire load. Written once. |
| `VolumeCommand` / `Reading` | GPUI and STA | the other | numbers only. The COM pointer stays on the STA. |
| `SEEK_REFUSED` | background seek | GPUI | `AtomicBool`. A plain `bool` would be a data race. |
| `CoverCache` | poll threads | poll threads | `OnceLock<Mutex<_>>`. Init once, then the mutex. |
| `PerfSample` | background executor | GPUI | one `Copy` value. The `Sampler` itself moves with the task and comes back. |

The hook procedure and the tray window procedure are function pointers, so
neither can carry a context argument. The hook keeps its `Sender` in a thread
local. The tray keeps its `Sender` in a `OnceLock` (written once, read with no
lock) and a raw pointer to its stack `Tray` in a thread local. That pointer is
cleared before the `Tray` drops.

## Startup and shutdown

1. `tray::spawn` starts the tray thread and blocks until the icon exists, so a
   fast quit cannot race it. The `Receiver<Command>` returns to `main`.
2. `open_island` asks `stage_frame` for a 420 by 360 stage, centered on the
   primary display, top flush (`STAGE_TOP` is 0). A zero-size display bails
   and the process quits.
3. `gpui_kit::open_window` builds a borderless, non-activating, transparent
   window. `hide_system_border` strips the DWM frame and sets
   `WS_EX_TOOLWINDOW` so the shell does not grow a taskbar button. One
   `SetWindowPos(HWND_TOPMOST)` runs here. Later raises belong to the tray.
4. The deferred init on the new view calls `IslandView::arm` and
   `take_tray`. Arm starts the hook thread, the volume STA, and the three
   detached loops below.
5. Once the window is on screen, `show_without_taskbar` hides and shows with
   `SW_SHOWNOACTIVATE`. A `Normal` window that has already been shown keeps
   its taskbar button unless it is hidden once after `WS_EX_APPWINDOW` is
   cleared. `tray::watch_layer` release-stores the hwnd.

Drop order is the shutdown. `Endpoint` drops its command sender first, so
the STA's `recv` returns disconnected, then joins. Joining first deadlocks:
the thread is blocked in `recv`. `PointerWatch` stores `HOOK_STOP` and
joins. `PostThreadMessage(WM_QUIT)` would not work there, because that
thread peeks and the quit would be discarded. The tray `Drop` removes the
icon, the event hook, the timer, and the message window. The three loops
have no `JoinHandle`. When the view is gone, `view.update` fails and each
loop ends on its next tick.

## Three loops on the view

`IslandView::arm` detaches all three. None of them touches the view except
inside `view.update`.

### 32ms drain

`poll_pointer`, `poll_tray`, `poll_volume`, then the card-idle check. This
loop never calls out.

`poll_pointer` drains every `ScreenPoint` queued since the last tick. A
release finishes the seek. The last point is converted from screen pixels to
window-local logical pixels (`(physical - origin) / scale`) and handed to
`hover_at`. While a seek drag is active the same x moves the local fraction.
`over_card` feeds `watch_card`. Off the line, `set_click_through` sets
`WS_EX_TRANSPARENT`; entering the hot band clears it. Then `apply_hover`.

`poll_tray` matches `Toggle` (ignored during exclusive hide), `Quit`,
`ExclusiveHide`, and `ExclusiveShow`.

`poll_volume` keeps the newest `Reading`. The overlay timestamp is cleared
`OVERLAY_HOLD` (1500ms) after the last accepted step.

While the state is `Capsule`, the tick calls `cx.notify` so the marquee and
the volume overlay keep moving between the 1s polls.

### 1s media and toasts

`media::current` runs on `background_executor`. The snapshot is copied onto
the view unless `seek_drag` is set, in which case it is dropped and the clock
keeps following the pointer. A snapshot with `duration_secs == 0` calls
`mark_seek_unavailable`. Then the loop sleeps 1s and runs `toasts::poll`.
`absorb_toasts` applies the result.

Transport (`media::send`) is not on this loop and is not joined. A player can
leave `TrySkipNextAsync` unfinished and freeze the caller. `dispatch_transport`
starts the command, then reads one fresh `current` when it returns.

### 1s performance

A separate task. `Sampler::warm` runs once on the executor. Each tick moves
the sampler onto the executor, calls `sample`, and moves it back, so the
`System` is never shared and never rebuilt. `PerfHistory::push` caps each of
the four series at 60. `cx.notify` runs only when `tab == Perf` and the state
is not `Line`. A hidden chart does not repaint the media page. Changing pages
does not clear the history.

## One pointer sample

This is the path from a physical pixel to a new shape.

1. `on_mouse` copies `x`, `y`, `pressed`, `released` and returns. The hook
   thread sleeps 30ms between peeks, so a sample is at most that stale.
2. The 32ms loop drains the channel. Hidden or exclusive-fullscreen views
   still finish a seek on release, then return without hover.
3. `hover_at(state, x, y)` picks the band:

   | Shape | Hot band |
   |---|---|
   | Line | full stage width (420), 8px deep |
   | Capsule, card | 220px wide, 80px deep |
   | Card hit (`over_card`) | `CARD_WIDTH` by `CARD_HEIGHT` (420 by 288), idle timer only |

4. `apply_hover(Inside)` increments `hover_ticket` and calls `note_dwell`.
   On a line, the first sample records `dwell_since` and arms a 150ms timer
   (`DWELL_MS`). The timer opens the capsule only if the ticket still matches,
   the state is still `Line`, and `dwell_ready` is true. A pointer that leaves
   and comes back gets a new ticket; the old timer returns.
5. `apply_hover(Outside)` clears `dwell_since` and calls `schedule_tuck`.
   Tuck is armed only from a capsule that is showing media (no toast head).
   It increments the ticket, waits `HOVER_LEAVE_MS` (100ms), and goes to
   `Line` only if the ticket and the state still match.

`go` is the only writer of `state`. It records `from`, `to`, a new
`generation`, and `started_ms`. The render path eases with `ease_out_quint`
and requests another frame 16ms later while that generation is current.

## One body click

The body listener is installed in `paint_motion` and only fires when the
window is not click-through, so a line never receives it.

1. If `gesture_handled` is set, a control already took the press (`claim`).
   The flag clears and the click does nothing else.
2. On a card, a press inside the top-left 116px square calls `media::open_player`
   and does not fold.
3. On a capsule that is showing a toast, `pin` holds the head. The 6s timer
   will no longer expire it. The click still falls through to step 5, so the
   capsule opens into the card with the toast pinned.
4. On a card, `dismiss` drops the head before the fold.
5. `after_body_click` maps capsule to card and card to capsule. A line stays
   a line.

Seek, session, mode, and transport buttons call `claim` and
`stop_propagation` before the body listener runs. The page chevrons do the
same and call `turn_page`.

## One card idle

`watch_card` runs from the pointer drain when the pointer is over the card,
and from the 32ms loop while `card_left_ms` is set (so the idle still elapses
if the pointer stops producing samples).

On a card, leaving the card rect stores `card_left_ms`. Re-entering clears it.
After `CARD_IDLE_MS` (2000ms) the view calls `dismiss` and `go(Capsule)`.
The card does not tuck to the line. A later leave from the capsule does that,
and only when the queue is showing media.

## Notice queue

The head of `Queue` is what the capsule and the card show. No head means media.
The view never draws from the tail.

| Event | Queue | Shape |
|---|---|---|
| Toast while tucked or on the capsule | insert at 0, clear pin and badge, restart the 6s hold | line grows into the capsule |
| Toast while the card is open | push at the tail, raise `badge` | card stays |
| Same app, title, and body within 5s | dropped | unchanged |
| Hold elapsed, not pinned, not suspended | head removed, next head armed | capsule tucks if the pointer is gone and the queue is empty |
| Body click on a capsule that shows a toast | `pin` | opens the card; the hold no longer expires it |
| Card folds, or card idles out | `dismiss` the head | capsule |
| Poll no longer lists an id | `retain_ids` drops it, unless `local` | next head or media |
| Exclusive fullscreen | `suspended` | hold does not run; arrivals stay queued |

`arm_notice` sleeps `NOTICE_HOLD_MS` (6s) and then calls `finish_notice`.
That removes the id only if it is still the head and not pinned. A newer
toast inserted at 0 makes the woken timer a no-op, because `head_id` no
longer matches, and `push_notice` has already armed a fresh timer for the
new head. If a next head exists it is armed immediately. If the queue is
empty and the pointer is not on the capsule, the island tucks.

The first poll is the Action Center backlog. `absorb_toasts` records those
ids in `seen_toasts` and returns without showing them, unless the queue
already has a head or the dock is exclusive-hidden. Later polls only surface
ids that were not in `seen_toasts`. Ids the listener dropped are removed from
both the queue and the seen set.

`turn_page` returns immediately while a head exists, and `card_body` paints
`notice_page` instead of the sliding pages. The chevrons are not on that page.

## Pages

`Tab` is `Media` or `Perf`. `step_tab` swaps them. There is no timer, launcher,
or clipboard page.

A horizontal wheel, or a vertical wheel with Alt, on anything but the line
calls `turn_page`. So do the two chevrons in `expanded_body`. A toast head
blocks the swap. `page_slide` animates from 0 (media) to 1 (performance) over
`PAGE_SLIDE_MS` (180ms), stepped every 16ms by `advance_page`.

A vertical wheel without Alt is volume, and only while
`wheel_applies` is true: the capsule is up and an endpoint exists. An open
card swallows it. See Volume below.

## Volume

`wheel_applies` is the gate. `accumulate_wheel` folds the delta into
`wheel_pending`. Touchpad pixels accumulate until `PIXEL_STEP` (40px), which
is one `WHEEL_STEP` (2%). A step that would not change the endpoint (already
0% or 100%) sends nothing. Rolling up from mute sends `SetLevel`, and the STA
clears mute when the scalar comes up from zero. `apply_volume` sends the
command; `flash_volume` holds the overlay for `OVERLAY_HOLD` (1500ms) after
the last accepted step. The 32ms loop clears `overlay_until` and repaints
when that elapses. `VOLUME_HOLD_MS` (800ms) in `island` is not this timer.

## Performance

`perf::Sampler` keeps one `sysinfo::System` and one `Networks` for the process.
Each tick calls `refresh_cpu_usage` and `refresh_memory` only, then
`networks.refresh(true)`. `received` / `transmitted` are the bytes since that
last refresh, divided by the elapsed milliseconds (at least 1). Loopback
names (`lo`, `lo0`, anything starting with `loopback`) are left out.

`perf_page` paints three meters: CPU, memory, network. Network draws download
as the area and upload as a second stroke, both scaled by `network_ceiling`.
The title row shows `clock_label`. GPU, disk, temperature, and the process
list are not sampled.

## Fullscreen latch

`HideLatch` hides for exclusive fullscreen only
(`QUNS_RUNNING_D3D_FULL_SCREEN`). Borderless fullscreen (browser F11, video
players) stays visible. The fallback that treats a monitor-sized window as
fullscreen runs only when that query fails, and it cannot tell exclusive from
borderless, so it hides.

A hide waits `FULLSCREEN_DEBOUNCE_MS` (300ms) so a foreground switch does not
flicker the dock. Showing again is immediate: any sample with `hide == false`
clears `pending_since` and, if the dock was hidden, reports a change.
`USER_HIDDEN` blocks the show when the user hid the dock from the tray.

On a real change the tray calls `ShowWindow` (`SW_SHOWNA` or `SW_HIDE`) and
posts `ExclusiveHide` or `ExclusiveShow`. The view's `hold_for_exclusive`
sets `suspended` so the hold does not run. `release_exclusive` clears it,
and if a head is waiting and the island is a line, grows the capsule and
arms that head.

## Morph

`frame_shape` picks the duration: `TUCK_MS` (280ms) when either end is `Line`,
otherwise `MORPH_MS` (520ms). `blend` lerps `resting(from)` toward
`resting(to)`. `detail` is the card-content opacity. A line/capsule blend
keeps it at 0, because that exchange never opens the card.

`resting` is the closed pill for line and capsule, and the open card at
progress 1.

## Drag

A horizontal drag stores the cursor and the window origin at mouse-down. Each
move recomputes the left edge from those two anchors (`remember_stage`).
Summing deltas would drift when a move is dropped. `SWP_NOZORDER` keeps the
drag out of the z-order; the tray thread owns topmost.

A seek drag is separate. `SeekGesture` updates a local fraction and does not
call SMTC until mouse-up, which sends exactly one `media::seek`. Arrow keys
on an open, seekable card nudge 5 seconds through the same call. If the
session refuses, `SEEK_REFUSED` sticks and `face` reports `seekable: false`,
so the bar paints read-only. The 1s snapshot is ignored for the whole drag.

## Z-order

`EVENT_SYSTEM_FOREGROUND` is the fast path. A 2s `SetTimer` on the tray thread
covers a change the hook misses. `needs_topmost_raise` is true only when
`WS_EX_TOPMOST` is missing or a visible window sits above the dock. Calling
`SetWindowPos(HWND_TOPMOST)` while already on top reorders the band and
flickers. Flags are `SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE`. The dock never
takes focus, and it is never bounced through `HWND_NOTOPMOST`.

## Paint

`Render::render` builds a `Face` and calls `paint_motion`. `face` is a
snapshot: shape, tab, the notice head (suppressed while exclusive), the
seek-adjusted progress, `seekable` after `seek_refused`, and the perf history.
The render path does not re-borrow the view.

`paint_motion` builds the listeners, then `chrome::paint`:

1. Capsule title: toast marquee, else the media line, else the perf capsule line.
2. Line and capsule paint the compact row. A card calls `card_body`.
3. A notice head paints `notice_page` (app, title, body, and the badge line).
   No chevrons, no slide.
4. Otherwise `expanded_body` places the media page and `perf_page` side by side
   and translates by `page_slide * CARD_WIDTH`. Media fades as the slide goes
   to 1. The chevrons sit above both pages.

`media_page` is the 96px cover, the seek column, then the session mark and
the mode marks when the session offers them. The volume overlay is a sibling
of the pill, visible only while `overlay_until` is in the future.
