# 需求：重建 profile 依赖时必须对齐 pnpm store（否则插件页更新永久失败）

状态：待实现（2026-11-05 提）
触发场景：用户装完壳后，在 dsh 插件页点「更新」，报 `ERR_PNPM_UNEXPECTED_STORE`。

## 1. 现象

2026-11-04 20:44:46 用户 `sudo apt install ./dsh-desktop_0.1.14_amd64.deb`，壳自举并重建
`~/.dsh/profiles/web` 的依赖。此后用户点插件页「更新」（`dsh-plugin-manager` 执行
`pnpm add <全部直接依赖>`）一直失败：

```
[ERR_PNPM_UNEXPECTED_STORE] Unexpected store location
The dependencies at "/home/suveng/.dsh/profiles/web/node_modules" are currently linked
from the store at "/home/suveng/.pnpm-store/v11".
pnpm now wants to use the store at "/home/suveng/.local/share/pnpm/store/v11" to link dependencies.
(This error may happen if the node_modules was installed with a different major version of pnpm)
```

插件页只能看到一条 `operation-error`，用户以为是插件或市场的问题；实际是壳重建的
`node_modules` 与本机 pnpm 的 store 不是同一个。

## 2. 证据（2026-11-05 取证）

时间线（profile `~/.dsh/profiles/web`）：

| 时间 | 事件 |
|---|---|
| 20:44:46 | `apt install ./dsh-desktop_0.1.14_amd64.deb`（`~/.zsh_history`） |
| 20:46:03 | `~/.dsh/profiles` 被写 |
| 20:46:16 | `pnpm-lock.yaml` 被写 |
| 20:46:20 | `node_modules/*` 包目录（含 `dsh-plugin-feishu`）被写 |
| 20:46:28 | `node_modules/.modules.yaml` 被写；壳拉起宿主 |
| 20:46 | `.plugin-manager/logs/operation-Tr1fwn/pnpm.log`：`Command failed with exit code 1: pnpm add suveng-dsh-plugin-memory billion-context suveng-dsh-plugin-mcp-manager dsh-plugin-feishu` |
| 次日 00:15 | `.plugin-manager/logs/operation-odJmSp/pnpm.log`：上面那段 `ERR_PNPM_UNEXPECTED_STORE` |

关键事实：

- `node_modules/.modules.yaml`：`storeDir: /home/suveng/.pnpm-store/v11`、
  `packageManager: pnpm@11.21.0`、`prunedAt: Wed, 04 Nov 2026 12:46:28 GMT`。
- 硬链接溯源（不是记账错误）：
  `stat -c '%h %i' node_modules/dsh-plugin-feishu/package.json` → links=2 inode=13636178，
  `find ~/.pnpm-store/v11/files -maxdepth 2 -inum 13636178` 命中
  `~/.pnpm-store/v11/files/0f/af8aac…`；在新 store 下无命中。
- 本机唯一的 pnpm：`~/.nvm/versions/node/v24.13.0/bin/pnpm` v11.21.0，`pnpm store path`
  在 profile 目录内返回 `/home/suveng/.local/share/pnpm/store/v11`（3.9G）；
  `~/.pnpm-store/v11` 是 407M 的旧 pnpm 时代路径。
- 现在系统里找不到任何 store 配置：`pnpm config get store-dir` = undefined，
  `~/.config/pnpm/` 空，profile 无 `.npmrc`，`~/.npmrc` 只有 registry；
  `npm_config_store_dir` 在 pnpm 11 上实测被忽略（导出后 `pnpm store path` 不变）。
  **因此"当天那个 pnpm 进程为什么解析到旧 store"已无法从今天的环境复现** ——
  这也说明不能靠"猜环境"规避，必须在重建前显式对齐。
- 同机 `/home` 是独立分区（`/dev/nvme0n1p4`），pnpm 的 store 解析与 cwd 所在文件系统有关：
  在 `/tmp`（`/` 分区）里跑 `pnpm store path` 会给出 `/tmp/.pnpm-store/v11`。

## 3. 现有代码位置

- `crates/bin/src/onboard.rs:646-654` `Step::RepairDeps`：
  `Job::spawn(Path::new("pnpm"), &["install","--prefer-offline"], &env, Some(&dir))`，
  `dir = paths.profiles_root().join(&profile)`；由 `onboard.rs:950` 排队触发（还原/导入后）。
  `crates/setup/src/profiles.rs:335-342` 另有一份同样的 `repair_deps()`。
