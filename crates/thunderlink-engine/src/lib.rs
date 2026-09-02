//! ThunderLink engine: both roles as embeddable, event-reporting APIs.
//!
//! The CLI binary and the GUI app both drive this crate. Role functions
//! BLOCK until the session ends (window close, Stop/Bye, silence teardown,
//! or [`CancelToken`] cancellation); progress is observable two ways:
//! human-oriented `log` records (project convention) and structured
//! [`EngineEvent`]s through an [`EventSink`] for UI state.
//!
//! macOS is the only implemented platform so far; the Linux initiator
//! port implements the same API surface (see HANDOFF.md).

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use tl_proto::{Codec, StatsReport, StreamConfig};

/// Cooperative cancellation shared between the embedder and the role's
/// worker threads (the GUI Stop button).
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Structured progress for embedders (UI state machines). Emitted
/// best-effort; the sink never blocks the streaming path.
#[derive(Clone, Debug, serde::Serialize)]
pub enum EngineEvent {
    Negotiated(StreamConfig),
    /// Video path open; frames are flowing.
    Streaming,
    /// ~1 Hz receiver statistics.
    Stats(StatsReport),
    /// Latest measured encode-to-decode latency (target side, ~every 120
    /// frames).
    LatencyMs(f64),
    /// ~1 Hz audio playback statistics (target side, when audio streams).
    Audio(AudioStats),
    /// Session over; reason string is human-oriented.
    Ended(String),
    /// Non-fatal degraded-mode notice (input disabled, etc.).
    Warn(String),
}

/// Cloneable handle for emitting [`EngineEvent`]s from worker threads.
#[derive(Clone)]
pub struct EventSink {
    tx: std::sync::mpsc::Sender<EngineEvent>,
}

impl EventSink {
    /// Sink whose events are dropped (CLI: engine logs already cover it).
    pub fn discarded() -> Self {
        let (tx, _rx) = std::sync::mpsc::channel();
        Self { tx }
    }

    /// Channel pair: events can be drained with a blocking `recv`.
    pub fn channel() -> (Self, std::sync::mpsc::Receiver<EngineEvent>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Self { tx }, rx)
    }

    pub fn emit(&self, event: EngineEvent) {
        let _ = self.tx.send(event); // no receiver = nobody listening
    }

    pub fn warn(&self, msg: impl Into<String>) {
        self.emit(EngineEvent::Warn(msg.into()));
    }
}

/// Frame source for the initiator: synthetic pattern (no permissions) or
/// the primary/virtual display (needs Screen Recording).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    TestPattern,
    Screen,
}

/// Audio source for the initiator (SPEC §12). `Sine` needs no permissions
/// (validation harness); `System` is the real capture path (macOS Core
/// Audio tap; audio TCC prompt on first use).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioSource {
    Sine { freq_hz: f64 },
    System,
}

/// ~1 Hz audio playback statistics (SPEC §12.5 measurement duty).
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct AudioStats {
    pub played: u64,
    pub concealed: u64,
    pub dropped: u64,
    /// Playback head vs wall clock (positive = behind).
    pub drift_ms: f64,
}

/// Target role configuration.
#[derive(Clone, Debug)]
pub struct TargetConfig {
    /// Address for the control listener (usually `0.0.0.0`).
    pub bind: IpAddr,
    /// Present in a window instead of borderless fullscreen.
    pub windowed: bool,
    /// Do not capture/forward this machine's keyboard/mouse.
    pub no_input: bool,
    /// Accept and play an audio stream (SPEC §12).
    pub audio_playback: bool,
    pub cancel: CancelToken,
}

/// Initiator role configuration. `addr` is the RESOLVED target address
/// (use [`discover_target`] or direct parsing); `res` overrides the
/// target panel's native resolution when set.
#[derive(Clone, Debug)]
pub struct InitiatorConfig {
    pub addr: SocketAddr,
    pub source: Source,
    pub codec: Option<Codec>,
    pub bitrate_kbps: Option<u32>,
    pub fps: Option<u32>,
    pub res: Option<(u32, u32)>,
    pub virtual_display: bool,
    pub max_frames: Option<u64>,
    /// Stream audio alongside video (SPEC §12).
    pub audio: Option<AudioSource>,
    pub cancel: CancelToken,
}

/// Announce this machine as a ThunderLink target on mDNS (SPEC §3) until
/// the returned handle is dropped. Failure is non-fatal (direct-IP
/// connects still work); the error is returned for the caller to surface.
pub fn announce_target(name: &str) -> Result<tl_net::discovery::Announcer> {
    Ok(tl_net::discovery::Announcer::start(
        name,
        tl_proto::Role::Target,
        tl_proto::CONTROL_PORT,
    )?)
}

