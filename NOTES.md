# ThunderLink Dev Notes / Knowledge Base

Living log of decisions, findings, and environment facts. Updated each phase.

## Environment
- Dev machine: macOS (Darwin 25.6.0), Apple M1 (aarch64). Rust 1.98.0 via
  rustup (installed 2026-09-01, `~/.cargo`).
- No second Thunderbolt host available → link-layer validated on loopback;
  TB interface detection ships behind best-effort heuristics + `TL_IFACE`
  override until real hardware test.

## Decisions
- 2026-09-01: v1 constraints from owner: point-to-point only; no
  encryption/auth; 20 Gbps-class link target; native target resolution
  (no scaling); optimize latency AND quality.
- 2026-09-01: macOS-first implementation order (dev machine is a Mac);
  Linux/Windows platform crates follow once the macOS pipeline is proven.
  Original plan M0 (two-host iperf) deferred — needs second TB machine.
- 2026-09-01: No async runtime; std threads + blocking sockets (SPEC §1).
- 2026-09-01: Smoke test uses synthetic test-pattern source so it runs
  without Screen Recording TCC grant (SPEC §11).

## Progress log
- Contracts phase: tl-proto complete (5/5 tests green); SPEC.md v1 frozen;
  workspace + 5 pinned API stubs compile green as delegation baseline.
- Parallel phase launched: 5 agents (tl-net, tl-macos-capture, -render,
  -input, -display) against the stubs. Steering sent mid-flight:
  (a) render contract gained `Presenter::submit_handle() -> SubmitHandle`
  (Clone+Send+Sync) + `request_close()` — decode/control threads must
  submit/close while `run()` owns the main thread; (b) project rule:
  `parking_lot` over `std::sync` Mutex/RwLock/Condvar (guard API differs:
  `cv.wait_for(&mut g, dur)` returns only WaitTimeoutResult).
- Glue done: tl-session (handshake/state machines incl. StartPending
  ack-gate), tl-video (latest-wins channel, initiator/target loops,
  2/2 tests), thunderlink binary (both roles, `--frames N` for automated
  smoke runs). Awaiting agent crates to compile the binary.
- Recovery session (2026-09-01 pm): prior orchestrator died mid-clippy-fix
  (provider 5h limit) leaving unverified edits; everything committed in 10
  staged commits, then verified: check/clippy clean, 65/65 tests green.
- Two read-only SPEC audits (scout agents) + fixes: VideoRx discard-skip
  (a NACKed+superseded frame could still complete after a newer frame →
  stale decode; regression test added), pts stamped wall-clock BEFORE
  encode (test-pattern pts was zero-based, SCK host-clock → latency log
  was ~56 years off), control-channel 1 s heartbeat cadence + RTT from
  echo (reported in Stats) + 5 s silence teardown, tl-video Sender now
  closes the channel when the last producer drops.
- Presenter hardening: setReleasedWhenClosed(false) (AppKit double-free on
  every close path), CVMetalTexture wrappers + owning frame held one
  vsync generation (cache-recycle race), render ctx Arc-shared with the
  display-link callback (teardown no longer relies on undocumented
  CVDisplayLinkStop semantics), CVDisplayLinkStart status checked, 10-bit
  formats corrected to 'x420'/'xf20' (the old 'P010' arm was dead code).
- Decoder now prefers native biplanar YUV output (VT picks 420v; BGRA
  fallback) — 4× less decoder write bandwidth at 5K; presenter shader
  converts. Deferred: migrating render crate off core-foundation 0.10 to
  objc2-core-foundation (consistency nit, zero behavior change).
- Smoke (SPEC §11) on 127.0.0.1, panel 4480x2520@60, HEVC 550 Mbps,
  60 s automated run: clean handshake→stream→teardown, zero TCC prompts,
  encode-to-decode latency 26–38 ms typical (< 50 ✓), source and encoder
  sustain 60 fps after TestPattern got a cached background (was full
  per-pixel redraw ≈ 10 fps at 5K) and absolute-tick pacing. Decoded
  46 fps (77% frame survival): limiter is this kernel's loopback UDP
  burst drops (known environment fact), not the pipeline; near-lossless
  TB links should sustain 60. All workspace tests green (66/66 incl.
  regression), clippy clean, TL_E2E presenter window test green.
