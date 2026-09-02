#!/bin/bash
# ThunderLink AppRun — with fixes for glibc, GIO, and WebKitGTK rendering.
#
# Three issues this script addresses (diagnosed 2026-09-02):
# 1. GLIBC_PRIVATE undefined symbol: we do NOT bundle glibc (libc.so.6,
#    libpthread, etc.) — the host system provides them. Bundling an older
#    glibc while the host has a newer one causes symbol version conflicts.
# 2. GIO module load failure (libgcfsbus.so): set GIO_MODULE_DIR to empty
#    so the host's GIO modules (compiled against a different GLib) are
#    never loaded into our bundled-GLib process.
# 3. Blank screen / no UI: WebKitGTK's hardware compositing (DMA-BUF)
#    fails in many AppImage contexts (NVIDIA, Wayland, headless). Force
#    software rendering and X11 for maximum compatibility.
HERE="$(dirname "$(readlink -f "${0}")")"

# Library path: bundled libs only, glibc intentionally excluded
export LD_LIBRARY_PATH="$HERE/usr/lib:${LD_LIBRARY_PATH:-}"

# GIO: prevent host system modules from loading into our bundled GLib
export GIO_MODULE_DIR=""

# WebKitGTK: force software rendering (fixes blank screen on NVIDIA,
# Wayland, and in containers/VMs without GPU)
export WEBKIT_DISABLE_COMPOSITING_MODE=1
export WEBKIT_DISABLE_DMABUF_RENDERER=1

# GDK: prefer X11 for maximum compatibility (works via XWayland too)
if [ -z "$GDK_BACKEND" ]; then
    export GDK_BACKEND=x11
fi

exec "$HERE/usr/bin/thunderlink-desktop" "$@"
