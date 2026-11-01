# M2 验收记录：桌面化

> 目标（`architecture.md` §11 M2）：单实例、深链、原生菜单、桌面 shim、崩溃自动重启、可自动化的冒烟测试。
> 日期：2026-11-01

## 1. 结果

| 验收项 | 结果 | 证据 |
|---|---|---|
| 第二次启动只聚焦已有窗口 | ✅ | 第二次进程打印「已有实例在运行，请求已转发」、`exit=0`；宿主进程数不变（4）、窗口数不变（2 = toplevel+frame） |
| 深链唤起并导航 | ⚠️ 部分 | 转发路径正常；**冷启动带深链**的覆盖 bug 已修（Ready 时直接用深链目标加载，不再被根 URL 覆盖）。但上游没有 `/session/abc` 路由时仍是空页，回退到根页面的逻辑未实现 |
| 原生菜单 | ✅ | `docs/spike/m2-menu.png`：文件 / 编辑 / 视图 / 窗口 / 帮助 |
| 快捷键来自表 | ✅ | `crates/bin/src/menu.rs` 的 `bindings()` 是唯一真相：菜单项由它生成、加速键由它注册、「关于」里的帮助文本也由它渲染（`shortcuts_help()`） |
| shim 生效 | ✅ | selftest：`marker=1`、`protocolVersion=1`、`directoryPicker=true`、`hostPaths=true`、`deviceInfoType=function` |
| deviceInfo 真实往返 | ✅ | 返回 `dsh-desktop 0.1.0 · Ubuntu 24.04.5 LTS · x86_64 · ubuntu:GNOME`（请求与回填两侧都有日志） |
| 前端插件不被 shim 打挂 | ✅ | `pluginError=false` —— 见 §3 的 bug |
| `kill -9` 宿主 2 秒内恢复 | ⚠️ **通常 0.9–1.3s，偶发超限** | 常规路径：874/880/883/969/975/993/1014/1246 ms；另有 1943 ms 与一次 7253 ms（见 §4.4）。第一次崩溃不退避，且优先用 `apps/cli/lib/bin.js`（785ms 冷启动）。**不宣称必然 <2s** |
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

## 4.1 复审后修正（2026-11-01 12:05）

独立审计指出三条实测问题，已修：

1. **恢复时延曾超 2 秒**：旧实现无差别退避 1s，加上 `pnpm dsh` 1.9s 冷启动 → 3.1s。现在第一次崩溃**不退避**，并新增「优先用 `apps/cli/lib/bin.js`」的宿主解析（785ms 冷启动），实测 **975ms**。
2. **`--version` 探测缺失（D7）**：现已实现（5 秒超时、区间比较、日志 + 标题提示、进自检输出），两个方向都验过：全局 `dsh 0.1.1-rc.2` 报「不在支持区间」，工作区 `0.2.2-alpha.2` 报 `in-range`。
3. **更新提示曾是个假桩**：现在是真的读更新源（`{version,url}`）、比对版本、推给前端 `updates.status/subscribe`、并可从「帮助 → 检查更新」打开下载页；没配更新源时保持 `idle` 且不发任何请求。
4. **测试偶发 `ETXTBSY`**：并行用例里「刚写出的脚本立刻 exec」会撞内核的 i_writecount，测试入口改为带重试的 `spawn_host`，连续 6 轮全绿。

## 4.2 桌面集成四项（复审后补齐）

早先以「本机无法验证」为由推迟的四项，经用户确认改为**现在实现**。复审时实测发现本机其实**有** `StatusNotifierWatcher`（`gdbus` 查询返回 true），也就是说原来的判断依据本身是错的。

| 能力 | 实现 | 证据 |
|---|---|---|
| 托盘 | `gdbus` 探测 → 创建托盘（显示窗口/重新加载/退出） | 运行日志 `托盘已启用`；selftest `{"tray":true}` |
| 系统通知 | `notify-rust`（D-Bus），失败只记日志 | selftest `{"notify":true}`（真的弹了一条「dsh-desktop 自检」） |
| 全局热键 | `Ctrl+Alt+D` 唤回窗口 | 日志 `全局热键 Ctrl+Alt+D 已注册`；selftest `{"hotkeys":true}` |
| 拖拽真实路径 | wry `DragDropEvent::Drop` 记路径，按文件名回填 `pathFor(file)` | `--selftest --simulate-drop /tmp/m2/sample.txt` → `{"hostPath":"/tmp/m2/sample.txt"}`；另有 4 个单测覆盖「按名取用/取过即出队/新批次覆盖旧批次/空名不匹配」 |

降级策略写在 `architecture.md` 的 ADR-D8：探测不过就不创建，并在日志里说明原因 —— 不做「创建成功但看不见」的托盘。

## 4.3 R4 收口：token 不再落盘（复审后修）

审计用 `grep -rao "http://127.0.0.1:.*?token=…" ~/.cache ~/.local/share` 找到了 **20 条明文 token** ——
来自 WebKit 的磁盘缓存（子资源的 Referer 就是带 token 的首页 URL）。日志脱敏、`Debug` 脱敏、
`--probe` 只打 origin 都做到了，唯独「落盘」这条被浏览器缓存击穿。

修复分三层：

1. **搬到 tmpfs**：启动时把 `XDG_CACHE_HOME` / `XDG_DATA_HOME` 指向 `$XDG_RUNTIME_DIR/dsh-desktop-rs/webview`（0700，内存盘，注销即清）。
2. **进出各清一次**：启动时删除历史遗留的 `~/.cache/dsh-desktop` 与 `~/.local/share/dsh-desktop`，退出时 `remove_dir_all(webview_home())`。
3. **首屏后抹掉地址栏里的 token**：`history.replaceState(null, '', '/')`，后续子资源的 Referer 不再带 token。

验证：跑两轮实例并退出后，`~/.cache/dsh-desktop` 不存在、tmpfs 中 `token=` 计数为 **0**、残留宿主 0。

## 4.4 恢复时延分布（不写单值）

跨多次实测（含审计独立跑的 9 次与本机 3 次）：**874 / 880 / 883 / 969 / 975 / 993 / 1014 / 1246 ms**，
另有一次 **1943 ms** 与一次 **7253 ms** —— 后者发生在一个刚被深链导航过的实例上，原因未定位。

结论：**常规路径稳定在 1 秒左右**，满足 2 秒门槛；但存在偶发超限，暂不宣称「必然 < 2s」。
后续要做的是把重启也纳入结构化日志（记录 kill→Exited→ready 各段耗时），再判断 7.2s 是重复重启排队还是宿主冷启动抖动。

## 5. 已知未覆盖

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
