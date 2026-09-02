#!/bin/sh
# Build ThunderLink Linux AppImages (amd64 PRIMARY, aarch64 secondary)
# and a universal self-extracting installer.
#
# Usage:
#   scripts/build-universal-appimage.sh [--arm]  (default: x86_64 only)
#   scripts/build-universal-appimage.sh --both   (both architectures)
#
# Produces in apps/desktop/src-tauri/target/release/bundle/universal/:
#   ThunderLink_0.1.0_amd64.AppImage     (x86_64 — PRIMARY, most TB/USB4 systems)
#   ThunderLink_0.1.0_aarch64.AppImage   (ARM64 — optional, --arm or --both)
#   ThunderLink_universal.sh             (self-extracting, runs on both)
#
# NOTE: x86_64 build uses a Docker Desktop amd64/QEMU container (~45 min).
# The AppImage runtime's squashfs length MUST be patched after manual
# assembly (offset 0x0210, u64 LE) — see the Python inline patch below.
set -e
cd "$(dirname "$0")/.."

BUNDLE_DIR="apps/desktop/src-tauri/target/release/bundle"
UNIVERSAL="$BUNDLE_DIR/universal"
mkdir -p "$UNIVERSAL" "$BUNDLE_DIR/appimage-x64"

MODE="${1:---both}"

echo "=== ThunderLink Linux AppImage builder (amd64-first) ==="

# ---- shared: patch squashfs length in manually-assembled AppImage ------
patch_appimage() {
    python3 - "$1" << 'PYEOF'
import struct, sys
path = sys.argv[1]
with open(path, "r+b") as f:
    f.seek(0x0208)
    off = struct.unpack("<Q", f.read(8))[0]
    if off == 0:
        print(f"  {path}: runtime uses ELF-size mode (offset=0), no patch needed")
        sys.exit(0)
    f.seek(0, 2)
    size = f.tell()
    correct_len = size - off
    f.seek(0x0210)
    old_len = struct.unpack("<Q", f.read(8))[0]
    if old_len == correct_len:
        print(f"  {path}: length already correct ({old_len})")
        sys.exit(0)
    f.seek(0x0210)
    struct.pack_into("<Q", f, 0, correct_len)
    # re-write via seek+write
    f.seek(0x0210)
    f.write(struct.pack("<Q", correct_len))
    print(f"  {path}: patched length {old_len} → {correct_len}")
PYEOF
}

# ---- x86_64 build (PRIMARY) --------------------------------------------
build_x64() {
    echo "  [x86_64] building desktop app (QEMU-emulated, ~45 min)..."
    if ! docker exec tl-build-x64 bash -c 'true' 2>/dev/null; then
        echo "  [x86_64] creating container (ubuntu:24.04)..."
        docker run -d --name tl-build-x64 --platform linux/amd64 \
            -v "$(pwd)":/work -w /work ubuntu:24.04 sleep infinity
        docker exec tl-build-x64 bash -c '
            export DEBIAN_FRONTEND=noninteractive
            apt-get update -qq
            apt-get install -y -qq build-essential curl pkg-config libssl-dev file git \
                cmake libx264-dev patchelf libclang-dev squashfs-tools \
                libwebkit2gtk-4.1-dev libgtk-3-dev libglib2.0-dev \
                libayatana-appindicator3-dev librsvg2-dev \
                pipewire libpipewire-0.3-dev libspa-0.2-dev \
                xdg-desktop-portal dbus xvfb x11-utils > /dev/null 2>&1
            curl -fsSL https://deb.nodesource.com/setup_20.x | bash - > /dev/null 2>&1
            apt-get install -y -qq nodejs > /dev/null 2>&1
            curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal > /dev/null 2>&1
            echo "  provisioned"
        '
    fi

    docker exec tl-build-x64 bash -lc '
        cd /work/apps/desktop
        npm install --no-fund --no-audit > /dev/null 2>&1
        . ~/.cargo/env
        CARGO_TARGET_DIR=/tmp/tl-target-x64 npx tauri build --no-bundle 2>&1 | tail -1
        # Manually package (linuxdeploy crashes under QEMU):
        RT_SRC=/root/.cache/tauri/linuxdeploy-x86_64.AppImage
        if [ ! -s "$RT_SRC" ]; then
            echo "ERROR: linuxdeploy not cached; run a regular build first" >&2
            exit 1
        fi
        cd /tmp/tl-target-x64/release
        mkdir -p bundle/appimage/ThunderLink.AppDir/usr/bin
        cp thunderlink-desktop bundle/appimage/ThunderLink.AppDir/usr/bin/
        # Deploy dependencies (linuxdeploy without the appimage plugin)
        APPIMAGE_EXTRACT_AND_RUN=1 "$RT_SRC" \
            --appdir bundle/appimage/ThunderLink.AppDir --plugin gtk 2>/dev/null || true
        # Extract runtime
        RT_SIZE=$(od -A n -t u8 -j 520 -N 8 "$RT_SRC" | tr -d " ")
        [ "$RT_SIZE" -gt 0 ] 2>/dev/null || RT_SIZE=$(od -A n -t u4 -j 520 -N 4 "$RT_SRC" | tr -d " ")
        dd if="$RT_SRC" of=/tmp/tl-rt bs=1 count="$RT_SIZE" 2>/dev/null
        # Create squashfs + assemble
        cd bundle/appimage
        mksquashfs ThunderLink.AppDir /tmp/tl-sqfs -root-owned -noappend -comp zstd > /dev/null 2>&1
        cat /tmp/tl-rt /tmp/tl-sqfs > /work/'"$BUNDLE_DIR"'/appimage-x64/ThunderLink_0.1.0_amd64.AppImage
        chmod +x /work/'"$BUNDLE_DIR"'/appimage-x64/ThunderLink_0.1.0_amd64.AppImage
        echo "  [x86_64] AppImage assembled"
    ' 2>&1 | grep -v "^$" | tail -2

    local X64="$BUNDLE_DIR/appimage-x64/ThunderLink_0.1.0_amd64.AppImage"
    [ -s "$X64" ] || { echo "ERROR: x86_64 build failed"; exit 1; }

    echo "  [x86_64] patching squashfs length..."
    patch_appimage "$X64"
    cp "$X64" "$UNIVERSAL/"
    echo "  [x86_64] done: $(ls -lh "$UNIVERSAL/ThunderLink_0.1.0_amd64.AppImage" | awk '{print $5}')"
}