- Follow-up session (2026-09-01 eve): state re-verified personally
  (66/66 tests, 0 clippy warnings); fixed the Stats cadence bug (deadline-
  based 1 s sender, rates over real elapsed time; smoke-validated: 57–59
  fps reported at 1080p60 loopback — was ~half before); wired `--virtual`
  extended-display mode (VD at target native res/HiDPI, capture + input
  mapping aimed at its WindowServer frame, VD removed on teardown —
  smoke-verified: visible mid-run, 0 remain after); SPEC synced with the
  audit-hardened behaviors (NACK retention/revive, supersede discard,
  heartbeat echo RTT, 5 s silence teardown, NV12-preferred decode,
  wall-clock pts). Committed per stage.
- Handoff 2026-09-01 eve: full state dumped to HANDOFF.md (repo map,
  validation recipes, load-bearing conventions, environment facts, agent
  history, prioritized next steps). Tree clean at 13 commits; 66/66 tests,
  0 clippy warnings. Next operator: read HANDOFF.md first.
- mDNS wiring session (2026-09-01 night, post-handoff): target now
  announces `_thunderlink._tcp` (role TXT) for its lifetime; initiator
  gained `--discover`/`--discover-timeout` (mutually exclusive with
  --connect). Two robustness flaws surfaced during loopback validation
  and were fixed: (a) first mDNS resolutions can carry partial/stale
  address sets (a Tailscale ULA was picked while the target was
  loopback-bound → "No route to host") — candidates are now probed with
  a 1 s TCP connect in priority order (IPv4, global IPv6, non-link-local,
  last resort); (b) the target's control listener DIED on any connection
  that failed the handshake (a plain `nc -z` port probe was enough to
  kill it) — accept now retries up to 16 failures. E2E: discover →
  1080p60 stream, 60 fps decoded, 8.6–9.6 ms encode-to-decode, rtt
  135 µs, clean teardown, target served a session after a raw probe.
  README.md written (usage + no-auth threat model). 66/66, clippy 0.
- TCC + audio-spec session (2026-09-01 late): user granted Screen
  Recording, Input Monitoring and Accessibility to the dev app. LIVE
  validations now green: `--source screen` mirror smoke (1920x1080@60,
  ~31 ms encode-to-decode, 36–54 fps on an idle desktop — SCK sends
  empty samples for unchanged content, correctly dropped) and the
  inject→tap keyboard roundtrip (Input Monitoring + Accessibility).
  Note: `capture_frames_e2e` PASSES even on TCC denial (early-return
  path) — treat a 0.2 s pass as inconclusive; the binary's clean
  "permission" error is the real probe. CGEventPost silently drops
  without Accessibility (no API error) — that was the roundtrip's
  initial failure. Loopback session with input enabled stays untested
  by design (tap would re-capture injected events → echo loop; only
  meaningful across two hosts).
- Audio v1.1 contract written BEFORE implementation (SPEC §12, repo
  convention): Opus 48 kHz stereo 10 ms frames, UDP 47780, wall-clock
  pts shared with video, jitter buffer + PLC, negotiation fields, §12.7
  validation bar. Research finding: macOS 14.2+ ships a PUBLIC system-
  audio tap (AudioHardwareCreateProcessTap + aggregate device) — PLAN
  §6 updated; no BlackHole-style driver needed. tl-proto types land
  with the implementation.
- GUI + engine session (2026-09-02, per owner direction: complete the Mac
  side with a good UI, then prepare Linux initiator; IBM Carbon design):
  (a) role logic extracted from the binary into `thunderlink-engine`
  (EngineEvent stream + CancelToken; CLI is a thin front-end, behavior
  identical, smoke-verified); (b) Tauri v2 + carbon-web-components app
  (`apps/desktop`, separate workspace to keep the tauri dep tree out of
  the core lockfile) — role picker, both role views, live stats, activity
  log, permission banners reading REAL TCC state; frontend built by a
  designer sub-agent and personally browser-verified in mock mode, then
  visually reviewed (vision-model "icon/notification" complaints were
  false positives — warning--glyph circle + inverse g100 notification are
  canonical Carbon v10); (c) found + fixed the architecture gap that the
  GUI exposed: Presenter lifecycle split (show/hide main-thread;
  start/stop render + poll_events any-thread; run() rebuilt on those,
  E2E green), engine takes Option<EmbeddedPresenter>; (d) FULL-STACK
  verified on the real .app via accessibility driving: click Target →
  Start → mDNS announce → CLI initiator streamed 1080p60 HEVC through the
  embedded FULLSCREEN presenter, 8–28 ms latency, clean end + Start-again
  UI. AX scripting of the WKWebView (System Events entire contents) works
  and is the GUI automation path. docs/LINUX-PORT.md written (crate map,
  port order, risks; core+engine+UI transfer unchanged). Gotchas: tauri
  needs node (brew), icons via `npx tauri icon` from a source PNG;
  carbon-web-components 1.21 registers `bx-*` (not cds-*); sync Tauri
  commands run on the main thread (that's what makes EmbeddedPresenter
  creation legal); first mDNS resolve may need the probe-retry loop
  (already in).

