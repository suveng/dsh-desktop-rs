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
