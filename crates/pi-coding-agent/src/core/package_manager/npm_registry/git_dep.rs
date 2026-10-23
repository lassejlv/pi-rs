//! Git dependencies, fetched like pacote's `GitFetcher` with `@npmcli/git`: the ref is resolved
//! with `git ls-remote` (branches, tags, `#semver:` ranges over version tags), hosted
//! repositories are downloaded as the host's tarball of the resolved commit, everything else is
//! cloned with npm's clone strategies (plain, branch/tag, advertised ref, or a mirror clone for
//! refs `ls-remote` does not list), and submodules are updated.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, anyhow, bail};

use super::spec::GitSpec;
use crate::core::package_manager::node_semver;

const SHALLOW_HOSTS: &[&str] = &[
    "github.com",
    "gist.github.com",
    "gitlab.com",
    "bitbucket.com",
    "bitbucket.org",
];

#[derive(Debug, Clone)]
pub(super) struct RevDoc {
    pub sha: String,
    pub r#ref: String,
    pub raw_ref: String,
    pub kind: &'static str,
}

#[derive(Debug, Default)]
pub(super) struct Revs {
    pub refs: HashMap<String, RevDoc>,
    pub shas: HashMap<String, Vec<String>>,
    pub versions: BTreeMap<String, RevDoc>,
    pub latest: Option<String>,
}

fn ref_type(raw: &str) -> &'static str {
    if raw.starts_with("refs/tags/") {
        "tag"
    } else if raw.starts_with("refs/heads/") {
        "branch"
    } else if raw.starts_with("refs/pull/") {
        "pull"
    } else if raw == "HEAD" {
        "head"
    } else {
        "other"
    }
}

/// `@npmcli/git`'s `lines-to-revs`.
pub(super) fn lines_to_revs(output: &str) -> Revs {
    let mut revs = Revs::default();
    let mut order: Vec<String> = Vec::new();
    for line in output.lines() {
        let mut split = line.split_whitespace();
        let (Some(sha), Some(raw_ref)) = (split.next(), split.next()) else {
            continue;
        };
        let kind = ref_type(raw_ref);
        let r#ref = match kind {
            "tag" => raw_ref["refs/tags/".len()..].to_string(),
            "branch" => raw_ref["refs/heads/".len()..].to_string(),
            "pull" => {
                let r = &raw_ref["refs/".len()..];
                r.strip_suffix("/head").unwrap_or(r).to_string()
            }
            "head" => "HEAD".to_string(),
            _ => raw_ref.to_string(),
        };
        let doc = RevDoc {
            sha: sha.to_string(),
            r#ref: r#ref.clone(),
            raw_ref: raw_ref.to_string(),
            kind,
        };
        for key in [r#ref.clone(), raw_ref.to_string()] {
            if !revs.refs.contains_key(&key) {
                order.push(key.clone());
            }
            revs.refs.insert(key, doc.clone());
        }
    }
    // peelTags: refs/tags/x^{} carries the commit of an annotated tag.
    let peeled: Vec<String> = revs.refs.keys().filter(|r| r.ends_with("^{}")).cloned().collect();
    for key in peeled {
        let base = key.trim_end_matches("^{}").to_string();
        if let Some(sha) = revs.refs.get(&key).map(|d| d.sha.clone())
            && revs.refs.contains_key(&base)
        {
            for doc in revs.refs.values_mut() {
                if doc.r#ref == base || doc.raw_ref == base {
                    doc.sha = sha.clone();
                }
            }
            revs.refs.remove(&key);
            order.retain(|k| *k != key);
        }
    }
    // versions from tags like `v1.2.3` / `release-1.2.3`
    for key in &order {
        let Some(doc) = revs.refs.get(key) else { continue };
        if doc.kind != "tag" || doc.r#ref.ends_with("^{}") || key != &doc.r#ref {
            continue;
        }
        if let Some(version) = tag_version(&doc.r#ref) {
            revs.versions.insert(version, doc.clone());
        }
    }
    // shaList
    for key in &order {
        if let Some(doc) = revs.refs.get(key) {
            revs.shas.entry(doc.sha.clone()).or_default().push(key.clone());
        }
    }
    // distTags
    let head_sha = revs.refs.get("HEAD").map(|d| d.sha.clone());
    let latest_branch = revs.refs.get("latest").map(|d| d.sha.clone());
    for (version, doc) in &revs.versions {
        if latest_branch.as_deref() == Some(doc.sha.as_str()) {
            revs.latest = Some(version.clone());
        } else if head_sha.as_deref() == Some(doc.sha.as_str()) && latest_branch.is_none() {
            revs.latest = Some(version.clone());
        }
    }
    revs
}

