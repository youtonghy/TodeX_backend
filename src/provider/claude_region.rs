//! Fail-closed egress preflight for managed Claude Code. This checks the
//! actual destination, not the IP used to reach the geolocation service.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::error::AppError;

const API: &str = "api.anthropic.com";
const ACCOUNT: &str = "claude.ai";
const OAUTH: &str = "platform.claude.com";
const REPORT: &str = "https://ipx.al/api/report/";
const BUDGET: Duration = Duration::from_secs(10);
#[cfg(not(test))]
const PROXY_KEYS: &[&str] = &["https_proxy", "HTTPS_PROXY", "http_proxy", "HTTP_PROXY"];
#[cfg(not(test))]
const BYPASS_KEYS: &[&str] = &["no_proxy", "NO_PROXY"];
#[cfg(not(test))]
const ROUTING_KEYS: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_CUSTOM_OAUTH_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "NODE_EXTRA_CA_CERTS",
];

/// The CLI may pick a different route for each host. An opaque/custom
/// upstream is not evidence of the egress that Anthropic sees.
#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct CliEgress {
    proxy: Option<String>,
    bypass: Option<String>,
    extra_ca: Option<PathBuf>,
    check_account: bool,
    #[cfg(test)]
    test_workspace: Option<PathBuf>,
}

impl std::fmt::Debug for CliEgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CliEgress(<redacted>)")
    }
}

impl CliEgress {
    pub(super) async fn read(workspace: Option<&Path>) -> Result<Self, AppError> {
        // Existing provider unit tests run fake CLIs without an external
        // dependency. Dedicated preflight tests exercise the real checker.
        #[cfg(test)]
        return Ok(Self {
            test_workspace: workspace.map(Path::to_path_buf),
            ..Self::default()
        });
        #[cfg(not(test))]
        Self::read_settings(workspace).await
    }

    #[cfg(not(test))]
    async fn read_settings(workspace: Option<&Path>) -> Result<Self, AppError> {
        let mut settings = BTreeMap::new();
        for file in settings_paths(&crate::agent_providers::claude_config_dir(), workspace).await? {
            let bytes = match tokio::fs::read(&file).await {
                Ok(bytes) if bytes.len() <= 1_048_576 => bytes,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                _ => return Err(AppError::ClaudeRegionUnverified),
            };
            let settings_json: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(unverified)?;
            let Some(env) = settings_json.get("env") else {
                continue;
            };
            let env = env.as_object().ok_or(AppError::ClaudeRegionUnverified)?;
            for key in PROXY_KEYS
                .iter()
                .chain(BYPASS_KEYS)
                .chain(ROUTING_KEYS)
                .copied()
            {
                let Some(value) = env.get(key) else { continue };
                let value = value.as_str().ok_or(AppError::ClaudeRegionUnverified)?;
                // Do not guess which of two disagreeing settings layers the
                // installed CLI will trust; credentials are only presence bits.
                let value = if matches!(key, "ANTHROPIC_API_KEY" | "ANTHROPIC_AUTH_TOKEN") {
                    if value.trim().is_empty() {
                        ""
                    } else {
                        "configured"
                    }
                } else {
                    value
                };
                if settings
                    .insert(key, value.to_owned())
                    .is_some_and(|old| old != value)
                {
                    return Err(AppError::ClaudeRegionUnverified);
                }
            }
        }
        // Use exactly the proxy and CA vars forwarded by secure_command. The
        // other routing variables enter this child through Claude settings.
        for key in PROXY_KEYS
            .iter()
            .chain(BYPASS_KEYS)
            .copied()
            .chain(["NODE_EXTRA_CA_CERTS"])
        {
            let Some(value) = std::env::var_os(key) else {
                continue;
            };
            let value = value.into_string().map_err(unverified)?;
            if settings
                .insert(key, value.clone())
                .is_some_and(|old| old != value)
            {
                return Err(AppError::ClaudeRegionUnverified);
            }
        }
        let get = |key| settings.get(key).filter(|value| !value.trim().is_empty());
        if [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ]
        .into_iter()
        .any(|key| get(key).is_some_and(|value| !matches!(value.as_str(), "0" | "false")))
            || get("CLAUDE_CODE_CUSTOM_OAUTH_URL").is_some()
        {
            return Err(AppError::ClaudeRegionUnverified);
        }
        if let Some(base) = get("ANTHROPIC_BASE_URL") {
            let url = reqwest::Url::parse(base).map_err(unverified)?;
            if !official_api(&url) {
                return Err(AppError::ClaudeRegionUnverified);
            }
        }
        Ok(Self {
            proxy: PROXY_KEYS.iter().find_map(|key| get(*key)).cloned(),
            bypass: BYPASS_KEYS.iter().find_map(|key| get(*key)).cloned(),
            extra_ca: get("NODE_EXTRA_CA_CERTS").map(PathBuf::from),
            check_account: get("ANTHROPIC_API_KEY").is_none()
                && get("ANTHROPIC_AUTH_TOKEN").is_none(),
        })
    }

