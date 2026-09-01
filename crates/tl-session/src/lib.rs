//! Session state machines for both roles (SPEC §4 handshake sequence).
//!
//! Pure orchestration over `tl_net::control::ControlChannel`; no threads
//! here — steady-state heartbeats/stats are the caller's job after
//! `start()` succeeds.
#![forbid(unsafe_code)]

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tl_net::control::ControlChannel;
use tl_proto::{Msg, Role, StreamConfig, TargetCaps, PROTOCOL_VERSION};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Connected,
    Configured,
    Streaming,
}

fn io_err(kind: io::ErrorKind, msg: impl Into<String>) -> io::Error {
    io::Error::new(kind, msg.into())
}

fn expect_hello(msg: &Msg, want: Role) -> io::Result<()> {
    match msg {
        Msg::Hello { version, role, .. } if *role == want && *version == PROTOCOL_VERSION => {
            Ok(())
        }
        Msg::Hello { version, .. } => Err(io_err(
            io::ErrorKind::InvalidData,
            format!("protocol version mismatch (theirs {version}, ours {PROTOCOL_VERSION})"),
        )),
        Msg::Error { message } => {
            Err(io_err(io::ErrorKind::ConnectionRefused, format!("peer error: {message}")))
        }
        other => Err(io_err(
            io::ErrorKind::InvalidData,
            format!("expected Hello, got {other:?}"),
        )),
    }
}

fn expect_ack(msg: &Msg, what: &str) -> io::Result<()> {
    match msg {
        Msg::Ack { ok: true, .. } => Ok(()),
        Msg::Ack { ok: false, message } => {
            Err(io_err(io::ErrorKind::ConnectionRefused, format!("{what} refused: {message}")))
        }
        other => Err(io_err(
            io::ErrorKind::InvalidData,
            format!("expected Ack for {what}, got {other:?}"),
        )),
    }
}

/// Initiator side: connects to a target and drives negotiation.
pub struct InitiatorSession {
    chan: ControlChannel,
    caps: TargetCaps,
    state: State,
}

impl InitiatorSession {
    /// Connect, exchange Hello, receive target caps (SPEC §4 steps 1–2).
    pub fn connect(addr: SocketAddr, name: &str) -> io::Result<Self> {
        let mut chan = ControlChannel::connect(addr)?;
        chan.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        chan.send(&Msg::Hello {
            version: PROTOCOL_VERSION,
            role: Role::Initiator,
            name: name.to_string(),
        })?;
        expect_hello(&chan.recv()?, Role::Target)?;
        let caps = match chan.recv()? {
            Msg::Caps(c) => c,
            other => {
                return Err(io_err(
                    io::ErrorKind::InvalidData,
                    format!("expected Caps, got {other:?}"),
                ))
            }
        };
        log::info!("target caps: panel {}x{}@{:.2}Hz, decoders {:?}",
            caps.panel.width, caps.panel.height,
            caps.panel.refresh_millihertz as f64 / 1000.0,
            caps.decoders.iter().map(|d| d.codec).collect::<Vec<_>>());
        Ok(Self { chan, caps, state: State::Connected })
    }

