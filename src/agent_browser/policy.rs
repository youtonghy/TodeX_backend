//! Which pages the agent browser may show and reach, and what the clients'
//! browser preview may show. Everything that decides a URL lives here so
//! the MCP tools, the navigation guard and `/v2/browser/fetch` agree.
//!
//! The clients' preview shows loopback http(s) pages only
//! ([`is_allowed_target`]). The agent's tab also shows hosts the person
//! allowed ([`super::hosts`]), never with credentials in the URL, never a
//! link-local or cloud metadata address, never the daemon's own port.
//! Every request the tab makes is judged too ([`guard_request`]): public
//! and loopback destinations are reachable, private-network ones only when
//! their host was allowed. A redirect hop is a new request, judged again.

use std::net::IpAddr;

/// Longest URL accepted from an agent or client.
const MAX_URL_BYTES: usize = 2048;

/// How far a destination is from this computer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reach {
    /// This computer (loopback, `localhost`).
    Local,
    /// The local network: RFC 1918, carrier-grade NAT, unique-local IPv6,
    /// `.local` and similar names.
    Private,
    Public,
    /// Never: link-local (cloud metadata among it), unspecified, broadcast
    /// and multicast addresses.
    Forbidden,
}

pub(crate) fn ip_reach(ip: IpAddr) -> Reach {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            if ip.is_loopback() {
                Reach::Local
            } else if ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || a == 0
            {
                Reach::Forbidden
            } else if ip.is_private() || (a == 100 && (64..128).contains(&b)) {
                Reach::Private
            } else {
                Reach::Public
            }
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return ip_reach(IpAddr::V4(v4));
            }
            let first = ip.segments()[0];
            if ip.is_loopback() {
                Reach::Local
            } else if ip.is_unspecified()
                || ip.is_multicast()
                || (first & 0xffc0) == 0xfe80
                // AWS's IPv6 metadata address.
                || ip == std::net::Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254)
            {
                Reach::Forbidden
            } else if (first & 0xfe00) == 0xfc00 {
                Reach::Private
            } else {
                Reach::Public
            }
        }
    }
}

/// What a host is known to be without asking DNS: `localhost` names, IP
/// literals and local-network suffixes; `None` for other names.
pub(crate) fn literal_reach(host: &str) -> Option<Reach> {
    let host = host.trim_matches(['[', ']']).to_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return Some(Reach::Local);
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(ip_reach(ip));
    }
    [".local", ".lan", ".home.arpa", ".internal", ".intranet"]
        .iter()
        .any(|suffix| host.ends_with(suffix))
        .then_some(Reach::Private)
}

/// Parses an http(s) URL with a host, normalizing an empty path to `/`.
pub(crate) fn validate_url(raw: &str) -> Result<String, String> {
    let value = raw.trim();
    let invalid = || "only valid http and https URLs are allowed".to_owned();
    let mut parsed = reqwest::Url::parse(value).map_err(|_| invalid())?;
    if value.len() > MAX_URL_BYTES
        || !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
    {
        return Err(invalid());
    }
    if parsed.path().is_empty() {
        parsed.set_path("/");
    }
    Ok(parsed.to_string())
}

/// Loopback host without credentials. Does not look at the scheme; callers
/// pair it with [`validate_url`] or use [`allowed_top_level_for`].
pub(crate) fn is_allowed_target(url: &reqwest::Url) -> bool {
    let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false);
    is_loopback && url.username().is_empty() && url.password().is_none()
}

/// A URL an agent asked to open: a valid http(s) URL without credentials
/// that is not a forbidden address, or a readable reason. Whether its host
/// needs the person's approval is [`approval_host`]'s question.
pub(crate) fn agent_url(raw: &str) -> Result<String, String> {
    let url = validate_url(raw)?;
    let parsed = reqwest::Url::parse(&url).map_err(|error| error.to_string())?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(format!(
            "{url} carries credentials; the agent browser opens URLs without them."
        ));
    }
    if parsed
        .host_str()
        .and_then(literal_reach)
        .is_some_and(|reach| reach == Reach::Forbidden)
    {
        return Err(format!(
            "{url} is a link-local or cloud metadata address, which the agent browser never opens."
        ));
    }
    Ok(url)
}

