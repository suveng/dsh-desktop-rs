#!/usr/bin/env bash
# 一条命令走完一次发布：升版本 → 提交 → 打 tag → **先推 tag、再推 main**。
#
# 为什么要用脚本：release-reminder 这个 workflow 在每次 push main 时检查
# 「Cargo.toml 的版本是否已有对应 tag」。手工发布时习惯先 push main、再去打 tag，
# 中间那一小段时间它就判定「版本未发布」→ Actions 里加一条告警 + 开一个提醒 issue。
# 顺序反过来（tag 先到位）就不会有空窗，也就不再刷告警。
#
# 用法：
#   bash scripts/release.sh 0.1.13          # 升版本并发布
#   bash scripts/release.sh 0.1.13 --dry-run # 只打印要做什么
set -euo pipefail
cd "$(dirname "$0")/.."

version="${1:-}"
dry_run=""
for arg in "$@"; do
  case "${arg}" in
    --dry-run) dry_run=1 ;;
  esac
done

if [ -z "${version}" ]; then
  echo "用法：bash scripts/release.sh <版本号> [--dry-run]" >&2
  echo "例：bash scripts/release.sh 0.1.13" >&2
  exit 64
fi

case "${version}" in
  v*) echo "版本号不要带前缀 v（会自己加）：${version}" >&2; exit 65 ;;
  [0-9]*.[0-9]*.[0-9]*) ;;
  *) echo "版本号看起来不对：${version}" >&2; exit 65 ;;
esac

current=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
if [ "${version}" = "${current}" ]; then
  echo "Cargo.toml 已经是 ${current}，无需改版本" >&2
  exit 66
fi

tag="v${version}"
if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null; then
  echo "${tag} 已经存在，换个版本号" >&2
  exit 67
fi
if [ -n "$(git status --porcelain)" ]; then
  echo "工作区不干净，先提交或 stash 再发布" >&2
  git status --short >&2
  exit 68
fi

echo "==> ${current} → ${version}"

if [ -n "${dry_run}" ]; then
  echo "dry-run：会改 Cargo.toml、提交、打 ${tag}、先推 ${tag} 再推 main"
  exit 0
fi

# Cargo.toml 里有两类版本串：`[workspace.package] version = "…"` 和
# `[workspace.dependencies]` 里工作区内部 crate 的 `version = "…"`，都要跟着升。
python3 - "${current}" "${version}" <<'PY'
import re
import sys
from pathlib import Path

old, new = sys.argv[1], sys.argv[2]
path = Path("Cargo.toml")
text = path.read_text()
updated, count = re.subn(rf'version = "{re.escape(old)}"', f'version = "{new}"', text)
if count == 0:
    raise SystemExit("Cargo.toml 里没找到要替换的版本串")
path.write_text(updated)
print(f"改了 {count} 处版本号")
PY

# 让 Cargo.lock 里的内部 crate 一并跟上。注意不能用 `cargo metadata --no-deps`：
# 它不解析依赖图，lock 里的 version 根本不会变，于是 release.yml 的
# `cargo build --locked` 直接报 "cannot update the lock file because --locked was passed"。
cargo update --workspace --offline >/dev/null
# 兜底校验：lock 过期时这里会失败，别把坏锁提交上去。
cargo metadata --locked --no-deps --format-version 1 >/dev/null

git add Cargo.toml Cargo.lock
git commit -q -m "chore(release): ${version}"
git tag -a "${tag}" -m "dsh-desktop ${version}"

echo "==> 先推 tag（release.yml 开始出三平台产物）"
git push origin "${tag}"
echo "==> 再推 main"
git push origin main

echo
echo "已发布 ${tag}"
echo "  Actions: https://github.com/suveng/dsh-desktop-rs/actions"
echo "  本机装新包：bash packaging/macos/bundle.sh && cp -R target/package/dsh-desktop.app /Applications/"
