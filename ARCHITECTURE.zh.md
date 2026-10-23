# 架构

Dynamic Edge 是贴在屏幕顶部正中的 Windows 悬浮层。一个固定大小的窗口在三条形态之间变形：收起的细线、悬停胶囊、展开的卡片。几何和策略放在库里，不打开窗口也能测。GPUI 二进制拥有窗口、全部线程和每一种副作用。

从上往下读就是进程实际走的路：启动，三条循环，一次指针采样变成形态，一条通知变成队头，然后绘制。

## 线程

`IslandView` 只活在 GPUI 线程上，别的线程不能改它。它们把一个小值拷进通道，或者通过 `view.update` 回来。`view.update` 在拥有这份视图的线程上跑闭包，和 Win32 里 `SendMessage` 回到窗口线程是同一件事。

```
钩子线程 --ScreenPoint-->  GPUI 线程 (IslandView)  --VolumeCommand--> 音量 STA
托盘线程 --Command------>       |                          |
       ^                         |-- media::send / seek --> 后台 executor
       |                         |-- media::current, toasts::poll（1 秒）
       |                         |-- perf::Sampler::sample（1 秒，自己的循环）
       +------ DOCK_HWND（原子量）--+
```

| 线程 | 怎么泵消息 | 拥有什么 | 交给 GPUI 线程什么 |
|---|---|---|---|
| GPUI | GPUI 运行时 | `IslandView`、窗口样式、置顶请求 | 不交；它就是所有者 |
| 鼠标钩子 | `PeekMessage` 加 30ms 睡眠 | `WH_MOUSE_LL` | `mpsc` 上的 `ScreenPoint` |
| 托盘 | `GetMessage` | 通知图标、前台钩子、2 秒置顶定时器 | `mpsc` 上的 `Command` |
| 音量 STA | 阻塞 `recv` | `IAudioEndpointVolume`、COM 公寓 | `mpsc` 上的 `Reading` |
| 后台 executor | GPUI 线程池 | 一次性的 `WinRT` 和 `sysinfo` 调用 | 结果经 `view.update` 回来 |

音量接口在 STA 线程上创建，不是 `Send`。编译器拒绝把它挪走。托盘线程初始化自己的公寓，从不碰那个指针。`Sampler` 也不共享：性能循环把它移进 executor 做一次 `sample`，再移回来。每次新建 `System` 会让 CPU 停在 0，因为占用率是相对上一次刷新的差值。

视图上的 `Option<T>` 就是显式的空：还没 arm、没有拖动、没有通知。调用方先匹配再使用。

## 模块

| 文件 | 所在 crate | 职责 |
|---|---|---|
| `src/lib.rs` | lib | 再导出纯逻辑模块。开了 `missing_docs`。 |
| `src/island.rs` | lib | 形状、命中测试、变形计算、两页切换、全屏闩。没有 I/O。 |
| `src/notice.rs` | lib | 通知队列和抢占。没有自己的线程。 |
| `src/volume.rs` | lib | 滚轮和滑条规则，外加 `Endpoint` 线程。 |
| `src/perf.rs` | lib | CPU、内存、网络采样。整个进程共用一个 `System`。 |
| `src/media.rs` | lib | SMTC 快照、拖动定位、传输控制、封面缓存。 |
| `src/main.rs` | bin | 窗口、DWM 边框、隐藏任务栏按钮、托盘线程。 |
| `src/island_view/mod.rs` | bin | 视图。订阅每条通道，拥有循环，负责绘制。 |
| `src/island_view/chrome.rs` | bin | 外壳绘制：胶囊行、卡片体、通知页、音量叠加层。 |
| `src/island_view/media_page.rs` | bin | 卡片的媒体半边：封面、进度、会话、传输。 |
| `src/perf_view.rs` | bin | 卡片的性能半边，以及翻页箭头。 |
| `src/pointer.rs` | bin | 低级鼠标钩子和点击穿透。 |
| `src/toasts.rs` | bin | 对 `UserNotificationListener` 做一次轮询。 |

`island.rs` 做决定，`island_view` 去执行。一条不依赖窗口就能说清的规则，放在库里，旁边是锁住它的测试。`chrome`、`media_page`、`perf_view` 只画；它们收到的监听器已经捕获了视图。

