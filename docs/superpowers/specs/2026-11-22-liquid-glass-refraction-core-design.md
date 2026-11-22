# 液态玻璃折射核心 —— 设计

日期：2026-11-22
状态：待评审（已按第一轮评审修订）

## 目标

把现在的“64 环全脸缩放”换成一次 CPU SDF 折射，让圆角转角的折射方向由真实法线决定，而不是从面板中心径向发散。

具体：

- 只在边缘一条 bevel 带折射，中心保持清晰。
- 用圆角矩形 SDF 的外法线：上边中点竖直、右边水平、45° 转角对角。
- bevel 用超椭圆高度剖面，斜率经精确 Snell 得到位移，转角再加方向性增益。
- R/G/B 在略微不同的采样位置取色，色散进入 warp。
- live 和静态壁纸共用同一条 `warp` 路径。
- `lens` 从 64 次 `paint_image` 降到 1 次。

本 spec 取代 2026-11-03 设计中的“折射”一节；那版的透明度、rim、sheen 结论仍然有效。

## 非目标

- blur / 内部散射。
- squircle 轮廓（本 spec 仍是圆角矩形 SDF + GPUI 圆角裁切）。
- spec 2 的双层方向性高光 / Fresnel 层 / 内侧暗线。现有 3 条白色 rim 带保留，不在本次替换范围。
- 自适应阴影、明暗翻转、可读性 tint（spec 3）。
- 内容裁切、squircle 内容 mask。

## 已确认的选择

| 维度 | 选择 |
| --- | --- |
| 折射范围 | 边缘 bevel，中心清晰 |
| 轮廓几何 | 精确圆角矩形 SDF + 真法线 |
| blur | 本 spec 不加 |
| 实现路线 | CPU 单遍重采样，live 与静态共用 |

## 现状

- `glass::lens` 用 `LENS_RINGS = 64` 个同心环，每个环把 crop 以 shell 中心为锚点缩放 `pull`，再 `paint_image`。
- 位移方向是 `p - center`，不是 `∇SDF`。在 420×288、r=24 的卡片上，上边切点 `(w/2 - r, -h/2)` 相对面板中心的方向是 `(186, -144)`，与真实外法线 `(0,-1)` 相差约 52°。
- 旧实现的实际位移是 `P*span/(span-2P)`，不是 `P`：上边中点约 44.5px、右边缘约 40.6px、45° 转角约 57.2px。新模型把位移限制在 `depth*1.3 ≈ 37.4px` 以内。
- 转角没有局部法线，所以看起来像“斜着拉”，而不是“绕角弯”。
- 旧 rim 另有两条同心色散环，和 warp 的色散重复。

## 架构

```
静态壁纸 ── build_raw ──> RawGlass ─┐
                                    ├─> RawGlass::warp(shape) ──> GlassBackdrop ──> lens（1 次 paint_image）
WGC 帧 ── 裁剪/降采样/提饱和/luma ──> RawGlass ─┘
```

live 捕获线程仍然在自己那一侧把新帧 warp 完再发布，避免 UI 线程重采样；view 另外保留最近一帧的 `RawGlass`，在“桌面没变、只有形状变了”时重 warp。后者是这版相对早先“纯生产端 warp”的补充：WGC 只在桌面变化时出帧，morph 期间没有新帧，必须靠最近 raw 重算，否则卡片会显示胶囊形状烘出的 bevel。

### 新模块 `src/glass_optics.rs`

