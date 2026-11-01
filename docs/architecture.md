# dsh-desktop-rs 架构设计

> 一个用 Rust 写的 DeepSeek Harness 桌面客户端。首个交付平台：**Linux**。
>
  > 状态：**0.1.0 已实现并验收**（M0–M3，见 `docs/m*-acceptance.md`）。所有关于 dsh 的事实都标注了源码位置，位于 `/home/suveng/github/deepseek-harness`（version 0.2.2-alpha.2，HEAD `ce7073d512`）。

---

## 1. 目标与非目标

### 目标

1. **给 Linux 一个官方没有的桌面端**。dsh 官方 Electron 产物只覆盖 macOS 与 Windows（`apps/desktop/config/` 只有 `macos-packaging`、`windows-packaging`；发布 target 为 `mac-arm64`/`mac-x64`/`win-x64`），Linux 用户只能开浏览器。
2. **把「桌面」这件事做对**：单实例、深链、托盘/菜单、系统通知、原生目录选择、文件拖拽、窗口状态记忆、崩溃恢复、自动更新。
3. **不重写 UI**。复用 dsh 现有的 React 前端与全部插件生态。
4. **体积与内存显著优于 Electron**（目标：安装包 < 25 MB，空载 RSS < 150 MB）。
5. **协议零依赖**：Rust 侧不实现 dsh 的 RPC，只做进程监督 + 窗口 + 外壳能力。

### 非目标（明确不做）

- 不自绘聊天界面。**理由见 §3 D1：dsh 的 UI 即插件，60+ 个前端包以 JS bundle 形式贡献界面，原生 UI 等于放弃整个插件生态。**
- 不实现 `POST /api/<ns>/<method>` 与 `/api/remote.mux`。上游 77 个 Remote 方法的 payload 只在 TypeScript/Zod 描述符里，无 JSON Schema 导出、无 wire 版本协商。
- 不 fork dsh、不 patch dsh 的 Node 侧代码。宿主始终是上游原样的 `dsh`。
- 不做 Windows/macOS（M3 之后再评估）。

---

## 2. 背景：dsh 桌面端现状

### 2.1 Electron 壳到底在做什么

`apps/desktop`（约 10,454 行 TS）拆开只有三类事情：

| 类别 | 内容 | 位置 |
|---|---|---|
| **任何一个壳都必须做的** | spawn 宿主子进程、等就绪、开窗、托盘、菜单、单实例、崩溃恢复、自动更新、原生目录选择、设备信息 | `apps/desktop/src/host-process.ts:212`、`main.ts:506`、`tray.ts`、`single-instance.ts`、`update-coordinator.ts` |
| **只为绕开自定义 scheme 存在的** | `dsh-app://` 协议注册与静态托管、非 index 路径反代到 Host、WebSocket 请求头重写 | `main.ts:185,762,770,775,811-821` |
| **Electron 独有的增强** | 内嵌浏览器 `WebContentsView` 分区、`webUtils.getPathForFile` 取真实路径、`window.dshDesktop` 桥 | `platform-view.ts`、`preload-app.ts:17-118` |

第二类整个可以删掉：页面 origin 若是 `http://127.0.0.1:<port>`，cookie 自动同源携带，`Origin` 校验天然通过（`packages/client/connection/src/api-request-trust.ts:1-12`），不需要反代、不需要 header 重写。

### 2.2 宿主就绪协议

`apps/desktop-host`（仅 598 行）本质是 `dsh --profile web` 的同构启动器，把「打开浏览器」换成「通过 Node IPC 上报 URL」。

而 Web 侧自己就写了这是一条**面向 supervisor 的契约**：

```ts
// packages/bundle/web-app/src/index.ts:214-231
// The URL line and browser handoff are readiness signals: supervisors RPC
// as soon as they observe the line …
console.log(`dsh web: ${authenticatedUrl}`)
```

