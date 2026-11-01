# M0 尖刺：WebKitGTK 能否跑 dsh Web 前端

> 目标：验证 `docs/architecture.md` 的 **R1**（WebKitGTK 渲染差异）是否成立。
> 结论：**通过。** 方案 A（Rust 壳 + 现有 Web UI）在 Linux 上可行。
> 日期：2026-11-01

---

## 1. 结论

在 **WebKitGTK 2.52.6**（与 `wry` 在 Linux 上使用的是同一个运行时和同一套 API）里加载真实 `dsh web` 的 URL，完成了完整闭环：

| 验收项 | 结果 | 证据 |
|---|---|---|
| 登录 | ✅ | 带 token 的 URL 一次导航 → 303 + cookie，直接进入已登录界面（侧栏列出历史会话，无登录页） |
| 发送一条消息 | ✅ | 页面内受控输入 + 点击「发送消息」→ 会话中出现用户消息 |
| 看到流式输出 | ✅ | 同一条 WebSocket 上收到 **148 个 `item` 帧**（6 → 148），模型回复 `OK` 渲染完成 |
| 渲染正确 | ✅ | 见 §3 截图：中文字体、图标、布局、气泡、底部统计条全部正常 |
| 无 JS 报错 | ✅ | `window.__probe_errors` 全程为空，`console.error` 未触发 |

**不是**「DOM 里有文字」这种弱证据 —— 148 个 WS 帧加两张完整截图，等于把渲染 + 交互 + 流式传输三条链路都走通了。

---

## 2. 实测环境

| 项 | 值 |
|---|---|
| OS / 会话 | Ubuntu 24.04，X11（`DISPLAY=:0`） |
| WebKitGTK | 运行时 2.52.x；dev 头文件 2.52.6（`libwebkit2gtk-4.1-dev`） |
| GTK | 3.x（`webkit2gtk-4.1` 是 GTK3 后端，与 `wry` 的 Linux 后端一致） |
| dsh | 源码 `pnpm dsh`，`0.2.2-alpha.2-ce7073d` |
| 宿主启动 | `pnpm dsh --profile web --host 127.0.0.1 --port 0 --no-open` |
| 就绪行 | `dsh web: http://127.0.0.1:46007/?token=<43 字符 base64url>` |
| 探针 | Python + PyGObject（`WebKit2 4.1`），`/tmp/m0/probe3.py` |
| 截图 | `WebKit2.WebView.get_snapshot(VISIBLE)`，2880×1800（DPR 2，逻辑 1440×900） |

宿主日志只有三行，无警告：

```
$ node --import tsx/esm apps/cli/src/bin.ts --profile web --host 127.0.0.1 --port 0 --no-open
dsh web: http://127.0.0.1:46007/?token=<redacted>
[llm-deepseek-api-key] model listing from https://api.deepseek.com/models loaded 2 model(s), skipped 0 row(s)
```

---

## 3. 截图

`01-shell.png` —— 空白会话页。侧栏、工作区树、编辑器、模型选择器、发送按钮全部正确；CJK 字形无回退问题。
`04-final.png` —— 对话完成态。用户气泡右对齐、助手回复、工具行、`用量 12.1K tok`、底部 `170 tok/s · 缓存命中 0% · 上下文 1%` 全部渲染。

（`02-sending.png` / `03-streaming.png` 与 `04` 字节数相同 —— 见 §4 失败点 5：回复 1 秒就结束了，抓不到中间态。）

---

## 4. 失败点与坑（这是本节的主要价值）