纯函数 + 一个 CPU 重采样 pass，不依赖 GPUI：

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlassShape { pub width: f32, pub height: f32, pub radius: f32 }

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shell { pub x: f32, pub y: f32, pub width: f32, pub height: f32, pub radius: f32 }

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Margins { pub left: f32, pub top: f32, pub right: f32, pub bottom: f32 }

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect { pub x: f32, pub y: f32, pub width: f32, pub height: f32 }
```

- `sd_round_rect(p, half, r) -> f32`：标准圆角矩形 SDF，`d < 0` 在内部。
- `sdf_normal(p, half, r) -> Option<(f32, f32)>`：中心差分 `eps = 1` 图像像素，返回**外法线** `∇sd` 的单位向量；`|∇sd| < 0.1` 时返回 `None`（中轴线保护）。
- `bevel_width(shape) -> f32`：`(min(w,h) * 0.18).clamp(10, 36).min(min(w,h) / 2)`。
- `bend_lut() -> [f32; 64]`：按 `e` 预计算归一化 Snell 弯曲，见下；与具体 bevel 宽度无关。
- `displacement(p, shell, bevel_px, margins) -> Option<(f32, f32)>`：单像素采样偏移；`None` 表示直接拷贝。
- `warp(src, width, height, shell, bevel_px, margins) -> Option<Vec<u8>>`：一次重采样，输出同尺寸 RGBA；尺寸不匹配返回 `None`。
- `shell_rect(stage_width, shape, crop, image_scale) -> Shell`：把逻辑形状换算成 crop 内的图像矩形。
- `crop_rects(bounds, shape, crop) -> (Rect, Rect)`：`lens` 用的窗口空间 shell 与 crop 自然矩形，纯函数可测。

`margins` 是 crop 四边**可见** margin 的图像像素值，用来做屏幕边缘衰减。

### 光学模型

```text
sd     = sd_round_rect(p - shell_center, half, r)   // sd < 0 在内部
d      = -sd                                        // d > 0 在内部
n      = normalize(∇sd)                             // 外法线
e      = clamp(d / bevel_px, 0, 1)
bend   = lut(e) * depth * corner_boost(n)
depth  = 0.8 * bevel_px
corner_boost(n) = 1 + 0.30 * 4 * n.x² * n.y²
```

- `lut(e)`：超椭圆剖面 `h(e) = bevel * (1 - (1-e)^p)^(1/p)`，`p = 4`；斜率 `dh/dd` 经精确 Snell（`n = 1.5`）得到 `tan(θi - θt)`，按边缘上限 `sqrt(n²-1)` 归一化到 `0..1`；再乘 `1 - smoothstep(0.75, 1.0, e)` 让内缘 C1 收口。`h` 的高度取 `bevel`，所以 LUT 与具体 bevel 宽度无关；每次 warp 前算 64 项即可，`e = 0` 直接取 1。
- `corner_boost`：直边为 0，45° 转角为 1；`4 n.x² n.y² = sin²(2θ)`，比参考项目的 `max(min(x,1-x), min(y,1-y))` 更平滑，胶囊端头也适用。
- 只在 `0 <= d < bevel_px` 的带内重采样；`d < 0`（crop 的 margin 区）和 `d >= bevel_px`（中心）都直接拷贝。
- 采样：`sample = p + (bx, by)`，其中 `bx = (n.x * bend).clamp(-margins.left, margins.right)`，`by = (n.y * bend).clamp(-margins.top, margins.bottom)`。逐轴 clamp，贴屏一侧不越界，另一轴仍保留弯曲。
- 色散：R/G/B 分别在 `p + (bx, by) * (1 - s, 1, 1 + s)` 取样，`s = 0.03`；双线性插值；alpha 取绿色采样点的 alpha，**不强制 255**，保留 `Fit`/`Center` 壁纸的透明留边。
- `corner_boost` 的测试对象是 `4 n.x² n.y²`（0..1），完整 boost 是 1..1.3。

### 边界与退化

- `bevel_px <= min(w,h)/2`，避免上下 bevel 在中线重叠。
- `|∇sd| < 0.1` 时 `displacement` 返回 `None`；胶囊中轴线在 bevel 带外，只有极薄的过渡帧会触发。
- `height < 12` 仍由 `line_glass` 接管，`lens` 不画。
- 采样坐标按 crop 边界 clamp；逐轴 margin clamp 已经保证正常路径不越界。
- `warp` 失败（尺寸不匹配、空图）返回 `None`；调用方保留上一张 `GlassBackdrop`，下一次 tick 再试。
- `REFRACTION_MARGIN` 40 → 48：`depth * 1.3 + 色散 ≈ 38.6`，留 9px 余量。

## 集成

### `src/wallpaper.rs`

`GlassBackdrop`：

```rust
pub struct GlassBackdrop {
    pub image: Arc<RenderImage>,
    pub luma: f32,
    /// Crop rect in logical px, relative to the window's top-left.
    /// Can have a positive x/y when the window hangs off the screen's left/top.
    pub crop: Rect,
    /// The logical shape the warp was baked for.
    pub shape: GlassShape,
    /// The RawGlass generation this image was built from.
    pub generation: u64,
}
```

`RawGlass`：

```rust
pub struct RawGlass {
    pub rgba: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub crop: Rect,
    pub margins: Margins,
    pub image_scale: f32,
    pub stage_width: f32,
    pub luma: f32,
    pub generation: u64,
}
```

- `RawGlass::warp(shape, luma: Option<f32>) -> Option<GlassBackdrop>`：
  - `shell = shell_rect(stage_width, shape, crop, image_scale)`。
  - `bevel_px = bevel_width(shape) * image_scale`。
  - `margins_px = margins * image_scale`。
  - 调 `glass_optics::warp`，再 `render_image_rgba`。
  - `luma` 用 `luma.unwrap_or(self.luma)`；`generation` 透传。
- `build` 拆成 `build_raw(display, rect, scale) -> Option<RawGlass>`；解码/合成/裁切/提饱和/luma 不变，末尾不再建 `RenderImage`。
- 几何换算（`left/top/right/bottom` 是物理 crop 边界，`sx/sy/sw/sh` 是裁到屏幕内的 stage）：

```text
crop.x      = (left - rect.x) / scale
crop.y      = (top  - rect.y) / scale
crop.width  = (right - left) / scale
crop.height = (bottom - top) / scale
margins.left   = (sx - left) / scale
margins.top    = (sy - top) / scale
margins.right  = (right - (sx + sw)) / scale
margins.bottom = (bottom - (sy + sh)) / scale
image_scale    = scale                  // 静态
image_scale    = scale / factor         // live
stage_width    = rect.width / scale
```

`crop.x` 在窗口贴屏时是负 margin；窗口被拖出屏幕左侧时可以是正数。`margins` 只描述“可见 stage 边缘到 crop 边缘”的距离，和 `crop` 分开。

### `src/live_glass.rs`

`CropSpec` 分成几何与 shape，shape 变化不应让最近一帧 raw 失效：

```rust
pub(crate) struct CropGeometry {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
    pub factor: u32,
    pub crop: Rect,
    pub margins: Margins,
    pub image_scale: f32,
    pub stage_width: f32,
}

