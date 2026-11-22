# 液态玻璃折射核心 —— 设计

日期：2026-11-22
状态：待评审

## 目标

把现在的“64 环全脸缩放”换成一次 CPU SDF 折射，让圆角转角的折射方向由真实法线决定，而不是从面板中心径向发散。

具体：

- 只在边缘一条 bevel 带折射，中心保持清晰。
- 用圆角矩形 SDF 的 `∇d` 作为法线：上边中点竖直、右边水平、45° 转角对角。
- bevel 用超椭圆高度剖面，斜率经精确 Snell 得到位移，转角再加方向性增益。
- R/G/B 在略微不同的采样位置取色，色散进入 warp。
- live 和静态壁纸共用同一条 `warp` 路径。
- `lens` 从 64 次 `paint_image` 降到 1 次。

本 spec 取代 2026-11-03 设计中的“折射”一节；那版的透明度、rim、sheen 结论仍然有效。

## 非目标

- blur / 内部散射。
- squircle 轮廓（本 spec 仍是圆角矩形 SDF + GPUI 圆角裁切）。
- 双层方向性高光、Fresnel、内侧暗线（spec 2）。
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
- 位移方向是 `p - center`，不是 `∇SDF`。在 420×288、r=24、P=34 的卡片上，上边切线点附近的位移方向偏离真实法线约 39°，位移量比边缘大 13~41%。
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

- `GlassShape { width, height, radius }`：逻辑像素，`Copy`。
- `Shell { x, y, width, height, radius }`：图像像素，相对 crop 左上角。
- `sd_round_rect(p, half, r) -> f32`：标准圆角矩形 SDF，`d < 0` 在内部。
- `sdf_normal(p, half, r) -> Option<(f32, f32)>`：中心差分 `eps = 1` 图像像素；返回单位法线；`|∇d| < 0.1` 时返回 `None`（中轴线保护）。
- `bevel_width(shape) -> f32`：`(min(w,h) * 0.18).clamp(10, 36).min(min(w,h) / 2)`。
- `displacement(p, shell, bevel_px, margins_px) -> Option<(f32, f32)>`：单像素采样偏移；`None` 表示直接拷贝。
- `warp(src, width, height, shell, bevel_px, margins_px) -> Option<Vec<u8>>`：一次重采样，输出同尺寸 RGBA。
- `shell_rect(stage_width, shape, pad, image_scale) -> Shell`：把逻辑形状换算成 crop 内的图像矩形。
- `crop_rects(bounds, shape, pad) -> (Shell, Bounds)`：`lens` 用的 shell 与 crop 自然矩形，纯函数可测。

`margins_px` 是 crop 四边 margin 的图像像素值，用来做屏幕边缘衰减。

### 光学模型

```
d      = -sd_round_rect(p - shell_center, half, r)   // >0 在内部
n      = ∇d 归一化                                    // |∇d| 太小时跳过
e      = clamp(d / bevel_px, 0, 1)
bend   = lut(e) * depth * corner_boost(n)
depth  = 0.8 * bevel_px
corner_boost(n) = 1 + 0.30 * 4 * n.x² * n.y²
```

- `lut(e)`：超椭圆剖面 `h(e) = bevel * (1 - (1-e)^p)^(1/p)`，`p = 4`；斜率 `dh/dd` 经精确 Snell（`n = 1.5`）得到 `tan(θi - θt)`，按边缘上限 `sqrt(n²-1)` 归一化到 `0..1`；再乘 `smoothstep(1, 0.75, e)` 让内缘 C1 收口。`h` 的高度取 `bevel`，所以 LUT 与具体 bevel 宽度无关；每次 warp 前算 64 项即可。
- `corner_boost`：直边为 0，45° 转角为 1；`4 n.x² n.y² = sin²(2θ)`，比参考项目的 `max(min(x,1-x), min(y,1-y))` 更平滑，胶囊端头也适用。
- 只在 `d < bevel_px` 的带内重采样；中心直接拷贝。
- 采样：`sample = p + n * bend`；R/G/B 分别在 `p + n * bend * (1 - s, 1, 1 + s)` 取样，`s = 0.03`。双线性插值，alpha 保持 255。
- 屏幕边缘：`reach_x = n.x >= 0 ? margins.right : margins.left`，`reach_y` 同理；`bend *= min(1, reach_x / (bend*|n.x|), reach_y / (bend*|n.y|))`。这样贴屏那一侧不越界，另一侧仍能弯。

