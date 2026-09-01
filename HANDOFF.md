# HANDOFF — ThunderLink takeover guide

For the next agent/operator. Read PLAN.md (strategy/milestones), SPEC.md
(wire protocol + crate contracts), NOTES.md (decision/risk log) first; this
file is the operational state dump: what exists, what is verified, how to
prove it, and what to do next. Last updated 2026-09-01 (post-TCC + audio SPEC).

## 1. Where we are

**Working, verified on this machine (M1 iMac, macOS 26.6, Rust 1.98):**
full macOS implementation of both roles, point-to-point, no crypto.

- All 9 workspace crates compile; `cargo test --workspace` = **66/66 green**;
  `cargo clippy --workspace --all-targets` = **0 warnings**. Tree clean,
  everything committed (17 commits, see `git log --oneline`).
- End-to-end smoke (SPEC §11) passes on 127.0.0.1: handshake → HEVC stream →
  decode → Metal present → clean Stop teardown, exit 0 both processes.
  Measured: 4480x2520@60 HEVC 550 Mbps glass-to-glass 26–38 ms; 1080p60
  9.7–12.3 ms; decoded fps ≈ source fps at 1080p, ~46/60 at 5K **on loopback
  only** (kernel drops bursts — see §5).
- mDNS discovery wired end-to-end: target announces `_thunderlink._tcp`
  (role TXT) for its lifetime; initiator `--discover [--discover-timeout N]`
  browses and probes candidate addrs (IPv4 → global v6 → rest). The control
  listener survives stray connections/probes (accept retried ≤16 failures).
- README.md exists: usage, build/validation, **no-auth threat model**.
  `--virtual` extended-display mode: creates a real OS-visible virtual
  display at the target panel's native res/HiDPI, captures it, removes it on
  teardown. Lifecycle smoke-verified (system_profiler mid-run / 0 after).

**Not yet:** real Thunderbolt-link test (needs 2nd host), Linux/Windows crates,
USB/IP, packaging, any crypto, audio implementation (contract: SPEC §12).

**TCC (2026-09-01 late): grants are in place and LIVE-verified.** Screen
Recording (`--source screen` mirror smoke: ~31 ms, clean teardown), Input
Monitoring + Accessibility (inject→tap keyboard roundtrip). Caveats:
`capture_frames_e2e` passes even on denial (early return) — the binary's
"permission" error is the honest probe; CGEventPost silently drops without
Accessibility. Don't run loopback sessions with input enabled (tap echo loop).

## 2. Repo map

```
crates/
  tl-proto          Wire types, packet framing, ports, bitrate ladder. 5 tests.
  tl-net            Control(TCP len-prefix bincode), VideoTx/Rx (fragment,
                    NACK ring 16MiB, reassembly+drop policy), feedback/input
                    UDP, TB iface detect, mDNS. 20 tests.
  tl-session        Handshake state machines (InitiatorSession/TargetSession,
                    StartPending ack-gate). No threads inside.
  tl-video          latest-wins channel + run_initiator/run_target loops,
                    Counters. 2 tests.
  tl-macos-capture  ScreenCaptureKit capture, VT HEVC/H264 encoder (Annex B,
                    param sets on IDR), TestPattern source. 6 tests.
  tl-macos-render   VT decoder (Annex B in, NV12-preferred out), AppKit+Metal
                    presenter (vsync, latest-wins, SubmitHandle). 13 tests.
  tl-macos-input    CGEventPost injector, CGEventTap capturer, HID keycode
                    table. 16 tests.
  tl-macos-display  CGVirtualDisplay (runtime class lookup only), panel info
                    + EDID (IOKit), display_frame helper. 3 tests.
  thunderlink       CLI binary: `target` / `initiator` roles, all thread
                    orchestration lives here.
```

## 3. How to validate (do this first after any change)

```sh
. "$HOME/.cargo/env"                       # cargo NOT on default PATH
cargo check --workspace
cargo clippy --workspace --all-targets     # must stay 0 warnings
cargo test --workspace                     # must stay 66/66+
```

Smoke test (pattern source, no TCC needed):

```sh
cargo build -p thunderlink
./target/debug/thunderlink initiator --discover --source test-pattern \
    --res 1920x1080 --fps 60 --frames 300
# expect: "discovered target ...", ~60 fps decoded, exit 0 both sides.
# (or --connect 127.0.0.1 instead of --discover)
./target/debug/thunderlink initiator --connect 127.0.0.1 \
    --source test-pattern --res 1920x1080 --fps 60 --frames 300
# expect: initiator exit 0, logs "target: ~57-59 fps decoded, rtt ...";
# target logs frames + "session ended by initiator" then exits 0 itself.
```

Virtual-display lifecycle smoke:

```sh
./target/debug/thunderlink initiator --connect 127.0.0.1 --virtual \
    --source test-pattern --frames 600 &
sleep 4 && system_profiler SPDisplaysDataType | grep -i -A3 thunderlink
wait; sleep 2; system_profiler SPDisplaysDataType | grep -ci thunderlink  # -> 0
```

TL_E2E=1 enables permission/hardware tests (presenter window, VD create,
capture). Screen mirror (`--source screen`) and real input forwarding need
Screen Recording / Input Monitoring grants the dev shell does NOT yet have.

## 4. Conventions that are load-bearing

