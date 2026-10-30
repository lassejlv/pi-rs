//! Client identity: `User-Agent` and provider attribution headers (port of `utils/identity.ts`).

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::version::PI_AI_VERSION;

/// The `User-Agent` sent to providers: `"short"`, `"detailed"` or `{ custom }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserAgentMode {
    Named(UserAgentNamed),
    Custom { custom: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserAgentNamed {
    Short,
    Detailed,
}

/// Who requests are attributed to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientAttribution {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categories: Option<Vec<String>>,
}

/// pi's attribution, used by pi-agent-core, pi-durable, and the coding agent.
pub fn pi_attribution() -> ClientAttribution {
    ClientAttribution {
        name: "pi".to_string(),
        url: "https://pi.dev".to_string(),
        categories: Some(vec!["cli-agent".to_string()]),
    }
}

/// How pi-ai identifies the calling app to providers.
///
/// `attribution`: absent (`None`) = not set; `Some(None)` = explicitly `null` (turns off
/// attribution set by a merged identity); `Some(Some(_))` = set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<UserAgentMode>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_some"
    )]
    pub attribution: Option<Option<ClientAttribution>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// The provider and endpoint a request goes to.
#[derive(Debug, Clone, Copy)]
pub struct ClientIdentityTarget<'a> {
    pub provider: &'a str,
    pub base_url: &'a str,
}

const NVIDIA_NIM_HOST: &str = "integrate.api.nvidia.com";
const OPENCODE_HOST: &str = "opencode.ai";

/// Merge identities field by field; later sources win and unset fields are skipped.
pub fn merge_client_identity(sources: &[Option<&ClientIdentity>]) -> Option<ClientIdentity> {
    let mut merged: Option<ClientIdentity> = None;
    for source in sources.iter().flatten() {
        let mut next = merged.take().unwrap_or_default();
        if source.user_agent.is_some() {
            next.user_agent = source.user_agent.clone();
        }
        if source.attribution.is_some() {
            next.attribution = source.attribution.clone();
        }
        if source.version.is_some() {
            next.version = source.version.clone();
        }
        merged = Some(next);
    }
    merged
}

/// The version in the detailed `User-Agent`.
pub fn get_client_version(identity: Option<&ClientIdentity>) -> String {
    identity.and_then(|i| i.version.clone()).unwrap_or_else(|| PI_AI_VERSION.to_string())
}

fn platform_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn arch_name() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

/// The `User-Agent` value of an identity.
pub fn format_user_agent(identity: Option<&ClientIdentity>) -> String {
    match identity.and_then(|i| i.user_agent.as_ref()) {
        Some(UserAgentMode::Custom { custom }) => custom.clone(),
        Some(UserAgentMode::Named(UserAgentNamed::Short)) => "pi".to_string(),
        _ => {
            let product = format!("pi/{}", get_client_version(identity));
            format!("{product} ({}; rust/{}; {})", platform_name(), rust_version(), arch_name())
        }
    }
}

fn rust_version() -> &'static str {
    option_env!("CARGO_PKG_RUST_VERSION")
        .filter(|v| !v.is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}

fn matches_host(base_url: &str, expected_host: &str) -> bool {
    url::Url::parse(base_url).ok().and_then(|u| u.host_str().map(|h| h == expected_host)).unwrap_or(false)
}

fn is_open_router(target: ClientIdentityTarget) -> bool {
    target.provider == "openrouter" || target.base_url.contains("openrouter.ai")
}

fn is_nvidia_nim(target: ClientIdentityTarget) -> bool {
    target.provider == "nvidia" || matches_host(target.base_url, NVIDIA_NIM_HOST)
}

fn is_open_code(target: ClientIdentityTarget) -> bool {
    target.provider == "opencode" || target.provider == "opencode-go" || matches_host(target.base_url, OPENCODE_HOST)
}