# ---- aarch64 build (SECONDARY) ------------------------------------------
build_arm() {
    echo "  [aarch64] building..."
    if ! docker exec tl-build bash -c 'true' 2>/dev/null; then
        echo "  [aarch64] creating container (ubuntu:latest)..."
        docker run -d --name tl-build --platform linux/arm64 \
            -v "$(pwd)":/work -w /work ubuntu:latest sleep infinity
        # Provisioning is handled by scripts/build-linux-appimage.sh
        scripts/build-linux-appimage.sh 2>&1 | tail -3
    fi

    local ARM="$BUNDLE_DIR/appimage/ThunderLink_0.1.0_aarch64.AppImage"
    [ -s "$ARM" ] || { echo "ERROR: aarch64 build failed"; exit 1; }
    cp "$ARM" "$UNIVERSAL/"
    echo "  [aarch64] done: $(ls -lh "$UNIVERSAL/ThunderLink_0.1.0_aarch64.AppImage" | awk '{print $5}')"
}

# ---- universal installer -------------------------------------------------
build_universal() {
    echo "  [universal] assembling self-extracting installer..."
    local OUTPUT="$UNIVERSAL/ThunderLink_universal.sh"
    cp scripts/universal-installer-template.sh "$OUTPUT"
    chmod +x "$OUTPUT"
    {
        echo "# BEGIN_amd64"
        base64 < "$UNIVERSAL/ThunderLink_0.1.0_amd64.AppImage"
        echo "# END_amd64"
        if [ -s "$UNIVERSAL/ThunderLink_0.1.0_aarch64.AppImage" ]; then
            echo "# BEGIN_aarch64"
            base64 < "$UNIVERSAL/ThunderLink_0.1.0_aarch64.AppImage"
            echo "# END_aarch64"
        fi
    } >> "$OUTPUT"
    echo "  [universal] done: $(ls -lh "$OUTPUT" | awk '{print $5}')"
}

# ---- execute -------------------------------------------------------------
case "$MODE" in
    --x64)   build_x64 ;;
    --arm)   build_arm ;;
    --both)  build_x64; build_arm ;;
    *)       echo "Usage: $0 [--x64|--arm|--both]"; exit 1 ;;
esac

build_universal

# ---- restore mac node_modules --------------------------------------------
(cd apps/desktop && npm install --no-fund --no-audit > /dev/null 2>&1) || true

echo ""
echo "=== DONE ==="
ls -lh "$UNIVERSAL/"