## 什么跨过线程

| 值 | 从 | 到 | 为什么安全 |
|---|---|---|---|
| `ScreenPoint` | 钩子 | GPUI | 小结构体的拷贝。钩子回调除此之外什么都不做。 |
| `Command` | 托盘 | GPUI | 枚举的拷贝。用户操作的 `ShowWindow` 由视图调用，托盘不调用。 |
| `HWND`，存成 `AtomicIsize` | GPUI | 托盘 | release 存储配 acquire 加载。只写一次。 |
| `VolumeCommand` / `Reading` | GPUI 与 STA | 对方 | 只有数字。COM 指针留在 STA 上。 |
| `SEEK_REFUSED` | 后台 seek | GPUI | `AtomicBool`。普通 `bool` 跨线程是数据竞争。 |
| `CoverCache` | 轮询线程 | 轮询线程 | `OnceLock<Mutex<_>>`。初始化一次，之后靠这把锁。 |
| `PerfSample` | 后台 executor | GPUI | 一个 `Copy` 值。`Sampler` 本身跟着任务走，再回来。 |

钩子过程和托盘窗口过程都是函数指针，带不了上下文参数。钩子把 `Sender` 放在 thread local。托盘把 `Sender` 放进 `OnceLock`（写一次，之后读取不加锁），再把栈上 `Tray` 的裸指针放进 thread local。这个指针在 `Tray` drop 之前清掉。

## 启动和退出

1. `tray::spawn` 启动托盘线程，并阻塞到图标创建完成。这样快速退出不会和这条线程赛跑。`Receiver<Command>` 回到 `main`。
2. `open_island` 向 `stage_frame` 要一块 420 乘 360 的舞台，在主显示器上水平居中，顶边贴着屏幕（`STAGE_TOP` 是 0）。显示器尺寸为零就退出进程。
3. `gpui_kit::open_window` 建一个无边框、不激活、背景透明的窗口。`hide_system_border` 去掉 DWM 边框并设置 `WS_EX_TOOLWINDOW`，shell 就不会长出任务栏按钮。这里调用一次 `SetWindowPos(HWND_TOPMOST)`。之后的再次置顶归托盘。
4. 新视图上的延迟初始化调用 `IslandView::arm` 和 `take_tray`。arm 启动钩子线程、音量 STA，以及下面三条 detach 的循环。
5. 窗口已经在屏幕上之后，`show_without_taskbar` 先隐藏，再用 `SW_SHOWNOACTIVATE` 显示。一个已经显示过的 `Normal` 窗口会保住任务栏按钮，除非在清掉 `WS_EX_APPWINDOW` 之后再隐藏一次。`tray::watch_layer` 用 release 语义写下 hwnd。

退出就是 drop 的顺序。`Endpoint` 先丢掉命令发送端，STA 的 `recv` 得到断开，然后再 join。先 join 会死锁：那条线程正堵在 `recv` 里。`PointerWatch` 写下 `HOOK_STOP` 再 join。这里不能用 `PostThreadMessage(WM_QUIT)`，因为那条线程用 peek，退出消息会被丢掉。托盘的 `Drop` 去掉图标、事件钩子、定时器和消息窗口。三条循环没有 `JoinHandle`。视图没了之后 `view.update` 失败，循环在下一拍自己结束。

## 视图上的三条循环

`IslandView::arm` 把三条都 detach。除了 `view.update` 里面，它们都不碰视图。

### 32ms 排空

依次是 `poll_pointer`、`poll_tray`、`poll_volume`，然后是卡片空闲检查。这个循环不向外调用。

`poll_pointer` 排空上一拍之后排队的全部 `ScreenPoint`。有释放就结束进度拖动。最后一个点从屏幕像素换成窗口局部的逻辑像素（`(物理坐标 - 原点) / scale`），交给 `hover_at`。进度拖动进行中时，同一个 x 只改局部比例。`over_card` 喂给 `watch_card`。不在细线上时 `set_click_through` 设置 `WS_EX_TRANSPARENT`；进入热区就清掉。然后是 `apply_hover`。

