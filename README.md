# ThunderLink

Use a Thunderbolt/USB4-equipped computer as a high-resolution external
monitor + input peripheral for another computer. Software-only, open,
point-to-point.

One binary, two roles:

- **Initiator** — the machine whose desktop is extended/mirrored. Captures
  its screen (or a virtual display), hardware-encodes HEVC/H.264, streams
  it, and injects the input events that come back.
- **Target** — the machine acting as the monitor. Decodes and presents the
  stream fullscreen, captures its keyboard/mouse, and forwards them.

Status: **macOS v1 works end-to-end on one machine** (loopback). See
`HANDOFF.md` for the detailed state, `PLAN.md` for strategy/milestones,
`SPEC.md` for the wire protocol, `NOTES.md` for the decision/risk log.

## What is implemented

- Wire protocol (TCP control, UDP video with fragmentation + NACK
  retransmit + IDR recovery, UDP feedback, UDP input) — `tl-proto`, `tl-net`
- Session handshake and state machines — `tl-session`
- ScreenCaptureKit capture, VideoToolbox encode (Annex B, param sets on
  IDR, real-time, no B-frames) — `tl-macos-capture`
- VideoToolbox decode (native NV12 out), AppKit+Metal vsync presenter
  (latest-wins, zero-copy) — `tl-macos-render`
- Input both ways: CGEventTap capture + CGEventPost injection with USB HID
  usage tables — `tl-macos-input`
- Virtual display via private `CGVirtualDisplay` (runtime-resolved, no
  link-time private symbols; destroyed on teardown) + panel info/EDID —
  `tl-macos-display`
- mDNS discovery (`_thunderlink._tcp`) on the target, `--discover` on the
  initiator
- Audio (v1.1): system-audio or synthetic-sine streaming (Opus, UDP 47780,
  jitter buffer + PLC) — `target --audio` + `initiator --audio
  sine|system`; validated gap-free at 100% packet delivery on loopback

Not yet: Linux/Windows platform crates, audio, USB/IP, real-Thunderbolt-
link validation, packaging.

## Build and validate

Requires macOS with Xcode CLT and Rust (rustup).

```sh
cargo check --workspace
cargo clippy --workspace --all-targets     # 0 warnings expected
cargo test --workspace                     # 66+ tests, headless, no permissions needed
```

## Desktop app (easiest)

Build and install the GUI app (Carbon design, both roles, live stats,
permission guidance) in one step:

```sh
scripts/install-app.sh        # builds the DMG, installs to /Applications, launches
```

Or grab the installer directly:
`apps/desktop/src-tauri/target/release/bundle/dmg/ThunderLink_*.dmg` —
open it and drag ThunderLink to Applications.

**First run**: the app shows a Permissions panel. Grant as needed and use
the per-row "Open Settings" buttons to jump straight to the right System
Settings pane:

- *Screen Recording* — only for streaming your screen (the test pattern
  and acting as a display need nothing).
- *Accessibility* — for controlling the initiator with the target's
  keyboard/mouse.
- *Input Monitoring* — for forwarding the target's keyboard/mouse.

 After toggling a permission, restart the app if the status pill doesn't
 update.

The app is ad-hoc signed (no paid Apple developer account yet): it runs
directly on the machine where it was built. On another Mac, right-click →
Open once to pass Gatekeeper, or run
`xattr -dr com.apple.quarantine /Applications/ThunderLink.app`.
Notarization is planned once a signing certificate is procured.

## Linux (AppImage)

The desktop app and the initiator role run on Linux (aarch64 AppImage;
x86_64 builds the same way in an amd64 container). From the Mac repo:

```sh
scripts/build-linux-appimage.sh    # colima/docker Ubuntu 22.04 container build
```