    pub fn caps(&self) -> &TargetCaps {
        &self.caps
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// Send the chosen StreamConfig; fails when the target rejects it
    /// (SPEC §4 step 3).
    pub fn configure(&mut self, cfg: &StreamConfig) -> io::Result<()> {
        if !self.caps.supports(cfg) {
            return Err(io_err(
                io::ErrorKind::InvalidInput,
                format!("local caps check failed for {cfg:?}"),
            ));
        }
        self.chan.send(&Msg::Config(cfg.clone()))?;
        expect_ack(&self.chan.recv()?, "Config")?;
        self.state = State::Configured;
        Ok(())
    }

    /// Begin streaming (SPEC §4 step 4). The caller must have the video
    /// path ready before calling: the target replies Ack once it can accept
    /// frames and expects video immediately after.
    pub fn start(&mut self) -> io::Result<()> {
        self.chan.send(&Msg::Start)?;
        expect_ack(&self.chan.recv()?, "Start")?;
        self.chan.set_read_timeout(None)?;
        self.state = State::Streaming;
        Ok(())
    }

    /// Control channel for steady-state messages (Heartbeat, Led, Stats).
    pub fn channel(&mut self) -> &mut ControlChannel {
        &mut self.chan
    }

    /// Peer address (target) — used to derive UDP peer IPs.
    pub fn peer_addr(&self) -> SocketAddr {
        self.chan.peer_addr()
    }

    /// Graceful teardown (best-effort).
    pub fn stop(mut self) {
        if self.state == State::Streaming {
            let _ = self.chan.send(&Msg::Stop);
        }
        let _ = self.chan.send(&Msg::Bye);
    }
}

/// Target side: accepts one initiator session.
pub struct TargetSession {
    chan: ControlChannel,
    state: State,
    cfg: Option<StreamConfig>,
}

impl TargetSession {
    /// Accept a connection, exchange Hello, send caps (SPEC §4 steps 1–2).
    pub fn accept(listener: &TcpListener, name: &str, caps: &TargetCaps) -> io::Result<Self> {
        let (mut chan, peer) = ControlChannel::accept(listener)?;
        log::info!("initiator connected from {peer}");
        chan.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        expect_hello(&chan.recv()?, Role::Initiator)?;
        chan.send(&Msg::Hello {
            version: PROTOCOL_VERSION,
            role: Role::Target,
            name: name.to_string(),
        })?;
        chan.send(&Msg::Caps(caps.clone()))?;
        Ok(Self { chan, state: State::Connected, cfg: None })
    }

    /// Wait for the initiator's StreamConfig; validates against caps and
    /// answers Ack (SPEC §4 step 3).
    pub fn await_config(&mut self, caps: &TargetCaps) -> io::Result<StreamConfig> {
        let cfg = match self.chan.recv()? {
            Msg::Config(c) => c,
            other => {
                return Err(io_err(
                    io::ErrorKind::InvalidData,
                    format!("expected Config, got {other:?}"),
                ))
            }
        };
        if caps.supports(&cfg) {
            self.chan.send(&Msg::Ack { ok: true, message: String::new() })?;
            self.cfg = Some(cfg.clone());
            self.state = State::Configured;
            Ok(cfg)
        } else {
            let msg = format!("unsupported stream config {cfg:?}");
            self.chan.send(&Msg::Ack { ok: false, message: msg.clone() })?;
            Err(io_err(io::ErrorKind::InvalidInput, msg))
        }
    }

    /// Wait for Start; replies Ack once the caller confirms readiness via
    /// the returned value (SPEC §4 step 4). Call `ack_ready` after the
    /// decoder/presenter are up.
    pub fn await_start(&mut self) -> io::Result<StartPending<'_>> {
        match self.chan.recv()? {
            Msg::Start => Ok(StartPending { sess: self }),
            other => Err(io_err(
                io::ErrorKind::InvalidData,
                format!("expected Start, got {other:?}"),
            )),
        }
    }

    pub fn config(&self) -> Option<&StreamConfig> {
        self.cfg.as_ref()
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// Control channel for steady-state messages (Heartbeat, Stats, Stop).
    pub fn channel(&mut self) -> &mut ControlChannel {
        &mut self.chan
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.chan.peer_addr()
    }
}

/// Returned by `TargetSession::await_start`; lets the target bring up its
/// decode/present path before ack-ing (SPEC §4 step 4).
pub struct StartPending<'a> {
    sess: &'a mut TargetSession,
}

impl StartPending<'_> {
    /// Decoder/presenter ready — allow video to flow.
    pub fn ack_ready(self) -> io::Result<()> {
        self.sess.chan.send(&Msg::Ack { ok: true, message: String::new() })?;
        self.sess.chan.set_read_timeout(None)?;
        self.sess.state = State::Streaming;
        Ok(())
    }

    /// Something failed — refuse the start.
    pub fn ack_fail(self, message: &str) -> io::Result<()> {
        self.sess.chan.send(&Msg::Ack { ok: false, message: message.to_string() })
    }
}