    pub(super) async fn check(&self) -> Result<(), AppError> {
        if cfg!(test) {
            #[cfg(test)]
            if let Some(workspace) = &self.test_workspace {
                match tokio::fs::read_to_string(workspace.join("mock-claude-region")).await {
                    Ok(region) => return check_country(&country_code(region.trim())?),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                    Err(error) => return Err(unverified(error)),
                }
            }
            return Ok(());
        }
        let hosts: &[&str] = if self.check_account {
            &[API, ACCOUNT, OAUTH]
        } else {
            &[API]
        };
        self.check_hosts(hosts).await
    }

    async fn check_hosts(&self, hosts: &[&str]) -> Result<(), AppError> {
        tokio::time::timeout(BUDGET, async {
            let traces = futures_util::future::try_join_all(hosts.iter().map(|&host| async move {
                let client = self.client(host).await?;
                trace(&client, &format!("https://{host}/cdn-cgi/trace"), host).await
            }))
            .await?;
            let lookup = self.client("ipx.al").await?;
            check_reports(&lookup, REPORT, traces).await
        })
        .await
        .map_err(unverified)?
    }

    async fn client(&self, host: &str) -> Result<reqwest::Client, AppError> {
        let mut builder = base_client().no_proxy();
        if let Some(extra_ca) = &self.extra_ca {
            if !extra_ca.is_absolute() {
                return Err(AppError::ClaudeRegionUnverified);
            }
            let pem = tokio::fs::read(extra_ca).await.map_err(unverified)?;
            if pem.len() > 1_048_576 {
                return Err(AppError::ClaudeRegionUnverified);
            }
            for cert in reqwest::Certificate::from_pem_bundle(&pem).map_err(unverified)? {
                builder = builder.add_root_certificate(cert);
            }
        }
        if let Some(proxy) = &self.proxy {
            if !bypasses(self.bypass.as_deref().unwrap_or_default(), host)? {
                let url = reqwest::Url::parse(proxy).map_err(unverified)?;
                if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                    return Err(AppError::ClaudeRegionUnverified);
                }
                builder = builder.proxy(reqwest::Proxy::all(url).map_err(unverified)?);
            }
        }
        builder.build().map_err(unverified)
    }
}

pub(crate) async fn check_quota() -> Result<(), AppError> {
    if cfg!(test) {
        return Ok(());
    }
    CliEgress::read(None).await?.check().await
}

/// For daemon-owned HTTP requests, use the exact client sending the business
/// request (its proxy resolver need not be the same as the CLI's).
pub(crate) async fn check_http(
    client: &reqwest::Client,
    url: &reqwest::Url,
) -> Result<(), AppError> {
    if cfg!(test) {
        return Ok(());
    }
    if !official_api(url) {
        return Err(AppError::ClaudeRegionUnverified);
    }
    tokio::time::timeout(BUDGET, async {
        let outlet = trace(client, "https://api.anthropic.com/cdn-cgi/trace", API).await?;
        check_reports(client, REPORT, vec![outlet]).await
    })
    .await
    .map_err(unverified)?
}

fn official_api(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some(API)
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
}

#[cfg(not(test))]
async fn settings_paths(config: &Path, workspace: Option<&Path>) -> Result<Vec<PathBuf>, AppError> {
    let mut paths = vec![config.join("settings.json")];
    if let Some(workspace) = workspace {
        for dir in workspace.ancestors() {
            paths.extend([
                dir.join(".claude/settings.json"),
                dir.join(".claude/settings.local.json"),
            ]);
        }
    }
    #[cfg(target_os = "macos")]
    let managed = PathBuf::from("/Library/Application Support/ClaudeCode");
    #[cfg(target_os = "linux")]
    let managed = PathBuf::from("/etc/claude-code");
    #[cfg(target_os = "windows")]
    let managed = PathBuf::from(r"C:\Program Files\ClaudeCode");
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        paths.push(managed.join("managed-settings.json"));
        match tokio::fs::read_dir(managed.join("managed-settings.d")).await {
            Ok(mut entries) => {
                while let Some(entry) = entries.next_entry().await.map_err(unverified)? {
                    if entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "json")
                    {
                        paths.push(entry.path());
                        if paths.len() > 128 {
                            return Err(AppError::ClaudeRegionUnverified);
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(unverified(error)),
        }
    }
    Ok(paths)
}

fn bypasses(list: &str, host: &str) -> Result<bool, AppError> {
    let mut matched = false;
    for entry in list
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|s| !s.is_empty())
    {
        let (name, port) = match entry.rsplit_once(':') {
            Some((name, port)) if !name.contains(':') => (name, port.parse::<u16>().ok()),
            Some(_) if entry.starts_with('[') => continue, // IPv6 literal, never an official host.
            Some(_) => return Err(AppError::ClaudeRegionUnverified),
            None => (entry, Some(443)),
        };
        if port.is_none() {
            return Err(AppError::ClaudeRegionUnverified);
        }
        if port != Some(443) {
            continue;
        }
        let name = name.to_ascii_lowercase();
        matched |= match name.as_str() {
            "*" => true,
            name if name.starts_with("*.") => host.ends_with(&name[1..]),
            name if name.starts_with('.') => host.ends_with(name),
            name if name.contains('*') || name.contains('/') => {
                return Err(AppError::ClaudeRegionUnverified)
            }
            name => host == name,
        };
    }
    Ok(matched)
}

fn base_client() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .connect_timeout(Duration::from_secs(3))
        .timeout(BUDGET)
}