### 边界与退化

- `bevel_px <= min(w,h)/2`，避免上下 bevel 在中线重叠。
- `|∇d| < 0.1` 时 `displacement` 返回 `None`；胶囊中轴线在 bevel 带外，只有极薄的过渡帧会触发。
- `height < 12` 仍由 `line_glass` 接管，`lens` 不画。
- 采样坐标按 crop 边界 clamp；正常路径下 `reach` 衰减已经保证不越界。
- `REFRACTION_MARGIN` 40 → 48：`depth * 1.3 + 色散 ≈ 38.6`，留 9px 余量。

## 集成

### `src/wallpaper.rs`

- `GlassBackdrop` 增加 `shape: GlassShape`（这张图是为哪个逻辑形状烘的），其余字段不变。
- 新增 `RawGlass { rgba, width, height, pad, image_scale, stage_width, luma }`：
  - `image_scale`：图像像素 / 逻辑像素（静态 = `scale`，live = `scale / factor`）。
  - `stage_width`：未裁切的舞台逻辑宽度，用来把 shell 在舞台里居中。
- `RawGlass::warp(shape) -> Option<GlassBackdrop>`：
  - 用 `shell_rect(stage_width, shape, pad, image_scale)` 得到 shell。
  - `bevel_px = bevel_width(shape) * image_scale`。
  - `margins_px = (pad.0*image_scale, pad.1*image_scale, pad.2*image_scale, pad.3*image_scale)`。
  - 调 `glass_optics::warp`，再 `render_image_rgba`。
- `build` 拆成 `build_raw(display, rect, scale) -> Option<RawGlass>`；解码/合成/裁切/提饱和/luma 不变，末尾不再建 `RenderImage`。
- 测试：`RawGlass::warp` 的输出尺寸、`pad`、`shape`；`image_scale` 换算；`stage_width` 居中。

### `src/live_glass.rs`

- `CropSpec` 增加 `shape: GlassShape`、`image_scale: f32`、`stage_width: f32`；`PartialEq` 仍然可用。
- `crop_spec(stage, scale, screen, shape)` 计算这些字段。
- `update_crop(stage, scale, shape)` 从 view 接收当前形状。
- `on_frame_arrived`：降采样/提饱和/luma 后构造 `RawGlass`，用 `spec.shape` 调 `warp`，把 `RawGlass` 和 `GlassBackdrop` 一起发布；`Sink` 保存 `Frame { raw, warped, generation }`。
- 没有新桌面帧但形状变了：view 用最近一帧的 `RawGlass` 重 warp，见下。

### `src/island_view/mod.rs`

- 新增一个轻量 warp 队列，live 和静态共用：
  - `warp_pending: Option<(Arc<RawGlass>, GlassShape)>`，`warp_busy: bool`。
  - `request_warp(raw, shape)`：覆盖 pending；空闲就启动后台 `raw.warp(shape)`。
  - 完成时校验 `glass_epoch`、raw generation 和请求是否仍是最新；通过就 `self.glass = Some(Arc::new(backdrop))`，再启动 pending。
  - 切换 live/static 时 bump epoch、清 pending。
- `poll_live`：取 `Frame`，安装 `warped`，保存 `last_live_raw` 和 generation；如果 `warped.shape != current_shape`，`request_warp(last_live_raw, current_shape)`。
- `poll_glass`：`build_raw` 完成后保存 `static_raw`，`request_warp(static_raw, current_shape)`；之后每 tick 如果静态在管、`static_raw` 存在且 `glass.shape != current_shape`，再请求一次。
- 后台任务只做纯 CPU，UI 线程不重采样。
- 形状变化期间队列自然合并：最多一个 warp 在飞，最新形状胜出；旧图由 `lens` 拉伸，warp 落地后替换。