- Distribution session (2026-09-02): app made directly runnable.
  `scripts/install-app.sh` builds the DMG (4.0 MB, ad-hoc signed — no
  Apple developer cert on this machine), mounts it, installs to
  /Applications, launches. Verified from /Applications: window renders,
  new Permissions panel has per-row "Open Settings" buttons (macOS 26
  still accepts the com.apple.preference.security deep links) that open
  the right pane — AX-clicked live. TCC nuance: launching the binary
  from a terminal attributes permissions to the TERMINAL; launching the
  .app via Finder/open attributes to ThunderLink — banners reflect
  whatever TCC says at runtime, which is the designed first-run flow.
  Gatekeeper: runs directly where built; other Macs need one
  right-click→Open (or xattr quarantine removal) until notarized.
  tauri.conf gotchas: bundle JSON is strict (trailing-brace edits broke
  it twice); keep targets ["app","dmg"].

- Linux software session (2026-09-02, per owner direction): full Linux
  initiator + AppImage desktop app, built and verified in a colima
  Ubuntu 22.04 arm64 container (`docker exec tl-build`, see
  scripts/build-linux-appimage.sh). Sub-agents built tl-linux-input
  (uinput, 17/17 container tests, permission-flavored errors) and
  tl-linux-capture (x11rb GetImage + x264-sys FFI, 9/9 + live Xvfb);
  both personally re-verified. Engine got imp_linux (shared ctrl.rs
  control worker) — CROSS-PLATFORM E2E VERIFIED: container initiator
  streamed 1080p60 H.264 test-pattern to the macOS target (VT decoded,
  '2 parameter set(s)', Metal present, clean teardown). First Linux run
  of tl-net exposed a lo0 hardcode (fixed, portable now). AppImage
  (99 MB, self-contained webkit2gtk, glibc 2.35 baseline) + deb + rpm
  built; launch smoke under Xvfb mapped the 960x720 ThunderLink window.
  Build gotchas: linuxdeploy symlinks break on the virtiofs mount ->
  CARGO_TARGET_DIR=/tmp/tl-target; APPIMAGE_EXTRACT_AND_RUN=1 for
  FUSE-less containers; AppImage needs PNG icons in tauri.conf (icns
  alone fails); container npm install swaps esbuild -> re-run on Mac
  afterwards. macOS app gained first-run auto permission prompts
  (CGRequestScreenCaptureAccess + AXIsProcessTrustedWithOptions; verified
  via Finder-attributed launch — System Settings auto-opens; terminal-
  launched runs inherit the terminal's grants so no prompt fires).

- Audio v1.1 IMPLEMENTED + validated (2026-09-02, agent-built crates
  personally re-verified): tl-audio (platform-neutral: vendored-static
  libopus via opusic-sys — audiopus is an RC with broken CMake; opus
  48k stereo 10ms, FEC+5% loss expectation, DTX, VBR; TLA1 UDP channel;
  JitterBuffer 40ms with PLC/3-miss skip/wraparound; 15/15 tests on mac
  AND linux container) and tl-macos-audio (Core Audio process tap via
  ObjC-runtime CATapDescription + aggregate + IOProc + AudioConverter;
  DefaultOutput AudioUnit playback; SDK-verified layouts; bounded 3s
  teardown after observing 90s IOProc-destroy stalls on silent taps;
  9/9 headless — live tap needs the audio TCC grant which attributes to
  a bundled app, so it stayed in the exercised-but-silent path).
  Engine: AudioSource (Sine/System), audio feeder/sink workers, stats
  event; TargetCaps.accepts_audio + StreamConfig.audio negotiation
  (tl-proto same-commit per convention); CLI: target --audio,
  initiator --audio sine|system [--audio-freq].
  §12.7 loopback validation (30 s, 1800 frames video + sine): 100.0
  packets/s played, 0 concealed, 0 dropped — 100% delivery; drift
  oscillates +30..+63 ms with NO accumulation (that band IS the
  designed latency: 40 ms jitter depth + output buffer; growing desync
  would trend — none; resample correction stays v2 per §12.5). Two
  sink bugs found + fixed during validation: pop-rate (4 frames/tick =
  4x drain late-dropping ~70%) and Empty-break (exited the loop before
  depth filled). First GitHub publish: history purged of an accidental
  106MB core dump + target-local trees (filter-branch; repo 572MB →
  452KB), pushed main to github.com/BaiTian6641/ThunderLink.
  x86_64 AppImage deferred: colima lacks --vz-rosetta on the running
  VM and restarting it would destroy tl-build's toolchain.

- Ladder + Wayland + GUI-audio session (2026-09-02): adaptive bitrate
  (SPEC §8) is now LIVE end-to-end — engine ladder (loss/jitter-driven,
  [25%,150%] band, ×0.70/×0.85 down, ×1.15 up after 6 clean reports,
  5 unit tests) wired through both feedback workers; encoders gained
  runtime set_bitrate (VT AverageBitRate re-set; x264 reconfig+VBV).
  tl-linux-capture gained PortalCapturer (agent-built, verified 23/23
  in container): zbus 5 portal ScreenCast + pipewire-rs 0.8, consent
  dialog = Linux's permission flow, six format conversions to BGRA,
  dmabuf fallback; Wayland initiator capture is COMPLETE (live test
  needs compositor + portal backend — documented, container can't run
  it). GUI: Play-audio toggle (target), Audio radios off/tone/system
  (initiator), DMG rebuilt. Docker Desktop replaced colima (old
  tl-build vanished): container re-provisioned on ubuntu:26.04; the
  22.04 pull needs /Applications/Docker.app/Contents/Resources/bin on
  PATH for the credential helper. x264_encoder_reconfig takes *mut,
  x264_encoder_parameters returns void (bindgen quirk). Agent fixed a
  real latent break: encode.rs test missed the audio StreamConfig
  fields from 5cadfac. Workspace 101/101, clippy 0 both platforms.

- Universal Linux distribution session (2026-09-02): both-arch AppImages
  + a self-extracting universal installer. x86_64 built in a Docker
  Desktop amd64/QEMU container (ubuntu:24.04 — 22.04's libspa headers
  are too old for the libspa crate). Gotchas: Docker Desktop's
  credential helper (docker-credential-desktop) must be on PATH or
  removed from ~/.docker/config.json for pulls; linuxdeploy and
  appimagetool AppImages FAIL under QEMU (Exec format error / subprocess
  crash) — manual packaging needed: extract runtime via `dd` (size from
  offset 0x0208), `mksquashfs`, `cat runtime + squashfs`. The x86_64
  AppImage segfaults under QEMU (WebKitGTK + QEMU limitation) but the
  raw binary runs fine (exit 124 on timeout = window alive); the
  assembly method is verified working on aarch64 (window confirmed via
  xwininfo). A single-file dual-arch AppImage is architecturally
  impossible (the AppImage runtime is a native ELF — it can't execute
  on the wrong arch). Universal distribution =
  ThunderLink_0.1.0_aarch64.AppImage (100 MB) +
  ThunderLink_0.1.0_amd64.AppImage (30 MB) +
  ThunderLink_universal.sh (175 MB self-extracting, uname -m detection,
  TL_EXTRACT_ONLY=1 mode). scripts/build-universal-appimage.sh is the
  reproducible build. Docker Desktop note: containers tl-build (arm64,
  ubuntu:latest) and tl-build-x64 (amd64, ubuntu:24.04) both mount
  /work; npm install in one swaps esbuild arch — re-run on mac after.

- AppImage segfault FIX + amd64-first (2026-09-02, user report): the
  manually-assembled x86_64 AppImage segfaulted on REAL hardware. Root
  cause: the AppImage runtime reads the squashfs length from u64 LE at
  offset 0x0210 — I extracted the runtime from linuxdeploy without
  patching it, so it had LINUXDEPLOY's own squashfs length (4.3 MB)
  baked in instead of ours (30.8 MB). Fix: patch 0x0210 after assembly
  (now in scripts/build-universal-appimage.sh). Note: the linuxdeploy
  aarch64 runtime has zeros at 0x0208/0x0210 (ELF-size mode) — don't
  patch those. Also: Docker Desktop's credential helper broke pulls
  (fix: remove credsStore from ~/.docker/config.json); containers
  freed per user request (colima stopped). Build script restructured:
  amd64 is now PRIMARY (most TB/USB4 systems are x86_64), aarch64 is
  secondary (--arm flag); universal.sh rebuilt with fixed x86_64.

- x86_64 AppImage rebuilt from scratch (2026-09-02, second segfault
  fix): the previous attempt had TWO fatal flaws — (1) wrong runtime
  size (extracted 162608 bytes from linuxdeploy, but the actual ELF is
  193728 bytes; the extra 31120 bytes were linuxdeploy's own squashfs
  garbage baked in between our runtime and our squashfs), and (2)
  incomplete dependencies (linuxdeploy crashed under QEMU partway,
  leaving libx264.so.164 and others undeployed). Rebuild: fresh
  ubuntu:24.04 amd64 container, full toolchain (libclang-dev, cmake,
  libx264-dev, libpipewire-0.3-dev, libspa-0.2-dev), cargo build,
  ldd-based recursive dep deployment (128 libraries, all resolved),
  mksquashfs (105 MB), concatenated with official appimagetool runtime
  (944632 bytes, ELF-size mode). VERIFIED: window 960x720 via xwininfo,
  zero missing ldd deps, ELF end == squashfs start. QEMU can't test
  final AppImage (static-pie exec limitation on M1) — content verified,
  real hardware should work. Containers freed after build.

- Third x86_64 AppImage fix (2026-09-02, user report from Ubuntu 26.04):
  three issues diagnosed via web research: (1) GLIBC_PRIVATE undefined
  symbol — we were bundling libc.so.6/libm.so.6 from Ubuntu 24.04,
  but the host (26.04) has newer glibc; host system libs loaded via
  dlopen found our OLDER bundled libc and failed on private symbols.
  Fix: EXCLUDE all glibc libs from the bundle (libc, libm, libpthread,
  librt, libdl, ld-linux, libresolv, etc.) — the host always provides
  them. (2) GIO module load failure (libgcfsbus.so) — our bundled GLib
  was loading GIO modules from the HOST path which are compiled against
  a different GLib. Fix: GIO_MODULE_DIR="" in AppRun + bundle the build
  system's GIO modules. (3) Blank screen — WebKitGTK hardware
  compositing (DMA-BUF) fails without GPU/in AppImages. Fix:
  WEBKIT_DISABLE_COMPOSITING_MODE=1 + WEBKIT_DISABLE_DMABUF_RENDERER=1
  + GDK_BACKEND=x11 fallback in AppRun. All three fixes in
  scripts/apprun-fixed.sh. Window verified in container (960x720).
  User note: keep single VM at a time to avoid macOS swap.

- Fourth x86_64 AppImage fix (2026-09-02, 'Could not connect to
  localhost'): the previous build used raw cargo build --release which
  did NOT embed the frontend — the app fell back to devUrl
  (localhost:5173). Fix: use npx tauri build --no-bundle which runs the
  full Tauri build pipeline (frontend embedding via generate_context!).
  The localhost:5173 string appearing ONCE in the binary is just the
  config constant — release mode uses frontendDist (embedded assets).
  Window verified 960x720; container freed after build.

## Open risks / TODO
- CGVirtualDisplay is private API; fragile across macOS releases. Isolated
  in `tl-macos-display`; mirror-mode fallback exists (no virtual display).
- macOS TB bridge realistically ~5–6 Gbps: 20 Gbps design headroom helps
  other OS pairs; macOS↔macOS may cap near 4K60/400 Mbps anyway (fine).
- No auth on the wire (owner-accepted): document in README when shipped.
- Real TB hardware validation (iperf3, interface names, MTU) still pending.
- Smoke fps on loopback is kernel-bound (46/60 at 5K, 77% survival): macOS
  loopback UDP drops large bursts under load; retransmit ring recovers what
  fits the 33 ms window. Re-measure on real TB before tuning anything.
- mDNS discovery is wired (was: implemented-but-unused). Remaining
  HANDOFF §7 order: TB-link + TCC passes (need hardware/user), Linux/
  Windows crates (need matching platforms — do not write blind FFI),
  audio SPEC section, adaptive-bitrate policy (hooks exist), README ✓.
- Audio: contract done (SPEC §12); implementation order = sine tone
  source (no TCC) first, then Core Audio tap. Microphone backchannel
  explicitly v2.
