# 安装引导

壳本身不含 dsh。**任何一次启动失败**（没有 node、没有 dsh、版本不在支持区间、宿主起不来）
都会把窗口切到安装引导页；装好之后不用重开壳，直接进主界面。

## 从哪进

- **自动**：启动时环境不全、或宿主起不来（见下表）。
- **手动**：菜单「文件 → 安装引导 / 环境管理…」（`Ctrl+Shift+I`），或托盘的「安装引导 / 环境管理…」。
  宿主健康时也会停在「完成」页给环境概览——**不会**自动重启宿主（刚点开就被夺回界面，日志还没看）；
  改动完点「启动」即可回主界面。

## 什么时候会进引导页

| 触发 | 页面状态 |
|---|---|
| `node` 缺失或 < 24.9 | 第一步「环境」标红，可一键装 node |
| `dsh` 缺失 | 「dsh 未安装」，点安装即从 npm 装 |
| `dsh --version` 不在 `>=0.2.2-alpha.2 <0.3` | 「dsh 不支持」，装一个可用版本 |
| 宿主启动失败 / 连续退出两次 | 进引导页，这次失败的「命令 + 宿主日志尾巴」已经写进日志面板 |
| 没有可启动的 profile | 「Profile」步骤里没有可选宿主 |

显式传了 `--host-cmd` / `--host-program` 时**不会**因为环境不全而拦截启动（开发者 / CI 路径），
但那次启动如果失败，仍然会进引导页。

## 六步

1. **环境**：三行体检（node / dsh / profile）。装 node 时可以指定版本，留空用 `24.13.0`。
2. **代理**：`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` 与 npm 镜像，
   安装范围（`优先全局` / `只装全局` / `只装用户目录`）。
3. **版本**：下拉里是已发布且落在支持区间内的版本（默认 `0.2.2-alpha.3`）；下方是安装历史，用于回退。
4. **Profile**：选哪个 profile 当宿主（只列带 `@deepseek-ai/dsh-web-app` 的），
   还能备份 / 还原 / 导入 profile。
5. **安装**：实时日志，可展开收起、可取消。
6. **完成**：点「启动」进主界面；环境就绪时队列跑完会自动启动。

顶部三个胶囊（node 版本、dsh 版本、`$DSH_HOME`）在每一步都在，随时能看出卡在哪。

## node 从哪来

没有可用的 node 时，从 `https://nodejs.org/dist` 下官方 tarball（`node-v<版本>-linux-<arch>.tar.gz`），
解压到 `~/.local/share/dsh-desktop-rs/node/v<版本>/`。下载走系统 `curl`（和更新检查同一套路，
不额外引 TLS 栈），可以直接取消。

顺序是**先取 `SHASUMS256.txt` → 下载 tarball → 核 SHA-256 → 解压 → `<prefix>/bin/node --version` 自检**：
摘要对不上就删包并停在进度页报错，绝不会把来路不明的 tar 铺进用户目录
（单测 `a_bad_shasum_stops_the_node_install` / `a_good_shasum_lets_the_install_continue`）。

## dsh 装到哪

- **优先全局**：先 `npm install -g @deepseek-ai/dsh@<版本>`；遇到 `EACCES`（系统目录没权限）
  自动退回用户前缀 `~/.local/share/dsh-desktop-rs/host`，不用你手动加 `sudo`。
- 装完会重新探测，把实际启动的 dsh 记进历史（`settings.json` 的 `history`），
  「版本」页的「回退到上一个版本」用的就是它。
- 宿主解析链里，引导页装的 `host/bin/dsh` 排在 `PATH` 里的 `dsh` **前面**，
  但 `<cwd>/apps/cli/lib/bin.js`（dsh 源码工作区）仍然最优先，开发流不受影响。
- 命名行候选**每次探测都重算**（工作区 → 用户前缀 → `npm prefix -g` → `PATH` → `pnpm`）。
  这条是必须的：全新机器上「优先全局安装」装完时，启动那一刻 `PATH` 里还没有 dsh，
  若沿用旧名单就会永远判成「环境不全」。单测 `a_globally_installed_dsh_is_detected`、
  `a_dsh_on_path_makes_the_environment_ready` 就守这个分支。

## 代理与凭据

- 代理只注入**安装 / 下载子进程**（`npm`、`curl`）的环境变量，宿主进程不继承。
- 设置写在 `$XDG_STATE_HOME/dsh-desktop-rs/settings.json`，权限 `0600`，目录 `0700`。
- 界面与日志里凭据会被遮成 `scheme://***@host`；`LaunchSpec::command_line()` 同样会遮。

## profile 备份

- 备份：`$XDG_STATE_HOME/dsh-desktop-rs/backups/<profile>-<stamp>.tar.gz`，**不含 `node_modules`**。
- 还原前自动把现有目录挪成 `<profile>.bak-<stamp>`（还原失败会挪回来）。
- 导入：接受 `.tar.gz`，单层目录自动扒掉，必须含 `package.json`。
- 还原 / 导入之后**自动排队 `RepairDeps`**：归档不含 `node_modules`，依赖由壳补。

