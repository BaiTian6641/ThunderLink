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

# Ensure TCC usage descriptions are in the Info.plist (required for
# macOS to show permission prompts; Tauri picks up src-tauri/Info.plist
# but belt-and-suspenders for manual builds)
PLIST="$SRC/Contents/Info.plist"
if [ -f "$PLIST" ]; then
    /usr/libexec/PlistBuddy -c "Add :NSScreenCaptureDescription string 'ThunderLink needs Screen Recording to capture and stream your display.'" "$PLIST" 2>/dev/null
    /usr/libexec/PlistBuddy -c "Add :NSMicrophoneUsageDescription string 'ThunderLink needs Microphone access to stream system audio.'" "$PLIST" 2>/dev/null
    /usr/libexec/PlistBuddy -c "Add :NSAppleEventsUsageDescription string 'ThunderLink needs Accessibility to control the target computer.'" "$PLIST" 2>/dev/null
    /usr/libexec/PlistBuddy -c "Add :NSAudioCaptureUsageDescription string 'ThunderLink needs Audio Capture to stream this computer system audio.'" "$PLIST" 2>/dev/null
    codesign --force --sign - "$SRC" 2>/dev/null
fi

rm -rf /Applications/ThunderLink.app
cp -R "$SRC" /Applications/
[ "$1" = "--no-dmg" ] || hdiutil detach /Volumes/ThunderLink -quiet
echo "installed: /Applications/ThunderLink.app"
open -a ThunderLink
