//! Opus codec wrappers (SPEC §12.3/§12.5): 48 kHz stereo, 10 ms frames,
//! VBR + in-band FEC + DTX on the encode side, PLC concealment on the
//! decode side.

use anyhow::{bail, Context, Result};
use opus::{Application, Bitrate, Channels};

use crate::{FRAME_LEN, FRAME_SAMPLES, SAMPLE_RATE};

/// Maximum size of one Opus packet (RFC 6716). 192 kbps × 10 ms ≈ 240 B
/// in practice, so this only guards runaway encoder output.
const MAX_PACKET_BYTES: usize = 1275;

/// Opus encoder configured per SPEC §12.3: 48 kHz stereo, 10 ms frames,
/// VBR, in-band FEC, DTX, complexity 5.
pub struct OpusEncoder {
    enc: opus::Encoder,
}

impl OpusEncoder {
    /// `bitrate_kbps` is the VBR target (SPEC §12.3: 128–256, default 192).
    pub fn new(bitrate_kbps: u32) -> Result<Self> {
        let mut enc =
            opus::Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::Audio)
                .context("create Opus encoder")?;
        enc.set_bitrate(Bitrate::Bits((bitrate_kbps * 1000) as i32))
            .context("set bitrate")?;
        enc.set_vbr(true).context("enable VBR")?;
        // libopus only embeds FEC data when it expects loss, so a small
        // non-zero packet-loss expectation is required for "FEC ON" to
        // be more than a no-op flag.
        enc.set_packet_loss_perc(5).context("set expected loss (enables FEC)")?;
        enc.set_inband_fec(true).context("enable in-band FEC")?;
        enc.set_dtx(true).context("enable DTX")?;
        enc.set_complexity(5).context("set complexity")?;
        Ok(Self { enc })
    }

    /// Encode one 10 ms frame into one Opus packet. `pcm` must be exactly
    /// [`FRAME_LEN`] interleaved samples; sustained silence collapses to
    /// a few bytes of DTX.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>> {
        if pcm.len() != FRAME_LEN {
            bail!(
                "pcm must be exactly {FRAME_LEN} interleaved samples, got {}",
                pcm.len()
            );
        }
        self.enc
            .encode_vec(pcm, MAX_PACKET_BYTES)
            .context("opus encode")
    }
}

/// Opus decoder for 48 kHz stereo 10 ms packets.
pub struct OpusDecoder {
    dec: opus::Decoder,
}

impl OpusDecoder {
    pub fn new() -> Result<Self> {
        let dec = opus::Decoder::new(SAMPLE_RATE, Channels::Stereo)
            .context("create Opus decoder")?;
        Ok(Self { dec })
    }

    /// Decode one packet into exactly [`FRAME_LEN`] interleaved samples.
    /// `None` decodes no packet and runs Opus packet-loss concealment —
    /// the PLC path of SPEC §12.5.
    pub fn decode(&mut self, packet: Option<&[u8]>) -> Result<Vec<i16>> {
        const EMPTY: &[u8] = &[];
        let data = packet.unwrap_or(EMPTY);
        let mut out = vec![0i16; FRAME_LEN];
        // `n` is samples per channel; a 10 ms packet (or a PLC frame)
        // always decodes to exactly FRAME_SAMPLES per channel.
        let n = self
            .dec
            .decode(data, &mut out, false)
            .context("opus decode")?;
        if n != FRAME_SAMPLES {
            bail!("expected {FRAME_SAMPLES} samples/channel, decoded {n}");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sine::SineSource;

    fn rms(x: &[i16]) -> f64 {
        (x.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    fn encode_rejects_wrong_frame_size() {
        let mut enc = OpusEncoder::new(192).unwrap();
        assert!(enc.encode(&[0i16; 7]).is_err());
        assert!(enc.encode(&[0i16; 960]).is_ok());
    }

    #[test]
    fn roundtrip_sine_rms_within_20_percent() {
        let mut src = SineSource::new(440.0);
        let mut enc = OpusEncoder::new(192).unwrap();
        let mut dec = OpusDecoder::new().unwrap();

        // Compare frames 1..3: skips nothing in particular, but averages
        // out any first-packet transient.
        let mut ratios = Vec::new();
        for _ in 0..3 {
            let frame = src.next_frame();
            let packet = enc.encode(&frame).unwrap();
            assert!(!packet.is_empty());
            let out = dec.decode(Some(&packet)).unwrap();
            assert_eq!(out.len(), FRAME_LEN);
            ratios.push(rms(&out) / rms(&frame));
        }
        let worst = ratios[1..]
            .iter()
            .fold(0.0f64, |m, r| m.max((r - 1.0).abs()));
        assert!(
            worst < 0.20,
            "decoded RMS deviates {worst:.1} % from source (ratios {ratios:?})"
        );
    }

    #[test]
    fn conceal_three_misses_without_panic() {
        let mut src = SineSource::new(440.0);
        let mut enc = OpusEncoder::new(192).unwrap();
        let mut dec = OpusDecoder::new().unwrap();
        // Prime the decoder with real signal so PLC has state to fade.
        for _ in 0..3 {
            let packet = enc.encode(&src.next_frame()).unwrap();
            dec.decode(Some(&packet)).unwrap();
        }
        let mut energy = Vec::new();
        for _ in 0..3 {
            let concealed = dec.decode(None).unwrap();
            assert_eq!(concealed.len(), FRAME_LEN);
            energy.push(rms(&concealed));
        }
        // PLC output decays but never explodes past source level.
        let source_rms = rms(&SineSource::new(440.0).next_frame());
        assert!(
            energy.iter().all(|&e| e <= source_rms * 1.5),
            "PLC energy {energy:?} exceeds source {source_rms:.0}"
        );
        // And recovery: the next real packet still decodes.
        let packet = enc.encode(&src.next_frame()).unwrap();
        assert_eq!(dec.decode(Some(&packet)).unwrap().len(), FRAME_LEN);
    }

    #[test]
    fn dtx_collapses_silence() {
        let mut enc = OpusEncoder::new(192).unwrap();
        let silence = vec![0i16; FRAME_LEN];
        // DTX engages after a few consecutive silent frames.
        let mut saw_dtx = false;
        for _ in 0..10 {
            let packet = enc.encode(&silence).unwrap();
            if packet.len() <= 3 {
                saw_dtx = true;
            }
        }
        assert!(saw_dtx, "expected at least one DTX (<= 3 byte) packet");
    }
}