| # | 现象 | 根因 | 对 M1/M2 的影响 | 对策 |
|---|---|---|---|---|
| **1** | 只 `kill` 直接子进程后，4 个进程全部存活，46007 端口仍在监听 | `pnpm dsh` 不是单进程：`pnpm`(71914) → `pnpm.mjs`(71928) → `sh -c trap 'kill -s INT $$'`(71953) → `node bin.ts`(71954)，**跨至少两个进程组** | **高**：naive 的 `child.kill()` 会留下僵尸宿主，用户以为退出了，端口被占 | `setsid` 建新会话 + `setpgid`；关闭顺序 `SIGTERM 到进程组 → 轮询 10s → SIGKILL 到进程组 → 校验端口/pid`；再叠加 `PR_SET_PDEATHSIG`。**参见对 `architecture.md` §5 的修订** |
| **2** | `xdotool windowactivate` 卡死，报 `XGetWindowProperty[_NET_WM_DESKTOP] failed` | 该 X 会话**没有窗口管理器**（`_NET_ACTIVE_WINDOW` 不存在） | 中：M2 的窗口状态记忆、托盘、焦点行为在本机无法验证 | 自动化一律走**页面内 JS**（已验证可行）；WM 相关能力留到真实桌面会话复验 |
| **3** | `document.execCommand('insertText')` 返回 `false`，立刻读 `innerText` 为空 —— 但消息**确实**进了编辑器并被成功发送 | 编辑器是 Lexical 类富文本，DOM 变更异步提交；返回值与立即读回都不可信 | 低：自动化输入本身可用 | 不要用返回值/立即读回判定，等一帧（`requestAnimationFrame` 后 100ms）再读 |
| **4** | `performance.getEntriesByType('resource')` 里找不到 `remote.mux` | WebSocket 不属于 Resource Timing | 低，但会让人误判「WS 没连上」 | 探针必须 hook `window.WebSocket` 计数帧；本次靠此拿到 148 帧的证据 |
| **5** | 三次快照（+2.5s / +7s / +14s）字节数完全相同 | 回复 1 秒完成，抓不到流式中间态 | 中：M2 的视觉回归不能依赖真实 LLM 时序 | 视觉基线用**固定 fixture / 会话回放**，或故意用长输出 + 慢模型 |
| **6** | 全局 `dsh` 是 `0.1.1-rc.2`，源码 `pnpm dsh` 是 `0.2.2-alpha.2` | 两套安装在 PATH 里共存 | 中：壳如果直接 spawn `dsh` 会拉起旧版本 | D7 的版本区间检查必须**在启动后立刻做**，并把解析到的可执行文件与版本写进诊断信息 |
| **7** | 截图是 2880×1800 而不是 1440×900 | DPR = 2 | 低 | 视觉回归同时记录「逻辑尺寸 + scale factor」，比较前统一归一化 |

**没有出现的失败**（曾担心的）：`perMessageDeflate` 不兼容、cookie `SameSite=Strict` 被拒、CSP 拦截、字体回退错乱、react  hydration 失败、`backdrop-filter` 缺失导致的黑块。全部未复现。

---

## 5. 对 `architecture.md` 的三处修订

1. **§5 孤儿进程**：原文只写了 `PR_SET_PDEATHSIG`，**不够**。必须补「进程组」：`setsid` 启动 + 对 pgid 发信号 + 轮询校验。已验证：只 kill 直接子进程会留下 4 个进程。
2. **§10 测试策略**：补一条「自动化输入走页面内 JS（受控输入 + 点击按钮），不依赖 xdotool / WM」。
3. **§7 能力对齐 #2**：`webview 加载` 标记为**已实测通过**（WebKitGTK 2.52.6 / GTK3 / DPR 2）。

另外给 M1 的一条实现建议：**优先 spawn 单个 `dsh` 可执行文件**，而不是 `pnpm dsh` —— 进程链从 4 个降到 1 个，关闭逻辑立刻简单一半。开发期确实需要源码构建时，再退回 `pnpm dsh` 并接受进程组方案。

---

## 6. 未覆盖

- 真实窗口管理器下的行为（窗口状态记忆、托盘、全局快捷键）—— 本机无 WM，M2 需另找环境。
- 长时间会话的滚动性能（本次只有 2 条消息）。
- 大文件 diff、终端面板、内嵌 iframe 预览的渲染。
- 文件拖拽取真实路径（`__DSH_HOST_PATHS__`，即 `architecture.md` 的 R2）—— 本次未测，M2 处理。
- 3 个以上并发会话 / 多标签页。

---

## 7. 复现

```bash
# 1. 起宿主（不要打印 URL，token 会随之泄漏）
cd /home/suveng/github/deepseek-harness
setsid nohup pnpm dsh --profile web --host 127.0.0.1 --port 0 --no-open > /tmp/dsh-host.log 2>&1 &
grep '^dsh web: http' /tmp/dsh-host.log | tail -1 | sed 's/^dsh web: //' > /tmp/dsh-url.txt
chmod 600 /tmp/dsh-url.txt

# 2. 跑探针（会弹出一个 GTK 窗口，约 20 秒后自动退出）
DISPLAY=:0 python3 /tmp/m0/probe3.py

# 3. 收尾（务必按进程组杀）
PGID=$(ps -o pgid= -p "$(pgrep -f 'pnpm dsh --profile web' | head -1)" | tr -d ' ')
kill -TERM -"$PGID"; sleep 2; kill -KILL -"$PGID" 2>/dev/null
```

探针脚本不随仓库提交（它依赖 `/tmp/dsh-url.txt` 这种临时约定）；M2 会把等价能力做成仓内的 `xtask` 子命令。