/// `/v?(\d+\.\d+\.\d+(?:[-+].+)?)$/` + `semver.clean(.., true)`.
fn tag_version(tag: &str) -> Option<String> {
    let re = regex::Regex::new(r"v?(\d+\.\d+\.\d+(?:[-+].+)?)$").ok()?;
    let candidate = re.captures(tag)?.get(1)?.as_str();
    let mut version = node_semver::parse_version(candidate)?;
    version.build = semver::BuildMetadata::EMPTY;
    Some(version.to_string())
}

fn git_env(command: &mut tokio::process::Command) {
    if std::env::var_os("GIT_ASKPASS").is_none() {
        command.env("GIT_ASKPASS", "echo");
    }
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        command.env("GIT_SSH_COMMAND", "ssh -oStrictHostKeyChecking=accept-new");
    }
}

/// Run git (with `--no-replace-objects`) and return its stdout.
pub(super) async fn git(args: &[&str], cwd: Option<&Path>) -> anyhow::Result<String> {
    let git = crate::core::package_manager::find_executable("git")
        .ok_or_else(|| anyhow!("No git binary found in $PATH"))?;
    let mut command = tokio::process::Command::new(git);
    command.arg("--no-replace-objects").args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    git_env(&mut command);
    command.stdin(std::process::Stdio::null());
    let output = command
        .output()
        .await
        .with_context(|| format!("spawn git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "An unknown git error occurred\ncommand git --no-replace-objects {}\n{}",
            args.join(" "),
            stderr.trim_end()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(super) async fn revs(repo: &str) -> anyhow::Result<Revs> {
    let output = git(&["ls-remote", repo], None).await?;
    Ok(lines_to_revs(&output))
}

/// `resolveRef` (npm-pick-manifest over version tags for `#semver:` ranges).
pub(super) fn resolve_ref(revs: &Revs, spec: &GitSpec) -> Option<RevDoc> {
    if let Some(range) = &spec.range {
        if let Some(latest) = &revs.latest
            && node_semver::satisfies(latest, range)
        {
            return revs.versions.get(latest).cloned();
        }
        let best = node_semver::max_satisfying(revs.versions.keys().map(String::as_str), range)?;
        return revs.versions.get(&best).cloned();
    }
    let Some(committish) = &spec.committish else {
        return revs.refs.get("HEAD").cloned();
    };
    if let Some(doc) = revs.refs.get(committish) {
        return Some(doc.clone());
    }
    revs.shas
        .get(committish)
        .and_then(|names| names.first())
        .and_then(|name| revs.refs.get(name))
        .cloned()
}

fn is_shallow_host(repo: &str) -> bool {
    url::Url::parse(repo)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .is_some_and(|host| SHALLOW_HOSTS.contains(&host.as_str()))
}

async fn update_submodules(target: &Path) -> anyhow::Result<()> {
    if target.join(".gitmodules").exists() {
        git(&["submodule", "update", "-q", "--init", "--recursive"], Some(target)).await?;
    }
    Ok(())
}

/// `@npmcli/git` `clone`: check out the ref in `target`, returning the commit.
pub(super) async fn clone(repo: &str, revs: &Revs, spec: &GitSpec, target: &Path) -> anyhow::Result<String> {
    let target_str = target.to_string_lossy().into_owned();
    let longpaths: &[&str] = if cfg!(windows) {
        &["--config", "core.longpaths=true"]
    } else {
        &[]
    };
    let Some(doc) = resolve_ref(revs, spec) else {
        // unresolved: mirror clone, then check out the ref.
        let committish = spec.committish.clone().unwrap_or_else(|| "HEAD".to_string());
        std::fs::create_dir_all(target)?;
        let git_dir = format!("{target_str}/.git");
        let mut args = vec!["clone", "--mirror", "-q", repo, git_dir.as_str()];
        args.extend_from_slice(longpaths);
        git(&args, Some(target)).await?;
        git(&["init"], Some(target)).await?;
        git(&["checkout", &committish], Some(target)).await?;
        update_submodules(target).await?;
        let head = git(&["rev-parse", "--revs-only", "HEAD"], Some(target)).await?;
        return Ok(head.trim().to_string());
    };
    let shallow = is_shallow_host(repo);
    let head_sha = revs.refs.get("HEAD").map(|d| d.sha.as_str());
    if Some(doc.sha.as_str()) == head_sha {
        let mut args = vec!["clone", repo, target_str.as_str(), "--recurse-submodules"];
        if shallow {
            args.push("--depth=1");
        }
        args.extend_from_slice(longpaths);
        git(&args, None).await?;
        return Ok(doc.sha);
    }
    if doc.kind == "tag" || doc.kind == "branch" {
        let mut args = vec![
            "clone",
            "-b",
            doc.r#ref.as_str(),
            repo,
            target_str.as_str(),
            "--recurse-submodules",
        ];
        if shallow {
            args.push("--depth=1");
        }
        args.extend_from_slice(longpaths);
        git(&args, None).await?;
        return Ok(doc.sha);
    }
    std::fs::create_dir_all(target)?;
    git(&["init"], Some(target)).await?;
    if cfg!(windows) {
        git(&["config", "--local", "--add", "core.longpaths", "true"], Some(target)).await?;
    }
    git(&["remote", "add", "origin", repo], Some(target)).await?;
    let mut fetch = vec!["fetch", "origin", doc.raw_ref.as_str()];
    if shallow {
        fetch.push("--depth=1");
    }
    git(&fetch, Some(target)).await?;
    git(&["checkout", &doc.sha], Some(target)).await?;
    update_submodules(target).await?;
    Ok(doc.sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LS_REMOTE: &str = "aaa\tHEAD\naaa\trefs/heads/main\nbbb\trefs/heads/dev\nccc\trefs/tags/v1.0.0\nddd\trefs/tags/v1.1.0\neee\trefs/tags/v1.1.0^{}\nfff\trefs/tags/release-2.0.0\naaa\trefs/tags/v2.1.0\n111\trefs/pull/7/head\n";

    fn spec(committish: Option<&str>, range: Option<&str>) -> GitSpec {
        GitSpec {
            raw: String::new(),
            hosted: None,
            fetch_spec: Some("https://example.com/r.git".into()),
            committish: committish.map(str::to_string),
            range: range.map(str::to_string),
            subdir: None,
        }
    }

    #[test]
    fn parses_ls_remote_like_npmcli_git() {
        let revs = lines_to_revs(LS_REMOTE);
        assert_eq!(revs.refs["v1.1.0"].sha, "eee", "annotated tags are peeled");
        assert_eq!(revs.refs["dev"].kind, "branch");
        assert_eq!(revs.refs["pull/7"].sha, "111");
        assert_eq!(
            revs.versions.keys().cloned().collect::<Vec<_>>(),
            vec!["1.0.0", "1.1.0", "2.0.0", "2.1.0"]
        );
        assert_eq!(revs.latest.as_deref(), Some("2.1.0"));
        assert_eq!(resolve_ref(&revs, &spec(None, None)).unwrap().sha, "aaa");
        assert_eq!(resolve_ref(&revs, &spec(Some("dev"), None)).unwrap().sha, "bbb");
        assert_eq!(resolve_ref(&revs, &spec(Some("ccc"), None)).unwrap().r#ref, "v1.0.0");
        assert!(resolve_ref(&revs, &spec(Some("abc123"), None)).is_none());
        assert_eq!(resolve_ref(&revs, &spec(None, Some("^1.0.0"))).unwrap().sha, "eee");
        assert_eq!(resolve_ref(&revs, &spec(None, Some(">=2"))).unwrap().sha, "aaa");
    }
}
