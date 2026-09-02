#!/bin/sh
# Build a universal ThunderLink Linux distribution: both arch AppImages
# + a self-extracting universal installer script.
#
# Produces in apps/desktop/src-tauri/target/release/bundle/universal/:
#   ThunderLink_0.1.0_aarch64.AppImage   (native ARM64)
#   ThunderLink_0.1.0_amd64.AppImage     (native x86_64)
#   ThunderLink_universal.sh             (self-extracting, runs on both)
#
# Prerequisites:
#   - aarch64 build: scripts/build-linux-appimage.sh (container: tl-build)
#   - x86_64 build:  this script creates container tl-build-x64 if needed
set -e
cd "$(dirname "$0")/.."

BUNDLE_DIR="apps/desktop/src-tauri/target/release/bundle"
UNIVERSAL="$BUNDLE_DIR/universal"
mkdir -p "$UNIVERSAL"

echo "=== Universal Linux distribution builder ==="

# ---- 1. Verify aarch64 build ------------------------------------------
ARM_APPIMAGE="$BUNDLE_DIR/appimage/ThunderLink_0.1.0_aarch64.AppImage"
if [ ! -s "$ARM_APPIMAGE" ]; then
    echo "  no aarch64 AppImage; running scripts/build-linux-appimage.sh..."
    scripts/build-linux-appimage.sh
fi
cp "$ARM_APPIMAGE" "$UNIVERSAL/"
echo "  aarch64: $(ls -lh "$UNIVERSAL/ThunderLink_0.1.0_aarch64.AppImage" | awk '{print $5}')"

# ---- 2. Build x86_64 ----------------------------------------------------
if ! docker exec tl-build-x64 bash -c 'true' 2>/dev/null; then
    echo "  creating x86_64 container (ubuntu:24.04 for libspa compat)..."
    docker run -d --name tl-build-x64 --platform linux/amd64 \
        -v "$(pwd)":/work -w /work ubuntu:24.04 sleep infinity
    docker exec tl-build-x64 bash -c '
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        apt-get install -y -qq build-essential curl pkg-config libssl-dev file git cmake \
            libx264-dev patchelf libclang-dev libwebkit2gtk-4.1-dev libgtk-3-dev \
            libglib2.0-dev libayatana-appindicator3-dev librsvg2-dev \
            pipewire libpipewire-0.3-dev libspa-0.2-dev xdg-desktop-portal dbus \
            squashfs-tools > /dev/null 2>&1
        curl -fsSL https://deb.nodesource.com/setup_20.x | bash - > /dev/null 2>&1
        apt-get install -y -qq nodejs > /dev/null 2>&1
        curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal > /dev/null 2>&1
        echo "  provisioned"
    '
fi

echo "  building x86_64 desktop app (QEMU-emulated, ~45 min)..."
docker exec tl-build-x64 bash -lc '
    cd /work/apps/desktop
    npm install --no-fund --no-audit > /dev/null 2>&1
    . ~/.cargo/env
    CARGO_TARGET_DIR=/tmp/tl-target-x64 npx tauri build --no-bundle 2>&1 | tail -2
    # Package manually (linuxdeploy plugin fails under QEMU):
    # deploy deps + create AppDir + mksquashfs + prepend runtime
    cd /tmp/tl-target-x64/release
    # Run linuxdeploy for dependency deployment (not final AppImage)
    mkdir -p bundle/appimage/ThunderLink.AppDir/usr/bin
    cp thunderlink-desktop bundle/appimage/ThunderLink.AppDir/usr/bin/
    # Use linuxdeploy just for dependency resolution
    APPIMAGE_EXTRACT_AND_RUN=1 /root/.cache/tauri/linuxdeploy-x86_64.AppImage \
        --appdir bundle/appimage/ThunderLink.AppDir \
        --plugin gtk --output appimage 2>/dev/null || \
    echo "  (linuxdeploy partial; continuing with manual packaging)"
    # If linuxdeploy produced the AppImage, copy it; otherwise manual
    if ls bundle/appimage/*.AppImage >/dev/null 2>&1; then
        cp bundle/appimage/*.AppImage /work/'"$BUNDLE_DIR"'/appimage-x64/
    else
        # Manual: extract runtime, mksquashfs, cat
        RT_SIZE=$(od -A n -t u4 -j 520 -N 4 /root/.cache/tauri/linuxdeploy-x86_64.AppImage | tr -d " ")
        dd if=/root/.cache/tauri/linuxdeploy-x86_64.AppImage of=/tmp/rt bs=1 count=$RT_SIZE 2>/dev/null
        cd bundle/appimage
        mksquashfs ThunderLink.AppDir /tmp/sqfs -root-owned -noappend -comp zstd > /dev/null 2>&1
        cat /tmp/rt /tmp/sqfs > /work/'"$BUNDLE_DIR"'/appimage-x64/ThunderLink_0.1.0_amd64.AppImage
        chmod +x /work/'"$BUNDLE_DIR"'/appimage-x64/ThunderLink_0.1.0_amd64.AppImage
    fi
    echo "  x86_64 AppImage built"
' 2>&1 | grep -v "^$" | tail -3

mkdir -p "$BUNDLE_DIR/appimage-x64"
X64_APPIMAGE="$BUNDLE_DIR/appimage-x64/ThunderLink_0.1.0_amd64.AppImage"
if [ ! -s "$X64_APPIMAGE" ]; then
    echo "ERROR: x86_64 build failed"
    exit 1
fi
cp "$X64_APPIMAGE" "$UNIVERSAL/"
echo "  x86_64:  $(ls -lh "$UNIVERSAL/ThunderLink_0.1.0_amd64.AppImage" | awk '{print $5}')"

# ---- 3. Build universal self-extractor ---------------------------------
echo "  assembling universal installer..."
TEMPLATE="scripts/universal-installer-template.sh"
OUTPUT="$UNIVERSAL/ThunderLink_universal.sh"

cp "$TEMPLATE" "$OUTPUT"
chmod +x "$OUTPUT"

# Append base64 payloads after the marker
{
    echo "# BEGIN_aarch64"
    base64 < "$UNIVERSAL/ThunderLink_0.1.0_aarch64.AppImage"
    echo "# END_aarch64"
    echo "# BEGIN_amd64"
    base64 < "$UNIVERSAL/ThunderLink_0.1.0_amd64.AppImage"
    echo "# END_amd64"
} >> "$OUTPUT"

echo "  universal: $(ls -lh "$OUTPUT" | awk '{print $5}')"

# ---- 4. Restore mac node_modules ---------------------------------------
(cd apps/desktop && npm install --no-fund --no-audit > /dev/null 2>&1) || true

echo ""
echo "=== DONE ==="
ls -lh "$UNIVERSAL/"
echo ""
echo "Distribution:"
echo "  ARM64 Linux:   ThunderLink_0.1.0_aarch64.AppImage"
echo "  x86_64 Linux:  ThunderLink_0.1.0_amd64.AppImage"
echo "  Universal:     ThunderLink_universal.sh (self-extracting, detects arch)"
