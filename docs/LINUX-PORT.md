# Linux Initiator Port Plan

Goal: a Linux initiator (the machine whose desktop is streamed to a Mac
target), reusing everything platform-neutral. Written 2026-09-02 after the
macOS side was completed end-to-end (engine + Carbon GUI + CLI).

## What transfers unchanged

- `tl-proto`, `tl-net`, `tl-session`, `tl-video` — pure Rust, no Apple deps.
- `thunderlink-engine` — the role orchestration, mDNS announce/browse,
  CancelToken/EventSink API, and `run_initiator`/`run_target` signatures.
  `imp.rs` is `cfg(target_os = "macos")`; Linux gets its own `imp_linux.rs`
  behind the same public API.
- The Tauri app (`apps/desktop`): Carbon UI, command layer, and event
  contract are platform-neutral. Linux needs: webkit2gtk system deps
  (`libwebkit2gtk-4.1-dev`, `libgtk-3-dev`), `tauri.conf.json` bundle
  targets (`deb`/`rpm`/`appimage`), and an icon set (already generated).
  `get_permissions` already returns true/true on non-macOS; Linux has no
  TCC — Wayland/X11 permission models are per compositor (document, don't
  gate).
- The CLI (`thunderlink` binary) compiles as-is on Linux once the engine
  has a Linux `imp`.

## New crates (mirror the macOS crate APIs; SPEC §9/§10 contracts)

| Crate | Replaces | Implementation notes |
|---|---|---|
| `tl-linux-capture` | `tl-macos-capture` | PipeWire `xdg-desktop-portal` ScreenCast (portal Session + Stream fd → SPA_process / dmabuf) for Wayland; KMS/DRM plane grab (gbm) or X11 XSHM/XDamage fallback. Encode: VAAPI (`libva`, HEVC main) via `rust-va`/raw FFI, fallback x264 (ultrafast, zerolatency) — Annex B + param-sets-on-IDR contract identical (SPEC §5). `TestPattern` source: copy the macOS one's contract; it is CPU-only and portable (move it to a `tl-testsrc` crate both platforms use — mechanical refactor). |
| `tl-linux-input` | `tl-macos-input` | Inject: `uinput` virtual HID (absolute pointer + keyboard) — works on X11 AND Wayland. Capture (target role, later): `evdev` grab with `EVIOCGRAB`. Reuse the same USB HID usage table (move `keys.rs` mapping logic to a shared crate; the macOS kVK table stays macOS-only). |
| `tl-linux-display` | `tl-macos-display` | Initiator extended-display: EVDI (preferred; user-space connector control, used by Sunshine) with a DKMS dependency, VKMS fallback (no EDID control — mirror mode only first). `panel` equivalent is a TARGET-role concern on Linux; skip for the initiator milestone. |

Not needed for Linux *initiator v1*: decode/present (that's target-side),
CGVirtualDisplay equivalent beyond EVDI/VKMS.

## Engine wiring (`imp_linux.rs`)

- `run_initiator`: same skeleton as macOS imp — frame source → latest-wins
  channel → encode (wall-clock pts stamp, IDR latch) → VideoTx; feedback +
  input-inject workers; control worker identical (pure tl-net/tl-session).
  Differences: source enum gains nothing (TestPattern/Screen), `res`
  defaults still come from target caps.
- `EmbeddedPresenter` analog: only needed for the TARGET role on Linux
  (not in v1 Linux milestone) — the initiator has no presenter. The app's
  initiator view works unchanged.
- TCC preflight module in the app returns true on Linux (already does).

## Port order (each step compiles + tests green before the next)

1. Extract `tl-testsrc` + HID-usage table into shared crates (macOS keeps
   its kVK mapping) — pure refactor, verified by the existing suite.
2. `tl-linux-input` (uinput inject) — smallest FFI surface, testable in a
   VM/container with `/dev/uinput`.
3. `tl-linux-capture`: testsrc path first (engine runs end-to-end against
   a Mac target with a synthetic source), then PipeWire screen capture,
   then VAAPI encode (x264 fallback).
4. Engine `imp_linux.rs::run_initiator` + CLI on Linux → loopback smoke
   vs the macOS app target over a real network or the TB bridge.
5. EVDI virtual display (extended mode) — last, needs DKMS.
6. Desktop app on Linux (webkit2gtk), same Carbon UI.

## Tooling/CI needs before step 1

- Linux builder (container or VM) — do NOT write the FFI blind on macOS.
- `cargo check --workspace --target x86_64-unknown-linux-gnu` only proves
  pure-Rust crates; the platform crates need real headers/libs
  (`libpipewire-0.3-dev`, `libudev`, `libva-dev`, `linux-headers`).
- Add `cargo ndk`-style cross tasks only after native builds work.

## Risks

- PipeWire portal session prompts differ per compositor (GNOME/KDE
  fine; wlroots needs `xdg-desktop-portal-wlr` config) — document, and
  keep the test-pattern path prompt-free (same as macOS).
- VAAPI driver quality varies (Intel i965/iHD good; AMD Mesa fine;
  NVIDIA = NVENC via a separate path — v2).
- EVDI DKMS + Secure Boot signing friction (PLAN §10.3) — mirror mode
  ships first.
