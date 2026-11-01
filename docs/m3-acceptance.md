# M3 验收记录：分发

> 目标（`architecture.md` §11 M3）：产出 tar.gz / deb / AppImage，`.desktop` 与 MIME 关联生效，install.sh 在无 root 时可用。
> 日期：2026-11-01

## 1. 产物

| 产物 | 大小 | 校验 |
|---|---|---|
| `dist/dsh-desktop_0.1.0_amd64.deb` | 1392 KB | 同名 `.sha256` |
| `dist/dsh-desktop-0.1.0-x86_64.AppImage` | 2.4 MB | 同名 `.sha256` |
| `dist/dsh-desktop-0.1.0-x86_64.tar.gz` | 1791 KB | 同名 `.sha256` |

> **2026-11-01 12:30 重新打包**：首版产物（11:56）早于 D7 / 桌面集成 / 宿主解析三项修复，
> 属于过期构建。现已重跑 `scripts/package.sh`，并在**新包**上重做了下面的安装验收；
> 抽取 `/usr/bin/dsh-desktop` 后 `grep -ac` 确认包含 `托盘已启用`、`不在支持区间`、`apps/cli/lib/bin.js`。

`scripts/package.sh` 一条命令出全部三件（`SKIP_APPIMAGE=1` 可跳过 AppImage）。

## 2. 验收结果

| 验收项 | 结果 | 证据 |
|---|---|---|
| deb 结构正确 | ✅ | `dpkg-deb -c`：`/usr/bin/dsh-desktop`、`.desktop`、`hicolor/{128,256,512}` 图标、MIME xml 齐全 |
| deb 依赖声明 | ✅ | `Depends: libwebkit2gtk-4.1-0, libgtk-3-0` |
| **真实 `dpkg -i`** | ✅ | 在新包上重装成功（`正在解压` → `正在设置`），触发器正常执行；验收后 `dpkg -r` 复原 |
| `.desktop` 合法 | ✅ | `desktop-file-validate` 无输出（= 通过） |
| MIME 关联生效 | ✅ | `xdg-mime query default x-scheme-handler/dsh` → `dsh-desktop.desktop` |
| 安装后的二进制可跑 | ✅ | `/usr/bin/dsh-desktop --version` → `0.1.0`；`--selftest` 全绿（shim + deviceInfo） |
| AppImage 可直接运行 | ✅ | `./dsh-desktop-0.1.0-x86_64.AppImage --version` → `0.1.0`（本机 FUSE 可用） |
| AppImage 无 FUSE 也能跑 | ✅ | `--appimage-extract` → `./squashfs-root/AppRun --version` → `0.1.0` |
| AppImage 非 root 可用 | ✅ | 以普通用户运行 `AppRun --selftest`，输出 1 行 SELFTEST |
| install.sh 无 root 安装 | ✅ | `PREFIX=/tmp/...` 安装出 bin + desktop + 3 个尺寸图标 + MIME 数据库；二进制可执行 |
| install.sh 卸载干净 | ✅ | `--uninstall` 后 `find … | grep -i dsh` 为空 |
| 系统环境已复原 | ✅ | `dpkg -s dsh-desktop` → 未安装；`/usr/bin/dsh-desktop` 不存在 |

**这些都不是「看起来对」**：deb 是真 `dpkg -i` 装进系统、真从 `/usr/bin` 跑的；验证完已经 `dpkg -r` 卸掉，机器状态与开始时一致。

## 3. 与目标的偏差（见 `architecture.md` ADR-D9）

- **没做「干净容器」**：本机没有容器运行时（`docker`/`podman` 都不存在）。替代做法是**真实安装到当前系统后再卸载**，OS 与依赖都是真的，只是不隔离。这条偏离已按硬规则写成 **ADR-D9**。要复现容器版，用：
  ```bash
  docker run --rm -v $PWD/dist:/w ubuntu:24.04 bash -c \
    'apt-get update && apt-get install -y /w/dsh-desktop_0.1.0_amd64.deb && dsh-desktop --version'
  ```
  这一条留作 M4 的 CI 项。
- **托盘 / 系统通知 / 全局热键 / 拖拽真实路径**：**已实现**（复审后按用户要求补齐），细节见 `docs/m2-acceptance.md` §4.2 与 `architecture.md` 的 **ADR-D8**。早期版本以「本机没有 StatusNotifierWatcher」为由推迟，该判断本身是错的 —— `gdbus` 实测返回 true。
- **更新提示**：已实现并在 M2 复审里补上（`--check-update` 三条路径：无源→idle、有新版→available、同版本→up-to-date）。
- **「从应用菜单启动并完成一次对话」**：菜单项与 MIME 关联都用 `desktop-file-validate` / `xdg-mime` 校验过；「点菜单 → 发消息」这一步需要图形会话里的人手操作，本机无 WM 无法脚本化，等价证据是 `--selftest`（真实前端 + 真实宿主 + 真实 IPC 往返）与 M0 的对话闭环。

## 4. 打包踩到的两个小坑

1. **`.gitignore` 里的 `*.png` 会把图标一起忽略**，导致发布包里没有图标。已加 `!/packaging/linux/icons/*.png` 例外 —— 这类「产物被忽略」的问题只有真的打一次包才会发现。
2. **appimagetool 需要网络**：`scripts/package.sh` 在下载失败时会明确说「deb 与 tar.gz 已生成」并退出 0，而不是静默产出半个发布。

## 5. 复现

```bash
scripts/package.sh
dpkg-deb -c dist/dsh-desktop_0.1.0_amd64.deb | head
sudo dpkg -i dist/dsh-desktop_0.1.0_amd64.deb
desktop-file-validate /usr/share/applications/dsh-desktop.desktop
xdg-mime query default x-scheme-handler/dsh
dsh-desktop --version
sudo dpkg -r dsh-desktop
```
