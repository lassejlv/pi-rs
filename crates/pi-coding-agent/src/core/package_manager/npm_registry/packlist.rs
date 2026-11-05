//! The files npm packs from a package folder: a port of `npm-packlist` 9 on top of
//! `ignore-walk` 6 and the `minimatch` 9 options they use (`matchBase`, `dot`, `nocase`,
//! `flipNegate`, partial matching). Used for git dependencies, which npm installs by packing the
//! prepared checkout.
//!
//! Not ported: bundled dependencies of the packed folder (`bundleDependencies` are not added to
//! the list) and workspace-aware rules.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::Value;

use crate::core::package_manager::glob::brace_expand;

/// npm-packlist's default rules (applied everywhere).
const DEFAULTS: &[&str] = &[
    ".npmignore",
    ".gitignore",
    "**/.git",
    "**/.svn",
    "**/.hg",
    "**/CVS",
    "**/.git/**",
    "**/.svn/**",
    "**/.hg/**",
    "**/CVS/**",
    "/.lock-wscript",
    "/.wafpickle-*",
    "/build/config.gypi",
    "npm-debug.log",
    "**/.npmrc",
    ".*.swp",
    ".DS_Store",
    "**/.DS_Store/**",
    "._*",
    "**/._*/**",
    "*.orig",
    "/archived-packages/**",
];

const STRICT_DEFAULTS: &[&str] = &["/.git"];

#[derive(Debug, Clone)]
enum Part {
    Globstar,
    Literal(String),
    Pattern(Regex),
}

/// A minimatch pattern with `{ matchBase, dot, flipNegate, nocase }`.
#[derive(Debug, Clone)]
struct Rule {
    negate: bool,
    /// The preprocessed glob parts (strings), used for ignore-walk's "relative rule" test.
    glob_parts: Vec<Vec<String>>,
    set: Vec<Vec<Part>>,
}

fn slash_split(p: &str) -> Vec<String> {
    // p.split(/\/+/)
    let mut out = Vec::new();
    let mut current = String::new();
    let mut last_slash = false;
    for c in p.chars() {
        if c == '/' {
            if !last_slash {
                out.push(std::mem::take(&mut current));
            }
            last_slash = true;
        } else {
            current.push(c);
            last_slash = false;
        }
    }
    out.push(current);
    out
}

/// minimatch's `levelOneOptimize`.
fn level_one_optimize(parts: Vec<String>) -> Vec<String> {
    let mut set: Vec<String> = Vec::new();
    for part in parts {
        let prev = set.last().cloned();
        if part == "**" && prev.as_deref() == Some("**") {
            continue;
        }
        if part == ".."
            && let Some(prev) = &prev
            && prev != ".."
            && prev != "."
            && prev != "**"
        {
            set.pop();
            continue;
        }
        set.push(part);
    }
    if set.is_empty() {
        vec![String::new()]
    } else {
        set
    }
}

fn has_magic(part: &str) -> bool {
    part.contains(['*', '?', '[']) || part.contains("+(") || part.contains("@(")
}

/// Translate one segment into a regex (`dot: true`, `nocase: true`).
fn segment_regex(segment: &str) -> Option<Regex> {
    let chars: Vec<char> = segment.chars().collect();
    let mut re = String::from("(?i)^");
    let mut i = 0;
    let mut group_stack: Vec<char> = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if i + 1 < chars.len() => {
                re.push_str(&regex::escape(&chars[i + 1].to_string()));
                i += 2;
                continue;
            }
            '*' | '?' | '+' | '@' if i + 1 < chars.len() && chars[i + 1] == '(' => {
                group_stack.push(c);
                re.push_str("(?:");
                i += 2;
                continue;
            }
            '|' if !group_stack.is_empty() => re.push('|'),
            ')' if !group_stack.is_empty() => {
                let kind = group_stack.pop().unwrap_or('@');
                re.push(')');
                match kind {
                    '*' => re.push('*'),
                    '+' => re.push('+'),
                    '?' => re.push('?'),
                    _ => {}
                }
            }
            '*' => {
                while i + 1 < chars.len() && chars[i + 1] == '*' {
                    i += 1;
                }
                re.push_str("[^/]*?");
            }
            '?' => re.push_str("[^/]"),
            '[' => {
                let mut j = i + 1;
                if j < chars.len() && (chars[j] == '!' || chars[j] == '^') {
                    j += 1;
                }
                if j < chars.len() && chars[j] == ']' {
                    j += 1;
                }
                while j < chars.len() && chars[j] != ']' {
                    j += 1;
                }
                if j >= chars.len() {
                    re.push_str("\\[");
                } else {
                    let mut class = String::from("[");
                    let mut k = i + 1;
                    if chars[k] == '!' || chars[k] == '^' {
                        class.push('^');
                        k += 1;
                    }
                    while k < j {
                        let ch = chars[k];
                        if ch == '\\' || ch == '[' || (ch == '^' && class.len() == 1) {
                            class.push('\\');
                        }
                        class.push(ch);
                        k += 1;
                    }
                    class.push(']');
                    re.push_str(&class);
                    i = j;
                }
            }
            other => re.push_str(&regex::escape(&other.to_string())),
        }
        i += 1;
    }
    if !group_stack.is_empty() {
        return None;
    }
    re.push('$');
    Regex::new(&re).ok()
}

