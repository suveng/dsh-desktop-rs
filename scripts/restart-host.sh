#!/usr/bin/env bash
# 重启 dsh-desktop 托管的「本地服务」（dsh 宿主）而不关窗口。
#
# 原理：宿主是桌面壳的子进程（supervisor）。结束它之后，桌面壳的 supervisor 检测到
# HostEvent::Exited，会立刻拉起一个新的宿主（第一次退出零延迟），并在 HostEvent::Ready
# 时让窗口重新导航到新端口。所以窗口、单实例、窗口位置都不用动，只有宿主重启。
#
# 用法：
#   scripts/restart-host.sh               立即重启
#   scripts/restart-host.sh --delay 45    45 秒后重启（用来让正在进行的回复先送达）
#   scripts/restart-host.sh --dry-run     只打印将要做什么，不真的结束进程
#
# 退出码：0 重启成功；2 没找到运行中的桌面壳；3 宿主没有再起来。
# 每一步都写进 ~/.local/state/dsh-desktop-rs/restart.log（独立旁证，不依赖 stdout 还活着）。
set -euo pipefail

delay=0
dry_run=0
while [ $# -gt 0 ]; do
  case "$1" in
    --delay) delay="${2:?--delay 需要一个秒数}"; shift 2 ;;
    --dry-run) dry_run=1; shift ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) echo "未知参数：$1" >&2; exit 1 ;;
  esac
done

case "$delay" in
  ''|*[!0-9]*) echo "--delay 必须是整数秒" >&2; exit 1 ;;
esac

STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/dsh-desktop-rs"
TRACE="$STATE_DIR/restart.log"
mkdir -p "$STATE_DIR"

trace() {
  local line
  line="$(date '+%F %T') $*"
  printf '%s\n' "$line" >> "$TRACE"
  printf '%s\n' "$line"
}

# 桌面壳：排除僵尸（defunct）与本次脚本自身
find_desktop() {
  ps -eo pid=,stat=,args= | awk '
    $2 !~ /^Z/ && $0 ~ /dsh-desktop/ && $0 !~ /restart-host\.sh/ && $0 !~ /awk / { print $1; exit }
  '
}

# 宿主：桌面壳的直接子进程，且命令行里带 bin.js
find_host() {
  ps -eo pid=,ppid=,args= | awk -v parent="$1" '$2 == parent && $0 ~ /bin\.js/ { print $1; exit }'
}

desktop_pid="$(find_desktop)"
if [ -z "${desktop_pid:-}" ]; then
  trace "没有找到运行中的 dsh-desktop，放弃（先启动桌面版）"
  exit 2
fi
host_pid="$(find_host "$desktop_pid")"
if [ -z "${host_pid:-}" ]; then
  trace "桌面壳 pid=${desktop_pid} 下没有宿主进程，放弃（可能正在失败页）"
  exit 2
fi

trace "准备重启宿主：桌面壳 pid=${desktop_pid}，宿主 pid=${host_pid}，延迟 ${delay}s"

if [ "$delay" -gt 0 ]; then
  sleep "$delay"
fi

# 延迟期间用户可能已经手动重启/退出，重新确认一次
desktop_pid="$(find_desktop)"
host_pid="$(find_host "${desktop_pid:-0}")"
if [ -z "${desktop_pid:-}" ] || [ -z "${host_pid:-}" ]; then
  trace "延迟期间桌面壳或宿主已经变化，放弃本次重启"
  exit 2
fi

if [ "$dry_run" -eq 1 ]; then
  trace "dry-run：本应结束宿主 pid=${host_pid}，再等 supervisor 拉起新宿主"
  exit 0
fi

trace "向宿主 pid=${host_pid} 发送 SIGTERM"
kill -TERM "$host_pid" 2>/dev/null || true
for _ in $(seq 1 50); do
  kill -0 "$host_pid" 2>/dev/null || break
  sleep 0.2
done
if kill -0 "$host_pid" 2>/dev/null; then
  trace "宿主 10 秒内没有退出，升级为 SIGKILL"
  kill -KILL "$host_pid" 2>/dev/null || true
  sleep 0.5
fi

trace "等待桌面壳拉起新宿主"
new_pid=''
for _ in $(seq 1 200); do
  new_pid="$(find_host "$desktop_pid")"
  if [ -n "${new_pid:-}" ] && [ "$new_pid" != "$host_pid" ]; then
    break
  fi
  new_pid=''
  sleep 0.25
done
if [ -z "${new_pid:-}" ]; then
  trace "50 秒内没有看到新宿主，检查桌面窗口是否进了失败页（日志：${STATE_DIR}）"
  exit 3
fi
trace "新宿主已启动：pid=${new_pid}"

# 新宿主绑定端口（--port 0 是随机端口），等它就绪
port=''
for _ in $(seq 1 160); do
  port="$(ss -lntp 2>/dev/null | awk -v p="pid=${new_pid}," 'index($0, p) { print $4; exit }' | sed 's/.*://')"
  [ -n "${port:-}" ] && break
  sleep 0.25
done
if [ -z "${port:-}" ]; then
  trace "新宿主已启动但 40 秒内没有监听端口，请查看窗口里的提示"
  exit 3
fi

trace "重启完成：http://127.0.0.1:${port}（窗口会自动导航到新地址）"
