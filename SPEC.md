# ThunderLink Protocol & Architecture SPEC (v1)

Source of truth for wire formats and cross-crate contracts. Type definitions
live in `crates/tl-proto` (authoritative); this document defines behavior.

## 1. Constraints (from project owner)

- Point-to-point only: one initiator, one target, one cable.
- No encryption/authentication in v1. Do not add TLS, certs, or pairing.
- Design for 20 Gbps-class Thunderbolt 3/4/USB4 links; must also work on
  ~10 Gbps bridges (degraded bitrate, same protocol).
- Optimize for **latency AND quality**: stream at the target panel's **native
  (original) resolution** — no scaling. Bitrate is cheap; latency is not.
- No `async` anywhere. Std threads + channels. Blocking sockets with
  timeouts/polling. Keeps the pipeline deterministic.

## 2. Roles

- **Initiator** — desktop source. Creates/uses a display, captures, encodes,
  sends video; receives and injects input events.
- **Target** — acts as monitor. Sends caps (panel EDID/native res, decoders),
  receives/decodes/presents video fullscreen; captures local HID input and
  forwards to initiator.

## 3. Channels and ports

| Channel | Transport | Port | Direction |
|---|---|---|---|
| Control | TCP | 47776 (`tl_proto::CONTROL_PORT`) | initiator → target (connect); messages both ways |
| Video | UDP | 47777 (`VIDEO_PORT`) | initiator → target |
| Feedback | UDP | 47778 (`FEEDBACK_PORT`) | target → initiator |
| Input | UDP | 47779 (`INPUT_PORT`) | target → initiator |

Discovery: mDNS service `_thunderlink._tcp.local.` (`MDNS_SERVICE_TYPE`),
TXT record `role=initiator|target`. Announced on Thunderbolt interfaces.

## 4. Control channel (TCP)

- Framing: u32 little-endian length prefix + bincode(serde) of `tl_proto::Msg`.
- Reject frames larger than `MAX_CONTROL_MESSAGE` (1 MiB).
- Read with a 5 s timeout during handshake; after `Start`, 1 s tick for
  heartbeats. EOF/timeout → session teardown.

### Handshake sequence
1. Initiator connects → sends `Hello{version, role:Initiator, name}`.
2. Target replies `Hello{..., role:Target}` then `Caps(TargetCaps)`
   (panel native res/scale/refresh, EDID if readable, decoder list).
3. Initiator picks a `StreamConfig` (see §8 policy) and sends
   `Config(cfg)`. Target validates with `TargetCaps::supports`; on success
   replies `Ack{ok:true}`, else `Ack{ok:false, message}`.
4. Initiator sends `Start`. Target starts decoder+presenter, replies
   `Ack{ok:true}` when the first frame can be accepted.
5. Steady state: `Heartbeat` every 1 s both directions; target sends
   `Stats(StatsReport)` every 1 s; initiator may send `Led(LedState)`.
6. Either side sends `Stop`/`Bye` or closes the socket → teardown:
   initiator destroys its virtual display; target exits fullscreen.

## 5. Video channel (UDP)

- One UDP datagram = fixed 24-byte `VideoHeader` (`tl_proto::packet`) +
  fragment payload. `DEFAULT_DATAGRAM_PAYLOAD` = 1400 bytes total budget
  (header included); jumbo mode may raise to 8900 — negotiated later, v1 = 1400.
- Fragmentation: `EncodedUnit.data` split into `frag_count` chunks of at most
  (payload − 24) bytes; `frag_index` 0-based. `flags`: bit0 keyframe,
  bit1 config (parameter sets present). `frame_seq` increments per frame.
- Bitstream: Annex B. HEVC/H.264 parameter sets (VPS/SPS/PPS / SPS/PPS)
  MUST be prepended to every IDR/keyframe unit.
- Sender keeps a retransmit ring (default 16 MiB) of recent datagrams.
  On `Feedback::Nack`, retransmit listed fragment ranges if still in ring.
- Receiver reassembles per `frame_seq`. A frame is dropped (and NACKed
  once) when: a newer `frame_seq` arrives while incomplete, or 33 ms pass
  since its first fragment. After 3 dropped frames within 500 ms → send
  `IdrRequest`. Every 500 ms receiver sends `Feedback::Report`
  (received/lost counters, RTT estimate, jitter).
- Latest-wins latency policy: the receiver never queues more than one
  complete frame waiting for decode; stale frames are dropped, never
  delayed. Encoders: no B-frames, low-delay rate control.

