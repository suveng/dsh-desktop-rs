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
