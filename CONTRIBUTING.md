# 贡献指南

## 环境

Linux（Ubuntu 24.04）需要：

```bash
sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev \
  libjavascriptcoregtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev \
  xvfb xdotool openbox imagemagick python3-pil
```

## 提交前必须通过

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
xvfb-run -a -s "-screen 0 1600x1000x24" bash scripts/smoke.sh
```

## 结构约定

- 依赖方向只允许 `bin → {supervisor, platform}`；两个库 crate 互不依赖，谁都不依赖 `bin`。
- 平台差异一律用 `cfg` 门隔离（`embed.rs` 是范本），不要为某个平台写死路径。
- 涉及输入焦点的测试必须在 `xvfb + openbox` 下跑（见 `docs/architecture.md` ADR-D10）。
- 任何偏离 `docs/architecture.md` 决策的改动，必须在该文件追加 ADR，写明原因与被否决方案。
- 带 token 的 URL 不得进入日志、标题、崩溃报告或磁盘（ADR/R4）。

## 提交信息

Conventional Commits：`feat:` / `fix:` / `docs:` / `chore:` / `refactor:`。

## 版本与发布

版本号只有一处：根 `Cargo.toml` 的 `[workspace.package].version`。配套 tag 必须与它一致。

**规则：改版本就是发布的一部分——确认要发的那次改动里，tag 必须一起打上，不允许「先改版本、之后想起来再发」。**

确认发布时按顺序执行：

```bash
bash scripts/check-release-tag.sh          # 先确认是否已经漏了
git tag -a vX.Y.Z -m "dsh-desktop X.Y.Z"   # tag 与 Cargo.toml 版本必须相同
git push origin vX.Y.Z
```

- `release.yml` 只由 `v*` tag 触发：先 `verify`（fmt / clippy / test / build / smoke），再并行出 Linux（deb / AppImage / tar.gz）、macOS、Windows 产物，最后建 Release。手动 dispatch **只构建、不发布**。
- `.github/workflows/release-reminder.yml` 每次 push 到 main 都会跑 `scripts/check-release-tag.sh`：版本没有 tag 时在 Actions 里报警告、写 job summary，并开一个「同一版本只开一次」的 issue 提醒。它不会让流水线变红——发不发由人决定。
- tag 打错了（打在了改版本之前，或指向的提交版本不对）：删掉重打

  ```bash
  git tag -d vX.Y.Z
  git push origin :refs/tags/vX.Y.Z
  ```

- tag 是发布开关：删本地 tag 不会撤回已发布的 Release；撤回 Release 需要 `gh release delete`（按根目录 AGENTS.md，这属于对外可见操作，先确认）。
