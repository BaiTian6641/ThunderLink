/**
 * ThunderLink desktop frontend.
 *
 * Single-window Carbon (g100) UI over the Tauri command layer in
 * `src-tauri/src/lib.rs`. Two runtimes, one code path:
 *  - real mode (inside Tauri): @tauri-apps/api invoke/listen
 *  - mock mode (plain browser): src/mock.js — same call signatures
 */

// Carbon web components (side-effect registrations, bx-* elements).
import "carbon-web-components/es/components/ui-shell/index.js";
import "carbon-web-components/es/components/tile/index.js";
import "carbon-web-components/es/components/button/index.js";
import "carbon-web-components/es/components/toggle/index.js";
import "carbon-web-components/es/components/radio-button/index.js";
import "carbon-web-components/es/components/input/index.js";
import "carbon-web-components/es/components/notification/index.js";
import "carbon-web-components/es/components/tag/index.js";
import "carbon-web-components/es/components/structured-list/index.js";
import "carbon-web-components/es/components/accordion/index.js";
import "carbon-web-components/es/components/select/index.js";
import "carbon-web-components/es/components/number-input/index.js";
import "carbon-web-components/es/components/inline-loading/index.js";
import "./styles.css";

const isTauri = typeof window.__TAURI_INTERNALS__ !== "undefined";

let invoke;
let listen;

// ---------------------------------------------------------------- state

const state = {
  view: "home", // home | target | initiator
  status: { running: false, role: null },
  perms: { screen_recording: true, accessibility: true, platform: "" },
  // The name this Mac announces as a target (Rust: announce_target()).
  announcedName: isTauri ? "thunderlink-target" : "mock-studio-display",
  tgt: { windowed: false, forwardInput: true },
  ini: {
    conn: "discover",
    addr: "",
    source: "test-pattern",
    codec: "hevc",
    bitrate: "",
    fps: "",
    res: "",
    virtualDisplay: false,
    targets: [],
    selected: null,
    scanning: false,
    scanned: false,
  },
  session: null, // live/ended session, null while configuring
  log: [],
  busy: false,
  error: null,
  errorDismissed: false,
  warnDismissed: false,
};

const freshSession = () => ({
  negotiated: null,
  stats: null,
  latency: null,
  ended: null,
  warn: null,
  streaming: false,
  statCount: 0,
});

// ---------------------------------------------------------------- helpers

const app = document.getElementById("app");
const $ = (sel) => app.querySelector(sel);

const ESC = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" };
const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ESC[c]);
const fmtInt = (n) => Math.round(Number(n) || 0).toLocaleString("en-US");
const intOrNull = (s) => {
  const n = parseInt(String(s ?? "").trim(), 10);
  return Number.isFinite(n) && n > 0 ? n : null;
};
const codecLabel = (c) =>
  ({ hevc: "HEVC", Hevc: "HEVC", h264: "H.264", H264: "H.264" })[c] ?? String(c);

const setText = (sel, v) => {
  const el = $(sel);
  if (el) el.textContent = v;
};

/** Pick the address most likely to reach the target: IPv4, then non-link-local IPv6. */
function bestAddr(t) {
  const addrs = t?.addrs ?? [];
  const v4 = addrs.find((a) => /^\d{1,3}(?:\.\d{1,3}){3}$/.test(a));
  if (v4) return v4;
  const v6 = addrs.find((a) => a.includes(":") && !a.toLowerCase().startsWith("fe80"));
  return v6 ?? addrs[0] ?? "";
}

function validateDirect(raw) {
  const s = String(raw ?? "").trim();
  if (!s) {
    return { ok: false, msg: "Direct address is required — enter an IPv4/IPv6 address, e.g. 192.168.1.42 or [fe80::1]:50051." };
  }
  const m = s.match(/^(?:\[(?<v6>[0-9A-Fa-f:.]+)\]|(?<v4>\d{1,3}(?:\.\d{1,3}){3}))(?::(?<port>\d{1,5}))?$/);
  if (!m) {
    return { ok: false, msg: "Enter an IPv4 or IPv6 address, e.g. 192.168.1.42 or [fe80::1]:50051." };
  }
  if (m.groups.v4 && m.groups.v4.split(".").some((p) => +p > 255)) {
    return { ok: false, msg: "Each IPv4 octet must be between 0 and 255." };
  }
  if (m.groups.port && (+m.groups.port < 1 || +m.groups.port > 65535)) {
    return { ok: false, msg: "Port must be between 1 and 65535." };
  }
  return { ok: true, value: s };
}