pub(crate) struct CropSpec {
    pub geometry: CropGeometry,
    pub shape: GlassShape,
}
```

- `crop_spec(stage, scale, screen, shape) -> Option<CropSpec>` 计算上述字段。
- `update_crop(stage, scale, shape) -> bool`：更新 spec；返回 `true` 仅当 `geometry` 变化。shape-only 变化只更新 `shape`，不返回 `true`。
- `on_frame_arrived`：降采样/提饱和/luma 后构造 `RawGlass`（`generation` 由 `Sink` 递增），用 `spec.shape` 调 `warp`；发布 `Frame { raw, warped: Option<Arc<GlassBackdrop>>, generation }`。warp 失败时 `warped = None`，raw 仍发布。
- `Sink` 保存 `Frame`；`take_frame` 平滑 luma，把平滑值写进 `warped.luma`（若有），返回 `(Option<Frame>, Action)`。

### `src/island_view/mod.rs`

32ms 循环在 `poll_live` 之后调用 `poll_warp(cx)`；2s 的 `poll_glass` 只负责静态壁纸指纹与 `build_raw`。

- `poll_live`：
  - 取 `Frame`；`live_generation = frame.generation`，`last_live_raw = Some(frame.raw)`。
  - 有 `warped` 就直接安装；没有就留给 `poll_warp`。
  - `update_crop` 返回 `true`（DPI/crop 变化）时清空 `last_live_raw`、`live_generation`，回退到 `static_glass`，直到新几何的帧到达。`static_glass` 由同一 rect 的静态构建保证正确。
- `poll_warp`：
  - 当前 `shape = GlassShape::from(self.shown.shape.get())`。
  - live 且 `last_live_raw` 存在，并且 `glass.generation != raw.generation || glass.shape != shape` → 请求 Live warp。
  - 静态且 `static_raw` 存在，并且 `glass.generation != raw.generation || glass.shape != shape` → 请求 Static warp。
  - 同一 `(generation, shape, dest)` 已在 pending/busy 中则跳过。
- warp 队列：

```rust
enum WarpDest { Live, Static }

