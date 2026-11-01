#!/usr/bin/env bash
# 校验 Cargo.toml 的版本是否已经有对应的 vX.Y.Z tag，且 tag 指向的提交里版本一致。
#
# 用法：bash scripts/check-release-tag.sh
# 退出码：0 已发布；1 版本没有 tag；2 tag 存在但指向的提交版本不对；3 无法读取版本。
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
if [ -z "${version}" ]; then
  echo "无法从 Cargo.toml 读取版本" >&2
  exit 3
fi

tag="v${version}"

if ! git rev-parse -q --verify "refs/tags/${tag}" >/dev/null; then
  echo "未发布：Cargo.toml 是 ${version}，但仓库里没有 ${tag}" >&2
  echo "确认要发布时执行：" >&2
  echo "  git tag -a ${tag} -m \"dsh-desktop ${version}\" && git push origin ${tag}" >&2
  exit 1
fi

tagged=$(git show "${tag}:Cargo.toml" 2>/dev/null | sed -n 's/^version = "\(.*\)"$/\1/p' | head -1)
if [ "${tagged}" != "${version}" ]; then
  echo "${tag} 存在，但它指向的提交里 Cargo.toml 是 ${tagged:-未知}，与当前 ${version} 不一致" >&2
  echo "tag 打在了版本号之前，请删掉重打（git tag -d ${tag} && git push origin :refs/tags/${tag}）" >&2
  exit 2
fi

echo "已发布：${tag} -> $(git rev-parse --short "${tag}")"
