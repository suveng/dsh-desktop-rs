#!/usr/bin/env bash
# M2 冒烟测试：在虚拟 X 服务器里跑真实窗口，断言「窗口能起来、失败落到安装引导页、桌面 shim 生效」。
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

# 用临时 XDG_RUNTIME_DIR：否则「单实例」锁会被用户正在跑的桌面壳占着，
# 冒烟进程只会转发一条消息就退出，窗口永远等不到。
export XDG_RUNTIME_DIR=$OUT/runtime
rm -rf "$XDG_RUNTIME_DIR"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
# 也别读用户真实窗口状态：上次存的 1920x945 会让窗口比虚拟屏还大，截出来的图右侧被切掉。
export XDG_STATE_HOME=$OUT/state
rm -rf "$XDG_STATE_HOME"
mkdir -p "$XDG_STATE_HOME"

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

# ---- 场景 1：宿主不存在 → 必须落到安装引导页，且页面真的画出来了 ----------
scenario_onboarding() {
  echo "--- 场景 1：安装引导页 ---"
  # 显式给一个不存在的宿主：启动必然失败，失败就进引导页（这是产品行为）。
  "$BIN" --host-program /nonexistent/dsh >"$OUT/onboarding.log" 2>&1 &
  local pid=$!

  local win
  win=$(wait_window '安装引导') || {
    kill -TERM "$pid" 2>/dev/null || true
    echo "--- app 日志 ---"; sed -n '1,20p' "$OUT/onboarding.log" 2>/dev/null
    fail "没有出现安装引导窗口"
  }
  # CI 机器慢：页面可能还是白底 about:blank。轮询到「页面已变暗」再截图。
  local ok=0
  for _ in $(seq 1 20); do
    import -window "$win" "$OUT/onboarding.png" 2>/dev/null || true
    if python3 - "$OUT/onboarding.png" <<'PYEOF'
import sys
from PIL import Image, ImageStat
try:
    stat = ImageStat.Stat(Image.open(sys.argv[1]).convert("L"))
    mean, spread = stat.mean[0], stat.stddev[0]
except Exception:
    sys.exit(1)
# 全黑（未绘制）也可能「很暗」，必须同时要求有内容。
sys.exit(0 if mean < 90 and spread > 4 else 1)
PYEOF
    then ok=1; break; fi
    sleep 1
  done
  [ "$ok" = 1 ] || fail "引导页在 20 秒内仍是白/空页"
  # 第一帧只要变暗就通过，但 WebKit 是分块上屏的：底部按钮常比正文晚一帧。
  # 等两秒再截一次最终图，免得基线/人眼看到缺一半。
  sleep 2
  import -window "$win" "$OUT/onboarding.png" 2>/dev/null || true
  kill -TERM "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true

  # 引导页内容随机器环境变化（node 版本、已装 profile 都不同），不做像素基线，
  # 只断言「尺寸正确、页面够暗、有明显内容」——真正的内容断言在 Rust 单测里。
  python3 - "$OUT/onboarding.png" <<'PYEOF' || fail "引导页结构断言未通过"
import sys
from PIL import Image, ImageStat
im = Image.open(sys.argv[1]).convert("L")
stat = ImageStat.Stat(im)
width, height = im.size
background = stat.mean[0]
spread = stat.stddev[0]
print(f"尺寸 {width}x{height} 平均亮度 {background:.1f} 标准差 {spread:.1f}")
ok = width >= 900 and height >= 600 and background < 90 and spread > 4
sys.exit(0 if ok else 1)
PYEOF
  echo "    引导页 OK（截图 $OUT/onboarding.png）"
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

scenario_onboarding
scenario_selftest
echo "SMOKE OK"
