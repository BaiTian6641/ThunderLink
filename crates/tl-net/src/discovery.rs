//! mDNS discovery of `_thunderlink._tcp` peers (SPEC §3).

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use parking_lot::Mutex;
use tl_proto::{Role, MDNS_SERVICE_TYPE};

#[derive(Clone, Debug)]
pub struct Peer {
    pub name: String,
    pub addrs: Vec<IpAddr>,
    pub port: u16,
    pub role: Role,
}

#[derive(Clone, Debug)]
pub enum DiscoveryEvent {
    Added(Peer),
    Removed(String),
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::Initiator => "initiator",
        Role::Target => "target",
    }
}

fn parse_role(s: &str) -> Option<Role> {
    match s {
        "initiator" => Some(Role::Initiator),
        "target" => Some(Role::Target),
        _ => None,
    }
}

fn to_io(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

fn sanitize_host_label(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() || c == '-' {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "thunderlink".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Instance name from a full service name:
/// `"desk._thunderlink._tcp.local."` → `"desk"`.
fn instance_name(fullname: &str) -> String {
    fullname
        .strip_suffix(MDNS_SERVICE_TYPE)
        .and_then(|s| s.strip_suffix('.'))
        .unwrap_or(fullname)
        .to_string()
}

/// Announces `_thunderlink._tcp` until dropped (SPEC §3).
pub struct Announcer {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Announcer {
    pub fn start(name: &str, role: Role, port: u16) -> io::Result<Self> {
        let daemon = ServiceDaemon::new().map_err(to_io)?;
        let host = format!("{}.local.", sanitize_host_label(name));
        let mut props = HashMap::new();
        props.insert("role".to_string(), role_str(role).to_string());
        let info = ServiceInfo::new(MDNS_SERVICE_TYPE, name, &host, "", port, props)
            .map_err(to_io)?
            // Announce whatever addresses the host actually has; the
            // daemon keeps them current as interfaces change.
            .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        if let Err(e) = daemon.register(info) {
            let _ = daemon.shutdown();
            return Err(to_io(e));
        }
        log::info!("announced {fullname} role={} port={port}", role_str(role));
        Ok(Self { daemon, fullname })
    }

    /// Full service name as registered (e.g. `"desk._thunderlink._tcp.local."`).
    pub fn fullname(&self) -> &str {
        &self.fullname
    }
}

impl Drop for Announcer {
    fn drop(&mut self) {
        if let Err(e) = self.daemon.unregister(&self.fullname) {
            log::debug!("unregister failed during drop: {e}");
        }
        if let Err(e) = self.daemon.shutdown() {
            log::debug!("mDNS daemon shutdown failed during drop: {e}");
        }
    }
}

pub struct Browser {
    daemon: ServiceDaemon,
    rx: mdns_sd::Receiver<ServiceEvent>,
    /// fullname → last-announced peer record; suppresses duplicate Added
    /// events when a service re-resolves unchanged, and lets Removed carry
    /// the exact name that was Added.
    known: Mutex<HashMap<String, Peer>>,
}

impl Browser {
    pub fn start() -> io::Result<Self> {
        let daemon = ServiceDaemon::new().map_err(to_io)?;
        let rx = daemon.browse(MDNS_SERVICE_TYPE).map_err(to_io)?;
        Ok(Self {
            daemon,
            rx,
            known: Mutex::new(HashMap::new()),
        })
    }

    /// Wait up to `timeout` for the next add/remove event.
    pub fn next_event(&self, timeout: Duration) -> Option<DiscoveryEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let event = self.rx.recv_timeout(deadline - now).ok()?;
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    // Peers without a usable role TXT are not ThunderLink
                    // nodes; keep waiting.
                    let Some(role) = info
                        .get_property_val_str("role")
                        .and_then(parse_role)
                    else {
                        continue;
                    };
                    let fullname = info.get_fullname().to_string();
                    let mut addrs: Vec<IpAddr> =
                        info.get_addresses().iter().copied().collect();
                    addrs.sort_unstable();
                    let peer = Peer {
                        name: instance_name(&fullname),
                        addrs,
                        port: info.get_port(),
                        role,
                    };
                    {
                        let mut known = self.known.lock();
                        let unchanged = known.get(&fullname).is_some_and(|old| {
                            old.name == peer.name
                                && old.addrs == peer.addrs
                                && old.port == peer.port
                                && old.role == peer.role
                        });
                        if unchanged {
                            continue; // re-resolve of a known peer
                        }
                        known.insert(fullname, peer.clone());
                    }
                    return Some(DiscoveryEvent::Added(peer));
                }
                ServiceEvent::ServiceRemoved(_ty, fullname) => {
                    let name = self
                        .known
                        .lock()
                        .remove(&fullname)
                        .map(|p| p.name)
                        .unwrap_or_else(|| instance_name(&fullname));
                    return Some(DiscoveryEvent::Removed(name));
                }
                // SearchStarted / ServiceFound (unresolved) / SearchStopped:
                // keep waiting for an actionable event.
                _ => continue,
            }
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Err(e) = self.daemon.shutdown() {
            log::debug!("mDNS daemon shutdown failed during drop: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_and_role_helpers() {
        assert_eq!(
            instance_name("desk._thunderlink._tcp.local."),
            "desk"
        );
        assert_eq!(instance_name("garbage"), "garbage");
        assert_eq!(sanitize_host_label("My Desk (Office)"), "my-desk-office");
        assert_eq!(sanitize_host_label("---"), "thunderlink");
        assert_eq!(parse_role("initiator"), Some(Role::Initiator));
        assert_eq!(parse_role("target"), Some(Role::Target));
        assert_eq!(parse_role("bogus"), None);
    }

    /// Full announce/browse/removal loop over the local multicast group.
    /// Requires a multicast-capable interface; gated per SPEC §9.
    #[test]
    fn mdns_announce_browse_remove_loopback() {
        if std::env::var("TL_E2E").as_deref() != Ok("1") {
            eprintln!("skipping mDNS e2e test (set TL_E2E=1 to enable)");
            return;
        }
        let ann = Announcer::start("tl-test-node", Role::Target, 47776).unwrap();
        assert_eq!(
            ann.fullname(),
            "tl-test-node._thunderlink._tcp.local."
        );
        let browser = Browser::start().unwrap();

        let deadline = Instant::now() + Duration::from_secs(15);
        let peer = loop {
            assert!(Instant::now() < deadline, "peer never discovered");
            if let Some(DiscoveryEvent::Added(p)) =
                browser.next_event(Duration::from_millis(500))
            {
                if p.name == "tl-test-node" {
                    break p;
                }
            }
        };
        assert_eq!(peer.port, 47776);
        assert_eq!(peer.role, Role::Target);

        drop(ann); // goodbye packets → Removed
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "Removed event never arrived");
            if let Some(DiscoveryEvent::Removed(name)) =
                browser.next_event(Duration::from_millis(500))
            {
                if name == "tl-test-node" {
                    break;
                }
            }
        }
    }
}
