//! Control-channel messages (length-prefixed bincode over TCP).

use serde::{Deserialize, Serialize};

use crate::caps::{StreamConfig, TargetCaps};
use crate::input::LedState;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Initiator,
    Target,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StatsReport {
    pub decoded_fps: u32,
    pub presented_fps: u32,
    pub bitrate_kbps: u32,
    pub rtt_us: u32,
    pub loss_permille: u32,
    pub decode_ms_x100: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Msg {
    Hello { version: u16, role: Role, name: String },
    Caps(TargetCaps),
    Config(StreamConfig),
    Start,
    Stop,
    Ack { ok: bool, message: String },
    Heartbeat { ts_us: i64 },
    Led(LedState),
    Stats(StatsReport),
    Bye,
    Error { message: String },
}