/// Browse `_thunderlink._tcp` until a role=target peer resolves and one
/// of its addresses answers a 1 s TCP probe; prefer IPv4, then global
/// IPv6, then non-link-local (link-local v6 needs a scope id the TXT
/// record cannot carry). First mDNS resolutions can carry partial or
/// stale sets — reachability decides.
pub fn discover_target(timeout: Duration) -> Result<SocketAddr> {
    use std::net::TcpStream;
    use tl_net::discovery::{Browser, DiscoveryEvent};
    use tl_proto::Role;

    let browser = Browser::start().context("start mDNS browser")?;
    let deadline = std::time::Instant::now() + timeout;
    log::info!("browsing for a ThunderLink target (up to {timeout:?})...");
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        let Some(event) = browser.next_event(remaining) else { break };
        let DiscoveryEvent::Added(peer) = event else { continue };
        if peer.role != Role::Target {
            continue; // another initiator announcing itself
        }
        log::info!(
            "discovered target {:?} at {:?} port {}",
            peer.name,
            peer.addrs,
            peer.port
        );
        for addr in ordered_candidate_addrs(&peer) {
            match TcpStream::connect_timeout(&addr, Duration::from_secs(1)) {
                Ok(_) => return Ok(addr),
                Err(e) => log::warn!("target addr {addr} unreachable ({e}); trying next"),
            }
        }
        // Keep browsing: the peer may re-resolve with better addresses.
    }
    bail!("no ThunderLink target found via mDNS within {timeout:?}")
}

/// Enumerate ThunderLink targets visible via mDNS for up to `timeout`,
/// returning every distinct role=target peer resolved (for UI pickers).
pub fn browse_targets(timeout: Duration) -> Vec<tl_net::discovery::Peer> {
    use tl_net::discovery::{Browser, DiscoveryEvent};
    use tl_proto::Role;

    let Ok(browser) = Browser::start() else {
        return Vec::new();
    };
    let deadline = std::time::Instant::now() + timeout;
    let mut out: Vec<tl_net::discovery::Peer> = Vec::new();
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        let Some(event) = browser.next_event(remaining) else { break };
        let DiscoveryEvent::Added(peer) = event else { continue };
        if peer.role == Role::Target
            && !out.iter().any(|p| p.name == peer.name && p.addrs == peer.addrs)
        {
            log::info!("target discovered: {:?} at {:?}", peer.name, peer.addrs);
            out.push(peer);
        }
    }
    out
}

/// Connection candidates in priority order: IPv4 first, then global
/// unicast IPv6 (2000::/3), then anything except IPv6 link-local.
fn ordered_candidate_addrs(peer: &tl_net::discovery::Peer) -> Vec<SocketAddr> {
    use std::net::IpAddr;

    let is_link_local_v6 =
        |ip: &IpAddr| matches!(ip, IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80);
    let mut v6_global = Vec::new();
    let mut rest = Vec::new();
    for ip in &peer.addrs {
        match ip {
            IpAddr::V4(_) => {} // handled below (kept in original order)
            IpAddr::V6(v6) if (v6.segments()[0] & 0xe000) == 0x2000 => v6_global.push(*ip),
            ip if !is_link_local_v6(ip) => rest.push(*ip),
            _ => {} // link-local v6: unusable without scope id
        }
    }
    peer.addrs
        .iter()
        .filter(|ip| ip.is_ipv4())
        .chain(v6_global.iter())
        .chain(rest.iter())
        .map(|ip| SocketAddr::new(*ip, peer.port))
        .collect()
}

mod audio;
pub mod ladder;
mod ctrl;

#[cfg(target_os = "macos")]
mod imp;
#[cfg(target_os = "macos")]
pub use imp::{run_initiator, run_target, EmbeddedPresenter};

#[cfg(target_os = "linux")]
mod imp_linux;
#[cfg(target_os = "linux")]
pub use imp_linux::run_initiator;

/// Run the target role (blocks until the session ends).
#[cfg(not(target_os = "macos"))]
pub fn run_target(
    _cfg: TargetConfig,
    _presenter: Option<std::convert::Infallible>,
    _ev: &EventSink,
) -> Result<()> {
    bail!("target role is not implemented on this platform yet (macOS only)")
}

/// Run the initiator role (blocks until the session ends).
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn run_initiator(_cfg: InitiatorConfig, _ev: &EventSink) -> Result<()> {
    bail!("initiator role is not implemented on this platform yet")
}