function describeNegotiated(n) {
  if (!n) return "";
  return `${codecLabel(n.codec)} ${n.width}×${n.height} @ ${Math.round((n.fps_millihertz ?? 0) / 1000)} fps, ${fmtInt(n.bitrate_kbps)} kbps`;
}

function describeStats(st) {
  if (!st) return "";
  return `stats ${st.decoded_fps} fps · ${fmtInt(st.bitrate_kbps)} kbps · rtt ${((st.rtt_us ?? 0) / 1000).toFixed(1)} ms`;
}

// ---------------------------------------------------------------- templates

function homeHTML() {
  return `
  <section class="tl-view tl-home" aria-label="Choose a role">
    <div class="tl-home-head">
      <h1 class="tl-title" tabindex="-1">Use this Mac with ThunderLink</h1>
      <p class="tl-home-sub">Turn a Mac into an extra display, or push this Mac's desktop onto one — screen streaming between Macs.</p>
    </div>
    <div class="tl-tiles">
      <bx-clickable-tile class="tl-tile" href="#target" data-action="pick-role" data-role="target">
        <span class="tl-tile-kicker">Target</span>
        <h2 class="tl-tile-title">Use this Mac as a display</h2>
        <p class="tl-tile-body">Announce this Mac on the local network. Another Mac connects and streams its desktop here.</p>
        <p class="tl-tile-foot">Displaying needs no permissions. Forwarding this Mac's input needs Accessibility.</p>
      </bx-clickable-tile>
      <bx-clickable-tile class="tl-tile" href="#initiator" data-action="pick-role" data-role="initiator">
        <span class="tl-tile-kicker">Initiator</span>
        <h2 class="tl-tile-title">Extend another Mac's desktop</h2>
        <p class="tl-tile-body">Find a ThunderLink display and stream this Mac's screen — or a test pattern — onto it.</p>
        <p class="tl-tile-foot">Screen capture needs Screen Recording. The test pattern needs no permissions.</p>
      </bx-clickable-tile>
    </div>
    <section class="tl-perms" aria-label="Permissions">
      <h2 class="tl-perms-title">Permissions</h2>
      <bx-tile class="tl-perms-tile">
        <div class="tl-perm-row">
          <span>Screen Recording</span>
          <span id="perm-sr"></span>
        </div>
        <div class="tl-perm-row">
          <span>Accessibility</span>
          <span id="perm-ax"></span>
        </div>
        <p class="tl-perm-note">Permissions are granted in System Settings &gt; Privacy &amp; Security. Only screen capture and input forwarding need them.</p>
      </bx-tile>
    </section>
  </section>`;
}

function permBannersHTML() {
  const p = state.perms;
  const sr = p.screen_recording
    ? ""
    : `<bx-inline-notification kind="warning" title="Screen Recording required" subtitle="Screen Recording permission required for screen capture — System Settings &gt; Privacy &amp; Security" hide-close-button></bx-inline-notification>`;
  const ax = p.accessibility
    ? ""
    : `<bx-inline-notification kind="warning" title="Accessibility required" subtitle="Accessibility permission required for input control" hide-close-button></bx-inline-notification>`;
  return sr + ax;
}

