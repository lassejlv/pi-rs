//! Dependency specifiers, classified like `npm-package-arg` 12 (`npa.resolve`).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};

use crate::utils::git::hosted::{HostedInfo, from_url};

/// A hosted git repository (`hosted-git-info`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostedRepo {
    pub kind: &'static str,
    pub domain: String,
    pub user: Option<String>,
    pub project: String,
}

impl HostedRepo {
    fn from_info(info: &HostedInfo) -> Option<Self> {
        let kind = match info.domain.as_str() {
            "github.com" => "github",
            "gitlab.com" => "gitlab",
            "bitbucket.org" => "bitbucket",
            "gist.github.com" => "gist",
            "git.sr.ht" => "sourcehut",
            _ => return None,
        };
        Some(Self {
            kind,
            domain: info.domain.clone(),
            user: info.user.clone(),
            project: info.project.clone(),
        })
    }

    fn user(&self) -> &str {
        self.user.as_deref().unwrap_or("")
    }

    /// `hosted.https({ noGitPlus: true })`: the URL `git ls-remote`/`git clone` use first.
    pub fn https(&self) -> String {
        match self.kind {
            "gist" => format!("https://{}/{}.git", self.domain, self.project),
            "sourcehut" => format!("https://{}/~{}/{}.git", self.domain, self.user(), self.project),
            _ => format!("https://{}/{}/{}.git", self.domain, self.user(), self.project),
        }
    }

    /// `hosted.sshurl({ noGitPlus: true })`.
    pub fn sshurl(&self) -> String {
        match self.kind {
            "gist" => format!("ssh://git@{}/{}.git", self.domain, self.project),
            "sourcehut" => format!("ssh://git@{}/~{}/{}.git", self.domain, self.user(), self.project),
            _ => format!("ssh://git@{}/{}/{}.git", self.domain, self.user(), self.project),
        }
    }

    /// pacote's `repoUrl`: the `resolved` value (`git+ssh://...#<sha>`).
    pub fn resolved(&self, sha: &str) -> String {
        format!("git+{}#{sha}", self.sshurl())
    }

    /// `hosted.tarball()` for a commit.
    pub fn tarball(&self, committish: &str) -> String {
        match self.kind {
            "github" => format!(
                "https://codeload.{}/{}/{}/tar.gz/{committish}",
                self.domain,
                self.user(),
                self.project
            ),
            "gitlab" => format!(
                "https://{}/{}/{}/repository/archive.tar.gz?ref={committish}",
                self.domain,
                self.user(),
                self.project
            ),
            "bitbucket" => format!(
                "https://{}/{}/{}/get/{committish}.tar.gz",
                self.domain,
                self.user(),
                self.project
            ),
            "gist" => format!("https://codeload.github.com/gist/{}/tar.gz/{committish}", self.project),
            _ => format!(
                "https://{}/~{}/{}/archive/{committish}.tar.gz",
                self.domain,
                self.user(),
                self.project
            ),
        }
    }
}

/// A git dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitSpec {
    /// The specifier as written.
    pub raw: String,
    pub hosted: Option<HostedRepo>,
    /// The repository URL for non-hosted repositories (no `git+`, no fragment).
    pub fetch_spec: Option<String>,
    pub committish: Option<String>,
    /// `#semver:<range>`.
    pub range: Option<String>,
    /// `#path:<dir>` (with a leading `/`).
    pub subdir: Option<String>,
}

/// How a dependency is fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DepSpec {
    /// A registry package (`name` may differ from the install folder for `npm:` aliases).
    Registry { name: String, spec: String },
    /// A tarball URL.
    Tarball { url: String },
    /// A local tarball (`file:x.tgz`).
    LocalTarball { path: PathBuf, save_spec: String },
    /// A local folder, installed as a symlink (`file:../dir`, `./dir`).
    Directory { path: PathBuf, save_spec: String },
    /// A git repository.
    Git(GitSpec),
}

fn is_url(spec: &str) -> bool {
    // /^(?:git[+])?[a-z]+:/i
    let lower = spec.to_ascii_lowercase();
    let rest = lower.strip_prefix("git+").unwrap_or(&lower);
    let letters = rest.bytes().take_while(u8::is_ascii_lowercase).count();
    letters > 0 && rest.as_bytes().get(letters) == Some(&b':')
}

