//! Capability and stream-configuration types.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Codec {
    Hevc,
    H264,
    Av1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Chroma {
    Yuv420,
    Yuv444,
}

/// One decoder (or encoder) capability entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodecCaps {
    pub codec: Codec,
    pub max_width: u32,
    pub max_height: u32,
    pub hdr10: bool,
    pub chroma444: bool,
    /// Hardware-accelerated (vs software fallback).
    pub hw: bool,
}

/// Target panel description. `width`/`height` are the panel's native
/// (original) resolution in physical pixels — the stream MUST use them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PanelInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_millihertz: u32,
    /// Backing scale factor ×100 (e.g. 200 for Retina/HiDPI 2x).
    pub scale_x100: u32,
    /// Raw EDID bytes when readable (IOKit/DRM/WMI), else None.
    pub edid: Option<Vec<u8>>,
}

/// Everything a target tells the initiator during negotiation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetCaps {
    pub name: String,
    pub panel: PanelInfo,
    pub decoders: Vec<CodecCaps>,
    /// Target will capture and forward local keyboard/mouse.
    pub accepts_input: bool,
}

/// The initiator's chosen stream parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps_millihertz: u32,
    pub bitrate_kbps: u32,
    pub chroma: Chroma,
    pub hdr: bool,
}

impl TargetCaps {
    /// True when the target can decode this configuration.
    pub fn supports(&self, cfg: &StreamConfig) -> bool {
        self.decoders.iter().any(|d| {
            d.codec == cfg.codec
                && cfg.width <= d.max_width
                && cfg.height <= d.max_height
                && (!cfg.hdr || d.hdr10)
                && (cfg.chroma != Chroma::Yuv444 || d.chroma444)
        })
    }
}

/// Default bitrate ladder for 20 Gbps-class links (SPEC §8). kbps.
pub fn default_bitrate_kbps(width: u32, height: u32, codec: Codec) -> u32 {
    let px = width as u64 * height as u64;
    let base = if px <= 2_100_000 {
        120_000 // 1080p
    } else if px <= 3_700_000 {
        200_000 // 1440p
    } else if px <= 8_300_000 {
        400_000 // 4K
    } else {
        550_000 // 5K+
    };
    let scaled = match codec {
        Codec::Hevc => base,
        Codec::H264 => base * 8 / 5,
        Codec::Av1 => base * 4 / 5,
    };
    scaled.min(800_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_checks_all_dimensions() {
        let caps = TargetCaps {
            name: "t".into(),
            panel: PanelInfo {
                width: 5120,
                height: 2880,
                refresh_millihertz: 60_000,
                scale_x100: 200,
                edid: None,
            },
            decoders: vec![CodecCaps {
                codec: Codec::Hevc,
                max_width: 5120,
                max_height: 2880,
                hdr10: true,
                chroma444: false,
                hw: true,
            }],
            accepts_input: true,
        };
        let ok = StreamConfig {
            codec: Codec::Hevc,
            width: 5120,
            height: 2880,
            fps_millihertz: 60_000,
            bitrate_kbps: 550_000,
            chroma: Chroma::Yuv420,
            hdr: true,
        };
        assert!(caps.supports(&ok));
        assert!(!caps.supports(&StreamConfig { chroma: Chroma::Yuv444, ..ok.clone() }));
        assert!(!caps.supports(&StreamConfig { codec: Codec::H264, ..ok.clone() }));
        assert!(!caps.supports(&StreamConfig { width: 8192, ..ok }));
    }

    #[test]
    fn bitrate_ladder_values() {
        assert_eq!(default_bitrate_kbps(1920, 1080, Codec::Hevc), 120_000);
        assert_eq!(default_bitrate_kbps(3840, 2160, Codec::Hevc), 400_000);
        assert_eq!(default_bitrate_kbps(5120, 2880, Codec::Hevc), 550_000);
        assert_eq!(default_bitrate_kbps(7680, 4320, Codec::H264), 800_000); // capped
    }
}
