//! Symmetric UDP channel for Feedback messages (SPEC §6).

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use tl_proto::Feedback;

/// Symmetric UDP channel for Feedback messages (SPEC §6).
pub struct FeedbackChannel {
    sock: UdpSocket,
    peer: SocketAddr,
}

impl FeedbackChannel {
    pub fn bind(local: SocketAddr, peer: SocketAddr) -> io::Result<Self> {
        let sock = UdpSocket::bind(local)?;
        Ok(Self { sock, peer })
    }

    pub fn send(&self, fb: &Feedback) -> io::Result<()> {
        let payload = bincode::serialize(fb)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        self.sock.send_to(&payload, self.peer)?;
        Ok(())
    }

    /// Wait up to `timeout` and drain all queued datagrams.
    pub fn poll(&self, timeout: Duration) -> io::Result<Vec<Feedback>> {
        crate::udp_util::drain_udp(&self.sock, timeout, |payload| {
            match bincode::deserialize(payload) {
                Ok(fb) => Some(fb),
                Err(e) => {
                    log::debug!("discarding malformed feedback datagram: {e}");
                    None
                }
            }
        })
    }

    /// Local address this socket is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.sock.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn loopback() -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
    }

    #[test]
    fn feedback_roundtrip() {
        let b = FeedbackChannel::bind(loopback(), loopback()).unwrap();
        let a = FeedbackChannel::bind(loopback(), b.local_addr().unwrap()).unwrap();

        a.send(&Feedback::IdrRequest).unwrap();
        a.send(&Feedback::Report {
            received_frames: 10,
            lost_packets: 2,
            rtt_us: 0,
            jitter_us: 33,
        })
        .unwrap();
        a.send(&Feedback::Nack {
            frame_seq: 7,
            ranges: vec![(1, 3), (9, 9)],
        })
        .unwrap();

        let got = b.poll(Duration::from_millis(500)).unwrap();
        assert_eq!(
            got,
            vec![
                Feedback::IdrRequest,
                Feedback::Report {
                    received_frames: 10,
                    lost_packets: 2,
                    rtt_us: 0,
                    jitter_us: 33,
                },
                Feedback::Nack {
                    frame_seq: 7,
                    ranges: vec![(1, 3), (9, 9)],
                },
            ]
        );
        // Nothing left queued.
        let got = b.poll(Duration::from_millis(20)).unwrap();
        assert!(got.is_empty());
    }
}
