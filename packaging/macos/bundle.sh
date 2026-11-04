#!/usr/bin/env bash
# 在 macOS 上把 release 二进制包成 .app，再压成 zip。
#
# 签名策略：
#   - 配了 APPLE_CERTIFICATE / APPLE_SIGNING_IDENTITY → 真签名（可选公证）
#   - 没配 → ad-hoc 签名，用户首次打开需要「右键 → 打开」
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
  <key>CFBundleIconFile</key><string>$APP.icns</string>
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

if command -v iconutil >/dev/null && [ -f packaging/linux/icons/$APP-512.png ]; then
  ICONSET="$ROOT/target/package/$APP.iconset"
  rm -rf "$ICONSET" && mkdir -p "$ICONSET"
  # 必须成对给出 1x/2x，否则 iconutil 会整包失败（老脚本只给 1x，靠 `|| true` 把错误吞了，
  # 生成的 icns 只有半套尺寸）。1024 从 512 放大，Retina 下比缺图强。
  for pair in 16:16x16 32:16x16@2x 32:32x32 64:32x32@2x 128:128x128 256:128x128@2x 256:256x256 512:256x256@2x 512:512x512 1024:512x512@2x; do
    size=${pair%%:*}
    name=${pair##*:}
    sips -z "$size" "$size" "packaging/linux/icons/$APP-512.png" \
      --out "$ICONSET/icon_$name.png" >/dev/null
  done
  iconutil -c icns "$ICONSET" -o "$APP_DIR/Contents/Resources/$APP.icns" \
    || echo "!! icns 生成失败：应用会显示成默认图标"
fi

cp LICENSE "$APP_DIR/Contents/Resources/" 2>/dev/null || true

signing_identity=""
if [ -n "${APPLE_CERTIFICATE:-}" ] && [ -n "${APPLE_SIGNING_IDENTITY:-}" ]; then
  echo "==> 使用 Developer ID 真签名"
  KEYCHAIN="$RUNNER_TEMP/build.keychain-db"
  echo "$APPLE_CERTIFICATE" | base64 --decode > "$RUNNER_TEMP/cert.p12"
  security create-keychain -p actions "$KEYCHAIN"
  security set-keychain-settings -lut 21600 "$KEYCHAIN"
  security unlock-keychain -p actions "$KEYCHAIN"
  security import "$RUNNER_TEMP/cert.p12" -k "$KEYCHAIN" -P "${APPLE_CERTIFICATE_PASSWORD:-}" \
    -T /usr/bin/codesign
  security set-key-partition-list -S apple-tool:,apple: -k actions "$KEYCHAIN" >/dev/null
  security list-keychains -d user -s "$KEYCHAIN" $(security list-keychains -d user | tr -d '"')
  signing_identity="$APPLE_SIGNING_IDENTITY"
else
  echo "==> 未配置证书，使用 ad-hoc 签名（首次打开需右键 → 打开）"
  signing_identity="-"
fi

codesign --force --deep --options runtime --sign "$signing_identity" "$APP_DIR"

# 只有真签名 + 配了 Apple ID 才尝试公证；失败不阻断发布（但要留痕）
if [ "$signing_identity" != "-" ] && [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ] && [ -n "${APPLE_APP_PASSWORD:-}" ]; then
  DMG_STAGE="$ROOT/target/package/dmg"
  rm -rf "$DMG_STAGE" && mkdir -p "$DMG_STAGE"
  cp -R "$APP_DIR" "$DMG_STAGE/"
  DMG="$ROOT/target/package/$APP-$VERSION-$ARCH.dmg"
  hdiutil create -volname "DeepSeek Harness" -srcfolder "$DMG_STAGE" -ov -format UDZO "$DMG"
  echo "==> 提交公证"
  xcrun notarytool submit "$DMG" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" \
    --password "$APPLE_APP_PASSWORD" --wait || echo "!! 公证失败，产物仍需右键打开"
  xcrun stapler staple "$DMG" || true
  cp "$DMG" "$DIST/" || true
fi

( cd "$ROOT/target/package" && zip -qry "$DIST/$APP-$VERSION-macos-$ARCH.zip" "$APP.app" )
echo "built $DIST/$APP-$VERSION-macos-$ARCH.zip"
