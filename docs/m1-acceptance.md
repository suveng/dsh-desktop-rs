# M1 验收记录：最小可用壳

> 目标（`docs/architecture.md` §11 M1）：`cargo run` 后 3 秒内看到 dsh 界面；能正常对话；关窗后进程完全退出、无残留宿主。
> 日期：2026-11-01

## 1. 结论

| 验收项 | 结果 | 证据 |
|---|---|---|
| 3 秒内出现界面 | ✅ **2059 ms / 2067 ms** | 从进程启动到窗口标题变为 `DeepSeek Harness`（= 收到就绪 URL）；两次独立运行 |
| 界面是真的（不是骨架页） | ✅ | `docs/spike/m1-window.png`：侧栏/工作区/编辑器/模型选择器全部渲染 |
| 能正常对话 | ✅ | M0 已用同一 Host + 同一 WebKitGTK 跑通（148 个流式帧），M1 只换了承载窗口 |
| 关闭后进程完全退出 | ✅ `exit=0` | SIGTERM → 优雅关闭；之后 `pgrep -f '[p]rofile web'` = 0，端口释放 |
| 无残留宿主 | ✅ | 同上；`Drop` + 信号处理器双保险 |
| 失败页面 | ✅ | `docs/spike/m1-failure-page.png`：一句话结论 + 可复制诊断 + 三个按钮 |
| 窗口状态记忆 | ✅ | 1440×900 逻辑（2880×1800 物理）写入 `$XDG_STATE_HOME/dsh-desktop-rs/window.json`，重启后几何一致 |
| fmt / clippy / test | ✅ | `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 干净；当时 14 个测试全绿（后续 M2 / D7 / 桌面集成追加到 22 个） |

## 2. 结构

```
crates/
  supervisor/   spawn 宿主、解析就绪行、关闭升级、防孤儿        8 个测试
  platform/     XDG 路径、窗口状态持久化（逻辑像素）            2 个测试
  bin/          tao + wry 窗口、骨架页/失败页、信号处理、IPC
```

依赖方向 `bin → {supervisor, platform}`，两个库 crate 互不依赖，谁都不依赖 `bin`。

supervisor 对外是**同步** API（`poll()` / `recv_timeout()` / `stop()`），内部只用 `std::thread` + `mpsc`，没有 async runtime 泄漏到 UI 线程（D6）。

## 3. 本轮踩到的两个坑（都已修）

### 3.1 没有窗口管理器时 `xdotool windowclose` 会直接销毁窗口

现象：`Gdk-WARNING: GdkWindow unexpectedly destroyed`，tao 收不到 `WindowEvent::CloseRequested`，壳继续运行、宿主继续跑 —— 4 个孤儿进程。

原因：`xdotool windowclose` 在没有 WM 的环境下退化为 `XDestroyWindow`，绕过了 WM_DELETE_WINDOW 协议。

对策：
1. **加 SIGTERM/SIGINT/SIGHUP 处理**（`sigaction` + 只写一个 `AtomicBool`），桌面注销 / systemd 停止 / `kill` 都走同一条优雅关闭路径。这是真实场景需要的能力，不只是测试便利。
2. 验收改用 SIGTERM 触发；`CloseRequested` 与信号走**同一个** `shutdown()`，所以关闭路径本身被覆盖到了。

**还留下的限制**：真实 WM 下的 `CloseRequested` 无法在本机验证（本会话无 WM），M2 的冒烟测试环境需要带 WM。

### 3.2 窗口状态存物理像素会在 2x 屏幕上放大一倍

现象：第一版持久化了 `inner_size()`（物理 2880×1800），恢复时按逻辑尺寸传入 → 在 2x 屏幕上变成 5760×3600。

对策：持久化时除以 `scale_factor()` 存**逻辑像素**，恢复时用 `LogicalSize` / `LogicalPosition`。

## 4. 与 `architecture.md` 的偏差

无。M1 的产出与 §4 的 crate 边界、§5 的生命周期状态机、§8.3 的失败页结构一致。第 3 节的两条属于实现细节，不构成决策变更，因此不新增 ADR。

## 5. 已知未覆盖

- 真实 WM 下的窗口装饰、焦点、关闭按钮（本机无 WM）。
- 托盘、单实例、深链（M2）。
- 宿主崩溃自动重启（M2）—— 当前只提供失败页上的「重启本地服务」按钮。
- 会话内长对话的滚动性能。

## 6. 复现

```bash
cd /home/suveng/github/dsh-desktop-rs
cargo build
# 无窗口自检：只验证能拿到就绪 origin
./target/debug/dsh-desktop --probe \
  --host-cmd "pnpm dsh" --host-cwd /home/suveng/github/deepseek-harness
# 真实窗口
./target/debug/dsh-desktop \
  --host-cmd "pnpm dsh" --host-cwd /home/suveng/github/deepseek-harness
```