fn unverified(_error: impl std::fmt::Debug) -> AppError {
    // reqwest errors may include the URL (and therefore the outlet IP).
    AppError::ClaudeRegionUnverified
}

async fn bounded(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, AppError> {
    let mut response = response;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|len| len > limit as u64)
    {
        return Err(AppError::ClaudeRegionUnverified);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(unverified)? {
        if chunk.len() > limit - body.len() {
            return Err(AppError::ClaudeRegionUnverified);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn trace(
    client: &reqwest::Client,
    url: &str,
    host: &str,
) -> Result<(IpAddr, String), AppError> {
    let body = bounded(
        client
            .get(url)
            .header("Cache-Control", "no-cache, no-store")
            .send()
            .await
            .map_err(unverified)?,
        4096,
    )
    .await?;
    parse_trace(std::str::from_utf8(&body).map_err(unverified)?, host)
}

fn parse_trace(body: &str, host: &str) -> Result<(IpAddr, String), AppError> {
    fn single<'a>(body: &'a str, prefix: &str) -> Result<&'a str, AppError> {
        let mut values = body.lines().filter_map(|line| line.strip_prefix(prefix));
        let value = values.next().ok_or(AppError::ClaudeRegionUnverified)?;
        if values.next().is_some() {
            return Err(AppError::ClaudeRegionUnverified);
        }
        Ok(value)
    }
    if single(body, "h=")? != host {
        return Err(AppError::ClaudeRegionUnverified);
    }
    let ip = single(body, "ip=")?
        .parse::<IpAddr>()
        .map_err(unverified)?
        .to_canonical();
    let loc = country_code(single(body, "loc=")?)?;
    let public = match ip {
        IpAddr::V4(v4) => {
            !(v4.is_unspecified()
                || v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.octets()[0] == 0
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1])))
        }
        IpAddr::V6(v6) => v6.segments()[0] & 0xe000 == 0x2000,
    };
    if !public {
        return Err(AppError::ClaudeRegionUnverified);
    }
    Ok((ip, loc))
}

#[derive(Deserialize)]
struct Report {
    ip: IpAddr,
    location: Location,
}

#[derive(Deserialize)]
struct Location {
    country: String,
}

async fn check_reports(
    client: &reqwest::Client,
    base: &str,
    traces: Vec<(IpAddr, String)>,
) -> Result<(), AppError> {
    let ips = traces.iter().map(|(ip, _)| *ip).collect::<BTreeSet<_>>();
    let reports = futures_util::future::try_join_all(ips.into_iter().map(|ip| async move {
        let body = bounded(
            client
                .get(format!("{base}{ip}"))
                .send()
                .await
                .map_err(unverified)?,
            256 * 1024,
        )
        .await?;
        let report: Report = serde_json::from_slice(&body).map_err(unverified)?;
        if report.ip.to_canonical() != ip {
            return Err(AppError::ClaudeRegionUnverified);
        }
        Ok((ip, country_code(&report.location.country)?))
    }))
    .await?
    .into_iter()
    .collect::<BTreeMap<_, _>>();
    for (ip, loc) in traces {
        let country = &reports[&ip];
        if country != &loc {
            return Err(AppError::ClaudeRegionUnverified);
        }
        check_country(country)?;
    }
    Ok(())
}

fn country_code(value: &str) -> Result<String, AppError> {
    if value.len() != 2 || !value.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Err(AppError::ClaudeRegionUnverified);
    }
    let value = value.to_ascii_uppercase();
    if matches!(value.as_str(), "XX" | "ZZ" | "EU" | "AP" | "T1") {
        return Err(AppError::ClaudeRegionUnverified);
    }
    Ok(value)
}

