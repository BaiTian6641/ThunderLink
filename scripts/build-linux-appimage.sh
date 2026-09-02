#!/bin/sh
# Build the ThunderLink Linux AppImage (+deb/rpm) in a Linux container.
#
# Prereqs on the Mac: colima + docker CLI (brew install colima docker),
#   colima start --cpu 4 --memory 8
#
# Produces: apps/desktop/src-tauri/target/release/bundle/appimage/
#           ThunderLink_0.1.0_aarch64.AppImage
#
# Key gotchas encoded here (NOTES 2026-09-02):
#   - linuxdeploy creates .so SYMLINKS: bundling on the macOS bind mount
#     (virtiofs) breaks them -> keep CARGO_TARGET_DIR on container-native
#     storage (/tmp/tl-target).
#   - linuxdeploy itself is an AppImage: no FUSE in containers ->
#     APPIMAGE_EXTRACT_AND_RUN=1.
#   - Frontend node_modules is shared with the Mac: the container npm
#     install swaps esbuild to the Linux binary -> run `npm install` on
#     the Mac again afterwards.
set -e
cd "$(dirname "$0")/.."

docker exec tl-build bash -c 'true' 2>/dev/null || {
  echo "starting build container (first run installs ~600MB of deps)..."
  docker run -d --name tl-build --platform linux/arm64 \
    -v "$(pwd)":/work -w /work ubuntu:22.04 sleep infinity
}

docker exec tl-build bash -c '
set -e
if [ ! -x /root/.cargo/bin/cargo ]; then
  export DEBIAN_FRONTEND=noninteractive
  sed -i "s/archive.ubuntu.com/mirrors.edge.kernel.org/g" /etc/apt/sources.list
  apt-get update -qq
  apt-get install -y -qq build-essential curl pkg-config libssl-dev file git \
    libx264-dev xvfb x11-utils patchelf libclang-dev \
    libwebkit2gtk-4.1-dev libgtk-3-dev libglib2.0-dev \
    libayatana-appindicator3-dev librsvg2-dev > /dev/null
  curl -fsSL https://deb.nodesource.com/setup_20.x | bash - > /dev/null
  apt-get install -y -qq nodejs > /dev/null
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal > /dev/null
fi
cd /work/apps/desktop
npm install --no-fund --no-audit > /dev/null
. ~/.cargo/env
CARGO_TARGET_DIR=/tmp/tl-target APPIMAGE_EXTRACT_AND_RUN=1 npx tauri build 2>&1 | tail -5
mkdir -p /work/apps/desktop/src-tauri/target/release/bundle/appimage
cp /tmp/tl-target/release/bundle/appimage/*.AppImage \
   /work/apps/desktop/src-tauri/target/release/bundle/appimage/
ls -lh /work/apps/desktop/src-tauri/target/release/bundle/appimage/
'
echo ""
echo "DONE. Restore the Mac-side frontend deps with: cd apps/desktop && npm install"