- `crates/setup/src/install.rs:85-101` `Job::spawn`：只 `cmd.env(k,v)` 叠加，**其余继承壳进程环境**，
  cwd 设成 profile 目录。
- `crates/setup/src/settings.rs:163-169` `install_env()`：只拼 proxy + `npm_config_registry`，
  没有任何 pnpm store 相关处理。
- `crates/setup/src/profiles.rs:330-332` `needs_deps()`：只判断 `node_modules` 是否存在，
  存在就认为"依赖就绪"，不看 node_modules 是哪套 store 写的。

## 4. 为什么归壳，不归 dsh / 插件 / 市场

- `~/.dsh` 与 `~/.dsh/profiles/web` 由壳的引导流程创建、备份、还原、导入，壳也负责"还原后补依赖"。
- 20:46 那批 `node_modules` 写入时刻宿主还没起（宿主 20:46:28 才被拉起），只能是壳的 `RepairDeps` 干的；
  dsh 的 `dsh-plugin-manager` 只是后续使用者，它拿到的已是一个自相矛盾的 profile。
- 同理 `pnpm-workspace.yaml` 里的 `allowBuilds` 占位符也是壳那次 `pnpm install` 写下的。

## 5. 需求

**R1（必须）store 对齐。** `RepairDeps` 前先比对：读 `node_modules/.modules.yaml` 的 `storeDir`
与"在 profile 目录内执行当前 pnpm 得到的 store 路径"。两者不一致时按"重装"处理，而不是直接
`pnpm install`：`CI=true pnpm install --prefer-offline`（pnpm 只在 `CI=true` 时 purge 现有
`node_modules`，实测 16s / 183 包），或显式删掉 `node_modules` 后重装。

**R2（必须）不许写死/污染 store。** 不要 `pnpm config set store-dir`，也不要在 shell 里导出
`npm_config_store_dir`（pnpm 11 不认，且会影响用户其它项目）。原则是"整体跟随 profile 已有的那套
store，或整体换成用户当前 pnpm 的 store"，不允许半新半旧。

**R3（必须）`allowBuilds` 占位符定值。** `pnpm-workspace.yaml` 里的未决占位符
（形如 `<pkg>: set this to true or false`）会让后续**任何** install 直接
`ERR_PNPM_IGNORED_BUILDS` 退出（本机是 `protobufjs`）。`RepairDeps` 完成后必须校验
`pnpm-workspace.yaml` 里不再有 `set this to true or false`，有则定值（或按用户设置里的策略决定）。

**R4（必须）失败可见。** `RepairDeps` 非零退出时：引导页显示 pnpm 错误的尾部若干行，
完整输出落到固定路径（如 `~/.local/share/dsh-desktop-rs/logs/profile-deps.log`），
并把 profile 标为"依赖未就绪"，不允许进入可启动状态（现在的表现是宿主照常起、
用户之后在插件页撞上无解释的 store 报错）。

**R5（建议）完成判定。** 不看单次 exit code：`RepairDeps` 之后跑一次
`pnpm list --depth 0`（或 `pnpm install --frozen-lockfile`）确认依赖自洽，
再放行 `needs_deps()`。`needs_deps()` 也应把"storeDir 与当前 store 不一致"视为需要修复。

## 6. 验收

- **A1** 全新机器（空 `HOME`/`XDG`，只有系统 node）装壳 → 引导完成 → 宿主起来后点插件页更新：
  不出现 `ERR_PNPM_UNEXPECTED_STORE`，能装上插件市场里的新版本。
- **A2** 人为把 profile 的 `node_modules` 弄成"旧 store 的链接"（或改掉 `.modules.yaml` 的 `storeDir`）后
  触发 `RepairDeps`：能自愈，之后 install 正常。
- **A3** `pnpm-workspace.yaml` 出现 `allowBuilds` 占位符时，壳要么定值，要么在引导页阻断并给出可操作提示。
- **A4** `pnpm` 不可用 / 装依赖失败时，引导页有可读错误 + 固定日志文件，且宿主不启动。

## 7. 现状（临时处置，不属于修复）

用户机器上已手工执行 `CI=true pnpm install --prefer-offline` 并用 `pnpm add dsh-plugin-feishu@0.7.0`
把 profile 拉回新 store；这只是绕开缺陷，根因仍在壳侧。以后不再手工动 `~/.dsh/profiles/*`
（那是壳的资产），只提需求。
