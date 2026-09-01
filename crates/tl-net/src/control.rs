//! Length-prefixed bincode over TCP (SPEC §4).

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use tl_proto::{Msg, MAX_CONTROL_MESSAGE};

/// Read timeout during the handshake phase (SPEC §4). Callers move to the
/// 1 s heartbeat tick via [`ControlChannel::set_read_timeout`] after `Start`.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Normalize OS timeout errors (`WouldBlock` from a zeroed `SO_RCVTIMEO`,
/// `TimedOut` elsewhere) to a single `io::ErrorKind::TimedOut`.
fn map_timeout(e: io::Error) -> io::Error {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
            io::Error::new(io::ErrorKind::TimedOut, "control channel read timed out")
        }
        _ => e,
    }
}

fn invalid_data(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// Length-prefixed bincode over TCP (SPEC §4).
pub struct ControlChannel {
    stream: TcpStream,
    peer: SocketAddr,
}

impl ControlChannel {
    pub fn connect(addr: SocketAddr) -> io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        Self::init(stream)
    }

    /// Accept one inbound connection from a listener bound by the caller.
    pub fn accept(listener: &TcpListener) -> io::Result<(Self, SocketAddr)> {
        let (stream, peer) = listener.accept()?;
        Ok((Self::init(stream)?, peer))
    }

    fn init(stream: TcpStream) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let peer = stream.peer_addr()?;
        Ok(Self { stream, peer })
    }

    pub fn send(&mut self, msg: &Msg) -> io::Result<()> {
        let payload = bincode::serialize(msg).map_err(invalid_data)?;
        if payload.len() > MAX_CONTROL_MESSAGE {
            return Err(invalid_data(format!(
                "control message of {} bytes exceeds MAX_CONTROL_MESSAGE",
                payload.len()
            )));
        }
        let len = (payload.len() as u32).to_le_bytes();
        self.stream.write_all(&len)?;
        self.stream.write_all(&payload)?;
        self.stream.flush()?;
        Ok(())
    }

    /// Blocking recv honoring the configured read timeout
    /// (maps `WouldBlock`/`TimedOut` to `io::ErrorKind::TimedOut`).
    pub fn recv(&mut self) -> io::Result<Msg> {
        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf).map_err(map_timeout)?;
        let len = u32::from_le_bytes(len_buf) as usize;
        if len > MAX_CONTROL_MESSAGE {
            return Err(invalid_data(format!(
                "control frame of {len} bytes exceeds MAX_CONTROL_MESSAGE"
            )));
        }
        let mut buf = vec![0u8; len];
        self.stream.read_exact(&mut buf).map_err(map_timeout)?;
        bincode::deserialize(&buf).map_err(invalid_data)
    }

    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(dur)
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    /// Local address this channel is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.stream.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, TcpListener};
    use tl_proto::Role;

    fn loopback_listener() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    #[test]
    fn roundtrip_over_tcp() {
        let (listener, addr) = loopback_listener();
        let server = std::thread::spawn(move || {
            let (mut ch, peer) = ControlChannel::accept(&listener).unwrap();
            assert!(peer.ip().is_loopback());
            for _ in 0..3 {
                let msg = ch.recv().unwrap();
                ch.send(&msg).unwrap(); // echo
            }
        });
        let mut client = ControlChannel::connect(addr).unwrap();
        assert_eq!(client.peer_addr(), addr);
        let msgs = vec![
            Msg::Hello {
                version: tl_proto::PROTOCOL_VERSION,
                role: Role::Initiator,
                name: "test-initiator".to_string(),
            },
            Msg::Start,
            Msg::Heartbeat { ts_us: -42 },
        ];
        for m in &msgs {
            client.send(m).unwrap();
            assert_eq!(&client.recv().unwrap(), m);
        }
        server.join().unwrap();
    }

    #[test]
    fn read_timeout_maps_to_timed_out() {
        let (listener, addr) = loopback_listener();
        let server = std::thread::spawn(move || {
            let (_s, _p) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });
        let mut client = ControlChannel::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let err = client.recv().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        server.join().unwrap();
    }

    #[test]
    fn oversized_frame_rejected() {
        let (listener, addr) = loopback_listener();
        let server = std::thread::spawn(move || {
            let (mut ch, _p) = ControlChannel::accept(&listener).unwrap();
            let err = ch.recv().unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        });
        let mut raw = TcpStream::connect(addr).unwrap();
        let len = ((MAX_CONTROL_MESSAGE + 1) as u32).to_le_bytes();
        raw.write_all(&len).unwrap();
        raw.flush().unwrap();
        server.join().unwrap();
    }
}
