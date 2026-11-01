# dsh-desktop-rs

用 Rust 写的 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) 桌面客户端，目标平台 **Linux**。

- 壳用 `tao` + `wry`（Linux 后端 WebKitGTK），UI 复用 dsh 现有 Web 前端与全部插件生态。
- Rust 只负责：宿主进程监督、窗口、原生菜单、单实例、深链、窗口状态、崩溃自愈。
- 不实现 dsh 的 RPC 协议，也不 fork dsh。

设计文档：[`docs/architecture.md`](docs/architecture.md) ·
验收记录：[M0](docs/spike-webkitgtk.md) / [M1](docs/m1-acceptance.md) / [M2](docs/m2-acceptance.md) / [M3](docs/m3-acceptance.md)

## 安装

三种方式，任选其一。

### 1. deb（推荐，需要 root）

```bash
sudo dpkg -i dsh-desktop_0.1.0_amd64.deb
dsh-desktop
```

会安装 `/usr/bin/dsh-desktop`、桌面菜单项、图标，并把 `dsh://` 链接指向本应用。

### 2. tar.gz（用户级，不需要 root）

```bash
tar -xzf dsh-desktop-0.1.0-x86_64.tar.gz -C /tmp/dsh-desktop
/tmp/dsh-desktop/install.sh                 # 装到 ~/.local
# PREFIX=/opt/dsh /tmp/dsh-desktop/install.sh   # 或自定义前缀
/tmp/dsh-desktop/install.sh --uninstall     # 卸载
```

### 3. AppImage（不安装直接跑）

```bash
chmod +x dsh-desktop-0.1.0-x86_64.AppImage
./dsh-desktop-0.1.0-x86_64.AppImage
```

## 前置条件

壳不内置 dsh，它启动的是你本机的 `dsh`：

壳按这个顺序找宿主：

1. `--host-cmd "pnpm dsh"` / `--host-program <路径>`（显式指定）
2. `<cwd>/apps/cli/lib/bin.js`（dsh 源码工作区的编译产物，**就绪只要 ~0.8s**）
3. `PATH` 里的 `dsh`（注意：全局安装的往往是 0.1.x 旧版）
4. `pnpm dsh`

推荐第 2 种：它比 `pnpm dsh`（~1.9s）快一倍以上，也是崩溃恢复能进 2 秒的前提。

启动时会跑一次 `dsh --version`：落在 `>=0.2.2-alpha.2 <0.3` 之外不会拦你，但会在日志与窗口标题上说明。

## 升级

- **deb**：下载新包后 `sudo dpkg -i <新包>`，配置与窗口状态保留在 `$XDG_STATE_HOME/dsh-desktop-rs/`。
- **tar.gz**：重新执行 `install.sh` 覆盖即可；`--uninstall` 只删程序，不动你的状态目录。
- **AppImage**：直接换成新文件。

## 使用

| 操作 | 方式 |
|---|---|
| 启动 | 应用菜单 / `dsh-desktop` |
| 第二次启动 | 只把已有窗口提到前面，不会再起一个宿主 |
| 打开 `dsh://` 链接 | `xdg-open dsh://session/abc` → 转发给运行中的实例，导航到 `<origin>/session/abc` |
| 菜单 | 文件（重新加载 / 打开日志目录 / 退出）、编辑、视图（缩放 / 全屏）、窗口、帮助（含由快捷键表生成的清单） |
| 宿主崩溃 | 立即重启（第一次不退避，之后 1s）；连续 2 次起不来则显示诊断页 |
| 托盘 | 有 `StatusNotifierWatcher` 时提供：显示窗口 / 重新加载 / 退出；GNOME 需 AppIndicator 扩展 |
| 系统通知 | 宿主退出、自动重启、有新版本时发通知；没有通知服务时只记日志 |
| 全局热键 | `Ctrl+Alt+D` 唤回窗口 |
| 拖文件进窗口 | 取真实路径，前端按 `@path` 引用而不是上传字节 |
| 更新提示 | `--update-feed <url>` 或 `DSH_DESKTOP_UPDATE_FEED`；源返回 `{"version","url"}`，有新版就在「帮助 → 检查更新」里提示并打开下载页 |

诊断信息在 `$XDG_STATE_HOME/dsh-desktop-rs/`，窗口几何在 `window.json`。

## 从源码构建

```bash
# 依赖（Ubuntu 24.04）
sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev \
  libjavascriptcoregtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev

cargo build --release
./target/release/dsh-desktop --host-cmd "pnpm dsh" --host-cwd ~/github/deepseek-harness
```

### 开发用命令

```bash
cargo test --workspace                # supervisor / platform 全部无窗口可测
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
./target/debug/dsh-desktop --probe ...    # 只验证能拿到就绪 URL（不打印 token）
./target/debug/dsh-desktop --selftest ... # 有窗口自检：桌面 shim + deviceInfo 往返
xvfb-run -a bash scripts/smoke.sh         # 冒烟：失败页像素基线（+ 可选真实前端）
scripts/package.sh                        # 出 tar.gz / deb / AppImage 到 dist/
```

## 目录

```
crates/
  bin/          tao + wry 窗口、菜单、骨架页/失败页、信号处理、shim 回环
  supervisor/   宿主进程：spawn、就绪行解析、进程组关闭、退出码上报
  platform/     XDG 路径、窗口状态、单实例（loopback + 0600 发现文件）
packaging/linux/  .desktop、MIME xml、图标、install.sh
scripts/          package.sh、smoke.sh、compare_shots.py、gen_icons.py
docs/             architecture.md + M0–M3 验收记录
```
