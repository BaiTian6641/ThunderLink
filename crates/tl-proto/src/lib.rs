//! ThunderLink wire protocol: shared types, packet framing, constants.
//!
//! This crate is the authoritative definition of everything that crosses
//! the wire. Behavior is specified in `SPEC.md` at the repo root.
#![forbid(unsafe_code)]

pub mod caps;
pub mod input;
pub mod msg;
pub mod packet;
pub mod time;

pub use caps::*;
pub use input::*;
pub use msg::*;
pub use packet::*;

/// Protocol version; peers must match exactly in v1.
pub const PROTOCOL_VERSION: u16 = 1;

pub const CONTROL_PORT: u16 = 47776;
pub const VIDEO_PORT: u16 = 47777;
pub const FEEDBACK_PORT: u16 = 47778;
pub const INPUT_PORT: u16 = 47779;

/// Total UDP datagram budget including the 24-byte video header.
/// Conservative: works without jumbo frames on Thunderbolt bridges.
pub const DEFAULT_DATAGRAM_PAYLOAD: usize = 1400;

/// Maximum accepted control message size (1 MiB).
pub const MAX_CONTROL_MESSAGE: usize = 1 << 20;

pub const MDNS_SERVICE_TYPE: &str = "_thunderlink._tcp.local.";
