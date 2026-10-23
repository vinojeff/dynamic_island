# Dynamic Edge (rustedge)

一个贴在**屏幕顶部正中央**的 Windows 悬浮栏。平时是一条收起的细线,鼠标靠近时变成胶囊,点击展开成卡片——媒体控制、系统通知、CPU / 内存 / 网络一眼可见。基于 [gpui-kit](https://crates.io/crates/gpui-kit) 与纯 Rust Win32/WinRT 调用,不依赖 WebView。

## 功能一览

- **三形态变形**:收起细线(几乎不可见)→ 悬停胶囊(时间 / 音量等速览)→ 展开卡片(完整功能页),点击翻页箭头在「媒体」与「性能」两页之间切换
- **媒体控制**:读取 Windows 系统媒体会话(SMTC),展示封面、进度与曲目信息,支持拖动进度条定位、播放 / 暂停 / 上一曲 / 下一曲
- **通知中心**:轮询 Windows 通知(Toast),顶部直接预览新通知;独占全屏时自动排队,退出全屏后展示
- **音量调节**:在悬浮栏上滚动鼠标滚轮即可调节音量,支持静音切换
- **性能监视**:每秒采样 CPU、内存、网络吞吐
- **托盘常驻**:任务栏托盘图标,菜单一键显示 / 隐藏 / 退出,不占用任务栏按钮
- **全屏友好**:普通置顶应用压不倒它;进入**独占全屏**(游戏)时自动让路,退出即刻回归

## 环境要求

| 项目 | 要求 |
|---|---|
| 操作系统 | Windows 10 / 11(x64) |
| Rust | stable ≥ 1.85(见 `rust-toolchain.toml`) |

> 仅支持 Windows,dll/COM/WinRT 依赖决定了这一点。

## 构建与运行

```powershell
# 克隆仓库
git clone https://github.com/vinojeff/dynamic_island.git
cd dynamic_island

# 调试运行
cargo run

# 发布构建(推荐)
cargo build --release
# 产物:target/release/rustedge.exe
```

首次运行需要「通知访问」权限,收到 Windows 弹窗后允许即可。

## 使用说明

1. **唤出**:启动后,主显示器顶部正中出现一条细线——它就在那儿。
2. **悬停**:鼠标移到细线上,胶囊弹出,显示时间等速览信息。
3. **展开 / 翻页**:点击胶囊展开卡片;按左右箭头在媒体页与性能页之间切换。
4. **控制媒体**:卡片内直接点播放 / 暂停 / 切曲,拖动进度条 seek。
5. **调音量**:鼠标悬停在悬浮栏上滚动滚轮,音量随滚轮增减。
6. **看通知**:收到 Windows 通知时,悬浮栏顶部展示通知页,支持抢占队列。
7. **托盘操作**:右键托盘图标 → `Show dock` / `Hide dock` 控制显隐,`Quit` 完全退出。
8. **游戏模式**:进入独占全屏自动隐藏(可在 300ms 缓冲内防止闪烁),退出自动回来。

## 开发

```powershell
cargo test          # 库内逻辑测试(不依赖窗口)
cargo fmt           # rustfmt.toml 有固定风格
cargo clippy        # 拒绝 unwrap/panic,pedantic 级告警
```

架构、线程模型与设计约束的完整说明见 [ARCHITECTURE.zh.md](ARCHITECTURE.zh.md)(英文版[ARCHITECTURE.md](ARCHITECTURE.md))。

## License

以 [MIT](LICENSE) 协议开源。