fn is_file_spec(spec: &str) -> bool {
    if spec.to_ascii_lowercase().starts_with("file:") {
        return true;
    }
    // /^(?:[.]|~[/]|[/]|[a-zA-Z]:)/ (and `\` on Windows)
    let bytes = spec.as_bytes();
    spec.starts_with('.')
        || spec.starts_with("~/")
        || spec.starts_with('/')
        || (cfg!(windows) && (spec.starts_with('\\') || spec.starts_with("~\\")))
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

fn is_file_type(spec: &str) -> bool {
    let lower = spec.to_ascii_lowercase();
    lower.ends_with(".tgz") || lower.ends_with(".tar.gz") || lower.ends_with(".tar")
}

/// npa's `setGitAttrs`.
fn set_git_attrs(spec: &mut GitSpec, committish: Option<&str>) -> anyhow::Result<()> {
    let Some(committish) = committish.filter(|c| !c.is_empty()) else {
        return Ok(());
    };
    for part in committish.split("::") {
        if !part.contains(':') {
            if spec.range.is_some() {
                bail!("cannot override existing semver range with a committish");
            }
            if spec.committish.is_some() {
                bail!("cannot override existing committish with a second committish");
            }
            spec.committish = Some(part.to_string());
            continue;
        }
        let mut pieces = part.split(':');
        let name = pieces.next().unwrap_or("");
        let value = pieces.next().unwrap_or("");
        match name {
            "semver" => {
                if spec.committish.is_some() {
                    bail!("cannot override existing committish with a semver range");
                }
                if spec.range.is_some() {
                    bail!("cannot override existing semver range with a second semver range");
                }
                spec.range = Some(crate::utils::git::decode_uri_component(value).unwrap_or_else(|| value.to_string()));
            }
            "path" => {
                if spec.subdir.is_some() {
                    bail!("cannot override existing path with a second path");
                }
                spec.subdir = Some(format!("/{value}"));
            }
            other => tracing::warn!("npm-package-arg ignoring unknown key \"{other}\""),
        }
    }
    Ok(())
}

/// npa's `pathToFileURL` + `fromFile`: resolve a file specifier against `where_`.
fn from_file(spec: &str, where_: &Path) -> anyhow::Result<DepSpec> {
    let raw = spec.strip_prefix("file:").or_else(|| spec.strip_prefix("FILE:")).unwrap_or(spec);
    let raw = if let Some(rest) = raw.strip_prefix("//")
        && !rest.starts_with('/')
    {
        // file://path → /path (npm's RFC 8089 backwards compatibility)
        format!("/{rest}")
    } else {
        raw.to_string()
    };
    // file:/../path → ../path
    let trimmed = raw.trim_start_matches('/');
    let raw = if raw.starts_with('/') && (trimmed.starts_with("../") || trimmed.starts_with("./") || trimmed == ".." || trimmed == ".") && raw.len() - trimmed.len() <= 3 {
        trimmed.to_string()
    } else {
        raw
    };
    let decoded = crate::utils::git::decode_uri_component(&raw).unwrap_or(raw.clone());
    let home = dirs::home_dir().unwrap_or_default();
    let (path, save_spec) = if let Some(rest) = decoded.strip_prefix("~/").or(if decoded == "~" { Some("") } else { None }) {
        (home.join(rest), format!("file:{decoded}"))
    } else if Path::new(&decoded).is_absolute() {
        let path = normalize(Path::new(&decoded));
        let save = format!("file:{}", path.to_string_lossy());
        (path, save)
    } else {
        let path = normalize(&where_.join(&decoded));
        let relative = crate::utils::paths::node_relative(&where_.to_string_lossy(), &path.to_string_lossy());
        (path, format!("file:{relative}"))
    };
    let save_spec = save_spec.replace('\\', "/");
    if is_file_type(spec) {
        Ok(DepSpec::LocalTarball { path, save_spec })
    } else {
        Ok(DepSpec::Directory { path, save_spec })
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn from_hosted(spec: &str, info: &HostedInfo) -> anyhow::Result<Option<DepSpec>> {
    let Some(hosted) = HostedRepo::from_info(info) else {
        return Ok(None);
    };
    let mut git = GitSpec {
        raw: spec.to_string(),
        hosted: Some(hosted),
        fetch_spec: None,
        committish: None,
        range: None,
        subdir: None,
    };
    set_git_attrs(&mut git, info.committish.as_deref())?;
    Ok(Some(DepSpec::Git(git)))
}

/// npa's `fromURL`.
fn from_url_spec(spec: &str) -> anyhow::Result<DepSpec> {
    let mut git = GitSpec {
        raw: spec.to_string(),
        hosted: None,
        fetch_spec: None,
        committish: None,
        range: None,
        subdir: None,
    };
    if let Some(rest) = spec.strip_prefix("git+ssh://") {
        // scp-style: git+ssh://user@host:path[#committish]
        let (body, hash) = match rest.find('#') {
            Some(i) => (&rest[..i], Some(&rest[i + 1..])),
            None => (rest, None),
        };
        if let Some(colon) = body.find(':') {
            let after = &body[colon + 1..];
            let port_like = after.bytes().take_while(u8::is_ascii_digit).count();
            let is_port = port_like > 0 && matches!(after.as_bytes().get(port_like), None | Some(b'/'));
            if colon > 0 && !body[..colon].is_empty() && !after.is_empty() && !is_port {
                set_git_attrs(&mut git, hash)?;
                git.fetch_spec = Some(body.to_string());
                return Ok(DepSpec::Git(git));
            }
        }
    }
    let raw = if spec.starts_with("git+file://") {
        spec.replace('\\', "/")
    } else {
        spec.to_string()
    };
    let parsed = url::Url::parse(&raw).map_err(|_| anyhow!("Invalid URL"))?;
    let protocol = format!("{}:", parsed.scheme());
    match protocol.as_str() {
        "git:" | "git+http:" | "git+https:" | "git+rsync:" | "git+ftp:" | "git+file:" | "git+ssh:" => {
            set_git_attrs(&mut git, parsed.fragment())?;
            let mut without_hash = parsed.clone();
            without_hash.set_fragment(None);
            let fetch = without_hash.to_string();
            git.fetch_spec = Some(fetch.strip_prefix("git+").map(str::to_string).unwrap_or(fetch));
            Ok(DepSpec::Git(git))
        }
        "http:" | "https:" => Ok(DepSpec::Tarball { url: spec.to_string() }),
        _ => bail!("Unsupported URL Type \"{protocol}\": {spec}"),
    }
}

/// Classify the dependency `alias` with specifier `raw`, resolving paths against `where_` (the
/// folder of the package.json that declares it).
pub(crate) fn classify_spec(alias: &str, raw: &str, where_: &Path) -> anyhow::Result<DepSpec> {
    let spec = raw;
    if is_file_spec(spec) {
        return from_file(spec, where_);
    }
    if spec.to_ascii_lowercase().starts_with("npm:") {
        let rest = &spec[4..];
        let (name, version) = super::split_name_spec(rest);
        return Ok(DepSpec::Registry {
            name,
            spec: version.unwrap_or_default(),
        });
    }
    if !spec.is_empty()
        && let Some(info) = from_url(spec)
        && let Some(dep) = from_hosted(spec, &info)?
    {
        return Ok(dep);
    }
    if !spec.is_empty() && is_url(spec) {
        return from_url_spec(spec);
    }
    if !spec.is_empty() && (spec.contains('/') || is_file_type(spec)) {
        return from_file(spec, where_);
    }
    Ok(DepSpec::Registry {
        name: alias.to_string(),
        spec: spec.trim().to_string(),
    })
}

/// npa's argument parsing for `npm install <arg>`: `Some(spec)` when the argument is not a
/// `name@range` registry argument (URLs, git, paths).
pub(crate) fn non_registry_argument(arg: &str) -> bool {
    if is_url(arg) {
        return true;
    }
    // isGit: /^[^@]+@[^:.]+\.[^:]+:.+$/
    if let Some(at) = arg.find('@')
        && at > 0
        && let Some(colon) = arg[at + 1..].find(':')
    {
        let host = &arg[at + 1..at + 1 + colon];
        if host.contains('.') && !host.starts_with('.') && colon + at + 2 < arg.len() {
            return true;
        }
    }
    let name_ends_at = arg[1.min(arg.len())..].find('@').map(|i| i + 1);
    let name_part = match name_ends_at {
        Some(i) => &arg[..i],
        None => arg,
    };
    !name_part.starts_with('@') && (name_part.contains('/') || is_file_type(name_part))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(spec: &str) -> GitSpec {
        match classify_spec("x", spec, Path::new("/proj")).unwrap() {
            DepSpec::Git(git) => git,
            other => panic!("{spec}: {other:?}"),
        }
    }

    #[test]
    fn classifies_like_npm_package_arg() {
        assert_eq!(
            classify_spec("safer-buffer", "~2.1.0", Path::new("/p")).unwrap(),
            DepSpec::Registry { name: "safer-buffer".into(), spec: "~2.1.0".into() }
        );
        assert_eq!(
            classify_spec("a", "npm:@s/b@^1", Path::new("/p")).unwrap(),
            DepSpec::Registry { name: "@s/b".into(), spec: "^1".into() }
        );
        assert_eq!(
            classify_spec("lib", "file:../lib", Path::new("/p/root")).unwrap(),
            DepSpec::Directory { path: "/p/lib".into(), save_spec: "file:../lib".into() }
        );
        assert_eq!(
            classify_spec("lib", "./vendor/lib-1.0.0.tgz", Path::new("/p")).unwrap(),
            DepSpec::LocalTarball { path: "/p/vendor/lib-1.0.0.tgz".into(), save_spec: "file:vendor/lib-1.0.0.tgz".into() }
        );
        assert_eq!(
            classify_spec("t", "https://example.com/t.tgz", Path::new("/p")).unwrap(),
            DepSpec::Tarball { url: "https://example.com/t.tgz".into() }
        );
        let home = classify_spec("local", "~/src/local", Path::new("/p")).unwrap();
        assert!(matches!(home, DepSpec::Directory { ref save_spec, .. } if save_spec == "file:~/src/local"));
        assert_eq!(
            classify_spec("x", "link:../lib", Path::new("/p")).unwrap_err().to_string(),
            "Unsupported URL Type \"link:\": link:../lib"
        );
        assert_eq!(
            classify_spec("x", "workspace:*", Path::new("/p")).unwrap_err().to_string(),
            "Unsupported URL Type \"workspace:\": workspace:*"
        );
    }

    #[test]
    fn classifies_git_specs() {
        let shorthand = git("user/repo#v1.2.0");
        let hosted = shorthand.hosted.clone().unwrap();
        assert_eq!(hosted.kind, "github");
        assert_eq!(shorthand.committish.as_deref(), Some("v1.2.0"));
        assert_eq!(hosted.https(), "https://github.com/user/repo.git");
        assert_eq!(hosted.resolved("abc"), "git+ssh://git@github.com/user/repo.git#abc");
        assert_eq!(hosted.tarball("abc"), "https://codeload.github.com/user/repo/tar.gz/abc");
        let range = git("github:user/repo#semver:^1.0.0");
        assert_eq!(range.range.as_deref(), Some("^1.0.0"));
        let gitlab = git("gitlab:group/proj");
        assert_eq!(gitlab.hosted.unwrap().tarball("s"), "https://gitlab.com/group/proj/repository/archive.tar.gz?ref=s");
        let plain = git("git+https://example.com/repo.git#main::path:packages/a");
        assert_eq!(plain.fetch_spec.as_deref(), Some("https://example.com/repo.git"));
        assert_eq!(plain.committish.as_deref(), Some("main"));
        assert_eq!(plain.subdir.as_deref(), Some("/packages/a"));
        let scp = git("git+ssh://git@example.com:team/repo.git#dev");
        assert_eq!(scp.fetch_spec.as_deref(), Some("git@example.com:team/repo.git"));
        let file = git("git+file:///srv/repo.git");
        assert_eq!(file.fetch_spec.as_deref(), Some("file:///srv/repo.git"));
    }

    #[test]
    fn detects_non_registry_arguments() {
        assert!(!non_registry_argument("pkg@1.0.0"));
        assert!(!non_registry_argument("@scope/pkg@^2"));
        assert!(non_registry_argument("github:user/repo"));
        assert!(non_registry_argument("user/repo"));
        assert!(non_registry_argument("./local"));
        assert!(non_registry_argument("git@github.com:user/repo.git"));
        assert!(non_registry_argument("https://example.com/x.tgz"));
    }
}
