#!/usr/bin/env bash
# 在 macOS 上把 release 二进制包成 .app，再压成 zip（dmg 需要额外授权，先不生成）。
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$ROOT"

APP=dsh-desktop
VERSION=${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}
ARCH=$(uname -m)
APP_DIR="$ROOT/target/package/$APP.app"
DIST="$ROOT/dist"

rm -rf "$APP_DIR" "$DIST"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources" "$DIST"

install -m755 "target/release/$APP" "$APP_DIR/Contents/MacOS/$APP"

cat > "$APP_DIR/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>DeepSeek Harness</string>
  <key>CFBundleDisplayName</key><string>DeepSeek Harness</string>
  <key>CFBundleIdentifier</key><string>dev.suveng.dsh-desktop</string>
  <key>CFBundleExecutable</key><string>$APP</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>CFBundleURLTypes</key>
  <array><dict>
    <key>CFBundleURLName</key><string>dsh</string>
    <key>CFBundleURLSchemes</key><array><string>dsh</string></array>
  </dict></array>
</dict>
</plist>
PLIST

# 图标：把 512 的 PNG 转成 .icns（iconutil 需要 iconset 目录）
if command -v iconutil >/dev/null && [ -f packaging/linux/icons/$APP-512.png ]; then
  ICONSET="$ROOT/target/package/$APP.iconset"
  mkdir -p "$ICONSET"
  for size in 16 32 64 128 256 512; do
    sips -z $size $size "packaging/linux/icons/$APP-512.png" \
      --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  done
  iconutil -c icns "$ICONSET" -o "$APP_DIR/Contents/Resources/$APP.icns" || true
fi

# ad-hoc 签名：不装证书的前提下，让 Gatekeeper 至少不因「未签名」直接拒绝
codesign --force --deep --sign - "$APP_DIR" 2>/dev/null || true

cp LICENSE "$APP_DIR/Contents/Resources/" 2>/dev/null || true
( cd "$ROOT/target/package" && zip -qry "$DIST/$APP-$VERSION-macos-$ARCH.zip" "$APP.app" )
echo "built $DIST/$APP-$VERSION-macos-$ARCH.zip"
