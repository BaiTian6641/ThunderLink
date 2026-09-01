//! Best-effort Thunderbolt/USB4 bridge interface detection (SPEC §3).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};

#[derive(Clone, Debug)]
pub struct NetInterface {
    pub name: String,
    pub addrs: Vec<IpAddr>,
}

/// Environment override forcing one explicit interface name.
pub const IFACE_ENV: &str = "TL_IFACE";

fn is_thunderbolt_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    // Linux: thunderbolt0, ...; Windows & display names: "Thunderbolt".
    if lower.contains("thunderbolt") {
        return true;
    }
    // macOS: the Thunderbolt Bridge shows up as bridge100 and above.
    #[cfg(target_os = "macos")]
    if let Some(rest) = lower.strip_prefix("bridge") {
        if let Ok(n) = rest.parse::<u32>() {
            return n >= 100;
        }
    }
    false
}

fn collect_by_name() -> Vec<NetInterface> {
    let ifs = match if_addrs::get_if_addrs() {
        Ok(ifs) => ifs,
        Err(e) => {
            log::warn!("interface enumeration failed: {e}");
            return Vec::new();
        }
    };
    let mut by_name: HashMap<String, Vec<IpAddr>> = HashMap::new();
    for iface in ifs {
        let ip = match iface.addr {
            if_addrs::IfAddr::V4(a) => IpAddr::V4(a.ip),
            if_addrs::IfAddr::V6(a) => IpAddr::V6(a.ip),
        };
        by_name.entry(iface.name).or_default().push(ip);
    }
    by_name
        .into_iter()
        .map(|(name, addrs)| NetInterface { name, addrs })
        .collect()
}

/// Best-effort Thunderbolt/USB4 bridge detection (macOS: bridge100+ or
/// "Thunderbolt" interfaces; Linux: thunderbolt*; Windows: "Thunderbolt").
/// `TL_IFACE` env var forces one explicit interface name.
pub fn thunderbolt_interfaces() -> Vec<NetInterface> {
    let forced = std::env::var(IFACE_ENV).ok().filter(|s| !s.is_empty());
    let all = collect_by_name();
    let mut out: Vec<NetInterface> = all
        .into_iter()
        .filter(|i| match &forced {
            Some(want) => &i.name == want,
            None => is_thunderbolt_name(&i.name),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    if let Some(want) = &forced {
        if out.is_empty() {
            log::warn!("{IFACE_ENV}={want} set but no such interface exists");
        }
    }
    out
}

/// Link-local IPv6 address of an interface, if any.
pub fn link_local_v6(iface: &str) -> Option<Ipv6Addr> {
    let ifs = if_addrs::get_if_addrs().ok()?;
    let mut found = None;
    for i in ifs {
        if i.name != iface {
            continue;
        }
        if let if_addrs::IfAddr::V6(v6) = i.addr {
            let ip = v6.ip;
            if ip.is_unicast_link_local() {
                // Deterministic choice if several exist.
                if found.is_none_or(|cur: Ipv6Addr| ip < cur) {
                    found = Some(ip);
                }
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_never_panics_with_or_without_override() {
        // No override: may or may not find Thunderbolt interfaces, but
        // must not panic.
        let _ = thunderbolt_interfaces();

        // Forced to a nonexistent interface: empty + warning, no panic.
        std::env::set_var(IFACE_ENV, "definitely-not-an-iface-xyz");
        assert!(thunderbolt_interfaces().is_empty());

        // Forced to the loopback, which always exists on macOS/Linux.
        std::env::set_var(IFACE_ENV, "lo0");
        let v = thunderbolt_interfaces();
        assert!(v.iter().any(|i| i.name == "lo0"), "expected lo0, got {v:?}");

        std::env::remove_var(IFACE_ENV);
        let _ = thunderbolt_interfaces();

        let _ = link_local_v6("lo0");
        assert!(link_local_v6("definitely-not-an-iface-xyz").is_none());
    }

    #[test]
    fn name_classifier() {
        assert!(is_thunderbolt_name("thunderbolt0"));
        assert!(is_thunderbolt_name("Thunderbolt Ethernet"));
        #[cfg(target_os = "macos")]
        {
            assert!(is_thunderbolt_name("bridge100"));
            assert!(is_thunderbolt_name("bridge101"));
            assert!(!is_thunderbolt_name("bridge0"));
        }
        assert!(!is_thunderbolt_name("en0"));
        assert!(!is_thunderbolt_name("lo0"));
    }
}
