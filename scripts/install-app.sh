#!/bin/sh
# Build the ThunderLink desktop app and install it to /Applications.
# Usage: scripts/install-app.sh [--no-dmg]
set -e
cd "$(dirname "$0")/.."
. "$HOME/.cargo/env" 2>/dev/null || true

cd apps/desktop
npm install --no-fund --no-audit >/dev/null
if [ "$1" = "--no-dmg" ]; then
  npx tauri build --no-bundle
  SRC=src-tauri/target/release/bundle/macos/ThunderLink.app
else
  npx tauri build
  DMG=$(ls src-tauri/target/release/bundle/dmg/ThunderLink_*.dmg | tail -1)
  hdiutil attach -nobrowse -readonly "$DMG" >/dev/null
  SRC=/Volumes/ThunderLink/ThunderLink.app
fi

rm -rf /Applications/ThunderLink.app
cp -R "$SRC" /Applications/
[ "$1" = "--no-dmg" ] || hdiutil detach /Volumes/ThunderLink -quiet
echo "installed: /Applications/ThunderLink.app"
open -a ThunderLink