`poll_tray` 匹配 `Toggle`（独占隐藏期间忽略）、`Quit`、`ExclusiveHide`、`ExclusiveShow`。

`poll_volume` 留下最新的一条 `Reading`。最后一次被接受的步进过了 `OVERLAY_HOLD`（1500ms），叠加层时间戳清掉。

状态是胶囊时，这一拍会 `cx.notify`，跑马灯和音量叠加层在两次 1 秒轮询之间也能动。

### 1 秒的媒体和通知

`media::current` 跑在 `background_executor` 上。没有 `seek_drag` 时，快照拷进视图；有拖动就把这份快照丢掉，时钟继续跟着指针。`duration_secs == 0` 会调用 `mark_seek_unavailable`。然后循环睡 1 秒，跑 `toasts::poll`。`absorb_toasts` 消化结果。

传输（`media::send`）不在这条循环上，也不 join。播放器可以让 `TrySkipNextAsync` 永远不完成，把调用线程冻住。`dispatch_transport` 把命令启动，返回后再读一次 `current`。

### 1 秒的性能

这是另一条任务。`Sampler::warm` 在 executor 上跑一次。每一拍把 sampler 移进 executor，调用 `sample`，再移回来。`System` 既不共享，也不重建。`PerfHistory::push` 把四条序列都封在 60 个点。只有 `tab == Perf` 且状态不是细线时才 `cx.notify`。藏着的图表不会重绘媒体页。换页不清历史。

## 一次指针采样

从物理像素到新形态走这条路。

1. `on_mouse` 拷贝 `x`、`y`、`pressed`、`released` 就返回。钩子线程在两次 peek 之间睡 30ms，采样最多旧这么多。
2. 32ms 循环排空通道。视图处于隐藏或独占全屏时，释放仍会结束进度拖动，然后直接返回，不做悬停。
3. `hover_at(state, x, y)` 选热区：

   | 形状 | 热区 |
   |---|---|
   | 细线 | 整个舞台宽（420），深 8px |
   | 胶囊、卡片 | 宽 220px，深 80px |
   | 卡片命中（`over_card`） | `CARD_WIDTH` 乘 `CARD_HEIGHT`（420 乘 288），只给空闲计时器用 |

4. `apply_hover(Inside)` 把 `hover_ticket` 加一，调用 `note_dwell`。在细线上，第一拍记下 `dwell_since`，装一个 150ms 的定时器（`DWELL_MS`）。定时器只有在票据还对、状态仍是细线、并且 `dwell_ready` 为真时才打开胶囊。指针离开再进来会换一张新票据，旧定时器直接返回。
5. `apply_hover(Outside)` 清掉 `dwell_since`，调用 `schedule_tuck`。收起只从正在显示媒体（没有通知队头）的胶囊上启动。它把票据加一，等 `HOVER_LEAVE_MS`（100ms），票据和状态都还对得上才进入细线。

`go` 是 `state` 的唯一写入者。它记下 `from`、`to`、新的 `generation` 和 `started_ms`。绘制路径用 `ease_out_quint` 做缓动，这一代还有效就在 16ms 后再要一帧。

## 一次本体点击

本体监听器装在 `paint_motion` 里，只在窗口不是点击穿透时才会触发，所以细线收不到它。

1. `gesture_handled` 已经置上，说明某个控件用 `claim` 接走了这次按下。标志清掉，点击不再做别的。
2. 卡片上，点在左上角 116px 的方块里会调用 `media::open_player`，不折起。
3. 正在显示通知的胶囊上，`pin` 按住队头。6 秒定时器不再让它过期。这次点击仍继续到第 5 步，所以胶囊带着被钉住的通知打开成卡片。
4. 卡片上先 `dismiss` 丢掉队头，再折起。
5. `after_body_click` 把胶囊映成卡片，把卡片映成胶囊。细线还是细线。

进度、会话、模式和传输按钮在本体监听器之前调用 `claim` 和 `stop_propagation`。翻页箭头同样如此，然后调用 `turn_page`。

## 一次卡片空闲

指针在卡片上时，`watch_card` 从指针排空里调用；`card_left_ms` 有值时，32ms 循环也会调用。这样指针不再产生采样，空闲仍然会计时。

