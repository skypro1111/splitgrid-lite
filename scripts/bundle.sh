#!/bin/sh
# Build "SplitGrid Lite.app" (release) and, with --install, put it in /Applications.
# Ad-hoc signed: macOS notifications and a stable identity need a signature.
set -eu
cd "$(dirname "$0")/.."

NAME="SplitGrid Lite"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
APP="target/release/$NAME.app"

cargo build --release

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/splitgrid "$APP/Contents/MacOS/splitgrid"
# Lite's own icon: the same "S" on a lighter tile (scripts/make-icon.py).
[ -f icon/AppIcon.icns ] || python3 scripts/make-icon.py
cp icon/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
# The helpers agents call ($SPLITGRID_TERMINAL_CLI, $SPLITGRID_HOOK_CLI, …), as the Electron app ships them.
mkdir -p "$APP/Contents/Resources/helpers"
cp ../swapit/resources/splitgrid-*.sh "$APP/Contents/Resources/helpers/"
chmod +x "$APP/Contents/Resources/helpers/"*.sh
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$NAME</string>
  <key>CFBundleDisplayName</key><string>$NAME</string>
  <key>CFBundleIdentifier</key><string>dev.splitgrid.lite</string>
  <key>CFBundleExecutable</key><string>splitgrid</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
EOF
codesign --force --deep --sign - "$APP"
echo "built $APP"

if [ "${1:-}" = "--install" ]; then
  DEST="/Applications/$NAME.app"
  if pgrep -f "$DEST/Contents/MacOS/splitgrid" >/dev/null; then
    echo "$NAME is running; quit it first" >&2
    exit 1
  fi
  rm -rf "$DEST"
  ditto "$APP" "$DEST"
  echo "installed $DEST"
fi