fn check_country(country: &str) -> Result<(), AppError> {
    // https://www.anthropic.com/legal/supported-regions — policy effective
    // 2026-11-12, ISO 3166-1 alpha-2. Includes supported territories and
    // dependencies. Ukraine is conditional; the report has no reliable
    // subdivision code, so a country-level result cannot establish support.
    if country == "UA" {
        return Err(AppError::ClaudeRegionUnverified);
    }
    if SUPPORTED
        .split_ascii_whitespace()
        .any(|code| code == country)
    {
        Ok(())
    } else {
        Err(AppError::ClaudeRegionUnsupported(country.to_owned()))
    }
}

const SUPPORTED: &str = "AD AE AG AL AM AO AR AT AU AZ BA BB BD BE BF BG BH BJ BN BO BR BS BT BW
BZ CA CD CF CG CH CI CL CM CO CR CV CY CZ DE DJ DK DM DO DZ EC EE EG ER ES ET FI FJ FM FR GA GB GD GE GH GM GN GQ GR GT GW HN HR HT HU ID IE IL IN IQ IS IT JM JO JP KE KG KH KI KM KN KR KW KZ LA LB LC LI LK LR LS LT LU LV LY MA MC MD ME MG MH MK ML MN MR MT MU MV MW MX MY MZ NA NE NG NI NL NO NP NR NZ OM PA PE PG PH PK PL PS PT PW PY QA RO RS RW SA SB SC SD SE SG SI SK SL SM SN SO SR SS ST SV SZ TD TG TH TJ TL TM TN TO TR TT TV TW TZ UG US UY UZ VA VC VN VU WS ZM ZW
AS AI AW AX BL BM BQ BV CC CK CW CX FK FO GF GG GI GL GP GS GU HM IM IO JE KY MF MP MQ MS NC NF NU PF PM PN PR RE SH SJ SX TC TF TK UM VG VI WF YT";

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Path as RoutePath, routing::get, Json, Router};
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn trace_must_be_for_the_requested_host_and_have_one_public_ip() {
        let v6 = "h=api.anthropic.com\r\nip=2606:4700:4700::1111\r\nloc=US\r\n";
        assert_eq!(
            parse_trace(v6, API).unwrap().0,
            "2606:4700:4700::1111".parse::<IpAddr>().unwrap()
        );
        assert!(parse_trace(v6, OAUTH).is_err());
        assert!(parse_trace("h=api.anthropic.com\nip=127.0.0.1\nloc=US", API).is_err());
        assert!(parse_trace("h=api.anthropic.com\nip=1.1.1.1\nip=2.2.2.2\nloc=US", API).is_err());
        assert!(parse_trace("h=api.anthropic.com\nip=1.1.1.1\nloc=XX", API).is_err());
    }

    #[test]
    fn target_specific_proxy_bypass_is_not_reused_for_other_hosts() {
        let rules = "localhost, api.anthropic.com:443 .claude.com";
        assert!(bypasses(rules, API).unwrap());
        assert!(bypasses(rules, OAUTH).unwrap());
        assert!(!bypasses(rules, ACCOUNT).unwrap());
        assert!(!bypasses(rules, "ipx.al").unwrap());
        assert!(!bypasses("api.anthropic.com:8080", API).unwrap());
        assert!(bypasses("*", API).unwrap());
        assert!(bypasses("a*b", API).is_err());
    }

    #[tokio::test]
    async fn reports_are_not_reused_between_preflights_and_must_match_both_ip_and_region() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let app = Router::new().route(
            "/report/{ip}",
            get(move |RoutePath(ip): RoutePath<String>| {
                let hit = counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    Json(json!({ "ip": if hit == 3 { "8.8.8.8" } else { &ip },
                    "location": {"country": if hit == 0 { "US" } else { "CN" }} }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base = format!("http://{}/report/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let one_ip = |region: &str| vec![("1.1.1.1".parse().unwrap(), region.to_owned())];
        check_reports(&client, &base, one_ip("US")).await.unwrap();
        assert_eq!(
            check_reports(&client, &base, one_ip("US"))
                .await
                .unwrap_err()
                .code(),
            "CLAUDE_REGION_UNVERIFIED"
        );
        assert_eq!(
            check_reports(&client, &base, one_ip("CN"))
                .await
                .unwrap_err()
                .claude_region(),
            Some("CN")
        );
        assert_eq!(
            check_reports(&client, &base, one_ip("CN"))
                .await
                .unwrap_err()
                .code(),
            "CLAUDE_REGION_UNVERIFIED"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 4);
        server.abort();
    }
}