CLI 参数（`packages/bundle/web-app/src/startup.ts:64-66`）：`--host <ipv4|ipv6 字面量>`、`--port <n>`（`0` = OS 分配）、`--no-open`。

### 2.3 前端 boot 有两种模式

| 模式 | 触发 | boot 数据来源 |
|---|---|---|
| 浏览器 | 直接打开 Host 的 URL | Host 把 `window.__DSH_BOOT__` 注入 index.html —— `packages/host/frontend-static/src/index.ts:163` `ctx.webServer.renderIndex(html, peer)` |
| Electron | 页面 origin 是 `dsh-app://app` | 前端调 `window.dshDesktopBoot.ready()` 取 `{injections, streamBaseUrl}` —— `apps/web/src/main.ts:20-37` |

**结论：webview 加载 `http://127.0.0.1:<port>/?token=…` 属于「浏览器模式」，boot 注入零工作量。**

### 2.4 认证

`GET /?token=<每进程 32 字节 base64url>` → `303` 到 `./` + `Set-Cookie: dsh-auth-<b64url(sha256(authority))>=v2.<payload>.<hmac>`，`HttpOnly; SameSite=Strict; Path=/`。

因此：**桌面端只在启动时导航到带 token 的 URL 一次**，之后同源请求靠 cookie。token 绝不能让用户看到，也不能写入日志或磁盘。

---

## 3. 关键决策（ADR）

### D1 用 webview 复用 Web UI，而不是自绘 Rust UI

**决定**：`tao` + `wry`（Linux 后端 WebKitGTK）承载现有前端。

**理由**：
- dsh 的插件通过 `package.json` 的 `dsh.client` 声明并导出 `exports["./client"]`，Host 用 `GET /plugins/??<pkg>/client.js,…` 拼装脚本（`docs/subsystems/client-modules.md:1-40`）。**UI 是 JS，不是数据协议。** 原生 UI 会让 sidebar / settings / market / terminal / plugin-manager 等 60+ 官方前端包全部失效。
- 77 个 Remote 方法没有导出的 schema，跨 release 直接破。上游迭代速度是每周数十个 PR（PR 号已到 #7552）。

**代价**：无原生手感；WebKitGTK 渲染差异需实测（§9 风险 R1）。

**被否决的方案**：Rust 原生 UI + 复用 Node Host（6–12 个月，且丢生态）；连 Host 一起重写（等于重写产品）。

### D2 用 loopback HTTP origin，不用自定义 scheme

**决定**：webview 直接加载 `http://127.0.0.1:<port>/…`。

**理由**：省掉 Electron 的三处 hack（反代、header 重写、scheme 注册），并让 cookie / Origin / WebSocket 全部走标准路径。

### D3 就绪信号 = 解析 stdout 的 `dsh web: <url>`，不引入 Node 侧代码

**决定**：spawn `dsh --profile web --host 127.0.0.1 --port 0 --no-open`，正则等待 `^dsh web: (\S+)$`。

**理由**：这是上游写明的 supervisor 契约（§2.2）。用 Node IPC 需要我们自己维护一个 Node 侧 shim，违背「Rust 壳」的初衷。

**风险与缓解**：日志格式无版本保证 → 见 R3（启动探测 + 明确报错文案 + 可选 `--url-file` 兜底）。

### D4 Rust 不碰协议层；桌面增强用 JS shim 与 `window.dshDesktop` v1 同名

**决定**：Phase 1 通过 webview 的 initialization script 注入 shim，实现与 Electron `preload-app.ts` 同名的全局对象；能力缺失时前端本来就降级（`ui-file-drag`、`ui-sidebar-browser` 等用 `protocolVersion === 1` 探测）。

**理由**：前端零改动即可获得桌面能力；Electron 与 Rust 壳可以并存。

### D5 单实例与深链用「文件锁 + loopback socket + 发现文件」