### `src/island_view/glass.rs`

- `lens` 改为一次 `paint_image`：
  - `crop_rects` 得到 shell 和 crop 自然矩形。
  - `paint_image(shell, natural, Corners::all(radius), image, 0, false)`。
- 删除 `LENS_RINGS`、`REFRACTION_PULL`、`refraction_depth`、`refraction_pull`、`lens_bounds`、`shrink_factor`、`inset_shell`、`paint_lens`、`paint_crop`、`bounds_from` 及对应测试。
- `paint_rim` 删除两条色散环和 `DISPERSION` 常量；三条白色 Fresnel 带保留。
- `shell_offset` 改成 `shell_offset(stage_width, shape_width)`，`lens` 用 canvas 宽度，`RawGlass` 用 `stage_width`，两边共用同一公式。
- `veil_alpha`、`rim_alpha`、`shell_shadows`、`layers` 的 scrim/sheen 不变。

### `src/main.rs` 与文档

- 增加 `mod glass_optics;`，更新模块文档的像素管线说明。
- 更新 `ARCHITECTURE.md` 中 64 环透镜、`GlassBackdrop` 字段和 WGC 帧槽的描述。

## 测试

### `glass_optics` 纯函数

- SDF：边上为 0、内部为负、外部为正；角点距离等于圆心距离；四边/四角对称。
- 法线：上边中点 `(0,-1)`、右边 `(1,0)`、45° 角 `(0.707,-0.707)`；上边与圆角的切点处仍在 5° 内保持竖直（直接回归 39° 误差）。
- bevel：clamp 10..36、`<= min(w,h)/2`；card = 36、capsule ≈ 10.08。
- Snell：`e=0 → 1`、`e=1 → 0`、单调；`n=1.5` 上限归一化。
- corner：直边 0、45° 1、范围 `[0,1]`。
- `displacement`：
  - 上边中点偏移指向正上方，大小 ≈ `depth`。
  - 45° 角偏移方向与 `∇SDF` 夹角 < 5°。
  - 切点附近方向仍接近竖直。
  - 最大偏移 `<= margin`。
  - `margins.left = 0` 时左边不越界，右边仍弯。
- `warp`：中心像素完全不变；输出尺寸、alpha 不变；尺寸不匹配返回 `None`。
- `shell_rect`：card 居中偏移 0；capsule 偏移 `(stage_width - width)/2`；`image_scale` 正确缩放。
- `crop_rects`：shell 与 crop 自然矩形在 card/capsule 下都对齐。

### 集成

- `RawGlass::warp`：`pad`、`shape`、`luma` 透传；输出 `RenderImage` 尺寸正确。
- `CropSpec`：shape 变化会更新 spec；`image_scale = scale/factor`；`stage_width` 正确。
- 旧 64 环测试删除。

### 视觉

- 棋盘格壁纸：网格线在四角绕角弯折，不再被斜向拉伸。
- 照片壁纸：文字/枝叶在边缘仍可读，中心清晰。
- 胶囊端头：半圆端头的折射方向沿半径，不出现“斜拉”。
- 贴屏边缘：贴边一侧不弯，另一侧正常。

## 风险

- debug 构建的 CPU warp 可能每帧 10~20ms；release 约 1~3ms。若 capture 线程跟不上，先降级为“view 端 warp”（同一 `RawGlass` 路径，只换执行位置）。
- 极薄形状（高度 12~20px）上下 bevel 在中线重叠，靠 `|∇d|` 保护和 `height < 12` 禁用兜底。
- 静态路径在 morph 期间用旧图拉伸，落地时可能有一次轻微跳变；队列合并 + 一次 warp 通常看不出来。若明显，改为 morph 结束后再 warp。
- `REFRACTION_MARGIN` 增大会让静态 crop 略大，解码/合成成本几乎不变。
- 色散符号/强度需要视觉调参；先给 `s = 0.03`，不要一次调太大。

## 本 spec 不做

blur、squircle、双层方向性高光、Fresnel/内侧暗线、自适应阴影、明暗翻转、可读性 tint、内容裁切。留给 spec 2 / 3。
