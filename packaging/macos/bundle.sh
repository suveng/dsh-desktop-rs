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
APP_DIR="$ROOT/target/package/$APP.app"
DIST="$ROOT/dist"

# 二进制来源：默认 target/release，CI 可以传一个已经 lipo 好的通用版过来。
BIN=${BIN:-$ROOT/target/release/$APP}
[ -x "$BIN" ] || { echo "找不到可执行文件 $BIN，先 cargo build --release -p dsh-desktop" >&2; exit 1; }
# 产物命名用二进制里真实包含的架构，而不是跑脚本这台机器的架构：
# universal 就叫 universal（Intel + Apple silicon 同一个包）。
if [ -z "${ARCH:-}" ]; then
  archs=$(lipo -archs "$BIN" 2>/dev/null || true)
  case "$archs" in
    "x86_64 arm64"|"arm64 x86_64") ARCH=universal ;;
    "") ARCH=$(uname -m) ;;
    *) ARCH=$(printf '%s' "$archs" | tr ' ' '-') ;;
  esac
fi

rm -rf "$APP_DIR" "$DIST"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources" "$DIST"

install -m755 "$BIN" "$APP_DIR/Contents/MacOS/$APP"

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

# DMG 无条件产：没有证书时它照样能装（右键 → 打开），只是没法提交公证。
# 公证是 dmg 的附加步骤，不是产出 dmg 的前提。
DMG_STAGE="$ROOT/target/package/dmg"
rm -rf "$DMG_STAGE" && mkdir -p "$DMG_STAGE"
cp -R "$APP_DIR" "$DMG_STAGE/"
# 拖拽安装习惯：dmg 里放一个 /Applications 别名。
ln -sfn /Applications "$DMG_STAGE/Applications"
DMG="$ROOT/target/package/$APP-$VERSION-macos-$ARCH.dmg"

# hdiutil 会先造一个临时读写镜像、挂到 /Volumes 下再压缩。GitHub 的 macOS
# runner 上这一步很爱报 `hdiutil: create failed - Resource busy`（同名卷还挂着
# 或 Spotlight 正在索引 srcfolder），整个发布就断在这儿（v0.1.21 就是这么挂的：
# linux/windows 都绿，macos 的 Bundle 步失败，publish 被 skip）。所以：
# 每次失败先强行卸载同名卷再重试，第三次改用带版本号的卷名，彻底避开撞名。
make_dmg() {
  local attempt vol err
  for attempt in 1 2 3; do
    case $attempt in
      1|2) vol="DeepSeek Harness" ;;
      *) vol="DeepSeek Harness $VERSION" ;;
    esac
    err="$ROOT/target/package/dmg.err"
    if hdiutil create -volname "$vol" -srcfolder "$DMG_STAGE" -ov -format UDZO "$DMG" >/dev/null 2>"$err"; then
      return 0
    fi
    echo "!! dmg 第 $attempt 次失败（卷名 $vol）：$(tr '\n' ' ' < "$err")" >&2
    hdiutil detach "/Volumes/DeepSeek Harness" -force >/dev/null 2>&1 || true
    rm -f "$DMG"
    sleep $((attempt * 5))
  done
  return 1
}

make_dmg
cp "$DMG" "$DIST/"
echo "built $DIST/$APP-$VERSION-macos-$ARCH.dmg"

# 只有真签名 + 配了 Apple ID 才尝试公证；失败不阻断发布（但要留痕）
if [ "$signing_identity" != "-" ] && [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ] && [ -n "${APPLE_APP_PASSWORD:-}" ]; then
  echo "==> 提交公证"
  xcrun notarytool submit "$DIST/$APP-$VERSION-macos-$ARCH.dmg" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" \
    --password "$APPLE_APP_PASSWORD" --wait || echo "!! 公证失败，产物仍需右键打开"
  xcrun stapler staple "$DIST/$APP-$VERSION-macos-$ARCH.dmg" || true
fi

( cd "$ROOT/target/package" && zip -qry "$DIST/$APP-$VERSION-macos-$ARCH.zip" "$APP.app" )
echo "built $DIST/$APP-$VERSION-macos-$ARCH.zip"