在卡片上，离开卡片矩形就记下 `card_left_ms`。再进入就清掉。满 `CARD_IDLE_MS`（2000ms）后，视图调用 `dismiss` 和 `go(Capsule)`。卡片不直接收到细线。之后离开胶囊才会收成细线，而且只有队列正在显示媒体时才收。

## 通知队列

`Queue` 的队头就是胶囊和卡片正在显示的内容。没有队头就是媒体。视图不从队尾取内容来画。

| 事件 | 队列 | 形状 |
|---|---|---|
| 收起或胶囊状态下来了通知 | 插到下标 0，清掉钉住和徽章，6 秒停留重新计时 | 细线长成胶囊 |
| 卡片打开时来了通知 | 推到队尾，亮起 `badge` | 卡片不动 |
| 5 秒内应用、标题、正文都相同 | 丢掉 | 不变 |
| 停留到期，且没有钉住、没有挂起 | 移除队头，给下一个队头装计时器 | 指针已经离开且队列空了，胶囊才收起 |
| 在显示通知的胶囊上点击本体 | `pin` | 打开卡片；停留计时不再让它过期 |
| 卡片折起，或卡片空闲到期 | `dismiss` 队头 | 胶囊 |
| 轮询不再列出某个 id | `retain_ids` 丢掉它，除非 `local` | 下一个队头，或回到媒体 |
| 独占全屏 | `suspended` | 停留不计时；新通知留在队列里 |

`arm_notice` 睡 `NOTICE_HOLD_MS`（6 秒），然后调用 `finish_notice`。只有这个 id 仍是队头且没有被钉住才移除。一个更新的通知插到下标 0，会让这次醒来变成空操作，因为 `head_id` 已经对不上，而 `push_notice` 已经给新队头装了新计时器。还有下一个队头就立刻给它装计时器。队列空了且指针不在胶囊上，岛收成细线。

第一次轮询是操作中心里的存量。`absorb_toasts` 把这些 id 记进 `seen_toasts` 然后返回，不显示。队列里已经有队头，或者悬浮层正处于独占隐藏时，不走这条捷径。之后的轮询只把不在 `seen_toasts` 里的 id 浮出来。监听器不再报告的 id，会从队列和已见集合里一起去掉。

有队头时 `turn_page` 直接返回，`card_body` 画 `notice_page` 而不是滑动的两页。那一页上没有箭头。

## 页面

`Tab` 只有 `Media` 和 `Perf`。`step_tab` 在两者之间交换。没有计时器页、启动器页或剪贴板页。

细线以外，水平滚轮，或者按住 Alt 的垂直滚轮，调用 `turn_page`。`expanded_body` 上的两个箭头也走这里。有通知队头就挡住这次交换。`page_slide` 在 `PAGE_SLIDE_MS`（180ms）里从 0（媒体）滑到 1（性能），`advance_page` 每 16ms 推进一步。

没有按 Alt 的垂直滚轮是音量，而且只有 `wheel_applies` 为真才生效：胶囊升起且端点存在。打开的卡片会把它吃掉。见下面的音量。

## 音量

`wheel_applies` 是门。`accumulate_wheel` 把增量折进 `wheel_pending`。触控板的像素攒到 `PIXEL_STEP`（40px）才是一步，一步是 `WHEEL_STEP`（2%）。这一步不会改变端点时（已经是 0% 或 100%）什么都不发送。从静音往上滚发送 `SetLevel`，标量从 0 抬起来时 STA 清除静音。`apply_volume` 把命令送出去；`flash_volume` 让叠加层在最后一次被接受的步进之后停留 `OVERLAY_HOLD`（1500ms）。32ms 循环在到期时清掉 `overlay_until` 并重绘。`island` 里的 `VOLUME_HOLD_MS`（800ms）不是这个计时器。

## 性能

`perf::Sampler` 为整个进程保留一个 `sysinfo::System` 和一个 `Networks`。每一拍只调用 `refresh_cpu_usage` 和 `refresh_memory`，然后 `networks.refresh(true)`。`received` / `transmitted` 是上次刷新以来的字节数，除以经过的毫秒（至少 1）。回环名（`lo`、`lo0`、以 `loopback` 开头的）不算进去。