**决定**：照搬 desigma `crates/desigma-app/src/instance.rs` 的思路：监听 `127.0.0.1:<ephemeral>`，把 `{port, token}` 以 `0600` 写入 `$XDG_RUNTIME_DIR/dsh-desktop-rs/instance.json`，行协议 `<token> open-link <url>` / `open-file <path>` / `focus` / `ping`。

**理由**：跨平台、零依赖、可鉴权，比 D-Bus 注册 / 命名管道各写一遍省事。Linux 上真需要 D-Bus 激活时再加。

### D6 不在 UI 线程上跑 async runtime

**决定**：`tokio` 只存在于 `dsh-supervisor` 内部（子进程 stdio、HTTP、文件监听）。对外暴露同步 API：命令非阻塞投递，事件走通道，wake 回调触发主循环重绘。

**理由**：desigma 的 `desigma-net/src/lib.rs:8-11` 已经证明这是「GUI 主循环 + 网络世界」的正确接缝；反过来把 runtime 塞进 UI 线程会污染所有调用点。

### D7 版本态度：探测 + 报警，不硬拦（✅ 已实现）

**决定**：启动时执行 `dsh --version`，与内置的 `SUPPORTED = ">=0.2.2-alpha.2 <0.3"` 比对。不匹配 → 标题栏横幅 + 日志，不阻止启动。

**理由**：协议无版本协商（唯一锚点是 boot graph 的 `rev` + `release`，且是全有全无策略）。硬拦会在上游小幅改动时把用户锁死；不提示则会在协议破坏时给出无法诊断的故障。

---

### D8 托盘 / 通知 / 全局热键 / 拖拽路径：探测后启用

**决定**：四项都实现，但**先探测**，不能用的安静降级并在日志里说明原因。不引 D-Bus 客户端库（`global-hotkey`、`notify-rust`、`tray-icon` 自带实现），也不因此引入第二套 UI 框架。

**理由**：
- 早期版本的推迟理由是「本机没有 `StatusNotifierWatcher`」——复审时实测发现**有**：`gdbus` 查询返回 true，托盘真实创建成功。说明「不能验证」这个判断本身需要靠探测而不是靠假设。
- 但用户环境千差万别（GNOME 默认没有 AppIndicator、无头会话没有通知服务），所以正确形态不是「有/没有」二选一，而是**运行时探测 + 降级 + 说清楚**。

**实现**：
- 托盘：`gdbus` 查 `org.kde.StatusNotifierWatcher` → 有才创建（菜单：显示窗口 / 重新加载 / 退出）。
- 通知：`notify-rust` 走 D-Bus，失败只在日志里记一行，不阻塞主流程。
- 全局热键：`Ctrl+Alt+D` 唤回窗口；注册失败只记日志。
- 拖拽路径（R2）：wry 的原生 `DragDropEvent::Drop` 带完整路径，按**文件名**回填给 `__DSH_HOST_PATHS__.pathFor(file)`，取过即出队；WebKitGTK 的 `File` 本身没有 `.path`。

**被否决的方案**：
- 无条件创建托盘（GNOME 上会「创建成功但看不见」，比没有更糟）。
- 自绘一条 D-Bus 客户端（体积/维护成本远超收益，现有 crate 已经够薄）。
- 放弃拖拽真实路径、只保留「上传字节」（用户体验明显退化，且实现只多 40 行）。

**验收**：`--selftest --simulate-drop <文件>` 输出 `{"tray":true,"hotkeys":true,"notify":true}` 与 `{"hostPath":"/tmp/…"}`；见 `docs/m2-acceptance.md` §4.2。

## 4. 架构

