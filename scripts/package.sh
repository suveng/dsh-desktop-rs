#!/usr/bin/env bash
# 打包 Linux 产物：tar.gz / deb / AppImage。
#
#   scripts/package.sh              # 全部
#   SKIP_APPIMAGE=1 scripts/package.sh
#
# 产物落在 dist/。
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

APP=dsh-desktop
ARCH=$(uname -m)                    # x86_64
VERSION=${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}
DIST=$ROOT/dist
PKG=$ROOT/target/package
STAGE=$PKG/$APP-$VERSION

echo "==> 构建 $APP $VERSION ($ARCH)"
cargo build --release --locked -p dsh-desktop

echo "==> 生成图标"
python3 scripts/gen_icons.py >/dev/null

rm -rf "$STAGE" "$DIST"
mkdir -p "$STAGE" "$DIST"

install -Dm755 target/release/$APP "$STAGE/usr/bin/$APP"
install -Dm644 packaging/linux/$APP.desktop "$STAGE/usr/share/applications/$APP.desktop"
install -Dm644 packaging/linux/$APP-mime.xml "$STAGE/usr/share/mime/packages/$APP-mime.xml"
for size in 128 256 512; do
  install -Dm644 packaging/linux/icons/$APP-$size.png \
    "$STAGE/usr/share/icons/hicolor/${size}x${size}/apps/$APP.png"
done

# ---------------------------------------------------------------- tar.gz
echo "==> tar.gz"
TARBALL=$PKG/tarball
rm -rf "$TARBALL"
mkdir -p "$TARBALL/icons"
install -m755 target/release/$APP "$TARBALL/$APP"
install -m644 packaging/linux/$APP.desktop "$TARBALL/$APP.desktop"
install -m644 packaging/linux/$APP-mime.xml "$TARBALL/$APP-mime.xml"
install -m755 packaging/linux/install.sh "$TARBALL/install.sh"
cp packaging/linux/icons/$APP-*.png "$TARBALL/icons/"
tar -C "$TARBALL" -czf "$DIST/$APP-$VERSION-$ARCH.tar.gz" .
( cd "$DIST" && sha256sum "$APP-$VERSION-$ARCH.tar.gz" > "$APP-$VERSION-$ARCH.tar.gz.sha256" )

# ---------------------------------------------------------------- deb
echo "==> deb"
DEB=$PKG/deb
rm -rf "$DEB"
mkdir -p "$DEB/DEBIAN"
cp -r "$STAGE/." "$DEB/"
INSTALLED_SIZE=$(du -sk "$STAGE" | cut -f1)
cat > "$DEB/DEBIAN/control" <<CONTROL
Package: $APP
Version: $VERSION
Section: devel
Priority: optional
Architecture: amd64
Installed-Size: $INSTALLED_SIZE
Maintainer: dsh-desktop contributors <noreply@example.com>
Depends: libwebkit2gtk-4.1-0, libgtk-3-0
Recommends: xdg-utils
Description: DeepSeek Harness desktop shell for Linux
 A thin Rust shell around the DeepSeek Harness web UI: process supervision,
 native window and menu, single instance, deep links, and crash recovery.
CONTROL

cat > "$DEB/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
  update-desktop-database -q /usr/share/applications 2>/dev/null || true
  update-mime-database /usr/share/mime 2>/dev/null || true
  gtk-update-icon-cache -f -t /usr/share/icons/hicolor 2>/dev/null || true
  xdg-mime default dsh-desktop.desktop x-scheme-handler/dsh 2>/dev/null || true
fi
exit 0
POSTINST
chmod 755 "$DEB/DEBIAN/postinst"

dpkg-deb --build --root-owner-group "$DEB" "$DIST/${APP}_${VERSION}_amd64.deb" >/dev/null
( cd "$DIST" && sha256sum "${APP}_${VERSION}_amd64.deb" > "${APP}_${VERSION}_amd64.deb.sha256" )

# ---------------------------------------------------------------- AppImage
if [ "${SKIP_APPIMAGE:-0}" = "1" ]; then
  echo "==> 跳过 AppImage（SKIP_APPIMAGE=1）"
else
  echo "==> AppImage"
  TOOL=$PKG/appimagetool
  if [ ! -x "$TOOL" ]; then
    URL=https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
    echo "    下载 appimagetool…"
    if ! curl -fsSL --retry 3 -o "$TOOL" "$URL"; then
      echo "!! 无法下载 appimagetool（需要网络/代理）。deb 与 tar.gz 已生成。" >&2
      exit 0
    fi
    chmod +x "$TOOL"
  fi

  APPDIR=$PKG/AppDir
  rm -rf "$APPDIR"
  mkdir -p "$APPDIR/usr/bin"
  install -m755 target/release/$APP "$APPDIR/usr/bin/$APP"
  install -m644 packaging/linux/$APP.desktop "$APPDIR/$APP.desktop"
  install -m644 packaging/linux/icons/$APP-256.png "$APPDIR/$APP.png"
  install -m644 packaging/linux/icons/$APP-256.png "$APPDIR/.DirIcon"
  ln -sf usr/bin/$APP "$APPDIR/AppRun"

  # 在容器/CI 里没有 FUSE 时，appimagetool 自己需要 --appimage-extract-and-run。
  ARCH=$ARCH "$TOOL" --no-appstream "$APPDIR" "$DIST/$APP-$VERSION-$ARCH.AppImage" \
    || "$TOOL" --appimage-extract-and-run --no-appstream "$APPDIR" "$DIST/$APP-$VERSION-$ARCH.AppImage"
  chmod +x "$DIST/$APP-$VERSION-$ARCH.AppImage"
  ( cd "$DIST" && sha256sum "$APP-$VERSION-$ARCH.AppImage" > "$APP-$VERSION-$ARCH.AppImage.sha256" )
fi

echo
echo "==> 产物"
ls -lh "$DIST"
