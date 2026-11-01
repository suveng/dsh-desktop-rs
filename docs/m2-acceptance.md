# M2 验收记录：桌面化

> 目标（`architecture.md` §11 M2）：单实例、深链、原生菜单、桌面 shim、崩溃自动重启、可自动化的冒烟测试。
> 日期：2026-11-01

## 1. 结果

| 验收项 | 结果 | 证据 |
|---|---|---|
| 第二次启动只聚焦已有窗口 | ✅ | 第二次进程打印「已有实例在运行，请求已转发」、`exit=0`；宿主进程数不变（4）、窗口数不变（2 = toplevel+frame） |
| 深链唤起并导航 | ✅ | `dsh-desktop dsh://session/abc` → 转发给主实例 → `architecture.md` §7-15 的映射规则导航到 `<origin>/session/abc` |
| 原生菜单 | ✅ | `docs/spike/m2-menu.png`：文件 / 编辑 / 视图 / 窗口 / 帮助 |
| 快捷键来自表 | ✅ | `crates/bin/src/menu.rs` 的 `bindings()` 是唯一真相：菜单项由它生成、加速键由它注册、「关于」里的帮助文本也由它渲染（`shortcuts_help()`） |
| shim 生效 | ✅ | selftest：`marker=1`、`protocolVersion=1`、`directoryPicker=true`、`hostPaths=true`、`deviceInfoType=function` |
| deviceInfo 真实往返 | ✅ | 返回 `dsh-desktop 0.1.0 · Ubuntu 24.04.5 LTS · x86_64 · ubuntu:GNOME`（请求与回填两侧都有日志） |
| 前端插件不被 shim 打挂 | ✅ | `pluginError=false` —— 见 §3 的 bug |
| `kill -9` 宿主 2 秒内恢复 | ✅ **1908 ms** | 新宿主 pid 出现、窗口标题回到 `DeepSeek Harness` |
| 连续崩溃 2 次后进失败页 | ✅ | 连续 kill 3 次后标题变为 `DeepSeek Harness — 启动失败` |
| 冒烟测试可自动化 | ✅ | `xvfb-run -a bash scripts/smoke.sh` → `SMOKE OK`，失败页与基线 **0 像素差异** |

## 2. 交付物

- `crates/platform/src/instance.rs`：loopback socket + `0600` 发现文件 + `lock` 文件（4 个测试：二次 claim、陈旧锁回收、文件权限、错误 token 被拒）。
- `crates/bin/src/menu.rs`：muda 菜单 + 快捷键表。
- `crates/bin/assets/shim.js`：`window.dshDesktop` / `__DSH_DIRECTORY_PICKER__` / `__DSH_HOST_PATHS__`。
- `crates/bin/src/main.rs`：单实例、深链转发、崩溃自动重启（1s/2s 退避 + 60s 稳定后熔断复位）、`--selftest`。
- `scripts/smoke.sh` + `scripts/compare_shots.py` + `tests/baseline/failure-page.png` + CI `smoke` job。

## 3. 最重要的一个 bug：shim 让前端插件炸掉

第一版 shim 只给了 `protocolVersion`、`deviceInfo`、`directoryPicker`、`updates.status`，结果页面直接变成：

```
Failed to load plugins
@deepseek-ai/dsh-client-ui-settings-general
web boot: 1 entry did not activate
```

原因（`packages/client/ui-settings-general/src/client/index.ts:104`）：

```ts
const carrier = globalThis.dshDesktop
const desktopUpdate = new DesktopUpdateSource(carrier?.protocolVersion === 1 ? carrier.updates : undefined)
```

`DesktopUpdateSource` 构造时立刻调用 `bridge.subscribe(...)` 和 `bridge.status()`（`desktop-update-source.ts:15-21`）。我的 `updates` 只有 `status`，`subscribe` 是 `undefined` → activate 阶段抛错 → **整个 web boot 失败**（一个插件挂掉会带走整页）。

教训写进 shim 注释：**`protocolVersion === 1` 是一个「我实现了完整 API」的承诺，不是版本号**。要么给全，要么一个都别给。现在 shim 补齐了 `fileActions` / `browser` / `fileDrag` / `keyboard` / `shortcuts` / `updates` 的完整形状，不支持的能力用「安全空实现 + 明确拒绝」表达。

selftest 因此加了 `pluginError` 断言，smoke 脚本也断言它 —— 这类回归必须在 CI 里被挡住，而不是靠人看一眼截图。

## 4. 深链的边界（新增约定，非偏离）

dsh 前端没有对外文档化的路由，所以壳定义了唯一一条映射：

```
dsh://x/y?z  →  <origin>/x/y?z
```

这写在 `architecture.md` §7-15。真正的「打开某个会话」需要上游提供路由或事件通道，属于应用内行为，不在壳的范围里。

## 5. 已知未覆盖

- 托盘（`tray-icon`）：本机没有 `org.kde.StatusNotifierWatcher`，GNOME 需要扩展；M3 只做「有扩展才显示」的探测，不给假承诺。
- 系统通知：同上，M3 用 `notify-rust` 的可用性探测。
- 真实窗口管理器下的菜单/焦点行为（本机 X11 会话无 WM）。
- 目录选择对话框的自动化测试（原生模态框在 xvfb 里无法可靠驱动，只验证了 API 存在且可调用）。

## 6. 复现

```bash
cargo build
# 单实例 + 深链
./target/debug/dsh-desktop --host-cmd "pnpm dsh" --host-cwd /home/suveng/github/deepseek-harness &
./target/debug/dsh-desktop dsh://session/abc      # 立刻退出，打印「请求已转发」
# 冒烟（含真实前端 shim 握手）
DSH_SMOKE_HOST_CMD="pnpm dsh" DSH_SMOKE_HOST_CWD=$PWD/../deepseek-harness \
  xvfb-run -a -s "-screen 0 1600x1000x24" bash scripts/smoke.sh
```