## 6. Feedback channel (UDP, target → initiator)

bincode of `tl_proto::Feedback`: `Nack{frame_seq, ranges}`,
`IdrRequest`, `Report{received_frames, lost_packets, rtt_us, jitter_us}`.
Initiator applies: NACK → retransmit; IdrRequest → `Encoder::request_idr`;
Report → update stats + adaptive bitrate ladder (§8) — v1 may keep bitrate
fixed; ladder hooks must exist.

## 7. Input channel (UDP, target → initiator)

bincode of `tl_proto::InputBatch{seq, events}`. Fire-and-forget; batches
sent at up to 500 Hz, coalescing moves. Coordinates normalized
0..=`COORD_MAX` over the streamed display rectangle; initiator denormalizes
into its desktop coordinate space via `Mapping`. `InputEvent::Leave`
releases captured state (all buttons up).

## 8. Quality/bitrate policy (20 Gbps links)

Default: HEVC (`Codec::Hevc`), native panel resolution, panel refresh,
`Chroma::Yuv420`, 10-bit when `hdr` (v1 HDR optional flag, SDR pipeline).
Bitrate ladder (kbps) by pixels/sec: floor `pixels*fps*0.10` bits ≈
"visually lossless HEVC" territory; v1 table:

| Stream | Bitrate |
|---|---|
| 1080p60 | 120 Mbps |
| 1440p60 | 200 Mbps |
| 4K60 | 400 Mbps |
| 5K60 | 550 Mbps |

Cap: 800 Mbps (bridges at 10 Gbps still fit). Fallback codec H.264 (same
table ×1.6, capped 800 Mbps). These are starting points — adaptive ladder
may adjust within [25%, 150%].

## 9. Crate boundaries (agent contracts)

Each crate's `src/lib.rs` contains its pinned public API. Rules for all:

- Only edit inside your own `crates/<name>/` directory. Do NOT touch the
  workspace `Cargo.toml`, `tl-proto`, or any other crate.
- Add dependencies only to your own `Cargo.toml`. Prefer the `objc2`
  ecosystem (0.6+) consistently for Apple frameworks; `anyhow::Result` for
  fallible APIs; `log` for logging (no `println!` in libraries).
- Validate with:
  `CARGO_TARGET_DIR=target-local cargo check -p <crate>` and
  `CARGO_TARGET_DIR=target-local cargo test -p <crate>` (per-crate target
  dir avoids lock contention with parallel agents). Never run
  workspace-wide builds.
- Tests must pass WITHOUT TCC permissions (Screen Recording / Accessibility
  may be denied), WITHOUT a Thunderbolt peer, and headless. Gate
  permission/hardware-dependent tests behind env vars (`TL_E2E=1`).
- Every `unsafe` must have a `// SAFETY:` comment. FFI error paths must be
  checked (null returns, OSStatus != 0).
- On macOS, AppKit objects are main-thread-only; honor the documented
  thread contract in each API.

## 10. macOS crate APIs (summary; see stubs)

- `tl-macos-capture`: ScreenCaptureKit capture → `CapturedFrame` (zero-copy
  CVPixelBuffer wrapper); VideoToolbox `Encoder` (HEVC/H.264, Annex B,
  param sets on IDR, real-time session preset); `testsrc::TestPattern`
  synthetic frames requiring NO permissions (used by the smoke test).
- `tl-macos-render`: VideoToolbox `Decoder` → `DecodedFrame`; AppKit+Metal
  `Presenter` (windowed/borderless-fullscreen, vsync, latest-wins submit,
  CVMetalTextureCache zero-copy); `decoder_caps()`.
- `tl-macos-input`: `EventTap` (CGEventTap → normalized `InputEvent`s,
  errors mention "permission" on TCC denial); `Injector` (CGEventPost of
  `InputEvent`s through `Mapping`, USB HID usage → CGKeyCode table).
- `tl-macos-display`: `VirtualDisplay` via private `CGVirtualDisplay`
  (runtime class lookup — no link-time private symbols; recreate-on-drop
  semantics: Drop destroys the display); `panel::main_panel()` →
  `PanelInfo` incl. EDID from IOKit when available.

## 11. Smoke-test definition of done (this phase)

`thunderlink target` on 127.0.0.1 + `thunderlink initiator --connect
127.0.0.1 --source test-pattern` on the same Mac: target window presents
the animated pattern at 60 fps with end-to-end latency logged < 50 ms
(localhost), no TCC prompts, all workspace tests green.
