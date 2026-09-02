//! Encode → decode roundtrip (SPEC §9/§11): synthetic TestPattern frames
//! through tl-macos-capture's VT encoder into tl-macos-render's VT decoder.
//! Needs no TCC permissions, no Thunderbolt peer, no display.

use tl_macos_capture::encode::Encoder;
use tl_macos_capture::testsrc::TestPattern;
use tl_macos_render::decode::Decoder;
use tl_proto::{Chroma, Codec, StreamConfig};

#[test]
fn hevc_roundtrip_30_frames() {
    let _ = env_logger::try_init();
    let cfg = StreamConfig {
        codec: Codec::Hevc,
        width: 640,
        height: 480,
        fps_millihertz: 30_000,
        bitrate_kbps: 8_000,
        chroma: Chroma::Yuv420,
        hdr: false,
        audio: false,
        audio_bitrate_kbps: None,
    };
    let mut src = TestPattern::new(cfg.width, cfg.height, 30);
    let mut enc = Encoder::new(&cfg).expect("encoder init");
    let mut dec = Decoder::new().expect("decoder init");

    let mut decoded = Vec::new();
    for _ in 0..30 {
        let frame = src.next().expect("test pattern frame");
        let units = enc.encode(&frame).expect("encode");
        for unit in &units {
            for df in dec.decode(unit).expect("decode") {
                decoded.push(df);
            }
        }
    }

    assert_eq!(decoded.len(), 30, "30 encoded frames must yield 30 decoded frames");
    let mut last_pts = i64::MIN;
    for f in &decoded {
        assert_eq!(f.width(), 640, "decoded width");
        assert_eq!(f.height(), 480, "decoded height");
        assert!(f.pts_us() > last_pts, "pts not strictly monotonic: {} after {}", f.pts_us(), last_pts);
        last_pts = f.pts_us();
    }
}
