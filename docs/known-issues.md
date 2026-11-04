# 已知问题

## 1. 菜单/浮层只显示空白框（v0.1.3 已修）

### 症状

点工作区行的「…」或会话的「⋮」，弹出一个**位置、尺寸、边框都正确，但没有文字**的半透明面板。
鼠标在别处点一下、或者切到别的窗口再切回来，文字才出现，而且之后不再消失。

### 根因

dsh 的菜单表面在入场时给所有直接子元素套了一段淡入动画：

- `packages/client/ui-primitives/src/MenuSurface.module.css:61`
  `.surface[data-menu-phase='enter'] > * { animation: dsh-context-menu-in var(--dsh-menu-enter) ease-out; }`
- 同文件 `:68` `@keyframes dsh-context-menu-in { from { opacity: 0 } to { opacity: 1 } }`

**在这个 X 会话里没有窗口管理器**（`xprop -root _NET_ACTIVE_WINDOW` 不存在），窗口永远不会变成 active，
WebKitGTK 也就不推进 CSS 动画时钟。于是子元素停死在 `from` 帧：**`opacity: 0`**。

因此：布局完全正常（devtools 里 `span._itemLabel_…` 是 `100px × 20px`），只是画出来是透明的。
切换窗口会产生一次焦点/重排事件，动画时钟随之启动，淡入在 100ms 内走完 —— 这就是"切窗口字就出来了"。

### 修法

壳在页面里注入（`crates/bin/src/main.rs` 的 `MENU_ANIM_FIX`）：

```css
[data-menu-phase] > * { animation: none !important }
.backing[data-menu-phase] { animation: none !important }
```

元素渲染即不透明。代价只有"菜单没有淡入过渡"，毛玻璃、阴影、圆角全部保留。

想保留动画：`DSH_DESKTOP_KEEP_MENU_ANIMATION=1`。

### 归属

| 层次 | 结论 |
|---|---|
| dsh 前端 | 无责。菜单淡入是标准做法，Chromium/Firefox/Safari 都正常 |
| WebKitGTK | 行为可议：非 active 窗口里冻结动画时间轴（与后台标签页节流同源）。优先级低，未提 |
| 本壳 | 有责，已兜。触发条件是"无 WM 的 X 会话 + WebKitGTK"，正常桌面大概率不出现 |

### 排查过程中被证伪的假设（别重复走）

| 假设 | 手段 | 结果 |
|---|---|---|
| DMA-BUF 渲染器有缺陷 | 默认 `WEBKIT_DISABLE_DMABUF_RENDERER=1`（v0.1.2） | 无效 |
| `backdrop-filter` 合成层内容漏画 | 注入 `*{backdrop-filter:none!important}` | 无效 |
| 点击后没提交合成帧 | 点击后 `documentElement` opacity 抖动 | 无效 |
| 菜单 `max-height`/flex 压成 0 高 | 覆盖 `max-height:none` + `overflow:visible` | 无效 |
| 内容晚于 700ms 才挂载 | MutationObserver 记录新增节点 | 无效（内容一直在 DOM 里） |

### 教训

以上五条都是"看起来相关就动手"，没有先建立可判定真假的观测。
一条 devtools 元素面板（尺寸 `100×20`、`data-menu-phase="enter"`）就同时排除了布局、挂载、裁剪三类原因，
把范围锁到计算样式/动画上。下一步永远应该是：**尺寸对了没有 → 计算样式是什么 → 动画/过渡在推进没有**。

## 2. 壳重建的 profile 依赖链到旧 pnpm store，插件页更新永久失败（v0.1.16 已修）

### 症状

装完壳（v0.1.14，2026-11-04）后，dsh 插件页点「更新」一直失败，只能看到
`operation-error`。`~/.dsh/profiles/web/.plugin-manager/logs/operation-*/pnpm.log` 里是：

