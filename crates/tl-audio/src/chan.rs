//! Audio UDP channel (SPEC §12.4): each datagram is the fixed 16-byte
//! header + exactly one Opus packet. Fire-and-forget — no retransmit
//! path exists for audio (SPEC §12.1).

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use tl_proto::{AUDIO_HEADER_LEN, AUDIO_MAGIC};

/// One received audio datagram: wire header fields plus the Opus packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioPacket {
    /// Wrapping per-packet sequence number.
    pub seq: u32,
    /// Wall-clock µs stamped at capture time (same domain as video
    /// `pts_us`; the A/V sync anchor).
    pub pts_us: i64,
    /// One Opus packet.
    pub payload: Vec<u8>,
}

/// Sender half: bound socket with a fixed peer, per SPEC §12.4 framing.
pub struct AudioTx {
    sock: UdpSocket,
    peer: SocketAddr,
}

impl AudioTx {
    /// Bind the sending socket; all [`AudioTx::send`] calls go to `peer`.
    pub fn bind(local: SocketAddr, peer: SocketAddr) -> io::Result<Self> {
        Ok(Self {
            sock: UdpSocket::bind(local)?,
            peer,
        })
    }

    /// Send one datagram: `AUDIO_MAGIC`/`seq`/`pts_us` little-endian
    /// header followed by `payload`.
    pub fn send(&self, seq: u32, pts_us: i64, payload: &[u8]) -> io::Result<()> {
        let mut datagram = Vec::with_capacity(AUDIO_HEADER_LEN + payload.len());
        datagram.extend_from_slice(&AUDIO_MAGIC.to_le_bytes());
        datagram.extend_from_slice(&seq.to_le_bytes());
        datagram.extend_from_slice(&pts_us.to_le_bytes());
        datagram.extend_from_slice(payload);
        self.sock.send_to(&datagram, self.peer)?;
        Ok(())
    }

    /// Local address this socket is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.sock.local_addr()
    }
}

/// Receiver half: drains datagrams, skipping malformed ones.
pub struct AudioRx {
    sock: UdpSocket,
}

impl AudioRx {
    pub fn bind(local: SocketAddr) -> io::Result<Self> {
        Ok(Self {
            sock: UdpSocket::bind(local)?,
        })
    }

    /// Wait up to `timeout` for the first datagram, then drain everything
    /// already queued (same pattern as the other UDP channels).
    /// Malformed datagrams — shorter than a header plus one payload byte,
    /// or with a bad magic — are skipped with a debug log. Returns the
    /// valid packets, oldest first; empty on timeout.
    pub fn poll(&mut self, timeout: Duration) -> io::Result<Vec<AudioPacket>> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; 65_535];
        self.sock.set_read_timeout(Some(timeout))?;
        loop {
            match self.sock.recv_from(&mut buf) {
                Ok((n, _src)) => match parse_packet(&buf[..n]) {
                    Some(pkt) => out.push(pkt),
                    None => {
                        log::debug!("audio rx: skipping malformed datagram ({n} bytes)")
                    }
                },
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(e) => return Err(e),
            }
            // Subsequent iterations only drain what is already queued.
            self.sock.set_read_timeout(Some(Duration::from_millis(1)))?;
        }
        Ok(out)
    }

    /// Local address this socket is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.sock.local_addr()
    }
}

/// Parse one datagram; `None` = malformed (short, or bad magic). An Opus
/// packet is at least one byte, so a bare header carries no audio.
fn parse_packet(datagram: &[u8]) -> Option<AudioPacket> {
    if datagram.len() <= AUDIO_HEADER_LEN {
        return None;
    }
    if u32::from_le_bytes(datagram[0..4].try_into().unwrap()) != AUDIO_MAGIC {
        return None;
    }
    Some(AudioPacket {
        seq: u32::from_le_bytes(datagram[4..8].try_into().unwrap()),
        pts_us: i64::from_le_bytes(datagram[8..16].try_into().unwrap()),
        payload: datagram[AUDIO_HEADER_LEN..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn loopback() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
    }

    #[test]
    fn header_roundtrip_and_bad_magic_skip() {
        let mut rx = AudioRx::bind(loopback()).unwrap();
        let dest = rx.local_addr().unwrap();
        let tx = AudioTx::bind(loopback(), dest).unwrap();

        tx.send(7, 123_456, &[1, 2, 3]).unwrap();
        // Bad magic, header-only, and truncated datagrams — all skipped.
        let raw = UdpSocket::bind(loopback()).unwrap();
        let mut bad_magic = vec![0u8; 20];
        bad_magic[..4].copy_from_slice(&u32::from_le_bytes(*b"XXXX").to_le_bytes());
        raw.send_to(&bad_magic, dest).unwrap();
        raw.send_to(&AUDIO_MAGIC.to_le_bytes(), dest).unwrap(); // header only
        raw.send_to(&[1, 2, 3], dest).unwrap(); // shorter than a header
        tx.send(u32::MAX, -9_000_000_000_123, &[0xAA; 240]).unwrap();

        let pkts = rx.poll(Duration::from_millis(500)).unwrap();
        assert_eq!(pkts.len(), 2, "malformed datagrams must be skipped");
        assert_eq!(pkts[0].seq, 7);
        assert_eq!(pkts[0].pts_us, 123_456);
        assert_eq!(pkts[0].payload, vec![1, 2, 3]);
        // Wraparound seq + negative wall-clock pts survive the roundtrip.
        assert_eq!(pkts[1].seq, u32::MAX);
        assert_eq!(pkts[1].pts_us, -9_000_000_000_123);
        assert_eq!(pkts[1].payload, vec![0xAA; 240]);
    }

    #[test]
    fn poll_times_out_empty() {
        let mut rx = AudioRx::bind(loopback()).unwrap();
        let t = std::time::Instant::now();
        let pkts = rx.poll(Duration::from_millis(50)).unwrap();
        assert!(pkts.is_empty());
        assert!(t.elapsed() >= Duration::from_millis(50));
    }
}