impl Rule {
    fn new(pattern: &str) -> Option<Rule> {
        if pattern.starts_with('#') || pattern.is_empty() {
            return None;
        }
        let mut negate = false;
        let mut offset = 0;
        for c in pattern.chars() {
            if c != '!' {
                break;
            }
            negate = !negate;
            offset += 1;
        }
        let pattern = &pattern[offset..];
        let mut seen = BTreeSet::new();
        let mut glob_parts = Vec::new();
        for expanded in brace_expand(pattern) {
            if !seen.insert(expanded.clone()) {
                continue;
            }
            glob_parts.push(level_one_optimize(slash_split(&expanded)));
        }
        let set = glob_parts
            .iter()
            .filter_map(|parts| {
                parts
                    .iter()
                    .map(|part| {
                        if part == "**" {
                            Some(Part::Globstar)
                        } else if !has_magic(part) && part.to_lowercase() == part.to_uppercase() {
                            Some(Part::Literal(part.replace('\\', "")))
                        } else if !has_magic(part) {
                            Regex::new(&format!("(?i)^{}$", regex::escape(&part.replace('\\', ""))))
                                .ok()
                                .map(Part::Pattern)
                        } else {
                            segment_regex(part).map(Part::Pattern)
                        }
                    })
                    .collect::<Option<Vec<Part>>>()
            })
            .collect();
        Some(Rule {
            negate,
            glob_parts,
            set,
        })
    }

    /// `minimatch.match(f, partial)` with `flipNegate`: true when the pattern itself matches.
    fn matches(&self, f: &str, partial: bool) -> bool {
        if f == "/" && partial {
            return true;
        }
        let ff = slash_split(f);
        let mut filename = ff.last().cloned().unwrap_or_default();
        if filename.is_empty() {
            for part in ff.iter().rev().skip(1) {
                if !part.is_empty() {
                    filename = part.clone();
                    break;
                }
            }
        }
        for pattern in &self.set {
            let hit = if pattern.len() == 1 {
                match_one(std::slice::from_ref(&filename), pattern, partial)
            } else {
                match_one(&ff, pattern, partial)
            };
            if hit {
                return true;
            }
        }
        false
    }

    fn is_relative(&self) -> bool {
        self.glob_parts.iter().any(|part| {
            let last_nonempty = part.last().is_some_and(|l| !l.is_empty());
            part.len() <= if last_nonempty { 1 } else { 2 }
        })
    }
}