这些都对应 `crates/setup/src/profiles.rs`，纯逻辑 + 单测，不碰界面。

## 依赖重建（`RepairDeps`）

跑 `pnpm install` 之前先做三件事，见 `crates/setup/src/pnpm.rs`：

1. `allowBuilds` 占位符定值（`<pkg>: set this to true or false` → `<pkg>: true`），
   否则 pnpm 直接 `ERR_PNPM_IGNORED_BUILDS` 退出；
2. store 自检：`node_modules/.modules.yaml` 的 `storeDir` vs profile 目录里 `pnpm store path`；
3. 不一致就带 `CI=true` 整装重装（pnpm 只在非交互下 purge `node_modules`）；
   判定不了时按「照常 install」处理，不凭猜测删依赖。

完整输出落 `<state>/logs/profile-deps.log`（成功也写：能对照当时用的哪套 store）。

## 实现位置

| 文件 | 职责 |
|---|---|
| `crates/setup/src/settings.rs` | `settings.json`：代理、镜像、版本、profile、安装历史；`0600` 原子写 |
| `crates/setup/src/envcheck.rs` | node / dsh 探测、版本区间、宿主候选链、PATH 前缀 |
| `crates/setup/src/node.rs` | 官方 tarball 下载 + SHA-256 校验 + 解压 + 自检 |
| `crates/setup/src/install.rs` | 长任务：逐行日志、可取消、超时 |
| `crates/setup/src/profiles.rs` | profile 枚举 / 备份 / 还原 / 导入 / 补依赖 |
| `crates/setup/src/pnpm.rs` | 依赖重建：store 自检、`allowBuilds` 定值、`CI=true` 重装、`pnpm store path` 探测 |
| `crates/bin/src/onboard.rs` | 状态机：六步、命令、执行队列、`view()` 快照 |
| `crates/bin/assets/onboarding.html` | 纯渲染：只吃 `view()` 的 JSON，点击回传命令 |
| `crates/bin/src/main.rs` | 失败路由、IPC 分发、把快照推给页面、装完重启宿主 |

界面是「哑」的：状态全在 Rust 侧，刷新页面、丢消息都不影响正确性。

## 验收

```bash
cargo test --workspace                 # 含 onboard 状态机、假 npm 的安装/回退用例
xvfb-run -a scripts/smoke.sh           # 场景 1：启动失败 → 引导页真的画出来了
```

`scripts/smoke.sh` 用临时 `XDG_RUNTIME_DIR` / `XDG_STATE_HOME` 跑，不会碰你正在用的桌面壳，
也不会读你存下的窗口大小。

## 验收记录（2026-11-04，真机 + xvfb 实跑）

四类启动失败都进引导页：

| 场景 | 怎么造 | 结果 |
|---|---|---|
| 没有 node / 没有 dsh | `PATH=/nonexistent target/release/dsh-desktop` | 「准备运行环境」+ 两个红色胶囊，`docs/shots/onboarding-env.png` |
| dsh 版本越界 | 本机 `PATH` 上是 `dsh 0.1.1-rc.2` | 胶囊 `dsh 0.1.1-rc.2（不支持）`，版本页只列区间内的版本，`docs/shots/onboarding-proxy.png` |
| 没有可启动 profile | profile 目录里只有不带 Web App 的 | profile 页只把带 `@deepseek-ai/dsh-web-app` 的列为可选，其余标「无 Web」 |
| 宿主起不来 | `--host-program /nonexistent/dsh` | 同样进引导页，日志里是「启动失败 → 命令 → 最后 40 行宿主日志」，`scripts/smoke.sh` 就在断言这条 |

**用户目录装一遍**（本机 `~/.local/share`，代理 + npmmirror，`install_scope=user`）：

```
$ ls -l ~/.local/share/dsh-desktop-rs/host/bin/dsh
lrwxrwxrwx 1 suveng suveng 47 11月  4 01:11 …/host/bin/dsh -> ../lib/node_modules/@deepseek-ai/dsh/lib/bin.js
$ ~/.local/share/dsh-desktop-rs/host/bin/dsh --version
0.2.2-alpha.3
$ du -sh ~/.local/share/dsh-desktop-rs
927M
$ stat -c '%a %y' ~/.local/state/dsh-desktop-rs/settings.json
600 2026-11-04 01:11:15
$ python3 -c "import json;d=json.load(open('$HOME/.local/state/dsh-desktop-rs/settings.json'));print(d['history'])"
[{'version': '0.2.2-alpha.3', 'program': '/home/suveng/.local/share/dsh-desktop-rs/host/bin/dsh',
  'installed_at': 1793725875}]
```

装完的同一进程里日志就是 `dsh-desktop: 启动 /home/suveng/.local/share/dsh-desktop-rs/host/bin/dsh
--profile web --host 127.0.0.1 --port 0 --no-open` + `shim 回填 … {"phase":"idle"}`（自动进主界面，
`docs/shots/onboarding-finished.png`）。再启动一次仍然解析到这份 dsh（用户前缀压过 `PATH` 上的旧版）。

