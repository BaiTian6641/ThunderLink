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