/// The host whose approval opening `url` needs: every host but this
/// computer's.
pub(crate) fn approval_host(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    if is_allowed_target(&parsed) {
        return None;
    }
    super::hosts::host_key(&parsed)
}

/// The port a URL opens, for the daemon-port check.
pub(crate) fn url_port(url: &str) -> Option<u16> {
    reqwest::Url::parse(url).ok()?.port_or_known_default()
}

/// Whether `port` is the daemon's own listener.
pub(crate) fn is_daemon_port(port: Option<u16>) -> bool {
    port.is_some() && port == super::daemon_port()
}

/// Whether the agent's tab may show `url`: `about:blank`, a local http(s)
/// page other than the daemon itself, or a page of a host `approved` says
/// the person allowed (never a forbidden address). `data:`, `file:`,
/// `blob:` and `javascript:` are refused.
pub(crate) fn allowed_top_level_for(url: &str, approved: impl Fn(&str) -> bool) -> bool {
    if url == "about:blank" {
        return true;
    }
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return false;
    }
    if is_allowed_target(&parsed) {
        return !is_daemon_port(parsed.port_or_known_default());
    }
    let Some(host) = super::hosts::host_key(&parsed) else {
        return false;
    };
    literal_reach(&host) != Some(Reach::Forbidden) && approved(&host)
}

/// What the request guard does with a paused request.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Navigation {
    Continue,
    /// Cancel it (`Fetch.failRequest` `Aborted`; a page keeps showing).
    Block,
    /// Its host's addresses decide: see [`judge_addresses`].
    Resolve(String),
}

/// A paused request of the agent's tab. The main frame's documents are
/// pages the tab would show ([`allowed_top_level_for`]). Everything else
/// (iframes, scripts, `fetch`) may reach public addresses and this computer
/// (not the daemon), and the local network only on hosts `approved` allows.
pub(crate) fn guard_request(
    url: &str,
    resource_type: &str,
    main_frame: bool,
    approved: impl Fn(&str) -> bool,
) -> Navigation {
    if resource_type == "Document" && main_frame {
        return if allowed_top_level_for(url, approved) {
            Navigation::Continue
        } else {
            Navigation::Block
        };
    }
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return Navigation::Continue;
    };
    // data:, blob: and the like reach no host.
    if !matches!(parsed.scheme(), "http" | "https" | "ws" | "wss") {
        return Navigation::Continue;
    }
    let Some(host) = super::hosts::host_key(&parsed) else {
        return Navigation::Block;
    };
    let daemon = is_daemon_port(parsed.port_or_known_default());
    match literal_reach(&host) {
        Some(Reach::Forbidden) => Navigation::Block,
        Some(Reach::Local) if daemon => Navigation::Block,
        Some(Reach::Local | Reach::Public) => Navigation::Continue,
        Some(Reach::Private) if approved(&host) => Navigation::Continue,
        Some(Reach::Private) => Navigation::Block,
        None if approved(&host) => Navigation::Continue,
        None => Navigation::Resolve(host),
    }
}

/// [`Navigation::Resolve`] once the host's addresses are known (none: it
/// does not resolve, and the request fails on its own). `daemon`: the
/// request goes to the daemon's port.
pub(crate) fn judge_addresses(reaches: &[Reach], daemon: bool) -> Navigation {
    let blocked = reaches.iter().any(|reach| match reach {
        Reach::Forbidden | Reach::Private => true,
        Reach::Local => daemon,
        Reach::Public => false,
    });
    if blocked {
        Navigation::Block
    } else {
        Navigation::Continue
    }
}

/// What happens to a window a page opened (`window.open`, `target=_blank`).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Popup {
    /// Close it and load the URL in the agent's tab instead.
    LoadInTab,
    /// Close it; nothing loads.
    Refuse,
}