- **No async anywhere.** Std threads + blocking sockets + parking_lot.
  `parking_lot` over `std::sync` Mutex/RwLock/Condvar (project rule; note
  `cv.wait_for(&mut guard, dur)` returns only `WaitTimeoutResult`).
- **Latest-wins everywhere**: capture→encode channel, reassembly, presenter
  submit. Never queue stale frames.
- Wire format changes → edit `tl-proto` + SPEC.md in the same commit.
- All frame `pts_us` are wall-clock µs (`tl_proto::time::now_us`) stamped at
  source — latency logs depend on it.
- Annex B on the wire; parameter sets on every IDR; no B-frames.
- Decoder prefers native biplanar YUV; presenter shader converts (BT.709).
- AppKit main-thread contract: `Presenter::new`/`run` main thread;
  `SubmitHandle::submit`/`request_close` any thread.
- Every `unsafe` needs `// SAFETY:`; TCC-denial errors must contain
  "permission"; libraries use `log`, never println.
- Tests must pass headless without TCC/hardware; gate real-hardware tests
  behind `TL_E2E=1`.
- Commit per stage (see git log style: `type(scope): summary` + body).

## 5. Environment facts (hard-won, do not rediscover)

- This Mac's panel: **4480x2520@60, scale 200%** (iMac 4.5K). EDID is NOT
  exposed for built-in Apple Silicon panels via IOKit → `edid: None` is
  correct behavior here.
- **macOS loopback UDP drops ~45–75% of multi-MB bursts** (even with 8 MiB
  rcvbuf). The 5K loopback fps ceiling (~46/60) is THIS, not the pipeline.
  Do not tune the stack against loopback; wait for a real TB link.
- macOS TB bridge negotiates 10 Gbps, ~5–6 Gbps real TCP. Linux
  thunderbolt-net hits 16–20 Gbps. Windows TbtNet ~9–10 Gbps.
- CGVirtualDisplay (private): descriptor MUST carry a dispatch queue and a
  unique serialNum or creation fails/leaks; hiDPI=true means mode dims are
  POINTS (halve pixels); Drop releases the display ~1 s later.
- VT decode output chosen by VT: '420v' (NV12 biplanar) — BGRA was the wrong
  default (4× write bandwidth).
- VideoToolbox rejects `kVTCompressionPropertyKey_MaxFrameDelayCount` on
  this M1 (-12900) — expected, logged at debug, not an error.
- `cargo` needs `. "$HOME/.cargo/env"` in every fresh shell.

## 6. Subagent orchestration history (what to reuse)

Five parallel task agents built the platform crates against pinned stubs in
each crate's lib.rs (contract-first delegation — worked well). Contracts
lived in SPEC.md + stub signatures; agents were forbidden to touch other
crates and used per-crate `CARGO_TARGET_DIR=target/agent-<name>` to avoid
lock contention. Mid-flight steering via hub worked: SubmitHandle/
request_close contract addition, parking_lot rule broadcast.

Agent-reported deviations that are now part of the design (see their final
reports in NOTES/git history):
- CaptureConfig has no width/height: SCK captures at the display's native
  pixel size (matches SPEC §1 native-res policy).
- H.264 test asserts NAL TYPE not literal 0x67 (VT emits nal_ref_idc=01).
- HID usage 0x64 (non-US \|) deliberately unmapped (roundtrip identity).
- PrintScreen/ScrollLock/Pause map to F13/F14/F15 (no dedicated kVK).
- VideoRx retains/re-NACKs frames (up to 3x/500 ms) beyond SPEC-literal
  once-only — SPEC §5 has been updated to match.

## 7. Next steps, in priority order
1. **Real TB link validation** (needs 2nd host): iperf3 both directions,
   then full session over the cable; re-measure fps/latency; validate
   `link::thunderbolt_interfaces()` detection on all OSes.
2. ~~TCC pass on dev machine~~ **done 2026-09-01 late** — all three grants,
   mirror smoke + inject→tap roundtrip live-verified (see §1 caveats).
3. **Linux platform crates** (`tl-linux-*`): EVDI/VKMS virtual display,
   KMS/PipeWire capture, VAAPI encode/decode, uinput inject, evdev capture.
   Core crates are platform-neutral already; mirror the macOS crate APIs.
   Do NOT write these blind on a Mac — wait for Linux hardware/CI.
4. **Windows platform crates** (`tl-windows-*`): IddCx driver (fork
   VirtualDrivers/Virtual-Display-Driver), DXGI DD capture, MFT/D3D11VA,
   SendInput. Start EV code-signing cert procurement NOW (weeks of lead).
5. ~~Wire `discovery`/mDNS into the binary~~ **done 2026-09-01 night**
   (target announces; `--discover` on initiator; addr probing; probe-proof
   accept loop).
6. **Audio implementation** against the now-written contract (SPEC §12;
   start with the TCC-free sine tone source, then Core Audio tap);
   USB/IP (v2); adaptive bitrate ladder hooks (Report is wired; policy
   not implemented).
7. Housekeeping: ~~README~~ **done** (incl. no-auth threat model); render
   crate core-foundation 0.10 → objc2-core-foundation consistency nit.

## 8. Known open risks (verbatim from NOTES.md — keep it current)

Private CGVirtualDisplay fragility · macOS TB bridge bandwidth cap ·
loopback fps is kernel-bound · no auth on wire (owner-accepted, documented
in README) · real-hardware validation pending.