function targetConfigHTML() {
  const ax = state.perms.accessibility;
  return `
  <div class="tl-config">
    <fieldset class="tl-section">
      <legend class="tl-section-title">Announce</legend>
      <p class="tl-helper tl-announce">This Mac will be announced on the local network as <span class="tl-mono">${esc(state.announcedName)}</span>; initiators can discover it automatically over mDNS.</p>
    </fieldset>
    <fieldset class="tl-section">
      <legend class="tl-section-title">Presentation</legend>
      <div class="tl-field">
        <bx-toggle id="tgt-windowed" label-text="Windowed presentation" checked-text="On" unchecked-text="Off"></bx-toggle>
        <p class="tl-helper">Present the incoming stream in a window instead of taking over the whole screen.</p>
      </div>
      <div class="tl-field">
        <bx-toggle id="tgt-forward" label-text="Forward input" checked-text="On" unchecked-text="Off" ${ax ? "checked" : "disabled"}></bx-toggle>
        <p class="tl-helper">${ax ? "Send this Mac's keyboard and mouse to the initiator while it streams." : "Forwarding input requires the Accessibility permission."}</p>
      </div>
    </fieldset>
    <div class="tl-actions">
      <bx-btn kind="primary" id="start-btn" data-action="start-target">Start</bx-btn>
      <p class="tl-action-help" id="start-help">Starts announcing and waits for an initiator to connect.</p>
    </div>
  </div>`;
}

function initiatorConfigHTML() {
  const ini = state.ini;
  const vdEnabled = ini.source === "screen";
  return `
  <div class="tl-config">
    <fieldset class="tl-section">
      <legend class="tl-section-title">Target</legend>
      <bx-radio-button-group id="conn-group" name="conn" orientation="vertical" value="${esc(ini.conn)}">
        <bx-radio-button value="discover" label-text="Auto-discover (mDNS)" ${ini.conn === "discover" ? "checked" : ""}></bx-radio-button>
        <bx-radio-button value="direct" label-text="Direct address" ${ini.conn === "direct" ? "checked" : ""}></bx-radio-button>
      </bx-radio-button-group>
      <div class="tl-direct-wrap ${ini.conn === "direct" ? "" : "tl-hidden"}" id="direct-wrap">
        <bx-input id="direct-input" label-text="Host or IP address" placeholder="192.168.1.42 or [fe80::1]:50051"
          value="${esc(ini.addr)}" helper-text="IPv4 or bracketed IPv6; append :port to override the default."></bx-input>
      </div>
      <div class="tl-scan">
        <div class="tl-scan-head">
          <bx-btn kind="secondary" size="sm" id="scan-btn" data-action="scan">Scan for targets</bx-btn>
          <bx-inline-loading id="scan-loading" status="active" class="tl-hidden">Scanning…</bx-inline-loading>
        </div>
        <div id="scan-results"></div>
      </div>
    </fieldset>
    <fieldset class="tl-section">
      <legend class="tl-section-title">Source</legend>
      <bx-radio-button-group id="source-group" name="source" value="${esc(ini.source)}">
        <bx-radio-button value="test-pattern" label-text="Test pattern" ${ini.source === "test-pattern" ? "checked" : ""}></bx-radio-button>
        <bx-radio-button value="screen" label-text="Screen" ${ini.source === "screen" ? "checked" : ""}></bx-radio-button>
      </bx-radio-button-group>
      <div class="tl-field tl-vd-wrap">
        <bx-toggle id="vd-toggle" label-text="Create virtual display (extended desktop)" checked-text="On" unchecked-text="Off"
          ${ini.virtualDisplay ? "checked" : ""} ${vdEnabled ? "" : "disabled"}></bx-toggle>
        <p class="tl-helper">Adds a virtual display so the stream becomes extra screen space instead of a mirror. Only applies to the Screen source.</p>
      </div>
      <p class="tl-helper">The test pattern needs no permissions; capturing the screen requires Screen Recording.</p>
    </fieldset>
    <bx-accordion class="tl-section tl-advanced">
      <bx-accordion-item title-text="Advanced settings">
        <div class="tl-grid-2">
          <div class="tl-field">
            <bx-select id="codec-select" label-text="Codec" value="${esc(ini.codec)}">
              <bx-select-item value="hevc" label="HEVC (default)" ${ini.codec === "hevc" ? "selected" : ""}></bx-select-item>
              <bx-select-item value="h264" label="H.264" ${ini.codec === "h264" ? "selected" : ""}></bx-select-item>
            </bx-select>
          </div>
          <div class="tl-field">
            <bx-number-input id="bitrate-input" label-text="Bitrate (kbps)" placeholder="Auto" min="500" step="500" value="${esc(ini.bitrate)}"></bx-number-input>
          </div>
          <div class="tl-field">
            <bx-number-input id="fps-input" label-text="Frame rate (fps)" placeholder="Auto" min="1" max="240" value="${esc(ini.fps)}"></bx-number-input>
          </div>
          <div class="tl-field">
            <bx-input id="res-input" label-text="Resolution" placeholder="2560x1440" value="${esc(ini.res)}"></bx-input>
          </div>
        </div>
        <p class="tl-helper">Leave values empty to let the engine negotiate defaults from the target's capabilities.</p>
      </bx-accordion-item>
    </bx-accordion>
    <div class="tl-actions">
      <bx-btn kind="primary" id="start-btn" data-action="start-initiator">Start session</bx-btn>
      <p class="tl-action-help" id="start-help"></p>
    </div>
  </div>`;
}

