//! Glob helpers for the package manager: `minimatch(path, pattern)` with its default options,
//! Node's `fs.globSync(pattern, { cwd })`, and the `ignore` package (gitignore rules) used while
//! collecting resource files.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use regex::Regex;

/// Expand `{a,b}` alternations (nested), like minimatch's brace expansion.
pub(crate) fn brace_expand(pattern: &str) -> Vec<String> {
    let bytes = pattern.as_bytes();
    let mut depth = 0usize;
    let mut open = None;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' => escaped = true,
            b'{' => {
                if depth == 0 {
                    open = Some(i);
                }
                depth += 1;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0
                    && let Some(start) = open
                {
                    let inner = &pattern[start + 1..i];
                    let alternatives = split_top_level_commas(inner);
                    if alternatives.len() < 2 {
                        // `{a}` is literal in brace-expansion; keep scanning after it.
                        open = None;
                        continue;
                    }
                    let prefix = &pattern[..start];
                    let suffix = &pattern[i + 1..];
                    let mut out = Vec::new();
                    for alt in alternatives {
                        for expanded in brace_expand(&format!("{prefix}{alt}{suffix}")) {
                            out.push(expanded);
                        }
                    }
                    return out;
                }
            }
            _ => {}
        }
    }
    vec![pattern.to_string()]
}