/// minimatch's `matchOne` (with `dot: true`).
fn match_one(file: &[String], pattern: &[Part], partial: bool) -> bool {
    let (fl, pl) = (file.len(), pattern.len());
    let (mut fi, mut pi) = (0, 0);
    while fi < fl && pi < pl {
        let f = &file[fi];
        match &pattern[pi] {
            Part::Globstar => {
                let pr = pi + 1;
                if pr == pl {
                    return file[fi..].iter().all(|f| f != "." && f != "..");
                }
                let mut fr = fi;
                while fr < fl {
                    if match_one(&file[fr..], &pattern[pr..], partial) {
                        return true;
                    }
                    let swallowee = &file[fr];
                    if swallowee == "." || swallowee == ".." {
                        break;
                    }
                    fr += 1;
                }
                return partial && fr == fl;
            }
            Part::Literal(p) => {
                if p != f {
                    return false;
                }
            }
            Part::Pattern(re) => {
                if !re.is_match(f) {
                    return false;
                }
            }
        }
        fi += 1;
        pi += 1;
    }
    if fi == fl && pi == pl {
        true
    } else if fi == fl {
        partial
    } else {
        fi == fl - 1 && file[fi].is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum IgnoreSource {
    Defaults,
    PackageJson,
    NpmIgnore,
    GitIgnore,
    Strict,
}

impl IgnoreSource {
    fn file_name(self) -> Option<&'static str> {
        match self {
            IgnoreSource::PackageJson => Some("package.json"),
            IgnoreSource::NpmIgnore => Some(".npmignore"),
            IgnoreSource::GitIgnore => Some(".gitignore"),
            _ => None,
        }
    }
}

fn parse_rules(content: &str) -> Vec<Rule> {
    content
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter(|line| {
            let trimmed = line.trim();
            !(trimmed.starts_with('#') || trimmed.is_empty())
        })
        .filter_map(|line| Rule::new(line.trim()))
        .collect()
}

struct Walker<'a> {
    path: PathBuf,
    basename: String,
    ignore_files: Vec<IgnoreSource>,
    rules: HashMap<IgnoreSource, Option<Vec<Rule>>>,
    parent: Option<&'a Walker<'a>>,
    exact: bool,
    is_package: bool,
    required_files: Vec<String>,
}

impl<'a> Walker<'a> {
    fn new(
        path: PathBuf,
        parent: Option<&'a Walker<'a>>,
        is_package: bool,
        exact: bool,
        required_files: Vec<String>,
    ) -> Self {
        let ignore_files = if is_package {
            vec![
                IgnoreSource::Defaults,
                IgnoreSource::PackageJson,
                IgnoreSource::NpmIgnore,
                IgnoreSource::GitIgnore,
                IgnoreSource::Strict,
            ]
        } else {
            vec![
                IgnoreSource::Defaults,
                IgnoreSource::NpmIgnore,
                IgnoreSource::GitIgnore,
                IgnoreSource::Strict,
            ]
        };
        let basename = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut walker = Walker {
            path,
            basename,
            ignore_files,
            rules: HashMap::new(),
            parent,
            exact,
            is_package,
            required_files,
        };
        walker.inject(IgnoreSource::Defaults, DEFAULTS.iter().map(|s| s.to_string()).collect());
        if !walker.is_package {
            let mut strict: Vec<String> = STRICT_DEFAULTS.iter().map(|s| s.to_string()).collect();
            strict.extend(walker.required_files.iter().map(|f| format!("!{f}")));
            walker.inject(IgnoreSource::Strict, strict);
        }
        walker
    }

    fn inject(&mut self, source: IgnoreSource, rules: Vec<String>) {
        let content = format!("{}\n", rules.join("\n"));
        self.rules.insert(source, Some(parse_rules(&content)));
    }