function liveHTML() {
  const s = state.session;
  const ended = s && s.ended != null;
  return `
  <div class="tl-live">
    <div class="tl-live-head">
      <bx-inline-loading id="state-line" status="active">Waiting…</bx-inline-loading>
      ${ended ? "" : `<bx-btn kind="danger" data-action="stop">Stop</bx-btn>`}
    </div>
    ${ended ? endedHTML() : `
      <div class="tl-tags" id="neg-tags"></div>
      <bx-structured-list>
        <bx-structured-list-head>
          <bx-structured-list-header-row>
            <bx-structured-list-header-cell>Decoded FPS</bx-structured-list-header-cell>
            <bx-structured-list-header-cell>Bitrate</bx-structured-list-header-cell>
            <bx-structured-list-header-cell>RTT</bx-structured-list-header-cell>
            <bx-structured-list-header-cell>Latency</bx-structured-list-header-cell>
          </bx-structured-list-header-row>
        </bx-structured-list-head>
        <bx-structured-list-body>
          <bx-structured-list-row>
            <bx-structured-list-cell><span class="tl-stat-val" id="v-fps">—</span></bx-structured-list-cell>
            <bx-structured-list-cell><span class="tl-stat-val" id="v-br">—</span></bx-structured-list-cell>
            <bx-structured-list-cell><span class="tl-stat-val" id="v-rtt">—</span></bx-structured-list-cell>
            <bx-structured-list-cell><span class="tl-stat-val" id="v-lat">—</span></bx-structured-list-cell>
          </bx-structured-list-row>
        </bx-structured-list-body>
      </bx-structured-list>`}
  </div>`;
}

function endedHTML() {
  const reason = state.session.ended || "";
  const isError = /^error/i.test(reason);
  return `
  <div class="tl-ended">
    <bx-inline-notification kind="${isError ? "error" : "info"}" title="Session ended"
      subtitle="${esc(reason || "The session is no longer running.")}" hide-close-button></bx-inline-notification>
    <div class="tl-actions">
      <bx-btn kind="primary" data-action="start-again">Start again</bx-btn>
      <bx-btn kind="ghost" data-action="back">Back to roles</bx-btn>
    </div>
  </div>`;
}

function logHTML() {
  return `
  <section class="tl-log-panel" aria-label="Activity log">
    <h2 class="tl-log-title">Activity</h2>
    <ol class="tl-log" id="log"></ol>
  </section>`;
}

function targetHTML() {
  const live = !!state.session;
  return `
  <section class="tl-view" aria-label="This Mac as a display">
    <div class="tl-titlebar">
      ${live ? "" : `<bx-btn kind="ghost" size="sm" data-action="back">Back</bx-btn>`}
      <h1 class="tl-title" tabindex="-1">This Mac as a display</h1>
      <bx-tag type="gray">Target</bx-tag>
    </div>
    <div class="tl-notes">
      ${permBannersHTML()}
      <div id="role-error"></div>
      <div id="warn-note"></div>
    </div>
    ${live ? liveHTML() : targetConfigHTML()}
    ${logHTML()}
  </section>`;
}

