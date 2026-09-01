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
## Open risks / TODO
- CGVirtualDisplay is private API; fragile across macOS releases. Isolated
  in `tl-macos-display`; mirror-mode fallback exists (no virtual display).
- macOS TB bridge realistically ~5–6 Gbps: 20 Gbps design headroom helps
  other OS pairs; macOS↔macOS may cap near 4K60/400 Mbps anyway (fine).
- No auth on the wire (owner-accepted): document in README when shipped.
- Real TB hardware validation (iperf3, interface names, MTU) still pending.
