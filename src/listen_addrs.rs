//! Resolves the addresses clients can use to reach the listener.
//!
//! Binding to an unspecified host (`0.0.0.0` / `::`) accepts connections on
//! every interface, but that host is not itself connectable. These helpers
//! expand it into concrete interface addresses so operators can see which IP
//! to enter on a device.

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectAddress {
    /// Host as it appears in a URL authority (IPv6 is bracketed).
    pub host: String,
    /// Interface owning the address; `None` for an explicitly configured host.
    pub interface: Option<String>,
}

impl ConnectAddress {
    pub fn ws_url(&self, port: u16) -> String {
        format!("ws://{}:{port}/v2/ws", self.host)
    }
}

impl fmt::Display for ConnectAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.interface {
            Some(interface) => write!(f, "{} ({interface})", self.host),
            None => f.write_str(&self.host),
        }
    }
}

/// Returns the hosts clients can connect to for `bind_host`, preferred first.
///
/// An unspecified bind host expands to the addresses of every interface that
/// is up, ordered default-route IPv4 first, then other IPv4, then IPv6, with
/// loopback last. Any other host is returned unchanged.
pub fn connect_addresses(bind_host: &str) -> io::Result<Vec<ConnectAddress>> {
    let host = bind_host.trim();
    let bind_ip = match host.parse::<IpAddr>() {
        Ok(ip) if ip.is_unspecified() => ip,
        Ok(ip) => {
            return Ok(vec![ConnectAddress {
                host: url_host(ip),
                interface: None,
            }])
        }
        Err(_) => {
            return Ok(vec![ConnectAddress {
                host: host.to_owned(),
                interface: None,
            }])
        }
    };
    let interfaces = if_addrs::get_if_addrs()?
        .into_iter()
        .filter(|interface| interface.is_oper_up())
        .map(|interface| {
            let ip = interface.ip();
            (interface.name, ip)
        });
    Ok(select_addresses(bind_ip, interfaces, default_route_ipv4()))
}

/// Local IPv4 address the OS would use for outbound traffic, if any.
///
/// Connecting a UDP socket only selects a route; no packet is sent.
pub fn default_route_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

fn select_addresses(
    bind_ip: IpAddr,
    interfaces: impl IntoIterator<Item = (String, IpAddr)>,
    primary: Option<Ipv4Addr>,
) -> Vec<ConnectAddress> {
    let mut seen = HashSet::new();
    let mut candidates: Vec<(String, IpAddr)> = interfaces
        .into_iter()
        .filter(|(_, ip)| accepts(bind_ip, *ip) && is_connectable(*ip) && seen.insert(*ip))
        .collect();
    // Stable sort keeps the OS interface order within each group.
    candidates.sort_by_key(|(_, ip)| {
        (
            ip.is_loopback(),
            primary.map(IpAddr::V4) != Some(*ip),
            ip.is_ipv6(),
        )
    });
    candidates
        .into_iter()
        .map(|(name, ip)| ConnectAddress {
            host: url_host(ip),
            interface: Some(name),
        })
        .collect()
}

fn accepts(bind_ip: IpAddr, ip: IpAddr) -> bool {
    match bind_ip {
        IpAddr::V4(_) => ip.is_ipv4(),
        // A `::` listener is dual-stack by default, except on Windows where
        // IPV6_V6ONLY defaults to on.
        IpAddr::V6(_) => ip.is_ipv6() || cfg!(not(windows)),
    }
}

fn is_connectable(ip: IpAddr) -> bool {
    // Link-local addresses are skipped: IPv6 ones need a zone id to be usable
    // and IPv4 ones only appear when DHCP failed.
    !ip.is_unspecified()
        && !ip.is_multicast()
        && match ip {
            IpAddr::V4(ip) => !ip.is_link_local(),
            IpAddr::V6(ip) => !ip.is_unicast_link_local(),
        }
}

fn url_host(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(name: &str, ip: &str) -> (String, IpAddr) {
        (name.to_owned(), ip.parse().unwrap())
    }

    fn hosts(addresses: &[ConnectAddress]) -> Vec<&str> {
        addresses
            .iter()
            .map(|address| address.host.as_str())
            .collect()
    }

    #[test]
    fn explicit_hosts_are_returned_unchanged() {
        assert_eq!(
            connect_addresses("192.168.1.20").unwrap(),
            vec![ConnectAddress {
                host: "192.168.1.20".to_owned(),
                interface: None,
            }]
        );
        assert_eq!(hosts(&connect_addresses("::1").unwrap()), ["[::1]"]);
        assert_eq!(
            hosts(&connect_addresses(" todex.local ").unwrap()),
            ["todex.local"]
        );
    }

    #[test]
    fn unspecified_ipv4_lists_reachable_ipv4_with_primary_first_and_loopback_last() {
        let interfaces = [
            iface("lo0", "127.0.0.1"),
            iface("docker0", "172.17.0.1"),
            iface("en0", "192.168.1.20"),
            iface("en0", "fe80::1"),
            iface("en0", "2001:db8::20"),
            iface("en5", "169.254.10.2"),
            iface("en1", "192.168.1.20"),
        ];
        let addresses = select_addresses(
            "0.0.0.0".parse().unwrap(),
            interfaces,
            Some("192.168.1.20".parse().unwrap()),
        );

        assert_eq!(
            addresses,
            vec![
                ConnectAddress {
                    host: "192.168.1.20".to_owned(),
                    interface: Some("en0".to_owned()),
                },
                ConnectAddress {
                    host: "172.17.0.1".to_owned(),
                    interface: Some("docker0".to_owned()),
                },
                ConnectAddress {
                    host: "127.0.0.1".to_owned(),
                    interface: Some("lo0".to_owned()),
                },
            ]
        );
        assert_eq!(addresses[0].to_string(), "192.168.1.20 (en0)");
        assert_eq!(addresses[0].ws_url(7345), "ws://192.168.1.20:7345/v2/ws");
    }

    #[test]
    fn unspecified_ipv6_lists_ipv6_and_dual_stack_ipv4() {
        let interfaces = [
            iface("lo0", "::1"),
            iface("en0", "2001:db8::20"),
            iface("en0", "fe80::1"),
            iface("en0", "192.168.1.20"),
        ];
        let addresses = select_addresses("::".parse().unwrap(), interfaces, None);

        let expected: &[&str] = if cfg!(windows) {
            &["[2001:db8::20]", "[::1]"]
        } else {
            &["192.168.1.20", "[2001:db8::20]", "[::1]"]
        };
        assert_eq!(hosts(&addresses), expected);
    }
}