```
┌────────────────────────────────────────────────────────────────────┐
│ dsh-desktop-rs (bin)                                               │
│   app.rs      主循环：事件分发、状态机、生命周期                    │
│   window.rs   tao 事件循环 + wry WebView + 导航策略                 │
│   menu.rs     muda 原生菜单（File/Edit/View/Window/Help）           │
│   tray.rs     tray-icon（可选，GNOME 需扩展）                       │
│   ipc.rs      webview ↔ Rust 消息路由（window.ipc.postMessage）     │
│   inject.rs   初始化脚本 + shim 版本握手                            │
└───────┬───────────────────────────────┬────────────────────────────┘
        │                               │
┌───────▼──────────────┐        ┌───────▼─────────────────────────┐
│ crate: supervisor    │        │ crate: platform（薄，按 cfg 分） │
│  spawn/parse/健康检查 │        │  dirs, single-instance, notify, │
│  优雅关闭 10s→TERM→KILL│       │  xdg-open, xdg-desktop-portal,  │
│  tokio 关在里面       │        │  pdeathsig, 窗口状态持久化       │
└───────┬──────────────┘        └─────────────────────────────────┘
        │
┌───────▼──────────────────────────────────────────────────────────┐
│ 外部：dsh 可执行文件（上游原样，不 fork）                          │
│   dsh --profile web --host 127.0.0.1 --port 0 --no-open           │
│   ├─ HTTP  GET /?token=… → 303 + cookie                           │
│   ├─ HTTP  POST /api/<ns>/<method>                                │
│   └─ WS    /api/remote.mux                                        │
└──────────────────────────────────────────────────────────────────┘
```

**依赖方向（硬约定，写进 CONTRIBUTING）**：
`bin → supervisor → platform`，`bin → platform`；`supervisor` 与 `platform` 互不依赖；**任何 crate 都不许依赖 `bin`**。这条来自 desigma 的 `CONTRIBUTING.md:70`，它让 80% 的逻辑能在无窗口的 CI 里测。

**为什么只有两个库 crate**：壳的逻辑量很小（预计 4–6k 行）。desigma 的 16 crate 切分是为 17 万行服务的；照搬会得到一堆 200 行的 crate 和双倍的 Cargo.toml 维护成本。

### 4.1 主循环与线程

| 角色 | 线程 | 说明 |
|---|---|---|
| tao 事件循环 + wry WebView | 主线程 | 唯一拥有窗口/菜单/托盘句柄的地方 |
| supervisor 的 tokio runtime | 独立线程 | 子进程 stdio、版本探测、更新检查 |
| 事件回传 | `crossbeam-channel` / `mpsc` | 每轮 `about_to_wait` 里 drain，再 `window.request_redraw` |

**禁止**：在事件回调里做阻塞 IO（读文件、等子进程）。全部转成消息。

---

## 5. 生命周期状态机

```
                 ┌──────────┐
      spawn ────▶│ Starting │──timeout(30s)──▶ Failed(msg)
                 └────┬─────┘
        parse "dsh web: <url>"
                      ▼
                 ┌──────────┐  webview 加载失败 ──▶ Failed(msg)
                 │  Ready   │──────────────────────┘
                 └────┬─────┘
     用户关闭/托盘隐藏 ▼
                 ┌──────────┐  子进程退出(code≠0) ─▶ Failed(msg)
                 │ Running  │──────────────────────┘
                 └────┬─────┘
        quit          ▼
                 ┌──────────┐ 10s 未退 ──SIGTERM──▶ 3s ──SIGKILL──▶ Stopped
                 │ Stopping │
                 └──────────┘
```

要点：

