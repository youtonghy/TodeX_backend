//! Which pages the agent browser (and the clients' browser preview) may
//! show at the top level: loopback http(s) without credentials, never the
//! daemon's own port. Everything that decides a URL lives here so the MCP
//! tools, the navigation guard and `/v2/browser/fetch` agree.
//!
//! Subframes and subresources are not restricted: a local page may embed
//! `data:` or external content, but it can never replace itself with a
//! non-local top-level page (redirects included: Chromium pauses each hop
//! as a new top-level `Document` request).

use std::net::IpAddr;

/// Longest URL accepted from an agent or client.
const MAX_URL_BYTES: usize = 2048;

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
/// pair it with [`validate_url`] or use [`allowed_top_level`].
pub(crate) fn is_allowed_target(url: &reqwest::Url) -> bool {
    let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false);
    is_loopback && url.username().is_empty() && url.password().is_none()
}

/// A URL an agent asked to open: valid, local, and with a readable reason
/// when it is not.
pub(crate) fn agent_url(raw: &str) -> Result<String, String> {
    let url = validate_url(raw)?;
    let parsed = reqwest::Url::parse(&url).map_err(|error| error.to_string())?;
    if !is_allowed_target(&parsed) {
        return Err(format!(
            "{url} is not a local page. Only localhost, 127.0.0.1 and [::1] URLs can be opened; \
             pages may still load external resources."
        ));
    }
    Ok(url)
}

/// The port a URL opens, for the daemon-port check.
pub(crate) fn url_port(url: &str) -> Option<u16> {
    reqwest::Url::parse(url).ok()?.port_or_known_default()
}

/// Whether `port` is the daemon's own listener.
pub(crate) fn is_daemon_port(port: Option<u16>) -> bool {
    port.is_some() && port == super::daemon_port()
}

/// Whether the tab may show `url` as its page: `about:blank`, or a local
/// http(s) page other than the daemon itself. `data:`, `file:`, `blob:`,
/// `javascript:` and every remote host are refused.
pub(crate) fn allowed_top_level(url: &str) -> bool {
    if url == "about:blank" {
        return true;
    }
    reqwest::Url::parse(url).is_ok_and(|parsed| {
        matches!(parsed.scheme(), "http" | "https")
            && is_allowed_target(&parsed)
            && !is_daemon_port(parsed.port_or_known_default())
    })
}

/// What the navigation guard does with a paused request.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Navigation {
    Continue,
    /// Cancel it and keep the current page (`Fetch.failRequest` `Aborted`).
    Block,
}

/// A paused `Document` request: only the main frame is guarded, so iframes
/// may load anything a page could embed anyway.
pub(crate) fn guard_request(url: &str, resource_type: &str, main_frame: bool) -> Navigation {
    if resource_type == "Document" && main_frame && !allowed_top_level(url) {
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

pub(crate) fn popup(url: &str) -> Popup {
    if url != "about:blank" && allowed_top_level(url) {
        Popup::LoadInTab
    } else {
        Popup::Refuse
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_urls_are_local_http_pages() {
        assert_eq!(
            agent_url("http://localhost:5173").unwrap(),
            "http://localhost:5173/"
        );
        assert!(agent_url("http://[::1]:3000/").is_ok());
        assert!(agent_url(" https://127.0.0.1/x ").is_ok());
        for url in [
            "https://example.com",
            "http://user:pw@localhost/",
            "http://localhost.evil/",
            "http://127.0.0.1.nip.io/",
            "http://192.168.1.2/",
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
        // A local page navigating away (or a redirect hop to a remote host,
        // which Chromium pauses as another Document request) is cancelled.
        assert_eq!(
            guard_request("https://evil.example/", "Document", true),
            Navigation::Block
        );
        assert_eq!(
            guard_request("http://localhost:3000/next", "Document", true),
            Navigation::Continue
        );
        for url in [
            "data:text/html,x",
            "file:///etc/hosts",
            "javascript:void(0)",
        ] {
            assert_eq!(guard_request(url, "Document", true), Navigation::Block);
        }
        // Iframes and subresources are not top-level pages.
        assert_eq!(
            guard_request("https://evil.example/", "Document", false),
            Navigation::Continue
        );
        assert_eq!(
            guard_request("data:text/html,x", "Document", false),
            Navigation::Continue
        );
        assert_eq!(
            guard_request("https://cdn.example/app.js", "Script", true),
            Navigation::Continue
        );
    }

    #[test]
    fn popups_fold_into_the_tab_only_when_local() {
        assert_eq!(popup("http://localhost:5173/login"), Popup::LoadInTab);
        for url in [
            "about:blank",
            "https://example.com/",
            "data:text/html,x",
            "blob:http://localhost:5173/0b5c",
        ] {
            assert_eq!(popup(url), Popup::Refuse, "{url}");
        }
    }
}
