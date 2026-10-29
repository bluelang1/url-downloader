#!/bin/sh
# macOS .app 번들 생성. INSTALL=0 이면 /Applications 설치를 건너뛴다 (CI 용)
set -e
cd "$(dirname "$0")"
NAME="URL Downloader"
APP="dist/$NAME.app"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
cargo build --release
rm -rf dist && mkdir -p "$APP/Contents/MacOS"
cp target/release/url_downloader "$APP/Contents/MacOS/url_downloader"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>$NAME</string>
  <key>CFBundleDisplayName</key><string>$NAME</string>
  <key>CFBundleIdentifier</key><string>kr.heesung.url-downloader</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleExecutable</key><string>url_downloader</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
</dict></plist>
PLIST
codesign --force --deep --sign - "$APP"
# 릴리스용 zip (업데이트 기능이 이 파일을 받는다)
ZIP="dist/URL-Downloader-v$VERSION-macos-$(uname -m).zip"
ditto -c -k --keepParent "$APP" "$ZIP"
echo "built: $APP, $ZIP"
if [ "${INSTALL:-1}" = "1" ]; then
  rm -rf "/Applications/$NAME.app"
  cp -R "$APP" /Applications/
  echo "installed: /Applications/$NAME.app"
fi
