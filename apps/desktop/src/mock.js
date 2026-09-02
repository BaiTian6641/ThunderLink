/**
 * Mock Tauri API layer — browser (design-verification) mode.
 *
 * When `window.__TAURI_INTERNALS__` is absent, `main.js` swaps the real
 * `@tauri-apps/api` invoke/listen pair for `createMockApi()` below. The mock
 * mirrors the Rust command contract in `src-tauri/src/lib.rs` exactly
 * (snake_case payload fields, camelCase invoke arguments, externally tagged
 * `engine://event` payloads) so the UI code has a single code path.
 */

const CHANNEL_STATE = "engine://state";
const CHANNEL_EVENT = "engine://event";

/** Simulated IPC latency. */
const delay = (ms) => new Promise((r) => setTimeout(r, ms));

const MOCK_PERMISSIONS = {
  screen_recording: false,
  accessibility: true,
  platform: "macos",
};

const MOCK_TARGETS = [
  {
    name: "studio-display-thunderbolt",
    addrs: ["192.168.64.3", "fe80::1a:2b:3c:4d%en5"],
    port: 50051,
  },
  {
    name: "mac-studio-lab",
    addrs: ["10.0.0.12"],
    port: 50051,
  },
];

/** Random walk helper for drifting stats. */
function walk(v, min, max, step) {
  const next = v + (Math.random() - 0.5) * 2 * step;
  return Math.min(max, Math.max(min, next));
}

export function createMockApi() {
  const listeners = new Map([
    [CHANNEL_STATE, new Set()],
    [CHANNEL_EVENT, new Set()],
  ]);

  const running = { on: false, role: null };
  let session = null; // { stats, lat, end, walk } timer handles + walk state

  const emit = (channel, payload) => {
    for (const fn of listeners.get(channel) ?? []) {
      fn({ event: channel, id: 0, payload });
    }
  };
  const emitState = () => emit(CHANNEL_STATE, { running: running.on, role: running.role });
  const emitEvent = (payload) => emit(CHANNEL_EVENT, payload);

  function clearTimers() {
    if (!session) return;
    clearInterval(session.stats);
    clearInterval(session.lat);
    clearTimeout(session.end);
    session = null;
  }

  function endSession(reason) {
    if (!running.on) return;
    clearTimers();
    running.on = false;
    running.role = null;
    emitEvent({ Ended: reason });
    emitState();
  }

  function beginSession(role, opts = {}) {
    running.on = true;
    running.role = role;
    const screen = role === "initiator" && opts.source === "screen";
    const negotiated =
      role === "target"
        ? { codec: "Hevc", width: 3008, height: 1692, fps_millihertz: 60000, bitrate_kbps: 90000, chroma: "Yuv420", hdr: false }
        : screen
          ? { codec: "Hevc", width: 2560, height: 1440, fps_millihertz: 60000, bitrate_kbps: 48000, chroma: "Yuv420", hdr: false }
          : { codec: "H264", width: 1280, height: 720, fps_millihertz: 30000, bitrate_kbps: 8000, chroma: "Yuv420", hdr: false };

    const baseFps = Math.round(negotiated.fps_millihertz / 1000);
    const baseBr = negotiated.bitrate_kbps;
    const s = (session = {
      walk: { fps: baseFps - 0.6, br: baseBr, rtt: 2400, dec: 3.1, loss: 0 },
    });
    emitState();
    setTimeout(() => emitEvent({ Negotiated: negotiated }), 300);
    setTimeout(() => emitEvent({ Streaming: null }), 900);
    s.stats = setInterval(() => {
      const w = s.walk;
      w.fps = walk(w.fps, baseFps - 3, baseFps, 0.9);
      w.br = walk(w.br, baseBr * 0.8, baseBr * 1.15, baseBr * 0.02);
      w.rtt = walk(w.rtt, 1800, 3600, 160);
      w.dec = walk(w.dec, 2.4, 4.4, 0.18);
      w.loss = Math.random() < 0.08 ? Math.min(6, w.loss + 1) : 0;
      emitEvent({
        Stats: {
          decoded_fps: Math.round(w.fps),
          presented_fps: Math.max(0, Math.round(w.fps) - 1),
          bitrate_kbps: Math.round(w.br),
          rtt_us: Math.round(w.rtt),
          loss_permille: w.loss,
          decode_ms_x100: Math.round(w.dec * 100),
        },
      });
    }, 1000);
    s.lat = setInterval(() => emitEvent({ LatencyMs: 8 + Math.random() * 7 }), 3000);
    s.end = setTimeout(() => endSession("mock session ended after 12 s"), 12000);
  }

  async function invoke(cmd, args = {}) {
    await delay(60 + Math.random() * 140);
    switch (cmd) {
      case "get_status":
        return { running: running.on, role: running.role };

      case "open_permission_pane":
        return null;
      case "get_permissions":
        return { ...MOCK_PERMISSIONS };

      case "list_targets": {
        const timeoutMs = Math.max(0, (args.timeoutSecs ?? 5) * 1000);
        await delay(Math.min(1100 + Math.random() * 800, timeoutMs));
        return MOCK_TARGETS.map((t) => ({ name: t.name, addrs: [...t.addrs], port: t.port }));
      }

      case "start_target": {
        if (running.on) throw "a session is already running";
        beginSession("target", { windowed: !!args.windowed, no_input: !!args.noInput });
        return null;
      }

      case "start_initiator": {
        if (running.on) throw "a session is already running";
        const opts = args.opts ?? {};
        if (!opts.discover) {
          const raw = String(opts.addr ?? "").trim();
          if (!raw) throw "invalid host: direct address is empty";
          if (!/^(?:\d{1,3}(?:\.\d{1,3}){3}|\[[0-9A-Fa-f:.]+\])(?::\d{1,5})?$/.test(raw)) {
            throw `invalid address: ${raw}`;
          }
        }
        beginSession("initiator", opts);
        return null;
      }

      case "stop_session": {
        if (running.on) endSession("stopped by user");
        return null;
      }

      default:
        throw `mock: unknown command '${cmd}'`;
    }
  }

  async function listen(event, handler) {
    if (!listeners.has(event)) listeners.set(event, new Set());
    listeners.get(event).add(handler);
    return () => {
      listeners.get(event)?.delete(handler);
    };
  }

  return { invoke, listen };
}