- **Starting 阶段必须渲染一个本地页面**（`about:blank` + 内联 HTML 骨架），否则用户看到的是空白窗口 1–3 秒。骨架里显示「正在启动本地服务…」，失败时在同一页面显示诊断 + 「打开日志」「复制诊断」两个按钮。
- **孤儿进程**（M0 已实测，见 `docs/spike-webkitgtk.md` §4-1）：`pnpm dsh` 会展开成 **4 个进程、跨至少两个进程组**，只 `kill` 直接子进程会让真正的宿主继续占着端口。因此：`setsid` 启动建新进程组 → 关闭时对 **pgid** 发 `SIGTERM` → 轮询 10s → 对 pgid 发 `SIGKILL` → 校验端口/pid 已释放。同时叠加 `prctl(PR_SET_PDEATHSIG, SIGTERM)`（`pre_exec`），保证壳被 `kill -9` 时宿主不残留。macOS/Windows 用 job object（M3）。
- **优先 spawn 单个 `dsh` 可执行文件**而非 `pnpm dsh`：进程链从 4 个降到 1 个，关闭逻辑立刻简单一半。仅在开发期需要源码构建时退回 `pnpm dsh`。
- **重试**：宿主崩溃后自动重启最多 2 次（指数退避 1s/3s），之后进 Failed 并给出「重启宿主 / 打开日志 / 退出」。
- **关闭窗口 ≠ 退出**：默认隐藏到托盘（可配置），`Stopping` 只在真正退出时进入。

---

## 6. 对 dsh 的契约面

我们只依赖这些。每一条都标注稳定性，因为上游没有 wire 版本协商。

| # | 契约 | 稳定性 | 破坏时如何发现 |
|---|---|---|---|
| C1 | `dsh --version` 输出可解析 | 高 | 启动探测；解析失败 → 仅告警 |
| C2 | `--profile web --host --port 0 --no-open` 存在 | 高 | 启动失败即报错（`startup.ts:64-66`） |
| C3 | stdout 出现 `dsh web: <url>` | **中**（格式无版本保证，但注释明说是 supervisor 契约） | 30s 超时；错误文案直接提示「宿主未打印就绪 URL，可能是不兼容的 dsh 版本」 |
| C4 | 该 URL 的 `?token=` 首次访问返回 303 + Set-Cookie | 高（`v1`/`v2` 显式版本化） | 收到 4xx → Failed，提示「认证 token 无效，请升级壳或 dsh」 |
| C5 | `window.__DSH_BOOT__` 由 Host 注入 index | 高（`frontend-static` 的职责描述） | 前端白屏 → 壳检测 `did_finish_load` 后 3s 内无 `dsh-shim:hello` 消息即提示 |
| C6 | `window.dshDesktop` 缺失时前端降级 | 高（`ui-*` 包显式探测 `protocolVersion === 1`） | 无需检测——这是我们的基线假设 |

**不做**：不硬编码任何 `/api` 路径、不解析 WebSocket 帧、不读 boot graph。

---

## 7. 能力对齐（Electron → Rust 壳）

