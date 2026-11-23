#!/bin/zsh
# Build the release binary and install "Juan Terminal.app" into /Applications
# (Finder, Spotlight, Raycast and the Dock can open it; no terminal needed).
set -euo pipefail
cd "$(dirname "$0")/.."
source "$HOME/.cargo/env" 2>/dev/null || true

APP_NAME="Juan Terminal"
DEST="/Applications/$APP_NAME.app"
WORK="target/bundle/$APP_NAME.app"
VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')

cargo build --release -p juan-terminal

rm -rf "$WORK"
mkdir -p "$WORK/Contents/MacOS" "$WORK/Contents/Resources"
cp target/release/juan-terminal "$WORK/Contents/MacOS/juan-terminal"

# Icon: 1024 PNG (rounded photo) -> iconset -> icns.
ICONSET=target/bundle/AppIcon.iconset
rm -rf "$ICONSET" && mkdir -p "$ICONSET"
PNG=packaging/icon.png
for s in 16 32 128 256 512; do
  sips -z $s $s "$PNG" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s*2)) $((s*2)) "$PNG" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$WORK/Contents/Resources/AppIcon.icns"

cat > "$WORK/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleIdentifier</key><string>dev.juan-terminal.app</string>
  <key>CFBundleExecutable</key><string>juan-terminal</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$(date +%Y%m%d%H%M)</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST

# Sign with the stable local identity if it exists (see create-signing-cert.sh): macOS keeps
# folder permissions across rebuilds. Otherwise ad-hoc (permissions may be asked again).
if security find-identity -p codesigning 2>/dev/null | grep -q "Juan Terminal Dev"; then
  codesign --force --deep --sign "Juan Terminal Dev" "$WORK" >/dev/null
else
  codesign --force --deep --sign - "$WORK" >/dev/null
fi

# Replace the installed copy (quit it first).
pkill -x juan-terminal 2>/dev/null || true
sleep 0.5
rm -rf "$DEST"
cp -R "$WORK" "$DEST"
# Let Launch Services / Spotlight / Raycast pick it up right away.
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$DEST" >/dev/null 2>&1 || true
echo "Installed $DEST"
