//! ThunderLink daemon/CLI: one binary, two roles.
//!
//!   thunderlink target     — act as a monitor (listens for an initiator)
//!   thunderlink initiator  — stream a display to a target
//!
//! Thin argument parser over the `thunderlink-engine` crate (which the
//! GUI app also drives). Protocol: SPEC.md §3–§8.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use thunderlink_engine::{
    announce_target, discover_target, run_initiator, run_target, CancelToken, EventSink,
    InitiatorConfig, Source, TargetConfig,
};
use tl_proto::{Codec, CONTROL_PORT};

#[derive(Parser)]
#[command(name = "thunderlink", version, about = "Use a computer as a Thunderbolt monitor")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SourceKind {
    /// Animated synthetic pattern — no Screen Recording permission needed.
    TestPattern,
    /// Mirror the primary display (needs Screen Recording TCC grant).
    Screen,
}

impl From<SourceKind> for Source {
    fn from(k: SourceKind) -> Self {
        match k {
            SourceKind::TestPattern => Source::TestPattern,
            SourceKind::Screen => Source::Screen,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CodecKind {
    Hevc,
    H264,
}

impl From<CodecKind> for Codec {
    fn from(k: CodecKind) -> Self {
        match k {
            CodecKind::Hevc => Codec::Hevc,
            CodecKind::H264 => Codec::H264,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Act as a monitor for an incoming initiator connection.
    Target {
        /// Address to bind the control listener on.
        #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::UNSPECIFIED))]
        bind: IpAddr,
        /// Present in a window instead of borderless fullscreen.
        #[arg(long)]
        windowed: bool,
        /// Do not forward this machine's keyboard/mouse.
        #[arg(long)]
        no_input: bool,
    },
    /// Stream a display to a target.
    Initiator {
        /// Target host (or host:port). Mutually exclusive with --discover.
        #[arg(long, conflicts_with = "discover")]
        connect: Option<String>,
        /// Find a target via mDNS (`_thunderlink._tcp`, TXT role=target)
        /// instead of --connect. Browses up to --discover-timeout.
        #[arg(long)]
        discover: bool,
        /// How long --discover may browse before giving up.
        #[arg(long, default_value_t = 10)]
        discover_timeout: u64,
        #[arg(long, value_enum, default_value_t = SourceKind::TestPattern)]
        source: SourceKind,
        #[arg(long, value_enum)]
        codec: Option<CodecKind>,
        /// Override bitrate (kbps). Default: SPEC §8 ladder.
        #[arg(long)]
        bitrate_kbps: Option<u32>,
        /// Override fps. Default: target panel refresh.
        #[arg(long)]
        fps: Option<u32>,
        /// Override resolution WxH. Default: target panel native resolution.
        #[arg(long)]
        res: Option<String>,
        /// Stop cleanly after N encoded frames (used by the smoke test).
        #[arg(long)]
        frames: Option<u64>,
        /// Create a virtual display (extended desktop): the target becomes a
        /// NEW monitor at its native resolution instead of mirroring.
        /// With `--source screen` the virtual display is captured.
        #[arg(long)]
        r#virtual: bool,
    },
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Target { bind, windowed, no_input } => {
            // Announce on mDNS until the session ends (SPEC §3); a failure
            // is non-fatal — direct-IP --connect still works.
            let announcer = announce_target("thunderlink-target")
                .map_err(|e| {
                    log::warn!("mDNS announce failed ({e}); discovery disabled");
                    e
                })
                .ok();
            let r = run_target(
                TargetConfig {
                    bind,
                    windowed,
                    no_input,
                    cancel: CancelToken::new(),
                },
                None,
                &EventSink::discarded(),
            );
            drop(announcer);
            r
        }
        Cmd::Initiator {
            connect,
            discover,
            discover_timeout,
            source,
            codec,
            bitrate_kbps,
            fps,
            res,
            frames,
            r#virtual,
        } => {
            let addr = match (connect, discover) {
                (Some(c), false) => parse_connect(&c)?,
                (None, true) => {
                    discover_target(Duration::from_secs(discover_timeout))?
                }
                (Some(_), true) => bail!("--connect and --discover are mutually exclusive"),
                (None, false) => bail!("specify --connect HOST[:PORT] or --discover"),
            };
            run_initiator(
                InitiatorConfig {
                    addr,
                    source: source.into(),
                    codec: codec.map(Into::into),
                    bitrate_kbps,
                    fps,
                    res: res.as_deref().map(parse_res).transpose()?,
                    virtual_display: r#virtual,
                    max_frames: frames,
                    cancel: CancelToken::new(),
                },
                &EventSink::discarded(),
            )
        }
    }
}

fn parse_connect(s: &str) -> Result<SocketAddr> {
    if s.contains(':') {
        s.parse().context("invalid --connect host:port")
    } else {
        Ok(SocketAddr::new(s.parse().context("invalid --connect host")?, CONTROL_PORT))
    }
}

fn parse_res(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s.split_once('x').context("--res must be WxH")?;
    Ok((w.parse().context("--res width")?, h.parse().context("--res height")?))
}