function initiatorHTML() {
  const live = !!state.session;
  return `
  <section class="tl-view" aria-label="Extend another Mac's desktop">
    <div class="tl-titlebar">
      ${live ? "" : `<bx-btn kind="ghost" size="sm" data-action="back">Back</bx-btn>`}
      <h1 class="tl-title" tabindex="-1">Extend another Mac's desktop</h1>
      <bx-tag type="gray">Initiator</bx-tag>
    </div>
    <div class="tl-notes">
      ${permBannersHTML()}
      <div id="role-error"></div>
      <div id="warn-note"></div>
    </div>
    ${live ? liveHTML() : initiatorConfigHTML()}
    ${logHTML()}
  </section>`;
}

function viewHTML() {
  if (state.view === "target") return targetHTML();
  if (state.view === "initiator") return initiatorHTML();
  return homeHTML();
}

// ---------------------------------------------------------------- log

function appendLog(text) {
  state.log.push({ t: new Date(), text });
  if (state.log.length > 30) state.log.splice(0, state.log.length - 30);
  renderLog();
}

function renderLog() {
  const ol = $("#log");
  if (!ol) return;
  if (!state.log.length) {
    ol.innerHTML = `<li class="tl-log-empty">No activity yet.</li>`;
    return;
  }
  const stick = ol.scrollHeight - ol.scrollTop - ol.clientHeight < 48;
  ol.innerHTML = state.log
    .map(
      (e) =>
        `<li><span class="tl-log-t">${esc(e.t.toLocaleTimeString("en-GB", { hour12: false }))}</span><span class="tl-log-x">${esc(e.text)}</span></li>`
    )
    .join("");
  if (stick) ol.scrollTop = ol.scrollHeight;
}

// ---------------------------------------------------------------- sync

function syncPermSummary() {
  const sr = $("#perm-sr");
  if (!sr) return;
  sr.innerHTML = state.perms.screen_recording
    ? `<bx-tag type="green">Granted</bx-tag>`
    : `<bx-tag type="red">Missing</bx-tag>`;
  const ax = $("#perm-ax");
  if (ax) {
    ax.innerHTML = state.perms.accessibility
      ? `<bx-tag type="green">Granted</bx-tag>`
      : `<bx-tag type="red">Missing</bx-tag>`;
  }
}

function syncStartGate() {
  const btn = $("#start-btn");
  if (!btn) return;
  let disabled = state.busy || !!state.session;
  let msg = "";
  if (state.view === "initiator" && state.ini.source === "screen" && !state.perms.screen_recording) {
    disabled = true;
    msg = "Start is disabled: Screen Recording permission required for screen capture — System Settings > Privacy & Security. The test-pattern source needs no permissions.";
  }
  btn.disabled = disabled;
  const help = $("#start-help");
  if (help) help.textContent = msg;
}

function syncStateLine() {
  const el = $("#state-line");
  if (!el || !state.session) return;
  const s = state.session;
  let status = "active";
  let text;
  if (s.ended != null) {
    status = /^error/i.test(s.ended) ? "error" : "finished";
    text = "Session ended";
  } else if (s.streaming) {
    text = "Streaming";
  } else {
    text = state.view === "target" ? "Waiting for initiator…" : "Connecting…";
  }
  el.status = status;
  el.textContent = text;
}

function syncStats() {
  const s = state.session;
  if (!s || s.ended != null) return;
  const st = s.stats;
  setText("#v-fps", st ? `${st.decoded_fps}` : "—");
  setText("#v-br", st ? `${fmtInt(st.bitrate_kbps)} kbps` : "—");
  setText("#v-rtt", st ? `${fmtInt(st.rtt_us)} µs` : "—");
  setText("#v-lat", s.latency != null ? `${Number(s.latency).toFixed(1)} ms` : "—");
}

function syncTags() {
  const el = $("#neg-tags");
  if (!el) return;
  const n = state.session?.negotiated;
  el.innerHTML = n
    ? `
      <bx-tag type="blue">${esc(codecLabel(n.codec))}</bx-tag>
      <bx-tag type="gray">${n.width}×${n.height} @ ${Math.round((n.fps_millihertz ?? 0) / 1000)} fps</bx-tag>
      <bx-tag type="gray">${fmtInt(n.bitrate_kbps)} kbps</bx-tag>`
    : "";
}

