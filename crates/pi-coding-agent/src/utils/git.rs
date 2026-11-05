//! Port of `packages/coding-agent/src/utils/git.ts`.
//!
//! `hosted-git-info` (the npm package the TS source uses to recognize GitHub, GitLab, Bitbucket,
//! Gist and SourceHut URLs and shortcuts) is ported in the private [`hosted`] module, limited to
//! `fromUrl` and the fields `parseGitUrl` reads (`domain`, `user`, `project`, `committish`).

use serde::{Deserialize, Serialize};

/// Parsed git URL information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitSource {
    /// Always "git" for git sources
    /// Always "git".
    pub r#type: String,
    /// Clone URL (always valid for git clone, without ref suffix)
    pub repo: String,
    /// Git host domain (e.g., "github.com")
    pub host: String,
    /// Repository path (e.g., "user/repo")
    pub path: String,
    /// Git ref (branch, tag, commit) if specified
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    /// True if ref was specified (package won't be auto-updated)
    pub pinned: bool,
}

/// JS `decodeURIComponent`: `None` where JS throws `URIError`.
pub(crate) fn decode_uri_component(value: &str) -> Option<String> {
    if !value.contains('%') {
        return Some(value.to_string());
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = value.get(i + 1..i + 3)?;
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// JS `str.lastIndexOf(ch, str.indexOf(before))` (search everything when `before` is absent).
fn last_index_of_before(s: &str, ch: char, before: char) -> Option<usize> {
    let end = match s.find(before) {
        Some(start) => (start + ch.len_utf8()).min(s.len()),
        None => s.len(),
    };
    s.get(..end).and_then(|prefix| prefix.rfind(ch))
}

/// JS URL `hash`: `#fragment`, or empty when the fragment is missing or empty.
fn url_hash(url: &url::Url) -> String {
    match url.fragment() {
        Some(fragment) if !fragment.is_empty() => format!("#{fragment}"),
        _ => String::new(),
    }
}

pub(crate) mod hosted {
    //! Port of the `hosted-git-info` `fromUrl` parser.

    use super::{decode_uri_component, last_index_of_before, url_hash};

    #[derive(Debug, Clone)]
    pub struct HostedInfo {
        pub domain: String,
        /// `null` in JS for gist URLs without a user.
        pub user: Option<String>,
        pub project: String,
        pub committish: Option<String>,
    }

    struct HostDef {
        name: &'static str,
        domain: &'static str,
        protocols: &'static [&'static str],
    }

    const HOSTS: &[HostDef] = &[
        HostDef {
            name: "github",
            domain: "github.com",
            protocols: &["git:", "http:", "git+ssh:", "git+https:", "ssh:", "https:"],
        },
        HostDef {
            name: "bitbucket",
            domain: "bitbucket.org",
            protocols: &["git+ssh:", "git+https:", "ssh:", "https:"],
        },
        HostDef {
            name: "gitlab",
            domain: "gitlab.com",
            protocols: &["git+ssh:", "git+https:", "ssh:", "https:"],
        },
        HostDef {
            name: "gist",
            domain: "gist.github.com",
            protocols: &["git:", "git+ssh:", "git+https:", "ssh:", "https:"],
        },
        HostDef {
            name: "sourcehut",
            domain: "git.sr.ht",
            protocols: &["git+ssh:", "https:"],
        },
    ];

    const BASE_PROTOCOLS: &[&str] = &[
        "git+ssh:",
        "ssh:",
        "git+https:",
        "git:",
        "http:",
        "https:",
        "git+http:",
    ];

    fn is_known_protocol(proto: &str) -> bool {
        BASE_PROTOCOLS.contains(&proto) || HOSTS.iter().any(|h| format!("{}:", h.name) == proto)
    }

    /// Look for github shorthand inputs, such as npm/cli.
    fn is_github_shorthand(arg: &str) -> bool {
        let first_hash = arg.find('#');
        let first_slash = arg.find('/');
        let second_slash = first_slash.and_then(|i| arg[i + 1..].find('/').map(|j| i + 1 + j));
        let first_colon = arg.find(':');
        let first_space = arg.find(char::is_whitespace);
        let first_at = arg.find('@');

        let after_hash = |index: Option<usize>| match index {
            None => true,
            Some(i) => first_hash.is_some_and(|h| i > h),
        };
        let space_only_after_hash = after_hash(first_space);
        let at_only_after_hash = after_hash(first_at);
        let colon_only_after_hash = after_hash(first_colon);
        let second_slash_only_after_hash = after_hash(second_slash);
        let has_slash = first_slash.is_some_and(|i| i > 0);
        let does_not_end_with_slash = match first_hash {
            Some(h) => h == 0 || arg.as_bytes()[h - 1] != b'/',
            None => !arg.ends_with('/'),
        };
        let does_not_start_with_dot = !arg.starts_with('.');

        space_only_after_hash
            && has_slash
            && does_not_end_with_slash
            && does_not_start_with_dot
            && at_only_after_hash
            && colon_only_after_hash
            && second_slash_only_after_hash
    }

    /// Accepts input like git:github.com:user/repo and inserts the // after the first :
    fn correct_protocol(arg: &str) -> String {
        let first_colon = arg.find(':');
        let proto_end = first_colon.map(|i| i + 1).unwrap_or(0);
        let proto = &arg[..proto_end];
        if is_known_protocol(proto) {
            return arg.to_string();
        }
        if let Some(first_at) = arg.find('@') {
            let after_colon = match first_colon {
                Some(c) => first_at > c,
                None => true,
            };
            return if after_colon {
                format!("git+ssh://{arg}")
            } else {
                arg.to_string()
            };
        }
        if arg.find("//") == Some(proto_end) {
            return arg.to_string();
        }
        format!("{}//{}", &arg[..proto_end], &arg[proto_end..])
    }

    /// Attempt to correct an scp style url so that it will parse with `new URL()`.
    fn correct_url(giturl: &str) -> String {
        let mut giturl = giturl.to_string();
        let first_at = last_index_of_before(&giturl, '@', '#');
        let last_colon_before_hash = last_index_of_before(&giturl, ':', '#');
        let greater = match (last_colon_before_hash, first_at) {
            (Some(c), Some(a)) => c > a,
            (Some(_), None) => true,
            _ => false,
        };
        if greater {
            let c = last_colon_before_hash.unwrap_or(0);
            giturl = format!("{}/{}", &giturl[..c], &giturl[c + 1..]);
        }
        if last_index_of_before(&giturl, ':', '#').is_none() && !giturl.contains("//") {
            giturl = format!("git+ssh://{giturl}");
        }
        giturl
    }

    fn parse_url(giturl: &str) -> Option<url::Url> {
        let with_protocol = correct_protocol(giturl);
        url::Url::parse(&with_protocol)
            .ok()
            .or_else(|| url::Url::parse(&correct_url(&with_protocol)).ok())
    }

    /// `pathname.split("/", limit)` (JS: at most `limit` pieces, the rest is dropped).
    fn split_limit(pathname: &str, limit: usize) -> Vec<String> {
        pathname
            .split('/')
            .take(limit)
            .map(str::to_string)
            .collect()
    }

    struct Segments {
        user: Option<String>,
        project: String,
        committish: String,
    }

    fn strip_git(project: &str) -> String {
        project.strip_suffix(".git").unwrap_or(project).to_string()
    }

    fn extract(name: &str, url: &url::Url) -> Option<Segments> {
        let pathname = url.path();
        let hash = url_hash(url);
        let hash_value = hash.get(1..).unwrap_or("").to_string();
        match name {
            "github" => {
                let parts = split_limit(pathname, 5);
                let user = parts.get(1).cloned().unwrap_or_default();
                let project = parts.get(2).cloned().unwrap_or_default();
                let kind = parts.get(3).cloned().unwrap_or_default();
                let mut committish = parts.get(4).cloned().unwrap_or_default();
                if !kind.is_empty() && kind != "tree" {
                    return None;
                }
                if kind.is_empty() {
                    committish = hash_value;
                }
                let project = strip_git(&project);
                if user.is_empty() || project.is_empty() {
                    return None;
                }
                Some(Segments {
                    user: Some(user),
                    project,
                    committish,
                })
            }
            "bitbucket" | "sourcehut" => {
                let parts = split_limit(pathname, 4);
                let user = parts.get(1).cloned().unwrap_or_default();
                let project = parts.get(2).cloned().unwrap_or_default();
                let aux = parts.get(3).cloned().unwrap_or_default();
                let blocked = if name == "bitbucket" { "get" } else { "archive" };
                if aux == blocked {
                    return None;
                }
                let project = strip_git(&project);
                if user.is_empty() || project.is_empty() {
                    return None;
                }
                Some(Segments {
                    user: Some(user),
                    project,
                    committish: hash_value,
                })
            }
            "gitlab" => {
                let path = pathname.get(1..).unwrap_or("");
                if path.contains("/-/") || path.contains("/archive.tar.gz") {
                    return None;
                }
                let mut segments: Vec<&str> = path.split('/').collect();
                let project = strip_git(segments.pop().unwrap_or(""));
                let user = segments.join("/");
                if user.is_empty() || project.is_empty() {
                    return None;
                }
                Some(Segments {
                    user: Some(user),
                    project,
                    committish: hash_value,
                })
            }
            "gist" => {
                let parts = split_limit(pathname, 4);
                let mut user = parts.get(1).cloned().filter(|s| !s.is_empty());
                let mut project = parts.get(2).cloned().unwrap_or_default();
                let aux = parts.get(3).cloned().unwrap_or_default();
                if aux == "raw" {
                    return None;
                }
                if project.is_empty() {
                    let u = user.take()?;
                    project = u;
                }
                let project = strip_git(&project);
                Some(Segments {
                    user,
                    project,
                    committish: hash_value,
                })
            }
            _ => None,
        }
    }

    /// `hostedGitInfo.fromUrl(giturl)`.
    pub fn from_url(giturl: &str) -> Option<HostedInfo> {
        if giturl.is_empty() {
            return None;
        }
        let corrected = if is_github_shorthand(giturl) {
            format!("github:{giturl}")
        } else {
            giturl.to_string()
        };
        let parsed = parse_url(&corrected)?;
        let protocol = format!("{}:", parsed.scheme());
        let hostname = parsed.host_str().unwrap_or("");
        let hostname = hostname.strip_prefix("www.").unwrap_or(hostname);

        let by_shortcut = HOSTS.iter().find(|h| format!("{}:", h.name) == protocol);
        let by_domain = HOSTS.iter().find(|h| h.domain == hostname);
        let host = by_shortcut.or(by_domain)?;

        if by_shortcut.is_some() {
            let raw = parsed.path();
            let mut pathname = raw.strip_prefix('/').unwrap_or(raw);
            if let Some(first_at) = pathname.find('@') {
                pathname = &pathname[first_at + 1..];
            }
            let (user, project) = match pathname.rfind('/') {
                Some(last_slash) => {
                    let user = decode_uri_component(&pathname[..last_slash])?;
                    let project = decode_uri_component(&pathname[last_slash + 1..])?;
                    (if user.is_empty() { None } else { Some(user) }, project)
                }
                None => (None, decode_uri_component(pathname)?),
            };
            let project = strip_git(&project);
            let hash = url_hash(&parsed);
            let committish = if hash.is_empty() {
                None
            } else {
                Some(decode_uri_component(&hash[1..])?)
            };
            return Some(HostedInfo {
                domain: host.domain.to_string(),
                user,
                project,
                committish,
            });
        }

        if !host.protocols.contains(&protocol.as_str()) {
            return None;
        }
        let segments = extract(host.name, &parsed)?;
        let user = match segments.user {
            Some(user) if !user.is_empty() => Some(decode_uri_component(&user)?),
            other => other,
        };
        let project = decode_uri_component(&segments.project)?;
        let committish = decode_uri_component(&segments.committish)?;
        Some(HostedInfo {
            domain: host.domain.to_string(),
            user,
            project,
            committish: if committish.is_empty() {
                None
            } else {
                Some(committish)
            },
        })
    }
}

struct SplitRef {
    repo: String,
    r#ref: Option<String>,
}

fn scp_like(url: &str) -> Option<(&str, &str)> {
    // /^git@([^:]+):(.+)$/
    let rest = url.strip_prefix("git@")?;
    let colon = rest.find(':')?;
    let host = &rest[..colon];
    let path = &rest[colon + 1..];
    if host.is_empty() || path.is_empty() || path.contains('\n') {
        return None;
    }
    Some((host, path))
}

fn split_ref(url: &str) -> SplitRef {
    let no_ref = || SplitRef {
        repo: url.to_string(),
        r#ref: None,
    };
    if let Some((host, path_with_maybe_ref)) = scp_like(url) {
        let Some(sep) = path_with_maybe_ref.find('@') else {
            return no_ref();
        };
        let repo_path = &path_with_maybe_ref[..sep];
        let r = &path_with_maybe_ref[sep + 1..];
        if repo_path.is_empty() || r.is_empty() {
            return no_ref();
        }
        return SplitRef {
            repo: format!("git@{host}:{repo_path}"),
            r#ref: Some(r.to_string()),
        };
    }

    if url.contains("://") {
        let Ok(mut parsed) = url::Url::parse(url) else {
            return no_ref();
        };
        let path_with_maybe_ref = parsed.path().trim_start_matches('/').to_string();
        let Some(sep) = path_with_maybe_ref.find('@') else {
            return no_ref();
        };
        let repo_path = &path_with_maybe_ref[..sep];
        let r = &path_with_maybe_ref[sep + 1..];
        if repo_path.is_empty() || r.is_empty() {
            return no_ref();
        }
        parsed.set_path(&format!("/{repo_path}"));
        let s = parsed.to_string();
        return SplitRef {
            repo: s.strip_suffix('/').map(str::to_string).unwrap_or(s),
            r#ref: Some(r.to_string()),
        };
    }

    let Some(slash) = url.find('/') else {
        return no_ref();
    };
    let host = &url[..slash];
    let path_with_maybe_ref = &url[slash + 1..];
    let Some(sep) = path_with_maybe_ref.find('@') else {
        return no_ref();
    };
    let repo_path = &path_with_maybe_ref[..sep];
    let r = &path_with_maybe_ref[sep + 1..];
    if repo_path.is_empty() || r.is_empty() {
        return no_ref();
    }
    SplitRef {
        repo: format!("{host}/{repo_path}"),
        r#ref: Some(r.to_string()),
    }
}

fn has_unsafe_git_install_part(value: &str, allow_slash: bool) -> bool {
    let Some(decoded) = decode_uri_component(value) else {
        return true;
    };
    for candidate in [value, decoded.as_str()] {
        if candidate.contains('\0') || candidate.contains('\\') || candidate.starts_with('/') {
            return true;
        }
        if !allow_slash && candidate.contains('/') {
            return true;
        }
        if candidate.split('/').any(|s| s == "..") {
            return true;
        }
    }
    false
}

fn build_git_source(repo: String, host: &str, path: &str, r#ref: Option<String>) -> Option<GitSource> {
    if path.starts_with('/') {
        return None;
    }
    let normalized_path = path.strip_suffix(".git").unwrap_or(path);
    let normalized_path = normalized_path.trim_start_matches('/');
    if host.is_empty() || normalized_path.is_empty() || normalized_path.split('/').count() < 2 {
        return None;
    }
    if has_unsafe_git_install_part(host, false) || has_unsafe_git_install_part(normalized_path, true)
    {
        return None;
    }
    let pinned = r#ref.as_deref().is_some_and(|r| !r.is_empty());
    Some(GitSource {
        r#type: "git".to_string(),
        repo,
        host: host.to_string(),
        path: normalized_path.to_string(),
        r#ref,
        pinned,
    })
}

fn parse_generic_git_url(url: &str) -> Option<GitSource> {
    let SplitRef {
        repo: repo_without_ref,
        r#ref,
    } = split_ref(url);
    let mut repo = repo_without_ref.clone();
    let host;
    let path;

    if let Some((h, p)) = scp_like(&repo_without_ref) {
        host = h.to_string();
        path = p.to_string();
    } else if ["https://", "http://", "ssh://", "git://"]
        .iter()
        .any(|prefix| repo_without_ref.starts_with(prefix))
    {
        let parsed = url::Url::parse(&repo_without_ref).ok()?;
        host = parsed.host_str().unwrap_or("").to_string();
        path = parsed.path().trim_start_matches('/').to_string();
    } else {
        let slash = repo_without_ref.find('/')?;
        host = repo_without_ref[..slash].to_string();
        path = repo_without_ref[slash + 1..].to_string();
        if !host.contains('.') && host != "localhost" {
            return None;
        }
        repo = format!("https://{repo_without_ref}");
    }

    build_git_source(repo, &host, &path, r#ref)
}

fn has_protocol_prefix(url: &str) -> bool {
    // /^(https?|ssh|git):\/\//i
    let lower = url.to_ascii_lowercase();
    ["http://", "https://", "ssh://", "git://"]
        .iter()
        .any(|p| lower.starts_with(p))
}

/// Parse git source into a GitSource.
///
/// Rules:
/// - With git: prefix, accept all historical shorthand forms.
/// - Without git: prefix, only accept explicit protocol URLs.
pub fn parse_git_url(source: &str) -> Option<GitSource> {
    let trimmed = source.trim();
    let has_git_prefix = trimmed.starts_with("git:");
    let url = if has_git_prefix {
        trimmed[4..].trim()
    } else {
        trimmed
    };

    if !has_git_prefix && !has_protocol_prefix(url) {
        return None;
    }

    let split = split_ref(url);

    let try_candidates = |candidates: Vec<String>, https_repo: bool| -> Option<Option<GitSource>> {
        for candidate in candidates {
            let Some(info) = hosted::from_url(&candidate) else {
                continue;
            };
            if split.r#ref.is_some() && info.project.contains('@') {
                continue;
            }
            let repo = if https_repo {
                format!("https://{}", split.repo)
            } else {
                let use_https_prefix = !["http://", "https://", "ssh://", "git://", "git@"]
                    .iter()
                    .any(|p| split.repo.starts_with(p));
                if use_https_prefix {
                    format!("https://{}", split.repo)
                } else {
                    split.repo.clone()
                }
            };
            let user = info.user.clone().unwrap_or_else(|| "null".to_string());
            let r#ref = info
                .committish
                .clone()
                .filter(|c| !c.is_empty())
                .or_else(|| split.r#ref.clone().filter(|r| !r.is_empty()));
            return Some(build_git_source(
                repo,
                &info.domain,
                &format!("{user}/{}", info.project),
                r#ref,
            ));
        }
        None
    };

    let mut hosted_candidates = Vec::new();
    if let Some(r) = &split.r#ref {
        hosted_candidates.push(format!("{}#{r}", split.repo));
    }
    hosted_candidates.push(url.to_string());
    if let Some(result) = try_candidates(hosted_candidates, false) {
        return result;
    }

    let mut https_candidates = Vec::new();
    if let Some(r) = &split.r#ref {
        https_candidates.push(format!("https://{}#{r}", split.repo));
    }
    https_candidates.push(format!("https://{url}"));
    if let Some(result) = try_candidates(https_candidates, true) {
        return result;
    }

    parse_generic_git_url(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(source: &str) -> GitSource {
        parse_git_url(source).unwrap_or_else(|| panic!("expected git source for {source}"))
    }

    #[test]
    fn parses_protocol_urls_without_prefix() {
        let r = parsed("https://github.com/user/repo");
        assert_eq!((r.host.as_str(), r.path.as_str()), ("github.com", "user/repo"));
        assert_eq!(r.repo, "https://github.com/user/repo");

        let r = parsed("ssh://git@github.com/user/repo");
        assert_eq!((r.host.as_str(), r.path.as_str()), ("github.com", "user/repo"));
        assert_eq!(r.repo, "ssh://git@github.com/user/repo");

        let r = parsed("https://github.com/user/repo@v1.0.0");
        assert_eq!((r.host.as_str(), r.path.as_str()), ("github.com", "user/repo"));
        assert_eq!(r.r#ref.as_deref(), Some("v1.0.0"));
        assert_eq!(r.repo, "https://github.com/user/repo");
        assert!(r.pinned);
    }

    #[test]
    fn parses_shorthand_urls_with_git_prefix() {
        let r = parsed("git:git@github.com:user/repo");
        assert_eq!((r.host.as_str(), r.path.as_str()), ("github.com", "user/repo"));
        assert_eq!(r.repo, "git@github.com:user/repo");
        assert!(!r.pinned);

        let r = parsed("git:github.com/user/repo");
        assert_eq!((r.host.as_str(), r.path.as_str()), ("github.com", "user/repo"));
        assert_eq!(r.repo, "https://github.com/user/repo");

        let r = parsed("git:git@github.com:user/repo@v1.0.0");
        assert_eq!((r.host.as_str(), r.path.as_str()), ("github.com", "user/repo"));
        assert_eq!(r.r#ref.as_deref(), Some("v1.0.0"));
        assert_eq!(r.repo, "git@github.com:user/repo");
    }

    #[test]
    fn rejects_unsafe_git_install_path_inputs() {
        for source in [
            "git:git@evil.example:../../victim/repo",
            "https://evil.example/..%2F..%2Fvictim/repo",
            "https://evil.example/..%2F..%2Fvictim/repo%",
            "git:git@evil.example:/absolute/repo",
            "git:git@evil.example:user\\repo/name",
            "git:git@evil.example:user/repo\0name",
        ] {
            assert_eq!(parse_git_url(source), None, "{source}");
        }
    }

    #[test]
    fn rejects_shorthand_without_git_prefix() {
        assert_eq!(parse_git_url("git@github.com:user/repo"), None);
        assert_eq!(parse_git_url("github.com/user/repo"), None);
        assert_eq!(parse_git_url("user/repo"), None);
    }

    #[test]
    fn parses_other_hosts_and_suffixes() {
        let r = parsed("https://github.com/user/repo.git");
        assert_eq!(r.path, "user/repo");
        let r = parsed("https://gitlab.com/user/repo");
        assert_eq!((r.host.as_str(), r.path.as_str()), ("gitlab.com", "user/repo"));
        let r = parsed("https://bitbucket.org/user/repo");
        assert_eq!((r.host.as_str(), r.path.as_str()), ("bitbucket.org", "user/repo"));
        let r = parsed("https://codeberg.org/user/repo");
        assert_eq!((r.host.as_str(), r.path.as_str()), ("codeberg.org", "user/repo"));
        let r = parsed("git:https://github.com/user/repo");
        assert_eq!((r.host.as_str(), r.path.as_str()), ("github.com", "user/repo"));
        let r = parsed("https://github.com/user/repo@feature/branch");
        assert_eq!(r.r#ref.as_deref(), Some("feature/branch"));
        let r = parsed("git:gitlab.com/group/sub/repo@v2");
        assert_eq!((r.host.as_str(), r.path.as_str()), ("gitlab.com", "group/sub/repo"));
        assert_eq!(r.r#ref.as_deref(), Some("v2"));
        let r = parsed("git:github:user/repo#main");
        assert_eq!((r.host.as_str(), r.path.as_str()), ("github.com", "user/repo"));
        assert_eq!(r.r#ref.as_deref(), Some("main"));
    }

    #[test]
    fn decodes_uri_components_like_js() {
        assert_eq!(decode_uri_component("a%2Fb").as_deref(), Some("a/b"));
        assert_eq!(decode_uri_component("a%"), None);
        assert_eq!(decode_uri_component("a%zz"), None);
        assert_eq!(decode_uri_component("%E2%82%AC").as_deref(), Some("€"));
        assert_eq!(decode_uri_component("%FF"), None);
    }
}
