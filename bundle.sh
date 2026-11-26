#!/bin/sh
# macOS .app 번들 생성. INSTALL=0 이면 /Applications 설치를 건너뛴다 (CI 용)
set -e
cd "$(dirname "$0")"
NAME="URL Downloader"
APP="dist/$NAME.app"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
# UNIVERSAL=1 이면 arm64 + x86_64 를 lipo 로 합친다 (CI). 아니면 호스트 아키텍처만.
if [ "${UNIVERSAL:-0}" = "1" ]; then
  # 두 타깃이 설치돼 있지 않으면 cargo 오류 대신 설치 방법을 알려 주고 멈춘다
  INSTALLED=$(rustup target list --installed 2>/dev/null || true)
  MISSING=""
  for T in aarch64-apple-darwin x86_64-apple-darwin; do
    echo "$INSTALLED" | grep -qx "$T" || MISSING="$MISSING $T"
  done
  if [ -n "$MISSING" ]; then
    echo "UNIVERSAL=1 빌드에 필요한 Rust 타깃이 없습니다:$MISSING" >&2
    echo "설치: rustup target add$MISSING" >&2
    exit 1
  fi
  cargo build --release --locked --target aarch64-apple-darwin --target x86_64-apple-darwin
  mkdir -p target/universal
  lipo -create -output target/universal/url_downloader \
    target/aarch64-apple-darwin/release/url_downloader \
    target/x86_64-apple-darwin/release/url_downloader
  BIN=target/universal/url_downloader
  ARCH=universal
else
  cargo build --release --locked
  BIN=target/release/url_downloader
  ARCH=$(uname -m)
fi
rm -rf dist && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/url_downloader"
cp assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
# 라이선스 고지 (서드파티 고지는 cargo about 로 생성해 커밋한 파일)
cp LICENSE THIRD-PARTY-LICENSES.html "$APP/Contents/Resources/"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>$NAME</string>
  <key>CFBundleDisplayName</key><string>$NAME</string>
  <key>CFBundleIdentifier</key><string>io.github.bluelang1.url-downloader</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleExecutable</key><string>url_downloader</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
</dict></plist>
PLIST
codesign --force --deep --sign - "$APP"
# 릴리스용 zip (업데이트 기능이 이 파일을 받는다)
ZIP="dist/URL-Downloader-v$VERSION-macos-$ARCH.zip"
ditto -c -k --keepParent "$APP" "$ZIP"
echo "built: $APP, $ZIP"
if [ "${INSTALL:-1}" = "1" ]; then
  rm -rf "/Applications/$NAME.app"
  cp -R "$APP" /Applications/
  echo "installed: /Applications/$NAME.app"
fi