`perf_page` 画三块表：CPU、内存、网络。网络把下载画成面积，把上传画成第二条描边，两者都按 `network_ceiling` 缩放。标题行是 `clock_label`。不采样 GPU、磁盘、温度和进程列表。

## 全屏闩

`HideLatch` 只为独占全屏隐藏（`QUNS_RUNNING_D3D_FULL_SCREEN`）。无边框全屏（浏览器 F11、视频播放器）保持可见。把铺满显示器的窗口当成全屏的那条退路，只在这次查询失败时才走；它分不清独占和无边框，所以选择隐藏。

隐藏要等 `FULLSCREEN_DEBOUNCE_MS`（300ms），前台切换才不会把悬浮层闪一下。重新显示是立刻的：任一 `hide == false` 的采样清掉 `pending_since`，如果当时是隐藏的，就报告一次变化。用户从托盘隐藏过悬浮层时，`USER_HIDDEN` 挡住这次显示。

真正发生变化时，托盘调用 `ShowWindow`（`SW_SHOWNA` 或 `SW_HIDE`），并投递 `ExclusiveHide` 或 `ExclusiveShow`。视图的 `hold_for_exclusive` 置上 `suspended`，停留就不再走。`release_exclusive` 清掉它；如果还有队头在等且岛是细线，就长成胶囊并给那个队头装上计时器。

## 变形

`frame_shape` 选时长：任一端是 `Line` 时用 `TUCK_MS`（280ms），否则用 `MORPH_MS`（520ms）。`blend` 把 `resting(from)` 插向 `resting(to)`。`detail` 是卡片内容的不透明度。细线和胶囊之间的混合把它保持在 0，因为这次交换从不打开卡片。

`resting` 对细线和胶囊是闭合的胶囊，对卡片是进度为 1 的展开卡片。

## 拖动

水平拖动在鼠标按下时记下光标和窗口原点（`remember_stage`）。每次移动都用这两个锚点重算左沿。把增量加起来会在丢掉一帧移动时漂移。`SWP_NOZORDER` 让这次拖动不碰 z 序；置顶归托盘线程。

进度拖动是另一件事。`SeekGesture` 只改一个局部比例，鼠标抬起之前不调用 SMTC，抬起时正好发送一次 `media::seek`。在打开且可定位的卡片上，方向键同走这个调用，每次挪 5 秒。会话拒绝之后 `SEEK_REFUSED` 保持为真，`face` 报 `seekable: false`，进度条画成只读。整个拖动期间，1 秒快照被忽略。

## Z 序

`EVENT_SYSTEM_FOREGROUND` 是快路径。托盘线程上 2 秒的 `SetTimer` 补上钩子漏掉的变化。只有 `WS_EX_TOPMOST` 不在，或者有一个可见窗口压在悬浮层上面时，`needs_topmost_raise` 才为真。已经在最上面还调用 `SetWindowPos(HWND_TOPMOST)` 会重排这个带，产生闪烁。标志是 `SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE`。悬浮层从不取得焦点，也从不经 `HWND_NOTOPMOST` 弹一下再置顶。

## 绘制

`Render::render` 先造一个 `Face`，再调用 `paint_motion`。`face` 是快照：形状、当前页、通知队头（独占隐藏时压掉）、按拖动修正过的进度、经过 `seek_refused` 之后的 `seekable`，以及性能历史。绘制路径不再回头借用视图。

`paint_motion` 把监听器装好，然后进入 `chrome::paint`：

1. 胶囊标题：通知跑马灯，否则媒体行，否则性能胶囊行。
2. 细线和胶囊画紧凑行。卡片调用 `card_body`。
3. 有通知队头就画 `notice_page`（应用、标题、正文，以及徽章那一行）。没有箭头，也不滑动。
4. 否则 `expanded_body` 把媒体页和 `perf_page` 并排放，按 `page_slide * CARD_WIDTH` 平移。滑向 1 时媒体页淡出。箭头压在两页上面。

`media_page` 是 96px 的封面、进度列，然后在会话提供时画出会话标记和模式标记。音量叠加层是胶囊的兄弟节点，只在 `overlay_until` 还在未来时可见。