function syncWarn() {
  const el = $("#warn-note");
  if (!el) return;
  const s = state.session;
  const show = s && s.warn && s.ended == null && !state.warnDismissed;
  el.innerHTML = show
    ? `<bx-inline-notification kind="warning" title="Warning" subtitle="${esc(s.warn)}"></bx-inline-notification>`
    : "";
}

function syncError() {
  const el = $("#role-error");
  if (!el) return;
  el.innerHTML =
    state.error && !state.errorDismissed
      ? `<bx-inline-notification kind="error" title="Something went wrong" subtitle="${esc(state.error)}"></bx-inline-notification>`
      : "";
}

function showRoleError(msg) {
  state.error = msg;
  state.errorDismissed = false;
  appendLog(`error — ${msg}`);
  syncError();
}

function syncScan() {
  const btn = $("#scan-btn");
  if (!btn) return;
  btn.disabled = state.ini.scanning || !!state.session;
  const l = $("#scan-loading");
  if (l) l.classList.toggle("tl-hidden", !state.ini.scanning);
}

function renderScanResults() {
  const el = $("#scan-results");
  if (!el) return;
  const ini = state.ini;
  if (!ini.scanned) {
    el.innerHTML = `<p class="tl-scan-empty">Scan the local network to list ThunderLink targets visible over mDNS.</p>`;
    return;
  }
  if (!ini.targets.length) {
    el.innerHTML = `<p class="tl-scan-empty">No targets found. Make sure the other Mac is running as a display, or connect by address.</p>`;
    return;
  }
  el.innerHTML = `<ul class="tl-scan-results">${ini.targets
    .map(
      (t, i) => `
      <li class="${ini.selected === i ? "tl-selected" : ""}">
        <button type="button" class="tl-scan-row" data-action="select-target" data-idx="${i}"
          aria-label="Use ${esc(t.name)} at ${esc(bestAddr(t))}">
          <span class="tl-scan-name">${esc(t.name)}</span>
          <span class="tl-scan-addr">${esc(bestAddr(t))}:${t.port}</span>
        </button>
      </li>`
    )
    .join("")}</ul>`;
}

function refresh() {
  syncPermSummary();
  syncStartGate();
  syncStateLine();
  syncStats();
  syncTags();
  syncWarn();
  syncError();
  syncScan();
}

// ---------------------------------------------------------------- wiring

function setRadioGroup(grp, value) {
  if (!grp) return;
  grp.value = value;
  grp.querySelectorAll("bx-radio-button").forEach((r) => {
    r.checked = r.value === value;
  });
}

function wireView() {
  if (state.view === "target" && !state.session) {
    const w = $("#tgt-windowed");
    if (w) w.addEventListener("bx-toggle-changed", (e) => { state.tgt.windowed = e.target.checked; });
    const f = $("#tgt-forward");
    if (f) f.addEventListener("bx-toggle-changed", (e) => { state.tgt.forwardInput = e.target.checked; });
  } else if (state.view === "initiator" && !state.session) {
    const conn = $("#conn-group");
    if (conn) {
      conn.addEventListener("bx-radio-button-group-changed", (e) => {
        state.ini.conn = e.detail.value;
        const wrap = $("#direct-wrap");
        if (wrap) wrap.classList.toggle("tl-hidden", state.ini.conn !== "direct");
        if (state.ini.conn === "direct") $("#direct-input")?.focus();
      });
    }
    const direct = $("#direct-input");
    if (direct) {
      direct.addEventListener("input", (e) => {
        state.ini.addr = e.target.value;
        e.target.invalid = false;
      });
      direct.addEventListener("keydown", (e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          startInitiator();
        }
      });
    }
    const source = $("#source-group");
    if (source) {
      source.addEventListener("bx-radio-button-group-changed", (e) => {
        state.ini.source = e.detail.value;
        const vd = $("#vd-toggle");
        if (vd) vd.disabled = state.ini.source !== "screen";
        syncStartGate();
      });
    }
    const vd = $("#vd-toggle");
    if (vd) vd.addEventListener("bx-toggle-changed", (e) => { state.ini.virtualDisplay = e.target.checked; });
    const codec = $("#codec-select");
    if (codec) codec.addEventListener("bx-select-selected", (e) => { state.ini.codec = e.detail.value; });
    const bitrate = $("#bitrate-input");
    if (bitrate) bitrate.addEventListener("input", (e) => { state.ini.bitrate = e.target.value; });
    const fps = $("#fps-input");
    if (fps) fps.addEventListener("input", (e) => { state.ini.fps = e.target.value; });
    const res = $("#res-input");
    if (res) {
      res.addEventListener("input", (e) => {
        state.ini.res = e.target.value;
        e.target.invalid = false;
      });
    }
    renderScanResults();
  }
  renderLog();
}

