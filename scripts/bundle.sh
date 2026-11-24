#!/bin/sh
# Build "SplitGrid Lite.app" (release) and, with --install, put it in /Applications.
# Signed with a stable identity when the keychain has one (see the signing step).
set -eu
cd "$(dirname "$0")/.."

NAME="SplitGrid Lite"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
APP="target/release/$NAME.app"

cargo build --release
# The persistent-terminal daemon and its menu-bar helper (src/ptyd.rs installs
# them from Contents/MacOS into the data dir): a plain build only builds the root package.
cargo build --release -p sg-ptyd -p sg-sessions

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/splitgrid "$APP/Contents/MacOS/splitgrid"
# Lite's own icon: the same "S" on a lighter tile (scripts/make-icon.py).
[ -f icon/AppIcon.icns ] || python3 scripts/make-icon.py
cp icon/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
# The persistent-terminal daemon and the menu-bar helper live in a helper bundle
# of their own, so macOS names them "SplitGrid Sessions" (background items,
# Activity Monitor) and the menu-bar icon is the app icon. src/ptyd.rs copies
# the whole bundle into the data dir and runs it from there.
HELPER="$APP/Contents/Helpers/SplitGrid Sessions.app"
mkdir -p "$HELPER/Contents/MacOS" "$HELPER/Contents/Resources"
cp target/release/splitgrid-ptyd target/release/splitgrid-sessions "$HELPER/Contents/MacOS/"
cp icon/AppIcon.icns "$HELPER/Contents/Resources/AppIcon.icns"
cat > "$HELPER/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>SplitGrid Sessions</string>
  <key>CFBundleDisplayName</key><string>SplitGrid Sessions</string>
  <key>CFBundleIdentifier</key><string>dev.splitgrid.lite.sessions</string>
  <key>CFBundleExecutable</key><string>splitgrid-sessions</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
# The helpers agents call ($SPLITGRID_TERMINAL_CLI, $SPLITGRID_HOOK_CLI, …): copies of
# the Electron app's (resources/, kept in step with it by hand).
mkdir -p "$APP/Contents/Resources/helpers"
cp resources/helpers/splitgrid-*.sh "$APP/Contents/Resources/helpers/"
chmod +x "$APP/Contents/Resources/helpers/"*.sh
# The sg-* skills Agent integrations install into ~/.claude/skills (src/agent/install.rs).
mkdir -p "$APP/Contents/Resources/skills"
cp -R resources/skills/. "$APP/Contents/Resources/skills/"
# The notification sounds (Settings → Notifications → Default sound).
mkdir -p "$APP/Contents/Resources/sounds"
cp resources/sounds/*.mp3 "$APP/Contents/Resources/sounds/"
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
# Sign with a certificate when there is one: macOS ties the privacy grants
# (Screen Recording, notifications, …) to the signature's designated
# requirement. Ad-hoc, that is the binary's hash, so every rebuild loses them;
# with a certificate it is the bundle id plus the certificate, stable across
# builds. SG_SIGN_IDENTITY picks one (a SHA-1 from `security find-identity -v
# -p codesigning`, or "-" for ad-hoc); by default the first Apple Development one.
IDENTITY="${SG_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null | awk '/"Apple Development: / { print $2; exit }')}"
IDENTITY="${IDENTITY:--}"
# Inside out: the helper bundle's extra executable, the helper bundle (it is
# copied into the data dir and runs from there, so it must be signed on its
# own), then Lite's bundle around it.
codesign --force --sign "$IDENTITY" --identifier dev.splitgrid.lite.ptyd "$HELPER/Contents/MacOS/splitgrid-ptyd"
codesign --force --sign "$IDENTITY" "$HELPER"
codesign --force --sign "$IDENTITY" "$APP"
codesign --verify --deep --strict "$APP"
if [ "$IDENTITY" = "-" ]; then
  echo "signed ad-hoc (no codesigning certificate found): privacy grants reset on every rebuild"
else
  echo "signed with certificate $(printf %.8s "$IDENTITY")…"
fi
echo "built $APP"

if [ "${1:-}" = "--install" ]; then
  DEST="/Applications/$NAME.app"
  if pgrep -f "$DEST/Contents/MacOS/splitgrid" >/dev/null; then
    echo "$NAME is running; quit it first" >&2
    exit 1
  fi
  rm -rf "$DEST"
  ditto "$APP" "$DEST"
  # Leave one copy: a second "SplitGrid Lite.app" under target/ would show up in
  # Spotlight, Launchpad and notification settings next to the installed one.
  LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
  "$LSREGISTER" -u "$APP" 2>/dev/null || true
  rm -rf "$APP"
  echo "installed $DEST"
fi