| # | Electron 能力 | Rust 壳 | Phase |
|---|---|---|---|
| 1 | spawn 宿主 + 等 ready | 解析 stdout（D3） | M1 |
| 2 | 开窗 + `loadURL` | wry（**M0 已在 WebKitGTK 2.52.6 实测通过**） | M1 |
| 3 | 自定义 `dsh-app://`、反代、header 重写 | **不做**（D2） | — |
| 4 | 单实例 | 文件锁 + loopback（D5） | M2 |
| 5 | 原生菜单 | `muda` | M2 |
| 6 | 托盘 | ✅ 已实现：`gdbus` 探测到 `org.kde.StatusNotifierWatcher` 才创建，菜单三动作，见 [ADR-D8](#d8-托盘--通知--全局热键--拖拽路径探测后启用) | M3 |
| 7 | 自动更新 | ✅ 已实现：读 `--update-feed` / `DSH_DESKTOP_UPDATE_FEED` 指向的 JSON（`{version,url}`），比对版本后用 `updates.status/subscribe` 推给前端、并可从「帮助 → 检查更新」打开下载页；未配置更新源时保持 `idle`、不发请求 | M3 |
| 8 | 原生目录选择 `__DSH_DIRECTORY_PICKER__` | shim → GTK `FileChooser`（`rfd`） | M2 |
| 9 | 文件拖拽取真实路径 `__DSH_HOST_PATHS__` | ✅ 已实现：wry 原生 `DragDropEvent::Drop` 记路径，按文件名回填给 `pathFor(file)`（R2 已收口） | M3 |
| 10 | 设备信息 `deviceInfo()` | 读 `/etc/os-release` + CPU/内存 | M2 |
| 11 | 快捷键系统 `keyboard` / `shortcuts` | 壳级快捷键（菜单加速键，来自 `bindings()` 表）+ 全局热键 `Ctrl+Alt+D` 均已实现；前端快捷键**配置**（`shortcuts.edit`）仍返回明确拒绝 | M3 |
| 12 | 内嵌浏览器分区 `browser` | **不做**：外链走系统浏览器；`ui-sidebar-browser` 退化为普通 iframe | — |
| 13 | 崩溃恢复 | 宿主重启 + 上一次会话 URL 恢复 | M2 |
| 14 | 窗口状态记忆 | `$XDG_STATE_HOME/dsh-desktop-rs/window.json` | M1 |
| 15 | 深链（`dsh://`） | ✅ 已实现：`xdg-mime` 注册 + `Exec=dsh-desktop %u`；映射规则 `dsh://x/y` → `<origin>/x/y` | M3 |

---

## 8. UI 与设计规范

**主 UI 是 Web，不由我们决定。** 本章只约束两处：原生 chrome（菜单/对话框/关于/更新窗）与失败页面；以及 Phase 2 若做原生侧栏时的基线。

借自 desigma（`/home/suveng/github/desigma`）并做三处修正：

### 8.1 令牌

| 项 | desigma | 我们的做法 | 理由 |
|---|---|---|---|
| 主题 | 暗色 `const` 单主题 | **运行时 `Tokens` 结构 + 跟随 `xdg-desktop-portal` 的 color-scheme** | 桌面 app 必须跟随系统；desigma 想加浅色要改 96 个 `Color32` |
| 间距 | 无刻度（`theme.rs:165` 空节，全组件绝对坐标） | **4/8 网格**：4,8,12,16,24,32 | 无刻度是 desigma 最大的可维护性债 |
| 字号 | 9/11/13/20 | 11/12/13/15，标题 20 | 9px 在 1x 屏太吃力 |
| 圆角 | 2/5/13 | 2/6/12 | 沿用「小控件 / 面板 / 浮层」三档语义 |
| 层级 | 2 档（200/400） | 沿用 2 档 | 够用，别造第三档 |

必抄的两个配方（数值可直接用）：
- **浮层**：双阴影 + 填充 + 1px 内环 + 顶部 0.5px 高光（desigma `widgets/mod.rs:79-87`）。
- **文字基线**：`INTER_CENTER_TO_BASELINE = 0.3638`；所有文字走 `text_on_baseline` / `centered_baseline` / `elide` 三条原语（`widgets/mod.rs:132-151`）。

### 8.2 交互

- **菜单必须全键盘可达**：`ArrowUp/Down` 跳过 disabled、`ArrowRight` 进子菜单、`ArrowLeft` 退一层、`Esc` 逐层退、新开菜单默认高亮当前勾选项、**只有指针真的移动才让 hover 抢高亮**（desigma `widgets/menu.rs:225-283, 381-397`）。
- **快捷键从第一天就是 `HashMap<(Key, Modifiers), Action>`**，不是 `match (key, shift, alt)`。desigma 写死三元组导致不可配置、帮助页只能手写 12 条（`shell.rs:410-470`、`dialog.rs:154-171`）。
- **单一 `Action` 枚举 + 单一 `run()` dispatcher**，菜单/快捷键/命令面板三处共用。

### 8.3 失败页面（唯一由我们设计的整页 UI）

三行结构：**一句话说明发生了什么 → 一个可复制的诊断块 → 两个按钮**（重启宿主 / 打开日志目录）。配色沿用 §8.1，错误色 `#F24822`。

---

## 9. 风险登记

| ID | 风险 | 影响 | 缓解 |
|---|---|---|---|
| **R1** | WebKitGTK 渲染 dsh 前端有差异（CSS/JS 特性、`perMessageDeflate`、长列表性能） | 高：可能整体方案不成立 | **M0 第一件事就是验证**：在 `webkit2gtk-4.1` 里打开 `dsh web`，跑一遍会话流式输出、文件树、diff、图表 |
| **R2** | `__DSH_HOST_PATHS__.pathFor(file)` 用的是 Electron 的 `webUtils`，WebKitGTK 的 `File` 没有真实路径 | 中：拖拽文件变成「上传」而不是 `@path` 引用 | 原生 GTK drop 目标捕获路径，通过 IPC 注入；退化行为本身可用 |
| **R3** | `dsh web: <url>` 格式变更 | 高：无法启动 | 启动探测 + 明确错误文案；向上游提 issue 建议 `--url-file` 或 `--print-url-json` |
| **R4** | token 泄漏到日志/崩溃报告 | 高：本机 RCE 面 | 内存持有、`Debug` 手写脱敏、日志过滤器对 `token=` 打码；**禁止**把 URL 写进窗口标题 |
| **R5** | GNOME 无托盘扩展 | 低 | 检测 `org.kde.StatusNotifierWatcher` 是否存在，缺失则隐藏托盘并保留「关闭即退出」语义 |
| **R6** | 子进程孤儿 | 中 | `PR_SET_PDEATHSIG` + 启动时扫描 `$XDG_RUNTIME_DIR` 的旧 pid 文件 |
| **R7** | 上游协议漂移 | 中 | D7 版本区间 + 启动横幅；不做静默兼容 |
| **R8** | 多显示器缩放 / HiDPI 模糊 | 中 | `tao` 的 `scale_factor` 透传给 webview；启动时记录并允许 `--force-scale` 覆盖 |

---

## 10. 测试策略

| 层 | 手段 | 何时 |
|---|---|---|
| supervisor 单元 | 用假 `dsh` 脚本（打印固定 URL / 延迟 / 崩退 / 不打印）驱动状态机 | M1，CI 无窗口 |
| 就绪解析 | 真实 `dsh --version` + 真实启动一次的契约测试（`#[ignore]`，本地跑） | M1 |
| 壳冒烟 | `xvfb-run` + WebKitGTK，断言：窗口出现、shim 握手、`document.title` 变化 | M2，CI |
| 自动化输入 | 页面内 JS（受控输入 + 点「发送消息」按钮），不依赖 `xdotool` / 窗口管理器 —— M0 已验证；本机会话无 WM，`windowactivate` 会直接卡死 | M2 |
| 视觉 | 失败页面与原生对话框：固定尺寸截图 + 逐像素对比（照搬 desigma 的 `--screenshot --scale 2` + SSIM 思路） | M2 |
| 布局 lint | 原生 chrome 里断言「文字不溢出容器、图标在方按钮里居中」（desigma `lint.rs` 的做法） | M2 |

**原则**：所有能在无窗口环境测的逻辑都必须无窗口可测 —— 这是 crate 边界（§4）存在的理由。

---

## 11. 里程碑

### M0 可行性验证（0.5–1 天）—— **gate，不通过就改方案** —— ✅ **已通过（2026-11-01）**

结果、截图与 7 条失败点见 [`docs/spike-webkitgtk.md`](spike-webkitgtk.md)。一句话：WebKitGTK 2.52.6 加载真实 `dsh web`，完成登录 / 发消息 / **148 个 WS 流式帧** / 无 JS 报错，R1 不成立。

1. `webkit2gtk-4.1` + Python/GTK 最小 webview 打开 `dsh web` 的 URL，看能否正常登录、发消息、流式输出。
2. 确认 `dsh web: <url>` 在实际 stdout 中的确切格式与出现时机。
3. 确认 loopback origin 下 cookie 与 WebSocket 正常（不出现 401/403）。

产出：`docs/spike-webkitgtk.md`（结论 + 截图 + 失败点）。

### M1 最小可用壳（3–5 天）

`tao` + `wry` 窗口 → spawn 宿主 → 解析 URL → 加载 → 窗口状态记忆 → 优雅退出（10s/SIGTERM/SIGKILL）→ `PR_SET_PDEATHSIG` → 失败页面。

**验收**：`cargo run` 后 3 秒内看到 dsh 界面；能正常对话；关窗进程完全退出、无残留 `dsh` 进程。

### M2 桌面化（1 周）

单实例 + 深链 + 原生菜单 + 原生目录选择 shim + 设备信息 + 宿主崩溃自动重启 + `xvfb` 冒烟测试。

**验收**：第二次启动只聚焦已有窗口；`dsh://` 链接能唤起；菜单项与快捷键全部可用；`kill -9` 宿主后 2 秒内自动恢复。

### M3 分发（3–5 天）—— ✅ 已完成（2026-11-01），见 [`docs/m3-acceptance.md`](m3-acceptance.md)

AppImage + `.deb` + `.tar.gz`；`.desktop` + MIME 注册；图标；托盘（有扩展时）；更新提示；`--screenshot` 类调试开关。

**验收**：干净容器里 `dpkg -i` 后从应用菜单启动即用；AppImage 在无 root 环境可用。

---

## 12. 目录结构

```
dsh-desktop-rs/
  Cargo.toml                 # workspace
  crates/
    bin/                     # 二进制 dsh-desktop-rs：main/app/window/menu/tray/ipc/inject
    supervisor/              # 宿主进程：spawn、就绪解析、健康检查、关闭升级
    platform/                # dirs、单实例、通知、xdg-open、窗口状态、pdeathsig
  docs/
    architecture.md          # 本文
    spike-webkitgtk.md       # M0 产出
  packaging/
    linux/                   # .desktop、MIME xml、install.sh、AppImage 构建
  .github/workflows/         # ci（fmt/clippy/test）、release（打包矩阵）
```

---

## 13. 附录：可迁移的 desigma 经验（只列与本项目相关的）

| 经验 | 出处 | 本项目如何用 |
|---|---|---|
| 依赖只向下、域逻辑不依赖 UI | `CONTRIBUTING.md:70` | §4 的 crate 边界 |
| 单一 `Action` + `run()` dispatcher，命令面板由菜单树派生 | `panels/menus.rs:129, 685-706` | §8.2 原生菜单 |
| 浮层配方 = 双阴影 + 填充 + 1px 内环 + 0.5px 高光 | `widgets/mod.rs:79-87` | §8.1 |
| 文字三条原语 + `0.3638` 基线常量 | `widgets/mod.rs:132-151, 142` | §8.1 |
| hover 用 1px 描边、selected 用填充 | `panels/layers.rs:239-252` | §8.2 |
| 菜单全键盘导航 + 指针移动才抢 hover | `widgets/menu.rs:225-283, 381-397` | §8.2 |
| 单实例 = loopback TCP + `0600` 发现文件 | `instance.rs:1-27` | D5 |
| tokio 关在库内、对外同步门面 + wake 回调 | `net/src/lib.rs:8-11` | D6 |
| headless 截图 + 像素 diff 做视觉回归 | `CONTRIBUTING.md:37-52` | §10 |
| 布局 lint（文字溢出 / 图标居中） | `lint.rs` | §10 |
| 失败要给用户「一句可操作的话」而不是堆栈 | `io.rs:19-30` | §8.3 |

**不适用本项目的 desigma 经验**：双渲染器（CPU 确定性 + GPU）、瓦片缓存与空间索引 diff、`.fig`/`.pen` 解析、Figma 测量坐标、固定不可拖布局、`const` 令牌。这些是「设计工具 + 无限画布」的解，不是「桌面壳」的解。