**优先全局装一遍**（沙箱：`PATH=/tmp/glob/bin:…`，假 `npm` 只把 dsh 链接到 `/tmp/glob/bin`，
全局前缀报 `/tmp/glob`，`install_scope=auto`）：装完复检立刻认出新路径，
日志 `启动 /tmp/glob/bin/dsh …` 并渲染出主界面——这正是「全局装成功也要能自己接上」的分支。

**手动入口**（宿主健康时）：`Ctrl+Shift+I` 或托盘 → 停在「完成」页
（`就绪 · node v24.13.0 · dsh 0.2.2-alpha.3 · profile web`，按钮 重新检测 / 启动 / 打开日志目录 / 退出），
不会自动重启宿主，`docs/shots/onboarding-entry.png`。

自动化：

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace     # bin 26 / platform 7 / setup 40 / supervisor 8
xvfb-run -a scripts/smoke.sh
```

## 全新机器验收（2026-11-04，真正空环境）

前面的真机验收是在「node 已就绪 + 数据目录已存在」的机器上做的，绕开了最容易断的两条路。
这一段换成审计用的那种环境复现：

```bash
rm -rf /tmp/fresh && mkdir -p /tmp/fresh/{home,data,state,runtime}
chmod 700 /tmp/fresh/runtime
env -i HOME=/tmp/fresh/home XDG_DATA_HOME=/tmp/fresh/data \
       XDG_STATE_HOME=/tmp/fresh/state XDG_RUNTIME_DIR=/tmp/fresh/runtime \
       PATH=/usr/bin:/bin target/debug/dsh-desktop --onboard-selftest
```

`PATH=/usr/bin:/bin` 下只有系统自带的 node 18.19.1（低于 24.9.0），没有 dsh、没有 profile、
数据目录也不存在。`--onboard-selftest` 跑的是 GUI 同一套 `Onboard`，只是不渲染：

```
ONBOARD ENV {"data_dir":"/tmp/fresh/data/dsh-desktop-rs","dsh":{"state":"missing"},
             "node":{"program":"/usr/bin/node","state":"too_old","version":"18.19.1"},
             "profiles":[],"ready":false}
  ⟳ 获取可用版本
  拿到 2 个可选版本
  ⟳ 获取 node v24.13.0 的校验清单
  ⟳ 下载 node v24.13.0
  ⟳ 校验 node v24.13.0 的 SHA-256
  node-v24.13.0-linux-x64.tar.gz SHA-256 校验通过（6223aad1a81f9d1e7b682c59d12e2de2337b4c37475cd40d1c89c42b737ffa8）
  ⟳ 解压并自检 node v24.13.0
  node v24.13.0 就绪：/tmp/fresh/data/dsh-desktop-rs/node/v24.13.0
  ⟳ 安装 dsh 0.2.2-alpha.3
  added 656 packages in 2m
  ⟳ 重新检测环境
ONBOARD RESULT {"dsh":{"program":"/tmp/fresh/data/dsh-desktop-rs/node/v24.13.0/bin/dsh",
                       "source":"随壳安装的 node","version":"0.2.2-alpha.3"},
                "ready":true,"screen":"done","start_host":true}
```

GUI 里同样跑了一遍：`docs/shots/onboarding-fresh-wizard.png` 是全新机器打开时的第一屏
（`node v18.19.1（过低）` / `dsh 未安装` / `还没有任何 profile`），点「开始安装」后装完自动启动，
`docs/shots/onboarding-fresh-machine.png` 是直接进主界面的样子。

### 这一轮修掉的东西

- **下载目录没人建**：`node::download` 现在自己 `create_dir_all` 目标目录的父级；`Onboard::open`
  也把 `data/`、`state/`、`backups/` 建好。之前全新机器上第一次点「开始安装」必然
  `curl: (23) Failure writing output to destination`。
- **壳自己装的 node 前缀里的 dsh 没人认**：「优先全局安装」落到可写的 app node 前缀后，
  这条路径不在 PATH 上。现在 `detect()` 会扫 `<data>/node/v*/bin/dsh`，启动链
  （`host_candidates`）也把同一批加进候选，优先级排在用户目录安装之后、PATH 之前。
- **装完启动还回退 `pnpm dsh`**：`launch_spec` 会把这台 node 的 `bin` 放进宿主 PATH，
  并写回 `settings.node_version`，保证 `#!/usr/bin/env node` 能解析到它。
- **刚打开向导就点「开始安装」没反应**：打开时会先拉版本列表，`busy()` 把点击吞掉了。
  现在请求会排队（`queued`），队列空了重放；`busy()` 也把排队算进去，界面不会提前停。
- **全新机器一个 profile 都没有被判成「不能启动」**：dsh 首次带 `--profile web` 起来会按模板建，
  空目录现在算就绪；只有「有 profile 但没一个带 Web App」才拦。
- **日志被 curl 进度表刷屏**：下载加 `--no-progress-meter -S`，只留错误行。