function renderView() {
  app.innerHTML = viewHTML();
  wireView();
  refresh();
  document.body.classList.add("tl-ready");
  app.querySelector(".tl-title")?.focus();
}

// ---------------------------------------------------------------- actions

async function startTarget() {
  state.busy = true;
  syncStartGate();
  appendLog("starting target role…");
  try {
    await invoke("start_target", { windowed: state.tgt.windowed, noInput: !state.tgt.forwardInput });
    beginLive();
  } catch (e) {
    showRoleError(String(e));
  } finally {
    state.busy = false;
    syncStartGate();
  }
}

async function startInitiator() {
  if (state.ini.conn === "direct") {
    const input = $("#direct-input");
    const v = validateDirect(input?.value);
    if (!v.ok) {
      input.invalid = true;
      input.validityMessage = v.msg;
      input.focus();
      return;
    }
    state.ini.addr = v.value;
  }
  const res = state.ini.res.trim();
  if (res && !/^\d{3,5}[x×]\d{3,5}$/.test(res)) {
    const el = $("#res-input");
    el.invalid = true;
    el.validityMessage = "Use WxH, e.g. 2560x1440.";
    el.focus();
    return;
  }
  const opts = {
    addr: state.ini.conn === "direct" ? state.ini.addr : null,
    discover: state.ini.conn === "discover",
    source: state.ini.source,
    codec: state.ini.codec,
    bitrateKbps: intOrNull(state.ini.bitrate),
    fps: intOrNull(state.ini.fps),
    res: res || null,
    virtualDisplay: state.ini.source === "screen" && state.ini.virtualDisplay,
  };
  state.busy = true;
  syncStartGate();
  appendLog("starting initiator role…");
  try {
    await invoke("start_initiator", { opts });
    beginLive();
  } catch (e) {
    showRoleError(String(e));
  } finally {
    state.busy = false;
    syncStartGate();
  }
}

function beginLive() {
  state.error = null;
  state.errorDismissed = false;
  state.warnDismissed = false;
  state.session = freshSession();
  renderView();
}

async function stopSession() {
  const btn = app.querySelector('[data-action="stop"]');
  if (btn) btn.disabled = true;
  appendLog("stopping session…");
  try {
    await invoke("stop_session");
  } catch (e) {
    showRoleError(String(e));
    if (btn) btn.disabled = false;
  }
}

async function runScan() {
  if (state.ini.scanning) return;
  state.ini.scanning = true;
  state.ini.selected = null;
  syncScan();
  try {
    const targets = await invoke("list_targets", { timeoutSecs: 5 });
    state.ini.targets = Array.isArray(targets) ? targets : [];
    state.ini.scanned = true;
    appendLog(`scan found ${state.ini.targets.length} target${state.ini.targets.length === 1 ? "" : "s"}`);
  } catch (e) {
    state.ini.targets = [];
    state.ini.scanned = true;
    showRoleError(String(e));
  } finally {
    state.ini.scanning = false;
    renderScanResults();
    syncScan();
  }
}

function selectTarget(i) {
  const t = state.ini.targets[i];
  if (!t) return;
  if (state.ini.selected === i) {
    state.ini.selected = null; // clicking again just deselects
  } else {
    state.ini.selected = i;
    state.ini.conn = "direct";
    state.ini.addr = bestAddr(t);
    const input = $("#direct-input");
    if (input) {
      input.value = state.ini.addr;
      input.invalid = false;
    }
    setRadioGroup($("#conn-group"), "direct");
    $("#direct-wrap")?.classList.remove("tl-hidden");
  }
  renderScanResults();
}