/// Provider attribution headers of an identity for a request target. Empty when attribution is off.
pub fn get_attribution_headers(
    target: ClientIdentityTarget,
    identity: Option<&ClientIdentity>,
) -> IndexMap<String, String> {
    let mut headers = IndexMap::new();
    let Some(attribution) = identity.and_then(|i| i.attribution.clone().flatten()) else {
        return headers;
    };
    let ClientAttribution { name, url, categories } = attribution;
    if is_open_router(target) {
        headers.insert("HTTP-Referer".to_string(), url);
        headers.insert("X-OpenRouter-Title".to_string(), name);
        if let Some(categories) = categories.filter(|c| !c.is_empty()) {
            headers.insert("X-OpenRouter-Categories".to_string(), categories.join(","));
        }
        return headers;
    }
    if is_nvidia_nim(target) {
        let mut chars = name.chars();
        let capitalized = match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        };
        headers.insert("X-BILLING-INVOKE-ORIGIN".to_string(), capitalized);
        return headers;
    }
    if is_open_code(target) {
        headers.insert("x-opencode-client".to_string(), name);
    }
    headers
}

/// OpenCode's per-conversation routing header. `None` without a session or for other targets.
pub fn get_open_code_session_headers(
    target: ClientIdentityTarget,
    session_id: Option<&str>,
) -> Option<IndexMap<String, String>> {
    match session_id {
        Some(id) if !id.is_empty() && is_open_code(target) => {
            let mut headers = IndexMap::new();
            headers.insert("x-opencode-session".to_string(), id.to_string());
            Some(headers)
        }
        _ => None,
    }
}

/// Default headers an identity adds to a request: `User-Agent` and provider attribution.
pub fn get_identity_headers(
    target: ClientIdentityTarget,
    identity: Option<&ClientIdentity>,
) -> IndexMap<String, String> {
    let mut headers = IndexMap::new();
    headers.insert("User-Agent".to_string(), format_user_agent(identity));
    headers.extend(get_attribution_headers(target, identity));
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_identities_field_by_field() {
        let a = ClientIdentity { version: Some("1".into()), attribution: Some(Some(pi_attribution())), ..Default::default() };
        let b = ClientIdentity { attribution: Some(None), ..Default::default() };
        let merged = merge_client_identity(&[Some(&a), None, Some(&b)]).unwrap();
        assert_eq!(merged.version.as_deref(), Some("1"));
        assert_eq!(merged.attribution, Some(None));
        assert!(merge_client_identity(&[None, None]).is_none());
    }

    #[test]
    fn user_agent_modes() {
        let short = ClientIdentity { user_agent: Some(UserAgentMode::Named(UserAgentNamed::Short)), ..Default::default() };
        assert_eq!(format_user_agent(Some(&short)), "pi");
        let custom = ClientIdentity { user_agent: Some(UserAgentMode::Custom { custom: "x/1".into() }), ..Default::default() };
        assert_eq!(format_user_agent(Some(&custom)), "x/1");
        assert!(format_user_agent(None).starts_with("pi/1.4.0 ("));
        let json: ClientIdentity = serde_json::from_str(r#"{"userAgent":"short","attribution":null}"#).unwrap();
        assert_eq!(json.attribution, Some(None));
        assert_eq!(serde_json::to_string(&json).unwrap(), r#"{"userAgent":"short","attribution":null}"#);
    }

    #[test]
    fn attribution_headers_per_provider() {
        let identity = ClientIdentity { attribution: Some(Some(pi_attribution())), ..Default::default() };
        let or = get_attribution_headers(ClientIdentityTarget { provider: "openrouter", base_url: "" }, Some(&identity));
        assert_eq!(or.get("HTTP-Referer").unwrap(), "https://pi.dev");
        assert_eq!(or.get("X-OpenRouter-Categories").unwrap(), "cli-agent");
        let nv = get_attribution_headers(
            ClientIdentityTarget { provider: "x", base_url: "https://integrate.api.nvidia.com/v1" },
            Some(&identity),
        );
        assert_eq!(nv.get("X-BILLING-INVOKE-ORIGIN").unwrap(), "Pi");
        let oc = get_attribution_headers(ClientIdentityTarget { provider: "opencode-go", base_url: "" }, Some(&identity));
        assert_eq!(oc.get("x-opencode-client").unwrap(), "pi");
        assert!(get_attribution_headers(ClientIdentityTarget { provider: "anthropic", base_url: "" }, Some(&identity)).is_empty());
    }
}