pub(crate) fn popup(url: &str, approved: impl Fn(&str) -> bool) -> Popup {
    if url != "about:blank" && allowed_top_level_for(url, approved) {
        Popup::LoadInTab
    } else {
        Popup::Refuse
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the agent's tab shows with no host allowed.
    fn allowed_top_level(url: &str) -> bool {
        allowed_top_level_for(url, |_| false)
    }

    #[test]
    fn agent_urls_are_http_pages_without_credentials_or_forbidden_hosts() {
        assert_eq!(
            agent_url("http://localhost:5173").unwrap(),
            "http://localhost:5173/"
        );
        assert!(agent_url("http://[::1]:3000/").is_ok());
        assert!(agent_url(" https://127.0.0.1/x ").is_ok());
        // Beyond this computer: allowed to ask for, approval decides.
        assert!(agent_url("https://example.com").is_ok());
        assert!(agent_url("http://192.168.1.2/").is_ok());
        assert_eq!(
            approval_host("https://Example.com/x").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            approval_host("http://192.168.1.2/").as_deref(),
            Some("192.168.1.2")
        );
        assert_eq!(approval_host("http://localhost:5173/"), None);
        for url in [
            "http://user:pw@localhost/",
            "https://user@example.com/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[fe80::1]/",
            "http://0.0.0.0:8080/",
            "file:///etc/passwd",
            "data:text/html,<h1>x</h1>",
            "javascript:alert(1)",
            "blob:http://localhost:5173/0b5c",
            "about:blank",
            "localhost:5173",
        ] {
            assert!(agent_url(url).is_err(), "{url}");
        }
        assert!(agent_url(&format!("http://localhost/{}", "a".repeat(MAX_URL_BYTES))).is_err());
    }

    #[test]
    fn approved_hosts_join_the_pages_the_agent_tab_shows() {
        let approved = |host: &str| host == "example.com" || host == "192.168.8.1";
        assert!(allowed_top_level_for("https://example.com/a", approved));
        assert!(allowed_top_level_for("http://192.168.8.1/", approved));
        assert!(!allowed_top_level_for("https://other.example/", approved));
        assert!(!allowed_top_level_for(
            "https://user:pw@example.com/",
            approved
        ));
        // Approval never opens a forbidden address.
        assert!(!allowed_top_level_for("http://169.254.169.254/", |_| true));
        // Nothing allowed: local pages only.
        assert!(!allowed_top_level("https://example.com/a"));
        assert_eq!(popup("https://example.com/b", approved), Popup::LoadInTab);
        assert_eq!(popup("https://other.example/", approved), Popup::Refuse);
    }

    #[test]
    fn requests_reach_the_local_network_only_on_allowed_hosts() {
        let none = |_: &str| false;
        let lan = |host: &str| host == "192.168.8.1";
        assert_eq!(
            guard_request("https://140.82.112.5/zen", "Fetch", true, none),
            Navigation::Continue
        );
        assert_eq!(
            guard_request("http://127.0.0.1:5173/x.js", "Script", true, none),
            Navigation::Continue
        );
        assert_eq!(
            guard_request("http://192.168.8.1/", "Fetch", true, none),
            Navigation::Block
        );
        assert_eq!(
            guard_request("http://192.168.8.1/", "Fetch", true, lan),
            Navigation::Continue
        );
        assert_eq!(
            guard_request("http://10.0.0.2/", "Image", false, lan),
            Navigation::Block
        );
        assert_eq!(
            guard_request("http://printer.local/", "Fetch", true, none),
            Navigation::Block
        );
        assert_eq!(
            guard_request("http://169.254.169.254/", "Fetch", true, |_| true),
            Navigation::Block
        );
        assert_eq!(
            guard_request("data:text/plain,x", "Fetch", true, none),
            Navigation::Continue
        );
        // A name: its addresses decide.
        assert_eq!(
            guard_request("https://cdn.example/app.js", "Script", true, none),
            Navigation::Resolve("cdn.example".to_owned())
        );
        assert_eq!(
            judge_addresses(&[Reach::Public], false),
            Navigation::Continue
        );
        assert_eq!(
            judge_addresses(&[Reach::Public, Reach::Private], false),
            Navigation::Block
        );
        assert_eq!(
            judge_addresses(&[Reach::Forbidden], false),
            Navigation::Block
        );
        assert_eq!(
            judge_addresses(&[Reach::Local], false),
            Navigation::Continue
        );
        assert_eq!(judge_addresses(&[Reach::Local], true), Navigation::Block);
    }

    #[test]
    fn addresses_are_told_apart() {
        let reach = |ip: &str| ip_reach(ip.parse().unwrap());
        assert_eq!(reach("127.0.0.1"), Reach::Local);
        assert_eq!(reach("::1"), Reach::Local);
        for private in [
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.1",
            "192.168.8.216",
            "100.64.0.1",
            "fd12::1",
            "::ffff:192.168.1.1",
        ] {
            assert_eq!(reach(private), Reach::Private, "{private}");
        }
        for forbidden in [
            "169.254.169.254",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "fe80::1",
            "fd00:ec2::254",
            "::",
        ] {
            assert_eq!(reach(forbidden), Reach::Forbidden, "{forbidden}");
        }
        for public in ["8.8.8.8", "172.32.0.1", "100.128.0.1", "2606:4700::1"] {
            assert_eq!(reach(public), Reach::Public, "{public}");
        }
        assert_eq!(literal_reach("Router.LAN"), Some(Reach::Private));
        assert_eq!(literal_reach("app.localhost"), Some(Reach::Local));
        assert_eq!(literal_reach("example.com"), None);
    }

    #[test]
    fn top_level_pages_stay_local_and_never_reach_the_daemon() {
        for allowed in [
            "http://localhost:5173/",
            "https://127.0.0.1/x",
            "http://127.8.9.10:8080/",
            "http://[::1]:8080",
            "about:blank",
        ] {
            assert!(allowed_top_level(allowed), "{allowed}");
        }
        for blocked in [
            "https://example.com",
            "file:///etc/passwd",
            "http://user:pw@localhost/",
            "javascript:alert(1)",
            "data:text/html,hi",
            "blob:http://localhost:5173/0b5c",
            "about:srcdoc",
            "chrome://settings",
            "http://192.168.1.2/",
            "ws://localhost:5173/",
        ] {
            assert!(!allowed_top_level(blocked), "{blocked}");
        }
    }

    #[test]
    fn the_guard_checks_main_frame_documents_including_redirects() {
        let none = |_: &str| false;
        // A local page navigating away (or a redirect hop to a host not
        // allowed, which is paused as another Document request) is
        // cancelled.
        assert_eq!(
            guard_request("https://evil.example/", "Document", true, none),
            Navigation::Block
        );
        assert_eq!(
            guard_request("http://localhost:3000/next", "Document", true, none),
            Navigation::Continue
        );
        for url in [
            "data:text/html,x",
            "file:///etc/hosts",
            "javascript:void(0)",
        ] {
            assert_eq!(
                guard_request(url, "Document", true, |_| true),
                Navigation::Block
            );
        }
        // Iframes are judged like any other request.
        assert_eq!(
            guard_request("https://evil.example/", "Document", false, none),
            Navigation::Resolve("evil.example".to_owned())
        );
        assert_eq!(
            guard_request("data:text/html,x", "Document", false, none),
            Navigation::Continue
        );
    }

    #[test]
    fn popups_fold_into_the_tab_only_when_shown_there() {
        let none = |_: &str| false;
        assert_eq!(popup("http://localhost:5173/login", none), Popup::LoadInTab);
        for url in [
            "about:blank",
            "https://example.com/",
            "data:text/html,x",
            "blob:http://localhost:5173/0b5c",
        ] {
            assert_eq!(popup(url, none), Popup::Refuse, "{url}");
        }
    }
}