```
[ERR_PNPM_UNEXPECTED_STORE] Unexpected store location
The dependencies at "/home/suveng/.dsh/profiles/web/node_modules" are currently linked
from the store at "/home/suveng/.pnpm-store/v11".
pnpm now wants to use the store at "/home/suveng/.local/share/pnpm/store/v11" to link dependencies.
```

### 根因

壳的 `RepairDeps`（`crates/bin/src/onboard.rs:646`，与 `crates/setup/src/profiles.rs:335`）在 profile 里跑
`pnpm install --prefer-offline`，它继承壳进程环境（`crates/setup/src/install.rs:85-101` 只 `cmd.env()` 叠加
proxy 与 `npm_config_registry`）。那次执行把 `node_modules` 写成了硬链自旧 store `~/.pnpm-store/v11`
（`.modules.yaml` 的 `storeDir` 与 `prunedAt` 可证；inode 溯源亦命中旧 store 的 `files/0f/af8aac…`），
而本机当前 pnpm（v11.21.0）解析到 `~/.local/share/pnpm/store/v11`，两边不一致，之后任何
`pnpm add` 都被 store 检查拦下。`needs_deps()` 只看 `node_modules` 是否存在，看不出这个矛盾。

附带第二个坑：那次 install 还在 `pnpm-workspace.yaml` 里留下未决占位符
`allowBuilds: protobufjs: set this to true or false`，使它之后的每次 install 都以
`ERR_PNPM_IGNORED_BUILDS` 直接退出。

### 归属

| 层次 | 结论 |
|---|---|
| dsh / 插件页 / 市场 | 无责。它们只是 `pnpm add` 的使用者，拿到的已是一个自相矛盾的 profile |
| pnpm | 行为正确：跨 store 不许混链 |
| 本壳 | 有责，**v0.1.16 已修**（见下） |

### 修法（v0.1.16）

新增 `crates/setup/src/pnpm.rs`，把「重建 profile 依赖」从「无脑 `pnpm install`」换成分四步：

1. **`allowBuilds` 占位符定值**：`fix_pending_builds()` 把 `pnpm-workspace.yaml` 里
   `<pkg>: set this to true or false` 换成 `<pkg>: true`（其余行原样，`minimumReleaseAgeExclude`
   那串列表不能动）。不定值的话后面任何 install 都只会 `ERR_PNPM_IGNORED_BUILDS` 退出。
2. **store 自检**：`inspect()` 读 `node_modules/.modules.yaml` 的 `storeDir`，再在 profile 目录里问
   `pnpm store path`（必须在 profile 目录问：pnpm 默认 store 跟着 cwd 所在文件系统走），
   两者 `canonicalize` 后比对。
3. **按自检结果决定重建方式**：一致 → `pnpm install --prefer-offline`；不一致 → 同一命令加
   `CI=true`（pnpm 只在非交互下才会 purge 现有 `node_modules` 重链，实测 16s / 183 包）。
   不写死 store、不 `pnpm config set store-dir`、不导出 `npm_config_store_dir`（pnpm 11 不认）。
   判定不了时（缺 `.modules.yaml` / pnpm 起不来）按「照常 install」处理，不凭猜测删依赖。
4. **失败可见**：`RepairDeps` 的完整输出落到 `<state>/logs/profile-deps.log`（`~/.local/state/dsh-desktop-rs/logs/`，
   只留最后 64 KiB），失败时错误文案里直接带这个路径；自检/定值做的每一件事都写进引导页日志。

顺手补齐的漏洞：**还原 / 导入 profile 之后自动排队 `RepairDeps`**（归档里本来就不含 `node_modules`，
以前这一步要用户自己点）。

验收：`crates/setup/src/pnpm.rs` 的单测覆盖解析、定值、重建参数、缺 `package.json` 时拒绝；
真机自检 `real_pnpm_detects_store_mismatch`（标 `#[ignore]`）用本机 pnpm 验证「不一致能认出来、
一致时判 Ready」。

用户机器上的临时处置（不属于修复）：手工 `CI=true pnpm install --prefer-offline` +
`pnpm add dsh-plugin-feishu@0.7.0`，profile 已回到新 store。
