//! ThunderLink transport: control (TCP), video (UDP fragment+NACK),
//! feedback (UDP), input (UDP), Thunderbolt link detection, mDNS discovery.
//! Behavior: SPEC.md §3–§7.
#![forbid(unsafe_code)]

pub mod control;
pub mod discovery;
pub mod feedback;
pub mod input_chan;
pub mod link;
pub mod video;

/// Shared helpers for the UDP channels.
pub(crate) mod udp_util {
    use std::io;
    use std::net::UdpSocket;
    use std::time::Duration;

    /// Wait up to `timeout` for the first datagram, then drain everything
    /// queued. `parse` maps each datagram payload to a message (None = skip
    /// malformed). Returns everything collected; empty vec on timeout.
    pub fn drain_udp<T>(
        sock: &UdpSocket,
        timeout: Duration,
        mut parse: impl FnMut(&[u8]) -> Option<T>,
    ) -> io::Result<Vec<T>> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; 65_535];
        sock.set_read_timeout(Some(timeout))?;
        loop {
            match sock.recv_from(&mut buf) {
                Ok((n, _src)) => {
                    if let Some(v) = parse(&buf[..n]) {
                        out.push(v);
                    }
                    // Subsequent iterations only drain what is already queued.
                    sock.set_read_timeout(Some(Duration::from_millis(1)))?;
                }
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
        }
        Ok(out)
    }
}
