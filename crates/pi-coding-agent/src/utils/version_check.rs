//! Port of `packages/coding-agent/src/utils/version-check.ts`.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::management_http::{FetchInit, FetchRetryOptions, fetch_with_retry};
use super::pi_user_agent::get_pi_user_agent;

const LATEST_VERSION_URL: &str = "https://pi.dev/api/latest-version";
const DEFAULT_VERSION_CHECK_TIMEOUT_MS: i64 = 10000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestPiRelease {
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Include useful errno details hidden behind generic "fetch failed" errors.
///
/// Walks the error's source chain: `io::Error` kinds become their errno-style codes
/// (`ECONNREFUSED`, `ETIMEDOUT`, ...); otherwise the first source message is appended.
pub fn format_version_check_error(error: &anyhow::Error) -> String {
    let root_message = error.to_string();
    let causes: Vec<&(dyn std::error::Error + 'static)> = error.chain().skip(1).collect();
    let mut codes: Vec<&'static str> = Vec::new();
    for cause in &causes {
        if let Some(code) = cause.downcast_ref::<pi_ai::utils::http::NetworkError>().and_then(|e| e.code) {
            if !codes.contains(&code) {
                codes.push(code);
            }
        } else if let Some(io) = cause.downcast_ref::<std::io::Error>()
            && let Some(code) = io_error_code(io)
            && !codes.contains(&code)
        {
            codes.push(code);
        }
    }
    if !codes.is_empty() {
        return format!("{root_message} ({})", codes.join(", "));
    }
    match causes.iter().map(|c| c.to_string()).find(|m| !m.is_empty()) {
        Some(cause_message) => format!("{root_message} (cause: {cause_message})"),
        None => root_message,
    }
}

fn io_error_code(error: &std::io::Error) -> Option<&'static str> {
    use std::io::ErrorKind::*;
    Some(match error.kind() {
        ConnectionRefused => "ECONNREFUSED",
        ConnectionReset => "ECONNRESET",
        ConnectionAborted => "ECONNABORTED",
        TimedOut => "ETIMEDOUT",
        HostUnreachable => "EHOSTUNREACH",
        NetworkUnreachable => "ENETUNREACH",
        AddrNotAvailable => "EADDRNOTAVAIL",
        BrokenPipe => "EPIPE",
        NotFound => "ENOTFOUND",
        _ => return None,
    })
}

/// A parsed semantic version (node-semver `valid` / `compare` semantics).
#[derive(Debug, Clone, PartialEq, Eq)]
struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Vec<PrereleaseId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PrereleaseId {
    Numeric(u64),
    Alpha(String),
}

fn parse_numeric(part: &str) -> Option<u64> {
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if part.len() > 1 && part.starts_with('0') {
        return None;
    }
    let value: u64 = part.parse().ok()?;
    // node-semver rejects components above Number.MAX_SAFE_INTEGER.
    (value <= 9_007_199_254_740_991).then_some(value)
}

/// node-semver `valid()` in strict mode: optional leading `v`/`=` and whitespace,
/// `MAJOR.MINOR.PATCH[-prerelease][+build]`.
fn parse_semver(version: &str) -> Option<SemVer> {
    let mut text = version.trim();
    text = text.trim_start_matches(['=', 'v']).trim_start();
    if text.len() > 256 {
        return None;
    }
    let (without_build, build) = match text.split_once('+') {
        Some((head, build)) => (head, Some(build)),
        None => (text, None),
    };
    if let Some(build) = build
        && (build.is_empty()
            || build.split('.').any(|id| id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')))
    {
        return None;
    }
    let (core, prerelease) = match without_build.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (without_build, None),
    };
    let mut parts = core.split('.');
    let major = parse_numeric(parts.next()?)?;
    let minor = parse_numeric(parts.next()?)?;
    let patch = parse_numeric(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    let mut ids = Vec::new();
    if let Some(pre) = prerelease {
        for id in pre.split('.') {
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                return None;
            }
            if id.bytes().all(|b| b.is_ascii_digit()) {
                ids.push(PrereleaseId::Numeric(parse_numeric(id)?));
            } else {
                ids.push(PrereleaseId::Alpha(id.to_string()));
            }
        }
    }
    Some(SemVer { major, minor, patch, prerelease: ids })
}

fn compare_semver(left: &SemVer, right: &SemVer) -> Ordering {
    let main = (left.major, left.minor, left.patch).cmp(&(right.major, right.minor, right.patch));
    if main != Ordering::Equal {
        return main;
    }
    match (left.prerelease.is_empty(), right.prerelease.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    for index in 0.. {
        match (left.prerelease.get(index), right.prerelease.get(index)) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(a), Some(b)) => {
                let ordering = match (a, b) {
                    (PrereleaseId::Numeric(x), PrereleaseId::Numeric(y)) => x.cmp(y),
                    (PrereleaseId::Numeric(_), PrereleaseId::Alpha(_)) => Ordering::Less,
                    (PrereleaseId::Alpha(_), PrereleaseId::Numeric(_)) => Ordering::Greater,
                    (PrereleaseId::Alpha(x), PrereleaseId::Alpha(y)) => x.cmp(y),
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
        }
    }
    Ordering::Equal
}

pub fn compare_package_versions(left_version: &str, right_version: &str) -> Option<i64> {
    let left = parse_semver(left_version.trim())?;
    let right = parse_semver(right_version.trim())?;
    Some(match compare_semver(&left, &right) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    })
}

pub fn is_newer_package_version(candidate_version: &str, current_version: &str) -> bool {
    match compare_package_versions(candidate_version, current_version) {
        Some(comparison) => comparison > 0,
        None => candidate_version.trim() != current_version.trim(),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetLatestPiReleaseOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<bool>,
}

fn non_empty_trimmed(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

pub async fn get_latest_pi_release(
    current_version: &str,
    options: Option<&GetLatestPiReleaseOptions>,
) -> anyhow::Result<Option<LatestPiRelease>> {
    get_latest_pi_release_from(LATEST_VERSION_URL, None, current_version, options).await
}

async fn get_latest_pi_release_from(
    url: &str,
    client: Option<reqwest::Client>,
    current_version: &str,
    options: Option<&GetLatestPiReleaseOptions>,
) -> anyhow::Result<Option<LatestPiRelease>> {
    if std::env::var_os("PI_OFFLINE").is_some_and(|v| !v.is_empty()) {
        return Ok(None);
    }
    let options = options.cloned().unwrap_or_default();
    let init = FetchInit {
        headers: vec![
            ("User-Agent".to_string(), get_pi_user_agent(current_version)),
            ("accept".to_string(), "application/json".to_string()),
        ],
        client,
        ..Default::default()
    };
    let retry_options = FetchRetryOptions {
        max_retries: Some(if options.retry.unwrap_or(false) { 2 } else { 0 }),
        timeout_ms: Some(options.timeout_ms.unwrap_or(DEFAULT_VERSION_CHECK_TIMEOUT_MS)),
        ..Default::default()
    };
    let response = fetch_with_retry(url, Some(&init), Some(&retry_options)).await?;
    if !response.status().is_success() {
        return Ok(None);
    }
    let data: Value = response.json().await?;
    let Some(version) = data.get("version").and_then(Value::as_str) else {
        return Ok(None);
    };
    if version.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(LatestPiRelease {
        version: version.trim().to_string(),
        package_name: non_empty_trimmed(data.get("packageName")),
        note: non_empty_trimmed(data.get("note")),
    }))
}

pub type GetLatestPiVersionOptions = GetLatestPiReleaseOptions;

pub async fn get_latest_pi_version(
    current_version: &str,
    options: Option<&GetLatestPiVersionOptions>,
) -> anyhow::Result<Option<String>> {
    Ok(get_latest_pi_release(current_version, options).await?.map(|release| release.version))
}

/// The Rust fork must not advertise releases of the upstream npm application.
/// Keep explicit release lookups available for management commands, but never query
/// the upstream endpoint or show its update notifications during startup.
pub async fn check_for_new_pi_version(_current_version: &str) -> anyhow::Result<Option<LatestPiRelease>> {
    Ok(None)
}

#[cfg(test)]
async fn check_for_new_pi_version_from(
    url: &str,
    client: Option<reqwest::Client>,
    current_version: &str,
) -> anyhow::Result<Option<LatestPiRelease>> {
    if std::env::var_os("PI_SKIP_VERSION_CHECK").is_some_and(|v| !v.is_empty()) {
        return Ok(None);
    }
    match get_latest_pi_release_from(url, client, current_version, None).await {
        Ok(Some(release)) if is_newer_package_version(&release.version, current_version) => Ok(Some(release)),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::super::management_http::test_server::{Reply, serve};
    use super::*;
    use std::sync::atomic::Ordering as AtomicOrdering;

    fn client() -> Option<reqwest::Client> {
        Some(reqwest::Client::builder().no_proxy().build().unwrap())
    }

    #[tokio::test]
    async fn rust_startup_never_advertises_upstream_releases() {
        assert_eq!(check_for_new_pi_version("0.0.0").await.unwrap(), None);
    }

    #[test]
    fn compares_package_versions() {
        assert!(compare_package_versions("0.70.6", "0.70.5").unwrap() > 0);
        assert_eq!(compare_package_versions("0.70.5", "0.70.5"), Some(0));
        assert!(compare_package_versions("0.70.4", "0.70.5").unwrap() < 0);
        assert!(compare_package_versions("5.0.0-beta.20", "5.0.0-beta.9").unwrap() > 0);
        assert!(!is_newer_package_version("0.70.5", "0.70.5"));
        assert!(is_newer_package_version("0.70.6", "0.70.5"));
        assert_eq!(compare_package_versions("v1.0.0", "1.0.0"), Some(0));
        assert_eq!(compare_package_versions("1.0.0+build", "1.0.0"), Some(0));
        assert_eq!(compare_package_versions("1.0", "1.0.0"), None);
        assert!(is_newer_package_version("nightly", "1.0.0"));
    }

    // PI_SKIP_VERSION_CHECK / PI_OFFLINE are process-global, so the network cases run in one
    // test to avoid racing with each other.
    #[tokio::test]
    async fn version_check_api() {
        // SAFETY: tests in this module do not read these variables concurrently.
        unsafe {
            std::env::remove_var("PI_OFFLINE");
            std::env::remove_var("PI_SKIP_VERSION_CHECK");
        }

        // returns only newer versions
        let server = serve(vec![Reply::Respond(200, r#"{"version":"1.2.3"}"#.into())]).await;
        assert_eq!(check_for_new_pi_version_from(&server.url, client(), "1.2.3").await.unwrap(), None);
        assert_eq!(
            check_for_new_pi_version_from(&server.url, client(), "1.2.2").await.unwrap(),
            Some(LatestPiRelease { version: "1.2.3".into(), package_name: None, note: None })
        );

        // uses a pi user agent
        let server = serve(vec![Reply::Respond(200, r#"{"version":"1.2.4"}"#.into())]).await;
        let release = get_latest_pi_release_from(&server.url, client(), "1.2.3", None).await.unwrap();
        assert_eq!(release.unwrap().version, "1.2.4");
        let request = server.requests.lock()[0].to_lowercase();
        assert!(request.contains("user-agent: pi/1.2.3 "), "{request}");
        assert!(request.contains("accept: application/json"), "{request}");

        // retries a transient version request when explicitly requested
        let server =
            serve(vec![Reply::Drop, Reply::Drop, Reply::Respond(200, r#"{"version":"1.2.4"}"#.into())]).await;
        let options = GetLatestPiReleaseOptions { retry: Some(true), ..Default::default() };
        let release = get_latest_pi_release_from(&server.url, client(), "1.2.3", Some(&options)).await.unwrap();
        assert_eq!(release, Some(LatestPiRelease { version: "1.2.4".into(), package_name: None, note: None }));
        assert_eq!(server.hits.load(AtomicOrdering::SeqCst), 3);

        // keeps automatic version checks to one request
        let server = serve(vec![Reply::Drop]).await;
        assert_eq!(check_for_new_pi_version_from(&server.url, client(), "1.2.3").await.unwrap(), None);
        assert_eq!(server.hits.load(AtomicOrdering::SeqCst), 1);

        // returns the active package metadata
        let server =
            serve(vec![Reply::Respond(200, r#"{"packageName":"@new-scope/pi","version":"1.2.4"}"#.into())]).await;
        assert_eq!(
            get_latest_pi_release_from(&server.url, client(), "1.2.3", None).await.unwrap(),
            Some(LatestPiRelease { version: "1.2.4".into(), package_name: Some("@new-scope/pi".into()), note: None })
        );

        // returns update notes
        let server =
            serve(vec![Reply::Respond(200, r#"{"note":" **Read this** ","version":"1.2.4"}"#.into())]).await;
        let release = get_latest_pi_release_from(&server.url, client(), "1.2.3", None).await.unwrap().unwrap();
        assert_eq!(release.note.as_deref(), Some("**Read this**"));
        assert_eq!(
            serde_json::to_value(&release).unwrap(),
            serde_json::json!({"version": "1.2.4", "note": "**Read this**"})
        );

        // skips automatic api calls when version checks are disabled, direct calls still work
        unsafe { std::env::set_var("PI_SKIP_VERSION_CHECK", "1") };
        let server = serve(vec![Reply::Respond(200, r#"{"version":"1.2.4"}"#.into())]).await;
        assert_eq!(check_for_new_pi_version_from(&server.url, client(), "1.2.3").await.unwrap(), None);
        assert_eq!(server.hits.load(AtomicOrdering::SeqCst), 0);
        let release = get_latest_pi_release_from(&server.url, client(), "1.2.3", None).await.unwrap();
        assert_eq!(release.unwrap().version, "1.2.4");
        assert_eq!(server.hits.load(AtomicOrdering::SeqCst), 1);
        unsafe { std::env::remove_var("PI_SKIP_VERSION_CHECK") };
    }

    #[test]
    fn formats_nested_network_error_details() {
        let error = anyhow::Error::new(std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timeout"))
            .context("fetch failed");
        assert_eq!(format_version_check_error(&error), "fetch failed (ETIMEDOUT)");
        let error = anyhow::anyhow!("inner").context("fetch failed");
        assert_eq!(format_version_check_error(&error), "fetch failed (cause: inner)");
        assert_eq!(format_version_check_error(&anyhow::anyhow!("plain")), "plain");
    }

    #[tokio::test]
    async fn reports_a_refused_connection_like_node_fetch() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let init = FetchInit { client: Some(client), ..Default::default() };
        let options = FetchRetryOptions { max_retries: Some(0), ..Default::default() };
        let error = fetch_with_retry(&format!("http://127.0.0.1:{port}/"), Some(&init), Some(&options)).await.unwrap_err();
        assert_eq!(error.to_string(), "fetch failed");
        assert_eq!(format!("{error:#}"), format!("fetch failed: connect ECONNREFUSED 127.0.0.1:{port}"));
        assert_eq!(format_version_check_error(&error), "fetch failed (ECONNREFUSED)");
    }
}
