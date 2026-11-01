#!/usr/bin/env bash
# M2 冒烟测试：在虚拟 X 服务器里跑真实窗口，断言「窗口能起来、失败页正确、桌面 shim 生效」。
#
# 用法：
#   xvfb-run -a scripts/smoke.sh
#   DSH_SMOKE_HOST_CMD="pnpm dsh" DSH_SMOKE_HOST_CWD=/path/to/deepseek-harness \
#     xvfb-run -a scripts/smoke.sh          # 额外验证真实前端的 shim 握手
#
# 需要：xvfb-run、xdotool、imagemagick(import)、python3-pil。
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=${BIN:-$ROOT/target/debug/dsh-desktop}
OUT=${OUT:-$ROOT/target/smoke}
mkdir -p "$OUT"

fail() {
  echo "SMOKE FAIL: $*" >&2
  exit 1
}

wait_window() {
  local name=$1 tries=${2:-100} win=''
  for _ in $(seq 1 "$tries"); do
    win=$(xdotool search --name "$name" 2>/dev/null | tail -1 || true)
    if [ -n "$win" ]; then
      printf '%s\n' "$win"
      return 0
    fi
    sleep 0.1
  done
  return 1
}

# ---- 场景 1：宿主不存在 → 必须出现失败页，且与基线一致 ----------------------
scenario_failure_page() {
  echo "--- 场景 1：失败页 ---"
  "$BIN" --host-program /nonexistent/dsh >"$OUT/failure.log" 2>&1 &
  local pid=$!

  local win
  win=$(wait_window 'startup failed') || {
    kill -TERM "$pid" 2>/dev/null || true
    echo "--- app 日志 ---"; sed -n '1,20p' "$OUT/failure.log" 2>/dev/null
    fail "没有出现失败页窗口"
  }
  sleep 1
  import -window "$win" "$OUT/failure-page.png" || fail "截图失败"
  kill -TERM "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true

  if [ "${SMOKE_RELAXED:-0}" = "1" ]; then
    # CI 的字体栈与本地不同，逐像素没有可比性；退化为「结构断言」：
    # 尺寸正确、页面足够暗、且有明显的非背景内容（不是空白页）。
    python3 - "$OUT/failure-page.png" <<'PYEOF' || fail "失败页结构断言未通过"
import sys
from PIL import Image, ImageStat
im = Image.open(sys.argv[1]).convert("L")
stat = ImageStat.Stat(im)
width, height = im.size
background = stat.mean[0]
spread = stat.stddev[0]
print(f"尺寸 {width}x{height} 平均亮度 {background:.1f} 标准差 {spread:.1f}")
ok = width >= 1200 and height >= 800 and background < 90 and spread > 4
sys.exit(0 if ok else 1)
PYEOF
  else
    python3 "$ROOT/scripts/compare_shots.py" \
      "$ROOT/tests/baseline/failure-page.png" "$OUT/failure-page.png" 0.05 \
      || fail "失败页与基线不一致"
  fi
  echo "    失败页 OK"
}

# ---- 场景 2（可选）：真实前端 + shim 握手 ------------------------------------
scenario_selftest() {
  [ -n "${DSH_SMOKE_HOST_CMD:-}" ] || {
    echo "--- 场景 2：跳过（未设置 DSH_SMOKE_HOST_CMD）---"
    return 0
  }
  echo "--- 场景 2：真实前端 shim ---"
  local args=(--selftest --host-cmd "$DSH_SMOKE_HOST_CMD")
  [ -n "${DSH_SMOKE_HOST_CWD:-}" ] && args+=(--host-cwd "$DSH_SMOKE_HOST_CWD")

  local out rc=0
  out=$(timeout 120 "$BIN" "${args[@]}" 2>"$OUT/selftest.log") || rc=$?
  [ "$rc" -eq 0 ] || fail "selftest 退出码 $rc（见 $OUT/selftest.log）"
  printf '%s\n' "$out" >>"$OUT/selftest.log"

  # 输出里的 JSON 是转义过的，去掉反斜杠再匹配，避免和 shell 的转义规则打架。
  local flat
  flat=$(printf '%s' "$out" | tr -d '\\')

  case "$flat" in
    *'"protocolVersion":1'*) ;;
    *) fail "shim 未生效：$out" ;;
  esac
  case "$flat" in
    *'"pluginError":false'*) ;;
    *) fail "前端插件加载失败：$out" ;;
  esac
  case "$flat" in
    *'"device":"dsh-desktop'*) ;;
    *) fail "deviceInfo 往返失败：$out" ;;
  esac
  # D7：版本区间与更新状态必须出现在自检输出里（实现存在，而不是只写在文档里）。
  case "$flat" in
    *'"tray"'*'"hotkeys"'*'"notify"'*) ;;
    *) fail "缺少桌面集成能力状态：$out" ;;
  esac
  case "$flat" in
    *'"versionNote"'*) ;;
    *) fail "缺少版本区间探测：$out" ;;
  esac
  case "$flat" in
    *'"updatePhase"'*) ;;
    *) fail "缺少更新状态：$out" ;;
  esac
  echo "    shim 握手 OK"

  if pgrep -f '[p]rofile web' >/dev/null; then
    fail "selftest 退出后仍有宿主残留"
  fi
}

scenario_failure_page
scenario_selftest
echo "SMOKE OK"