fn split_top_level_commas(inner: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    let mut escaped = false;
    for c in inner.chars() {
        if escaped {
            current.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => {
                current.push(c);
                escaped = true;
            }
            '{' => {
                depth += 1;
                current.push(c);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            ',' if depth == 0 => parts.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    parts.push(current);
    parts
}

fn has_magic(segment: &str) -> bool {
    segment.contains(['*', '?', '['])
}

/// Translate one path segment of a glob into an anchored regex.
fn segment_regex(segment: &str) -> Option<Regex> {
    let mut re = String::from("^");
    let chars: Vec<char> = segment.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if i + 1 < chars.len() => {
                re.push_str(&regex::escape(&chars[i + 1].to_string()));
                i += 2;
                continue;
            }
            '*' => {
                while i + 1 < chars.len() && chars[i + 1] == '*' {
                    i += 1;
                }
                re.push_str("[^/]*");
            }
            '?' => re.push_str("[^/]"),
            '[' => {
                // Character class; fall back to a literal `[` when unterminated.
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
    re.push('$');
    Regex::new(&re).ok()
}

fn segment_matches(pattern: &str, segment: &str) -> bool {
    if !has_magic(pattern) {
        return pattern.replace('\\', "") == segment || pattern == segment;
    }
    // Without `dot: true`, wildcards never match a leading dot.
    if segment.starts_with('.') && !pattern.starts_with('.') {
        return false;
    }
    segment_regex(pattern).is_some_and(|re| re.is_match(segment))
}

/// Resolve `a/../b` portions that are not preceded by a globstar (minimatch optimization).
fn resolve_dot_dot(parts: Vec<&str>) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for part in parts {
        if part == ".."
            && let Some(last) = out.last()
            && *last != ".."
            && *last != "**"
            && *last != "."
            && !last.is_empty()
        {
            out.pop();
            continue;
        }
        out.push(part);
    }
    out
}

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.first() {
        None => path.is_empty(),
        Some(&"**") => {
            if match_segments(&pattern[1..], path) {
                return true;
            }
            for i in 0..path.len() {
                let seg = path[i];
                if seg.starts_with('.') {
                    return false;
                }
                if match_segments(&pattern[1..], &path[i + 1..]) {
                    return true;
                }
            }
            false
        }
        Some(first) => match path.first() {
            Some(seg) if segment_matches(first, seg) => match_segments(&pattern[1..], &path[1..]),
            _ => false,
        },
    }
}

/// `minimatch(path, pattern)` with default options (POSIX paths, no `dot`, no `matchBase`).
pub fn minimatch(path: &str, pattern: &str) -> bool {
    if pattern.starts_with('#') {
        return false;
    }
    if pattern.is_empty() {
        return path.is_empty();
    }
    let path_parts: Vec<&str> = path.split('/').collect();
    brace_expand(pattern).iter().any(|expanded| {
        let parts = resolve_dot_dot(expanded.split('/').collect());
        // Collapse consecutive globstars.
        let mut collapsed: Vec<&str> = Vec::new();
        for part in parts {
            if part == "**" && collapsed.last() == Some(&"**") {
                continue;
            }
            collapsed.push(part);
        }
        match_segments(&collapsed, &path_parts)
    })
}

/// True when the input contains glob characters (`*` or `?`).
pub fn has_glob_pattern(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

fn is_dir_following_links(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

fn read_dir_names(dir: &Path) -> Vec<(String, bool, bool)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry.file_type().ok();
        let is_symlink = file_type.is_some_and(|t| t.is_symlink());
        let is_dir = if is_symlink {
            is_dir_following_links(&entry.path())
        } else {
            file_type.is_some_and(|t| t.is_dir())
        };
        out.push((name, is_dir, is_symlink));
    }
    out
}

/// All non-dot descendants of `rel` (relative to `root`), not traversing symlinked directories.
fn globstar_dirs(root: &Path, rel: &str, out: &mut Vec<String>) {
    out.push(rel.to_string());
    let dir = if rel.is_empty() { root.to_path_buf() } else { root.join(rel) };
    for (name, is_dir, is_symlink) in read_dir_names(&dir) {
        if name.starts_with('.') || !is_dir || is_symlink {
            continue;
        }
        let child = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        globstar_dirs(root, &child, out);
    }
}

fn join_rel(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else {
        format!("{rel}/{name}")
    }
}

/// Node's `fs.globSync(pattern, { cwd: root })`: relative matches (`.` for the root itself).
///
/// Wildcards never match dot entries and `**` does not descend into symlinked directories.
pub fn glob_sync(pattern: &str, root: &Path) -> Vec<String> {
    let mut results = BTreeSet::new();
    for expanded in brace_expand(pattern) {
        let mut parts: Vec<&str> = expanded.split('/').collect();
        let dir_only = parts.len() > 1 && parts.last() == Some(&"");
        if dir_only {
            parts.pop();
        }
        // Leading `./` segments are ignored; absolute patterns start at the filesystem root.
        let absolute = expanded.starts_with('/');
        let base = if absolute { PathBuf::from("/") } else { root.to_path_buf() };
        let parts: Vec<&str> = parts
            .into_iter()
            .enumerate()
            .filter(|(i, p)| !(p.is_empty() && *i == 0) && *p != ".")
            .map(|(_, p)| p)
            .collect();
        // (relative path, is directory)
        let mut current: Vec<(String, bool)> = vec![(String::new(), true)];
        for (index, part) in parts.iter().enumerate() {
            let last = index + 1 == parts.len();
            let mut next = Vec::new();
            for (rel, is_dir) in &current {
                if !is_dir {
                    continue;
                }
                let dir = if rel.is_empty() { base.clone() } else { base.join(rel) };
                if *part == "**" {
                    let mut dirs = Vec::new();
                    globstar_dirs(&base, rel, &mut dirs);
                    for d in dirs {
                        next.push((d, true));
                    }
                    if last {
                        // A trailing globstar also matches files below.
                        let mut all = Vec::new();
                        globstar_dirs(&base, rel, &mut all);
                        for d in all {
                            let path = if d.is_empty() { base.clone() } else { base.join(&d) };
                            for (name, child_is_dir, is_symlink) in read_dir_names(&path) {
                                if name.starts_with('.') || (child_is_dir && !is_symlink) {
                                    continue;
                                }
                                next.push((join_rel(&d, &name), false));
                            }
                        }
                    }
                } else if !has_magic(part) {
                    if part.starts_with('.') && *part != ".." {
                        continue;
                    }
                    let candidate = dir.join(part);
                    if std::fs::symlink_metadata(&candidate).is_ok() {
                        next.push((join_rel(rel, part), is_dir_following_links(&candidate)));
                    }
                } else {
                    for (name, child_is_dir, is_symlink) in read_dir_names(&dir) {
                        // Symlinked directories match as entries but are not traversed.
                        if segment_matches(part, &name) {
                            next.push((join_rel(rel, &name), child_is_dir && !is_symlink));
                        }
                    }
                }
            }
            // Deduplicate while keeping order.
            let mut seen = BTreeSet::new();
            next.retain(|(rel, _)| seen.insert(rel.clone()));
            current = next;
        }
        for (rel, is_dir) in current {
            if dir_only && !is_dir {
                continue;
            }
            let rel = if rel.is_empty() { ".".to_string() } else { rel };
            if absolute {
                results.insert(format!("/{rel}"));
            } else {
                results.insert(rel);
            }
        }
    }
    results.into_iter().collect()
}

/// `ignore()` from the `ignore` npm package: gitignore rules relative to a root, accumulated
/// while walking down a tree.
pub struct IgnoreMatcher {
    root: PathBuf,
    lines: Vec<String>,
    matcher: Option<Gitignore>,
}

impl IgnoreMatcher {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            lines: Vec::new(),
            matcher: None,
        }
    }

    /// `ig.add(patterns)`.
    pub fn add(&mut self, patterns: Vec<String>) {
        self.lines.extend(patterns);
        let mut builder = GitignoreBuilder::new(&self.root);
        for line in &self.lines {
            let _ = builder.add_line(None, line);
        }
        self.matcher = builder.build().ok();
    }

    /// `ig.ignores(path)` for a POSIX relative path; a trailing `/` marks a directory.
    pub fn ignores(&self, rel_path: &str) -> bool {
        let Some(matcher) = &self.matcher else {
            return false;
        };
        let is_dir = rel_path.ends_with('/');
        let trimmed = rel_path.trim_end_matches('/');
        if trimmed.is_empty() || trimmed.starts_with("..") || trimmed.starts_with('/') {
            return false;
        }
        matcher
            .matched_path_or_any_parents(Path::new(trimmed), is_dir)
            .is_ignore()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimatch_defaults() {
        let cases: &[(&str, &str, bool)] = &[
            ("extensions/a.ts", "./extensions/*.ts", false),
            ("extensions/a.ts", "extensions/*.ts", true),
            ("extensions/.a.ts", "extensions/*.ts", false),
            ("a/.b/c.ts", "a/**/c.ts", false),
            ("a/b/c.ts", "**/c.ts", true),
            ("c.ts", "**/c.ts", true),
            ("/tmp/x/a.ts", "/tmp/x/**", true),
            ("x/skills/bad-skill", "**/bad-skill", true),
            ("abc", "a{b,x}c", true),
            ("b", "a/../b", true),
            ("#foo", "#foo", false),
            ("a.ts", "*.{ts,js}", true),
            ("a/b.ts", "*.ts", false),
            ("legacy.ts", "legacy.ts", true),
            ("x/y/skip.ts", "**/skip.ts", true),
            ("a1.ts", "a[0-9].ts", true),
            ("ab.ts", "a[!b].ts", false),
        ];
        for (path, pattern, expected) in cases {
            assert_eq!(minimatch(path, pattern), *expected, "{path} ~ {pattern}");
        }
    }

    #[test]
    fn glob_sync_like_node() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(pkg.join("a/b")).unwrap();
        std::fs::create_dir_all(pkg.join(".d")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/x")).unwrap();
        for f in ["a/1.ts", "a/b/2.ts", ".d/3.ts", ".h.ts"] {
            std::fs::write(pkg.join(f), "").unwrap();
        }
        std::fs::write(dir.path().join("src/x/4.ts"), "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("../src", pkg.join("link")).unwrap();

        assert_eq!(glob_sync("**/*.ts", &pkg), vec!["a/1.ts", "a/b/2.ts"]);
        assert_eq!(glob_sync("*/*.ts", &pkg), vec!["a/1.ts"]);
        assert_eq!(glob_sync("./a/*.ts", &pkg), vec!["a/1.ts"]);
        assert_eq!(glob_sync("a/*/", &pkg), vec!["a/b"]);
        #[cfg(unix)]
        {
            assert_eq!(glob_sync("*", &pkg), vec!["a", "link"]);
            assert_eq!(glob_sync("link/**/*.ts", &pkg), vec!["link/x/4.ts"]);
            assert_eq!(glob_sync("*/", &pkg), vec!["a"]);
            assert_eq!(glob_sync("**", &pkg), vec![".", "a", "a/1.ts", "a/b", "a/b/2.ts", "link"]);
        }
        assert!(glob_sync("./**/.h.ts", &pkg).is_empty());
    }

    #[test]
    fn ignore_matcher_checks_parents() {
        let dir = tempfile::tempdir().unwrap();
        let mut ig = IgnoreMatcher::new(dir.path());
        assert!(!ig.ignores("a.md"));
        ig.add(vec!["venv".to_string(), "sub/x.md".to_string(), "*.log".to_string(), "!keep.log".to_string()]);
        assert!(ig.ignores("venv/"));
        assert!(ig.ignores("venv/a/SKILL.md"));
        assert!(ig.ignores("sub/x.md"));
        assert!(!ig.ignores("other/x.md"));
        assert!(ig.ignores("a.log"));
        assert!(!ig.ignores("keep.log"));
    }
}
