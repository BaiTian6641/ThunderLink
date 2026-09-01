//! Input channel (UDP, target → initiator, SPEC §7).

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use tl_proto::InputBatch;

pub struct InputTx {
    sock: UdpSocket,
    peer: SocketAddr,
}

impl InputTx {
    pub fn bind(local: SocketAddr, peer: SocketAddr) -> io::Result<Self> {
        let sock = UdpSocket::bind(local)?;
        Ok(Self { sock, peer })
    }

    pub fn send(&self, batch: &InputBatch) -> io::Result<()> {
        let payload = bincode::serialize(batch)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        self.sock.send_to(&payload, self.peer)?;
        Ok(())
    }

    /// Local address this socket is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.sock.local_addr()
    }
}

pub struct InputRx {
    sock: UdpSocket,
}

impl InputRx {
    pub fn bind(local: SocketAddr) -> io::Result<Self> {
        let sock = UdpSocket::bind(local)?;
        Ok(Self { sock })
    }

    /// Wait up to `timeout` and drain all queued batches.
    pub fn poll(&self, timeout: Duration) -> io::Result<Vec<InputBatch>> {
        crate::udp_util::drain_udp(&self.sock, timeout, |payload| {
            match bincode::deserialize(payload) {
                Ok(batch) => Some(batch),
                Err(e) => {
                    log::debug!("discarding malformed input datagram: {e}");
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
    use tl_proto::{InputEvent, Mods, MouseButton};

    fn loopback() -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
    }

    #[test]
    fn input_roundtrip() {
        let rx = InputRx::bind(loopback()).unwrap();
        let tx = InputTx::bind(loopback(), rx.local_addr().unwrap()).unwrap();

        tx.send(&InputBatch {
            seq: 1,
            events: vec![
                InputEvent::MouseMove { x: 100, y: 200 },
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    down: true,
                },
            ],
        })
        .unwrap();
        tx.send(&InputBatch {
            seq: 2,
            events: vec![InputEvent::Key {
                usage: 0x04,
                down: true,
                mods: Mods {
                    shift: true,
                    ..Mods::default()
                },
            }],
        })
        .unwrap();

        let got = rx.poll(Duration::from_millis(500)).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].seq, 1);
        assert_eq!(
            got[0].events,
            vec![
                InputEvent::MouseMove { x: 100, y: 200 },
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    down: true,
                },
            ]
        );
        assert_eq!(got[1].seq, 2);
        assert_eq!(got[1].events.len(), 1);

        assert!(rx.poll(Duration::from_millis(20)).unwrap().is_empty());
    }
}