struct WarpRequest {
    raw: Arc<RawGlass>,
    shape: GlassShape,
    dest: WarpDest,
    generation: u64,
    ticket: u64,
}

warp_pending: Option<WarpRequest>,
warp_busy: bool,
warp_ticket: u64,
live_generation: u64,
static_generation: u64,
```

  - `request_warp`：`warp_ticket += 1`，覆盖 pending；空闲就启动后台 `raw.warp(shape, luma)`。Live 的 `luma` 用当前 `glass_luma`；Static 用 `None`。
  - `finish_warp`：**无论成功失败**先 `warp_busy = false`；只有 `ticket == warp_ticket` 才安装：
    - Live：`live.is_live()` 且 `generation == live_generation` → 写 `glass`。
    - Static：`generation == static_generation` → 总是写 `static_glass`；`!live.is_live()` 时同时写 `glass`。
  - 失败（`None`）保留旧图，打日志；随后启动 pending。pending 为空时队列保持空闲。
  - 切换 live/static：bump `glass_epoch`、清 `warp_pending`；在飞的 completion 会因 ticket/generation 失败而丢弃。
- `poll_glass`（2s）：
  - 后台比较指纹；只有 fingerprint 真的变化时才 bump `glass_epoch` 并启动 `build_raw`，不再每次轮询都 bump。
  - 成功：`static_raw = Some(Arc::new(raw))`，`static_generation = epoch`，`request_warp(Static)`。
  - 失败：`static_raw = None`、`static_glass = None`；静态在管时 `glass = None`，回退 wash。
- `restore_static_glass`：清 `last_live_raw`、`live_generation`、`warp_pending`；保留 `static_raw`；bump epoch；下一次 `poll_warp` 按当前形状重 warp。

### `src/island_view/glass.rs`

- `lens` 用 `crop_rects` 得到 `current_shell` 与 `natural`：
  - `backdrop.shape == shape`：`image_bounds = natural`，1:1。
  - 形状过期：把 `natural` 从 baked shell 轴向缩放到 current shell，避免旧 bevel 留在面板内部形成“鬼环”：
    ```text
    sx = shape.width  / backdrop.shape.width
    sy = shape.height / backdrop.shape.height
    image.x      = current_shell.x + (natural.x - baked_shell.x) * sx
    image.y      = current_shell.y + (natural.y - baked_shell.y) * sy
    image.width  = natural.width  * sx
    image.height = natural.height * sy
    ```
  - `paint_image(current_shell, image_bounds, Corners::all(radius), image, 0, false)`。
  - 过期帧的 bevel 宽度会随轴向缩放；32ms 队列落地后回到 1:1。这是过渡帧的折中，不会留下永久错误。
- 删除 `LENS_RINGS`、`REFRACTION_PULL`、`refraction_depth`、`refraction_pull`、`lens_bounds`、`shrink_factor`、`inset_shell`、`paint_lens`、`paint_crop`、`bounds_from` 及对应测试。
- `paint_rim` 删除两条色散环和 `DISPERSION` 常量；三条白色 Fresnel 带保留。
- `shell_offset` 改成 `shell_offset(stage_width, shape_width)`，`lens` 用 canvas 宽度，`RawGlass` 用 `stage_width`，两边共用同一公式。
- `veil_alpha`、`rim_alpha`、`shell_shadows`、`layers` 的 scrim/sheen 不变。
- `GlassBackdrop` 的测试构造补 `crop`、`shape`、`generation`。

### `src/main.rs` 与文档

- 增加 `mod glass_optics;`，更新模块文档的像素管线说明。
- 更新 `ARCHITECTURE.md` 中 64 环透镜、`GlassBackdrop` 字段和 WGC 帧槽的描述。

## 测试

### `glass_optics` 纯函数

- SDF：边上为 0、内部为负、外部为正；角点距离等于圆心距离；四边/四角对称。
- 法线：上边中点 `(0,-1)`、右边 `(1,0)`、45° 角 `(0.707,-0.707)`；上边与圆角的切点 `(w/2-r, -h/2)` 处与竖直夹角 < 5°（直接回归旧实现 52° 的方向误差）。
- bevel：card = 36、capsule ≈ 10.08、12px 薄形状 = 6；`<= min(w,h)/2`。
- Snell：`e=0 → 1`、`e=1 → 0`、单调；用一个已知斜率做数值核对；`n=1.5` 上限归一化。
- corner：`4 n.x² n.y²` 在直边 0、45° 1、范围 `[0,1]`；完整 boost 在 `[1,1.3]`。
- `displacement`：
  - 上边中点偏移指向正上方，大小 ≈ `depth`。
  - 45° 角偏移方向与 `∇sd` 夹角 < 5°。
  - 切点附近方向仍接近竖直。
  - 最大偏移 `<= margin`。
  - `margins.left = 0` 且 45° 角时，y 分量不被清零（逐轴 clamp 的回归）。
  - `d < 0` 的 margin 区返回 `None`（不重采样）。
  - 直边剖面：切点与中点的 bend 接近，向内侧单调下降（回归旧的过位移）。
- `warp`：中心像素完全不变；crop margin 区不变；透明源保留 alpha；尺寸不匹配返回 `None`；输出尺寸正确。
- `shell_rect`：card 居中偏移 0；capsule 偏移 `(stage_width - width)/2`；窗口被拖出屏幕左侧（`crop.x > 0`）时 shell.x 为负；`image_scale` 正确缩放。
- `crop_rects`：card/capsule 的 shell 与自然矩形；过期 shape 的轴向缩放公式。

### 集成

- `RawGlass::warp`：`crop`、`shape`、`generation`、`luma` 透传；输出 `RenderImage` 尺寸正确。
- `CropSpec`：shape 变化只更新 shape，geometry 不变时 `update_crop` 返回 `false`；`image_scale = scale/factor`；`stage_width` 正确。
- warp 队列：Live 帧安装后，一个 Static completion 不覆盖 `glass`；Live completion 在更新帧到达后按 generation 丢弃；失败 completion 清 `warp_busy` 并启动 pending。
- 失败路径：`build_raw` 失败清 `static_raw`/`static_glass`；warp 失败保留旧图。
- `luma`：Live 重 warp 使用当前平滑值，不回退到 raw luma。
- 旧 64 环测试删除；`crop_spec` 测试的 margin 从 40 改 48（crop 边界随之变化）。

### 视觉

- 棋盘格壁纸：网格线在四角绕角弯折，不再被斜向拉伸。
- 照片壁纸：文字/枝叶在边缘仍可读，中心清晰。
- 胶囊端头：半圆端头的折射方向沿半径，不出现“斜拉”。
- 贴屏边缘：贴边一侧不弯，另一侧正常。
- 窗口拖出屏幕：crop 与 shell 不错位。

## 风险

- debug 构建的 CPU warp 可能每帧 10~20ms；release 约 1~3ms。若 capture 线程跟不上，先降级为“view 端 warp”（同一 `RawGlass` 路径，只换执行位置，UI 线程只负责派发）。
- 极薄形状（高度 12~20px）上下 bevel 在中线重叠，靠 `|∇sd|` 保护和 `height < 12` 禁用兜底。
- 过期 shape 用轴向缩放过渡，bevel 宽度会短暂失真；32ms 队列落地后恢复。若视觉上仍明显，改为 morph 结束后再 warp。
- `REFRACTION_MARGIN` 增大会让静态 crop 略大，解码/合成成本几乎不变。
- 色散符号/强度需要视觉调参；先给 `s = 0.03`，不要一次调太大。
- 内存：live 同时保留 raw RGBA、warped `RenderImage` 和上一张 `glass`，约 2~3MB；`Sink`/`CropSpec` 继续用 mutex，`Arc` 跨线程共享，warp 结果只在 UI 线程安装。
- 窗口部分出屏时 `crop` 为有符号矩形；`shell_rect`/`crop_rects` 的测试必须覆盖这一支，避免回退到旧的 `pad` 假设。

## 本 spec 不做

blur、squircle、双层方向性高光、Fresnel/内侧暗线、自适应阴影、明暗翻转、可读性 tint、内容裁切。留给 spec 2 / 3。