    /// npm-packlist's `processPackage`.
    fn process_package(&mut self, manifest: &Value) {
        let mut ignores: Vec<String> = Vec::new();
        let mut strict: Vec<String> = STRICT_DEFAULTS.iter().map(|s| s.to_string()).collect();
        strict.extend(
            [
                "!/package.json",
                "!/readme{,.*[^~$]}",
                "!/copying{,.*[^~$]}",
                "!/license{,.*[^~$]}",
                "!/licence{,.*[^~$]}",
                "/.git",
                "/node_modules",
                ".npmrc",
                "/package-lock.json",
                "/yarn.lock",
                "/pnpm-lock.yaml",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        if let Some(files) = manifest.get("files").and_then(Value::as_array) {
            for file in files.iter().filter_map(Value::as_str) {
                let mut file = file.to_string();
                if file.starts_with("./") {
                    file = file[1..].to_string();
                }
                if file.ends_with("/*") {
                    file.push('*');
                }
                let inverse = format!("!{file}");
                let stat_path = self.path.join(file.trim_start_matches('!').trim_start_matches('/'));
                match std::fs::symlink_metadata(&stat_path) {
                    Ok(meta) if meta.is_file() => {
                        strict.insert(0, inverse);
                        self.required_files
                            .push(file.strip_prefix('/').unwrap_or(&file).to_string());
                    }
                    Ok(meta) if meta.is_dir() => {
                        ignores.push(inverse.clone());
                        ignores.push(format!("{inverse}/**"));
                    }
                    Ok(_) => {}
                    Err(_) => ignores.push(inverse),
                }
            }
            let mut rules = vec!["*".to_string()];
            rules.extend(ignores);
            self.inject(IgnoreSource::PackageJson, rules);
        }
        if let Some(browser) = manifest.get("browser").and_then(Value::as_str) {
            strict.push(format!("!/{browser}"));
        }
        if let Some(main) = manifest.get("main").and_then(Value::as_str) {
            strict.push(format!("!/{main}"));
        }
        for bin in normalized_bins(manifest) {
            strict.push(format!("!/{bin}"));
        }
        self.inject(IgnoreSource::Strict, strict);
    }

    fn filter_entry(&self, entry: &str, partial: bool, entry_basename: Option<&str>) -> bool {
        let mut included = true;
        if let Some(parent) = self.parent {
            let parent_entry = format!("{}/{entry}", self.basename);
            let parent_basename = entry_basename.unwrap_or(entry);
            included = parent.filter_entry(&parent_entry, partial, Some(parent_basename));
            if !included && !self.exact {
                return false;
            }
        }
        for source in &self.ignore_files {
            let Some(Some(rules)) = self.rules.get(source) else {
                continue;
            };
            for rule in rules {
                if rule.negate == included {
                    continue;
                }
                let is_relative_rule = entry_basename.is_some() && rule.is_relative();
                let slash_entry = format!("/{entry}");
                let mut hit = rule.matches(&slash_entry, false) || rule.matches(entry, false);
                if !hit && partial {
                    hit = rule.matches(&format!("/{entry}/"), false)
                        || rule.matches(&format!("{entry}/"), false)
                        || (rule.negate
                            && (rule.matches(&slash_entry, true) || rule.matches(entry, true)));
                    if !hit && is_relative_rule {
                        let base = entry_basename.unwrap_or(entry);
                        hit = rule.matches(&format!("/{base}/"), false)
                            || rule.matches(&format!("{base}/"), false)
                            || (rule.negate
                                && (rule.matches(&format!("/{base}"), true) || rule.matches(base, true)));
                    }
                }
                if hit {
                    included = rule.negate;
                }
            }
        }
        included
    }

    fn walk(mut self, root: &Path, manifest: Option<&Value>, result: &mut BTreeSet<String>) {
        let Ok(read) = std::fs::read_dir(&self.path) else {
            return;
        };
        let entries: Vec<String> = read
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        for source in self.ignore_files.clone() {
            let Some(name) = source.file_name() else {
                continue;
            };
            if !entries.iter().any(|e| e == name) {
                continue;
            }
            if source == IgnoreSource::PackageJson {
                if self.is_package {
                    let fallback = Value::Null;
                    self.process_package(manifest.unwrap_or(&fallback));
                }
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(self.path.join(name)) {
                self.rules.insert(source, Some(parse_rules(&content)));
            }
        }
        // filterEntries: package.json rules disable .npmignore/.gitignore; .npmignore disables
        // .gitignore.
        if matches!(self.rules.get(&IgnoreSource::PackageJson), Some(Some(_))) {
            self.rules.insert(IgnoreSource::NpmIgnore, None);
            self.rules.insert(IgnoreSource::GitIgnore, None);
        } else if matches!(self.rules.get(&IgnoreSource::NpmIgnore), Some(Some(_))) {
            self.rules.insert(IgnoreSource::GitIgnore, None);
        }
        for entry in entries {
            let pass_file = self.filter_entry(&entry, false, None);
            let pass_dir = self.filter_entry(&entry, true, None);
            if !(pass_file || pass_dir) || entry.contains('*') {
                continue;
            }
            let abs = self.path.join(&entry);
            let Ok(meta) = std::fs::symlink_metadata(&abs) else {
                continue;
            };
            if meta.is_file() {
                if pass_file {
                    let rel = abs.strip_prefix(root).unwrap_or(&abs);
                    result.insert(rel.to_string_lossy().replace('\\', "/"));
                }
            } else if meta.is_dir() && pass_dir {
                let exact = pass_file || self.filter_entry(&format!("{entry}/"), false, None);
                let required: Vec<String> = self
                    .required_files
                    .iter()
                    .filter_map(|file| {
                        let prefix = format!("{entry}/");
                        file.strip_prefix(&prefix).map(str::to_string)
                    })
                    .collect();
                let child = Walker::new(abs, Some(&self), false, exact, required);
                child.walk(root, None, result);
            }
        }
    }
}

/// The package's `bin` paths, normalized like `read-package-json` (`./x` becomes `x`).
fn normalized_bins(manifest: &Value) -> Vec<String> {
    let normalize = |p: &str| {
        let joined = Path::new("/").join(p);
        let mut parts: Vec<String> = Vec::new();
        for component in joined.components() {
            match component {
                std::path::Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
                std::path::Component::ParentDir => {
                    parts.pop();
                }
                _ => {}
            }
        }
        parts.join("/")
    };
    match manifest.get("bin") {
        Some(Value::String(path)) => vec![normalize(path)],
        Some(Value::Object(map)) => map.values().filter_map(Value::as_str).map(normalize).collect(),
        _ => Vec::new(),
    }
}

/// The files (relative, `/`-separated) `npm pack` would include from `dir`.
pub(super) fn packlist(dir: &Path, manifest: &Value) -> Vec<String> {
    let mut result = BTreeSet::new();
    let walker = Walker::new(dir.to_path_buf(), None, true, false, Vec::new());
    walker.walk(dir, Some(manifest), &mut result);
    result.into_iter().collect()
}

/// Copy the packed files of `dir` into `dest` (like packing and extracting the tarball).
pub(super) fn copy_packed(dir: &Path, manifest: &Value, dest: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest)?;
    for file in packlist(dir, manifest) {
        let from = dir.join(&file);
        let to = dest.join(&file);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&from, &to)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let executable = std::fs::metadata(&from).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false);
            let mode = if executable { 0o755 } else { 0o644 };
            let _ = std::fs::set_permissions(&to, std::fs::Permissions::from_mode(mode));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn minimatch_options_match_like_ignore_walk() {
        let rule = Rule::new("*.log").unwrap();
        assert!(rule.matches("logs/debug.LOG", false));
        let rule = Rule::new("/build/config.gypi").unwrap();
        assert!(rule.matches("/build/config.gypi", false));
        assert!(!rule.matches("build/config.gypi", false));
        let rule = Rule::new("**/.git/**").unwrap();
        assert!(rule.matches("/a/.git/config", false));
        let rule = Rule::new("!lib/**").unwrap();
        assert!(rule.negate);
        assert!(rule.matches("lib", true));
        assert!(rule.matches("lib/a/b.js", false));
        let rule = Rule::new("!/readme{,.*[^~$]}").unwrap();
        assert!(rule.matches("/README.md", false));
        assert!(!rule.matches("/README.md~", false));
    }

    #[test]
    fn files_field_npmignore_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let manifest = serde_json::json!({
            "name": "x", "version": "1.0.0", "main": "main.js",
            "files": ["lib", "types/index.d.ts"], "bin": { "x": "./bin/x.js" }
        });
        write(d, "package.json", &manifest.to_string());
        write(d, "README.md", "r");
        write(d, "LICENSE", "l");
        write(d, "main.js", "m");
        write(d, "lib/a.js", "a");
        write(d, "lib/.npmignore", "secret.js\n");
        write(d, "lib/secret.js", "s");
        write(d, "lib/.DS_Store", "x");
        write(d, "types/index.d.ts", "t");
        write(d, "types/other.d.ts", "o");
        write(d, "bin/x.js", "b");
        write(d, "test/t.js", "t");
        write(d, "node_modules/dep/index.js", "d");
        write(d, ".npmrc", "x");
        write(d, "package-lock.json", "{}");
        let files = packlist(d, &manifest);
        assert_eq!(
            files,
            vec!["LICENSE", "README.md", "bin/x.js", "lib/a.js", "main.js", "package.json", "types/index.d.ts"]
        );
    }

    #[test]
    fn without_files_gitignore_applies_unless_npmignore_exists() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let manifest = serde_json::json!({ "name": "x", "version": "1.0.0" });
        write(d, "package.json", &manifest.to_string());
        write(d, ".gitignore", "dist\n*.tmp\n");
        write(d, "dist/out.js", "o");
        write(d, "src/a.js", "a");
        write(d, "src/b.tmp", "b");
        write(d, ".git/HEAD", "h");
        assert_eq!(packlist(d, &manifest), vec!["package.json", "src/a.js"]);
        write(d, ".npmignore", "src/a.js\n");
        assert_eq!(
            packlist(d, &manifest),
            vec!["dist/out.js", "package.json", "src/b.tmp"]
        );
    }
}