Artifacts in `apps/desktop/src-tauri/target/release/bundle/universal/`:
- `ThunderLink_0.1.0_aarch64.AppImage` — native ARM64 (100 MB)
- `ThunderLink_0.1.0_amd64.AppImage` — native x86_64 (30 MB)
- `ThunderLink_universal.sh` — self-extracting universal installer (175 MB,
  detects `uname -m`, extracts the right AppImage, runs it; `TL_EXTRACT_ONLY=1`
  to extract without running)

Each AppImage is self-contained (WebKitGTK + all libs bundled). On hosts
without FUSE, run with `--appimage-extract-and-run`. Build both with
`scripts/build-universal-appimage.sh` (uses the tl-build arm64 container +
creates tl-build-x64 if needed).

Linux specifics:
- The initiator is the supported role: test-pattern source (no
  permissions) or X11 screen capture (`$DISPLAY`; Wayland portal capture
  is planned). H.264 x264 encode (VAAPI HEVC planned).
- Input control of the Linux machine needs `/dev/uinput`: add yourself
  to the `input` group (`sudo usermod -aG input $USER`, re-login) or a
  udev rule. Without it, streaming works and input is disabled with a
  warning.
- The target (display) role is macOS-only for now.

## Usage (CLI)

Act as a monitor (target machine):

```sh
thunderlink target [--windowed] [--no-input] [--bind ADDR]
```

Stream to it (initiator machine):

```sh
thunderlink initiator --connect HOST[:PORT]   # or --discover for mDNS
    [--source test-pattern|screen] [--codec hevc|h264] [--bitrate-kbps N]
    [--fps N] [--res WxH] [--frames N] [--virtual]
```

- `--discover` finds targets via mDNS and picks a reachable address.
- `--virtual` creates a real OS-visible virtual display at the target
  panel's native resolution/HiDPI (extended desktop) instead of mirroring;
  it is removed on teardown.
- `--frames N` stops cleanly after N frames (automation).

First run of `--source screen` (capture) and input forwarding need
Screen Recording / Accessibility + Input Monitoring grants (TCC). The
test-pattern source and loopback smoke need no permissions.

Loopback smoke (single machine, no permissions):

```sh
thunderlink target --windowed --no-input &
thunderlink initiator --discover --source test-pattern \
    --res 1920x1080 --fps 60 --frames 300
```

Measured on an M1 iMac (5K panel): 1080p60 at 60 fps decoded with 9–12 ms
encode-to-decode latency; 5K60 at ~46 fps on loopback (kernel drops UDP
bursts; expect 60 on a real link).

## Security / threat model

**v1 has no authentication and no encryption.** This is an owner-accepted
constraint: the intended deployment is a single point-to-point cable, where
the physical link is the access boundary, similar to plugging in an HDMI
cable. Concretely:

- Anything that can open a TCP/UDP connection to the target's ports
  (47776–47779) can stream video to it and inject keystrokes/mouse events
  into the initiator. Only run this on interfaces you control — a
  Thunderbolt bridge, a direct cable, or an isolated/lab LAN. Do not
  expose it to untrusted networks.
- The target accepts the first initiator that completes the handshake and
  the initiator trusts the caps it receives. There is no confirmation
  dialog yet (planned, milestone M6).
- Protected content (HDCP) will not capture — expect black screens for
  Netflix etc., as with all screen capture.
- A crypto/pairing layer is planned for v2; the framing leaves room for it
  (SPEC §3.2).

## Repository layout

```
crates/tl-proto           wire types, framing, ports, bitrate ladder
crates/tl-net             control/video/feedback/input channels, link detect, mDNS
crates/tl-session         handshake state machines
crates/tl-video           latest-wins channel + streaming loops
crates/tl-macos-capture   ScreenCaptureKit + VideoToolbox encoder + test source
crates/tl-macos-render    VideoToolbox decoder + Metal presenter
crates/tl-macos-input     CGEventTap capture + CGEventPost injection
crates/tl-macos-display   CGVirtualDisplay + panel/EDID
crates/thunderlink        the CLI binary (both roles)
```

## License

MIT OR Apache-2.0.
