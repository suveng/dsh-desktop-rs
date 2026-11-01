#!/usr/bin/env bash
# 用户级安装（不需要 root）：装到 ~/.local 或 $PREFIX。
#
#   ./install.sh                 # 安装到 ~/.local
#   PREFIX=/opt/dsh ./install.sh # 自定义前缀
#   ./install.sh --uninstall
set -euo pipefail

APP=dsh-desktop
PREFIX=${PREFIX:-$HOME/.local}
BIN_DIR=$PREFIX/bin
APP_DIR=$PREFIX/share/applications
ICON_DIR=$PREFIX/share/icons/hicolor
MIME_DIR=$PREFIX/share/mime/packages

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

uninstall() {
  rm -f "$BIN_DIR/$APP"
  rm -f "$APP_DIR/$APP.desktop"
  rm -f "$MIME_DIR/$APP-mime.xml"
  rm -f "$PREFIX/share/mime/x-scheme-handler/dsh.xml"
  rm -f "$ICON_DIR"/128x128/apps/"$APP".png \
        "$ICON_DIR"/256x256/apps/"$APP".png \
        "$ICON_DIR"/512x512/apps/"$APP".png
  refresh
  echo "已卸载 $APP"
}

refresh() {
  command -v update-desktop-database >/dev/null && update-desktop-database "$APP_DIR" 2>/dev/null || true
  command -v update-mime-database >/dev/null && update-mime-database "$PREFIX/share/mime" 2>/dev/null || true
  command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -f -t "$PREFIX/share/icons/hicolor" 2>/dev/null || true
  command -v xdg-mime >/dev/null && xdg-mime default "$APP.desktop" x-scheme-handler/dsh 2>/dev/null || true
}

if [ "${1:-}" = "--uninstall" ]; then
  uninstall
  exit 0
fi

[ -x "$here/$APP" ] || { echo "找不到 $here/$APP（这是发布包目录里的脚本）" >&2; exit 1; }

install -Dm755 "$here/$APP" "$BIN_DIR/$APP"
install -Dm644 "$here/$APP.desktop" "$APP_DIR/$APP.desktop"
install -Dm644 "$here/$APP-mime.xml" "$MIME_DIR/$APP-mime.xml"
for size in 128 256 512; do
  install -Dm644 "$here/icons/$APP-$size.png" "$ICON_DIR/${size}x${size}/apps/$APP.png"
done
refresh

echo "已安装到 $PREFIX"
echo "  $BIN_DIR/$APP"
echo "从应用菜单启动，或在终端运行 $APP"