const actions = {
  "pick-role": (el, ev) => {
    ev.preventDefault();
    state.view = el.dataset.role === "target" ? "target" : "initiator";
    renderView();
  },
  back: (el, ev) => {
    ev.preventDefault();
    state.session = null;
    state.error = null;
    state.errorDismissed = false;
    state.warnDismissed = false;
    state.view = "home";
    renderView();
  },
  scan: () => runScan(),
  "select-target": (el) => selectTarget(Number(el.dataset.idx)),
  "start-target": () => startTarget(),
  "start-initiator": () => startInitiator(),
  stop: () => stopSession(),
  "start-again": (el, ev) => {
    ev.preventDefault();
    state.session = null;
    state.warnDismissed = false;
    renderView();
  },
};

app.addEventListener("click", (ev) => {
  const el = ev.target.closest?.("[data-action]");
  if (!el) return;
  const fn = actions[el.dataset.action];
  if (fn) {
    ev.preventDefault();
    fn(el, ev);
  }
});

app.addEventListener("bx-notification-closed", (ev) => {
  const note = ev.target;
  if (!note?.closest) return;
  if (note.closest("#warn-note")) state.warnDismissed = true;
  else if (note.closest("#role-error")) state.errorDismissed = true;
  syncWarn();
  syncError();
});

// ---------------------------------------------------------------- engine events

function handleEngineEvent(payload) {
  const entry = Object.entries(payload ?? {})[0];
  if (!entry) return;
  const [tag, val] = entry;
  const s = state.session;
  switch (tag) {
    case "Negotiated":
      if (s) s.negotiated = val;
      appendLog(`negotiated ${describeNegotiated(val)}`);
      break;
    case "Streaming":
      if (s) s.streaming = true;
      appendLog("streaming — video path open");
      break;
    case "Stats":
      if (s) {
        s.stats = val;
        s.statCount += 1;
      }
      if (((s?.statCount ?? 0) % 5) === 1) appendLog(describeStats(val));
      break;
    case "LatencyMs":
      if (s) s.latency = val;
      appendLog(`latency ${Number(val).toFixed(1)} ms`);
      break;
    case "Ended":
      if (s) {
        s.ended = val ?? "";
        s.streaming = false;
      }
      appendLog(`session ended — ${val || "no reason given"}`);
      renderView();
      return;
    case "Warn":
      if (s) s.warn = val;
      state.warnDismissed = false;
      appendLog(`warning — ${val}`);
      break;
    default:
      appendLog(JSON.stringify(payload));
      break;
  }
  refresh();
}

function onState(payload) {
  state.status = payload ?? { running: false, role: null };
  // Engine stopped without an Ended event (e.g. watchdog cleanup).
  if (!state.status.running && state.session && state.session.ended == null) {
    state.session.ended = "";
    appendLog("session ended");
    renderView();
    return;
  }
  refresh();
}

// ---------------------------------------------------------------- boot

async function boot() {
  if (isTauri) {
    const core = await import("@tauri-apps/api/core");
    const event = await import("@tauri-apps/api/event");
    invoke = core.invoke;
    listen = event.listen;
  } else {
    const { createMockApi } = await import("./mock.js");
    ({ invoke, listen } = createMockApi());
  }

  await listen("engine://state", (ev) => onState(ev.payload));
  await listen("engine://event", (ev) => handleEngineEvent(ev.payload));

  const [st, perms] = await Promise.all([
    invoke("get_status").catch(() => null),
    invoke("get_permissions").catch(() => null),
  ]);
  state.status = st ?? { running: false, role: null };
  state.perms = perms ?? { screen_recording: true, accessibility: true, platform: "" };
  if (state.status.running && state.status.role) {
    // App reloaded mid-session: jump straight into that role's live view.
    state.view = state.status.role === "target" ? "target" : "initiator";
    state.session = freshSession();
  }
  renderView();
}

boot().catch((e) => {
  document.body.classList.add("tl-ready");
  app.innerHTML = `
  <section class="tl-view">
    <bx-inline-notification kind="error" title="Failed to start" subtitle="${esc(String(e))}" hide-close-button></bx-inline-notification>
  </section>`;
});
