#!/bin/sh
# Compile the Icon Composer source icon/AppIcon.icon (the SplitGrid "S" on a
# lighter tile than the full app's) into icon/build: Assets.car (Liquid Glass,
# macOS 26+) and AppIcon.icns (flattened fallback). Needs Xcode's actool.
set -eu
cd "$(dirname "$0")/.."
mkdir -p icon/build
xcrun actool icon/AppIcon.icon --compile icon/build \
  --output-format human-readable-text --notices --warnings --errors \
  --output-partial-info-plist icon/build/partial.plist \
  --app-icon AppIcon --include-all-app-icons \
  --enable-on-demand-resources NO --development-region en \
  --target-device mac --minimum-deployment-target 26.0 --platform macosx
rm -f icon/build/partial.plist
echo "wrote icon/build/Assets.car and icon/build/AppIcon.icns"
