#!/bin/sh
# macOS .app 번들 생성 후 /Applications 에 설치
set -e
cd "$(dirname "$0")"
NAME="URL Downloader"
APP="dist/$NAME.app"
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
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleExecutable</key><string>url_downloader</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
</dict></plist>
PLIST
codesign --force --deep --sign - "$APP"
rm -rf "/Applications/$NAME.app"
cp -R "$APP" /Applications/
echo "installed: /Applications/$NAME.app"
