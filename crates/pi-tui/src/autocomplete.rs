//! Autocomplete providers for slash commands and file paths. Port of `autocomplete.ts`.
//!
//! Cursor columns are byte offsets.

use std::sync::Arc;

use crate::event_loop::{AbortSignal, BoxFuture, deferred, ready};
use crate::fuzzy::fuzzy_filter;
use crate::utils::{contains_autocomplete_separator, is_autocomplete_separator_char, js_trim, js_trim_start};

const PATH_DELIMITERS: [char; 5] = [' ', '\t', '"', '\'', '='];

fn path_wrapper_closer(c: char) -> Option<char> {
    match c {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '<' => Some('>'),
        '`' => Some('`'),
        _ => None,
    }
}

fn to_display_path(value: &str) -> String {
    value.replace('\\', "/")
}

fn escape_regex(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if ".*+?^${}()|[]\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn build_fd_path_query(query: &str) -> String {
    let normalized = to_display_path(query);
    if !normalized.contains('/') {
        return escape_regex(&normalized);
    }
    let has_trailing = normalized.ends_with('/');
    let trimmed = normalized.trim_matches('/');
    if trimmed.is_empty() {
        return normalized;
    }
    let sep = "[\\\\/]";
    let mut pattern = trimmed.split('/').filter(|s| !s.is_empty()).map(escape_regex).collect::<Vec<_>>().join(sep);
    if has_trailing {
        pattern.push_str(sep);
    }
    pattern
}

/// Byte index of the last path delimiter or separator, or `None`.
fn find_last_delimiter(text: &str) -> Option<usize> {
    let mut last = None;
    for (i, c) in text.char_indices() {
        if PATH_DELIMITERS.contains(&c) || is_autocomplete_separator_char(c) {
            last = Some(i);
        }
    }
    last
}

fn after_delimiter(text: &str, last: Option<usize>) -> &str {
    match last {
        None => text,
        Some(i) => &text[i + text[i..].chars().next().map_or(1, char::len_utf8)..],
    }
}

/// Strip opening wrappers before a path, unless the token also contains the closer.
fn strip_leading_wrappers(token: &str) -> &str {
    let mut result = token;
    while let Some(first) = result.chars().next() {
        let Some(closer) = path_wrapper_closer(first) else { break };
        if result[first.len_utf8()..].contains(closer) {
            break;
        }
        result = &result[first.len_utf8()..];
    }
    result
}

fn find_unclosed_quote_start(text: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut start = 0;
    for (i, b) in text.bytes().enumerate() {
        if b == b'"' {
            in_quotes = !in_quotes;
            if in_quotes {
                start = i;
            }
        }
    }
    if in_quotes { Some(start) } else { None }
}

/// `tokenStartRegex`: empty text or text ending with a separator.
pub(crate) fn is_token_start_text(text: &str) -> bool {
    match text.chars().last() {
        None => true,
        Some(c) => is_autocomplete_separator_char(c),
    }
}

fn is_token_start(text: &str, index: usize) -> bool {
    let mut start = index;
    while start > 0 {
        let prev = text[..start].chars().last().unwrap();
        if path_wrapper_closer(prev).is_none() {
            break;
        }
        start -= prev.len_utf8();
    }
    let prev = text[..start].chars().last();
    prev.is_some_and(|c| PATH_DELIMITERS.contains(&c)) || is_token_start_text(&text[..start])
}

fn extract_quoted_prefix(text: &str) -> Option<String> {
    let quote_start = find_unclosed_quote_start(text)?;
    if quote_start > 0 && text.as_bytes()[quote_start - 1] == b'@' {
        if !is_token_start(text, quote_start - 1) {
            return None;
        }
        return Some(text[quote_start - 1..].to_string());
    }
    if !is_token_start(text, quote_start) {
        return None;
    }
    Some(text[quote_start..].to_string())
}

struct PathPrefix<'a> {
    raw_prefix: &'a str,
    is_at_prefix: bool,
    is_quoted_prefix: bool,
}

fn parse_path_prefix(prefix: &str) -> PathPrefix<'_> {
    if let Some(r) = prefix.strip_prefix("@\"") {
        return PathPrefix { raw_prefix: r, is_at_prefix: true, is_quoted_prefix: true };
    }
    if let Some(r) = prefix.strip_prefix('"') {
        return PathPrefix { raw_prefix: r, is_at_prefix: false, is_quoted_prefix: true };
    }
    if let Some(r) = prefix.strip_prefix('@') {
        return PathPrefix { raw_prefix: r, is_at_prefix: true, is_quoted_prefix: false };
    }
    PathPrefix { raw_prefix: prefix, is_at_prefix: false, is_quoted_prefix: false }
}

fn build_completion_value(path: &str, is_at_prefix: bool, is_quoted_prefix: bool) -> String {
    let needs_quotes = is_quoted_prefix || contains_autocomplete_separator(path);
    let prefix = if is_at_prefix { "@" } else { "" };
    if !needs_quotes {
        return format!("{prefix}{path}");
    }
    format!("{prefix}\"{path}\"")
}

// ---------------------------------------------------------------------------
// POSIX path helpers (Node `path` semantics)
// ---------------------------------------------------------------------------

pub(crate) fn posix_normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let mut out = parts.join("/");
    if out.is_empty() && !absolute {
        out = ".".into();
    }
    if trailing && !out.is_empty() && out != "." {
        out.push('/');
    } else if trailing && out == "." {
        out = "./".into();
    }
    if absolute { format!("/{out}") } else { out }
}

pub(crate) fn posix_join(parts: &[&str]) -> String {
    let joined: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    if joined.is_empty() {
        return ".".into();
    }
    posix_normalize(&joined.join("/"))
}

pub(crate) fn posix_dirname(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let has_root = path.starts_with('/');
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".into();
    }
    match trimmed.rfind('/') {
        None => ".".into(),
        Some(i) => {
            let dir = &trimmed[..i];
            if dir.is_empty() && has_root { "/".into() } else if dir.is_empty() { ".".into() } else { dir.to_string() }
        }
    }
}

pub(crate) fn posix_basename(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    match trimmed.rfind('/') {
        None => trimmed.to_string(),
        Some(i) => trimmed[i + 1..].to_string(),
    }
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// `String.prototype.localeCompare` (ICU root collation).
pub(crate) fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    crate::collation::locale_compare(a, b)
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AutocompleteItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

impl AutocompleteItem {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self { value: value.into(), label: label.into(), description: None }
    }
}

/// Argument completion callback of a slash command.
pub type ArgumentCompletions = Arc<dyn Fn(&str) -> BoxFuture<Option<Vec<AutocompleteItem>>> + Send + Sync>;

#[derive(Clone, Default)]
pub struct SlashCommand {
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub get_argument_completions: Option<ArgumentCompletions>,
}

/// A command entry: a slash command or a plain autocomplete item.
#[derive(Clone)]
pub enum CommandEntry {
    Command(SlashCommand),
    Item(AutocompleteItem),
}

impl CommandEntry {
    fn name(&self) -> &str {
        match self {
            CommandEntry::Command(c) => &c.name,
            CommandEntry::Item(i) => &i.value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AutocompleteSuggestions {
    pub items: Vec<AutocompleteItem>,
    /// What we're matching against (e.g. "/" or "src/").
    pub prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResult {
    pub lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_col: usize,
}

#[derive(Clone, Default)]
pub struct SuggestOptions {
    pub signal: AbortSignal,
    pub force: bool,
}

/// Provides autocomplete suggestions for the editor.
pub trait AutocompleteProvider: Send + Sync {
    /// Characters that should naturally trigger this provider at token boundaries.
    fn trigger_characters(&self) -> Vec<String> {
        Vec::new()
    }

    /// Suggestions for the current text/cursor position (`None` if there are none).
    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: SuggestOptions,
    ) -> BoxFuture<Option<AutocompleteSuggestions>>;

    /// Apply the selected item; returns the new text and cursor position.
    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionResult;

    /// Whether file completion should trigger for explicit Tab completion.
    fn should_trigger_file_completion(&self, _lines: &[String], _cursor_line: usize, _cursor_col: usize) -> bool {
        true
    }

    /// Whether [`should_trigger_file_completion`](Self::should_trigger_file_completion) is implemented.
    fn has_should_trigger_file_completion(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone)]
struct FdEntry {
    path: String,
    is_directory: bool,
}

/// Run fd and collect entries; returns an empty list on errors or abort.
fn walk_directory_with_fd(
    base_dir: &str,
    fd_path: &str,
    query: &str,
    max_results: usize,
    signal: &AbortSignal,
    max_depth: Option<usize>,
) -> Vec<FdEntry> {
    if signal.is_aborted() {
        return Vec::new();
    }
    let mut args: Vec<String> = vec![
        "--base-directory".into(),
        base_dir.into(),
        "--max-results".into(),
        max_results.to_string(),
        "--type".into(),
        "f".into(),
        "--type".into(),
        "d".into(),
        "--follow".into(),
        "--hidden".into(),
        "--exclude".into(),
        ".git".into(),
        "--exclude".into(),
        ".git/*".into(),
        "--exclude".into(),
        ".git/**".into(),
    ];
    if let Some(d) = max_depth {
        args.push("--max-depth".into());
        args.push(d.to_string());
    }
    if to_display_path(query).contains('/') {
        args.push("--full-path".into());
    }
    if !query.is_empty() {
        args.push(build_fd_path_query(query));
    }
    let child = std::process::Command::new(fd_path)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let Ok(child) = child else { return Vec::new() };
    let child = Arc::new(std::sync::Mutex::new(Some(child)));
    let c2 = child.clone();
    signal.on_abort(move || {
        if let Some(c) = c2.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = c.kill();
        }
    });
    let taken = child.lock().unwrap_or_else(|e| e.into_inner()).take();
    let Some(child) = taken else { return Vec::new() };
    let Ok(output) = child.wait_with_output() else { return Vec::new() };
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if signal.is_aborted() || !output.status.success() || stdout.is_empty() {
        return Vec::new();
    }
    let mut results = Vec::new();
    for line in js_trim(&stdout).split('\n').filter(|l| !l.is_empty()) {
        let display = to_display_path(line);
        let has_trailing = display.ends_with('/');
        let normalized = if has_trailing { &display[..display.len() - 1] } else { &display[..] };
        if normalized == ".git" || normalized.starts_with(".git/") || normalized.contains("/.git/") {
            continue;
        }
        results.push(FdEntry { path: display.clone(), is_directory: has_trailing });
    }
    results
}

/// Combined provider that handles slash commands and file paths.
#[derive(Clone)]
pub struct CombinedAutocompleteProvider {
    commands: Vec<CommandEntry>,
    base_path: String,
    fd_path: Option<String>,
}

impl CombinedAutocompleteProvider {
    pub fn new(commands: Vec<CommandEntry>, base_path: impl Into<String>, fd_path: Option<String>) -> Self {
        Self { commands, base_path: base_path.into(), fd_path }
    }

    fn extract_at_prefix(&self, text: &str) -> Option<String> {
        if let Some(q) = extract_quoted_prefix(text)
            && q.starts_with("@\"")
        {
            return Some(q);
        }
        let token = strip_leading_wrappers(after_delimiter(text, find_last_delimiter(text)));
        if token.starts_with('@') { Some(token.to_string()) } else { None }
    }

    fn extract_path_prefix(&self, text: &str, force: bool) -> Option<String> {
        if let Some(q) = extract_quoted_prefix(text) {
            return Some(q);
        }
        let path_prefix = strip_leading_wrappers(after_delimiter(text, find_last_delimiter(text)));
        if force {
            return Some(path_prefix.to_string());
        }
        if path_prefix.contains('/') || path_prefix.starts_with('.') || path_prefix.starts_with("~/") {
            return Some(path_prefix.to_string());
        }
        if path_prefix.is_empty() && !text.is_empty() && is_token_start_text(text) {
            return Some(String::new());
        }
        None
    }

    fn expand_home_path(&self, path: &str) -> String {
        if let Some(rest) = path.strip_prefix("~/") {
            let expanded = posix_join(&[&home_dir(), rest]);
            if path.ends_with('/') && !expanded.ends_with('/') {
                return format!("{expanded}/");
            }
            return expanded;
        }
        if path == "~" {
            return home_dir();
        }
        path.to_string()
    }

    fn resolve_scoped_fuzzy_query(&self, raw_query: &str) -> Option<(String, String, String)> {
        let normalized = to_display_path(raw_query);
        let slash = normalized.rfind('/')?;
        let display_base = normalized[..=slash].to_string();
        let query = normalized[slash + 1..].to_string();
        let base_dir = if display_base.starts_with("~/") {
            self.expand_home_path(&display_base)
        } else if display_base.starts_with('/') {
            display_base.clone()
        } else {
            posix_join(&[&self.base_path, &display_base])
        };
        if !std::fs::metadata(&base_dir).map(|m| m.is_dir()).unwrap_or(false) {
            return None;
        }
        Some((base_dir, query, display_base))
    }

    fn scoped_path_for_display(display_base: &str, relative: &str) -> String {
        let rel = to_display_path(relative);
        if display_base == "/" {
            return format!("/{rel}");
        }
        format!("{}{rel}", to_display_path(display_base))
    }

    fn get_file_suggestions(&self, prefix: &str) -> Vec<AutocompleteItem> {
        let pp = parse_path_prefix(prefix);
        let raw = pp.raw_prefix;
        let mut expanded = raw.to_string();
        if expanded.starts_with('~') {
            expanded = self.expand_home_path(&expanded);
        }
        let is_root = matches!(raw, "" | "./" | "../" | "~" | "~/" | "/") || (pp.is_at_prefix && raw.is_empty());
        let abs_like = raw.starts_with('~') || expanded.starts_with('/');
        let (search_dir, search_prefix) = if is_root || raw.ends_with('/') {
            (if abs_like { expanded.clone() } else { posix_join(&[&self.base_path, &expanded]) }, String::new())
        } else {
            let dir = posix_dirname(&expanded);
            let file = posix_basename(&expanded);
            (if abs_like { dir } else { posix_join(&[&self.base_path, &dir]) }, file)
        };
        let Ok(entries) = std::fs::read_dir(&search_dir) else { return Vec::new() };
        let mut suggestions = Vec::new();
        let lower_prefix = search_prefix.to_lowercase();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.to_lowercase().starts_with(&lower_prefix) {
                continue;
            }
            let ft = entry.file_type().ok();
            let mut is_dir = ft.is_some_and(|t| t.is_dir());
            if !is_dir && ft.is_some_and(|t| t.is_symlink()) {
                is_dir = std::fs::metadata(posix_join(&[&search_dir, &name])).map(|m| m.is_dir()).unwrap_or(false);
            }
            let display_prefix = raw;
            let relative = if display_prefix.ends_with('/') {
                format!("{display_prefix}{name}")
            } else if display_prefix.contains('/') || display_prefix.contains('\\') {
                if let Some(home_rel) = display_prefix.strip_prefix("~/") {
                    let dir = posix_dirname(home_rel);
                    if dir == "." { format!("~/{name}") } else { format!("~/{}", posix_join(&[&dir, &name])) }
                } else if display_prefix.starts_with('/') {
                    let dir = posix_dirname(display_prefix);
                    if dir == "/" { format!("/{name}") } else { format!("{dir}/{name}") }
                } else {
                    let mut r = posix_join(&[&posix_dirname(display_prefix), &name]);
                    if display_prefix.starts_with("./") && !r.starts_with("./") {
                        r = format!("./{r}");
                    }
                    r
                }
            } else if display_prefix.starts_with('~') {
                format!("~/{name}")
            } else {
                name.clone()
            };
            let relative = to_display_path(&relative);
            let path_value = if is_dir { format!("{relative}/") } else { relative };
            let value = build_completion_value(&path_value, pp.is_at_prefix, pp.is_quoted_prefix);
            suggestions.push(AutocompleteItem {
                value,
                label: format!("{name}{}", if is_dir { "/" } else { "" }),
                description: None,
            });
        }
        suggestions.sort_by(|a, b| {
            let ad = a.label.ends_with('/');
            let bd = b.label.ends_with('/');
            match (ad, bd) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => locale_compare(&a.label, &b.label),
            }
        });
        suggestions
    }

    fn score_entry(file_path: &str, query: &str, is_dir: bool) -> i32 {
        let file_name = posix_basename(file_path).to_lowercase();
        let q = query.to_lowercase();
        let mut score = if file_name == q {
            100
        } else if file_name.starts_with(&q) {
            80
        } else if file_name.contains(&q) {
            50
        } else if file_path.to_lowercase().contains(&q) {
            30
        } else {
            0
        };
        if is_dir && score > 0 {
            score += 10;
        }
        score
    }

    fn get_fuzzy_file_suggestions(&self, query: &str, is_quoted_prefix: bool, signal: &AbortSignal) -> Vec<AutocompleteItem> {
        let Some(fd_path) = self.fd_path.as_deref() else { return Vec::new() };
        if signal.is_aborted() {
            return Vec::new();
        }
        let scoped = self.resolve_scoped_fuzzy_query(query);
        let fd_base_dir = scoped.as_ref().map_or(self.base_path.clone(), |s| s.0.clone());
        let fd_query = scoped.as_ref().map_or(query.to_string(), |s| s.1.clone());
        let base_entries = walk_directory_with_fd(&fd_base_dir, fd_path, &fd_query, 100, signal, Some(1));
        let recursive = walk_directory_with_fd(&fd_base_dir, fd_path, &fd_query, 100, signal, None);
        let mut seen: std::collections::HashSet<String> = base_entries.iter().map(|e| e.path.clone()).collect();
        let mut entries = base_entries;
        for e in recursive {
            if seen.insert(e.path.clone()) {
                entries.push(e);
            }
        }
        if signal.is_aborted() {
            return Vec::new();
        }
        let mut scored: Vec<(FdEntry, i32)> = entries
            .into_iter()
            .map(|e| {
                let s = if fd_query.is_empty() { 1 } else { Self::score_entry(&e.path, &fd_query, e.is_directory) };
                (e, s)
            })
            .filter(|(_, s)| *s > 0)
            .collect();
        let depth = |p: &str| to_display_path(p).split('/').filter(|s| !s.is_empty()).count();
        scored.sort_by(|(a, sa), (b, sb)| {
            sb.cmp(sa)
                .then_with(|| depth(&a.path).cmp(&depth(&b.path)))
                .then_with(|| crate::utils::utf16_len(&a.path).cmp(&crate::utils::utf16_len(&b.path)))
                .then_with(|| locale_compare(&a.path, &b.path))
        });
        scored
            .into_iter()
            .take(20)
            .map(|(e, _)| {
                let without_slash = if e.is_directory { &e.path[..e.path.len() - 1] } else { &e.path[..] };
                let display = match &scoped {
                    Some((_, _, base)) => Self::scoped_path_for_display(base, without_slash),
                    None => without_slash.to_string(),
                };
                let name = posix_basename(without_slash);
                let completion = if e.is_directory { format!("{display}/") } else { display.clone() };
                AutocompleteItem {
                    value: build_completion_value(&completion, true, is_quoted_prefix),
                    label: format!("{name}{}", if e.is_directory { "/" } else { "" }),
                    description: Some(display),
                }
            })
            .collect()
    }
}

impl AutocompleteProvider for CombinedAutocompleteProvider {
    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: SuggestOptions,
    ) -> BoxFuture<Option<AutocompleteSuggestions>> {
        let current = lines.get(cursor_line).map(String::as_str).unwrap_or("");
        let text_before = &current[..current.floor_char_boundary(cursor_col)];

        if let Some(at_prefix) = self.extract_at_prefix(text_before) {
            let pp = parse_path_prefix(&at_prefix);
            let raw = pp.raw_prefix.to_string();
            let quoted = pp.is_quoted_prefix;
            let me = self.clone();
            let signal = options.signal.clone();
            // Run fd off the UI thread.
            let (resolver, deferred) = deferred();
            std::thread::spawn(move || {
                let suggestions = me.get_fuzzy_file_suggestions(&raw, quoted, &signal);
                resolver.resolve(if suggestions.is_empty() {
                    None
                } else {
                    Some(AutocompleteSuggestions { items: suggestions, prefix: at_prefix })
                });
            });
            return Box::pin(deferred);
        }

        let command_text = js_trim_start(text_before);
        if !options.force && command_text.starts_with('/') {
            match command_text.find(' ') {
                None => {
                    let prefix = &command_text[1..];
                    struct Cmd {
                        name: String,
                        description: Option<String>,
                    }
                    let items: Vec<Arc<Cmd>> = self
                        .commands
                        .iter()
                        .map(|cmd| {
                            let name = cmd.name().to_string();
                            let (hint, desc) = match cmd {
                                CommandEntry::Command(c) => (
                                    c.argument_hint.clone().filter(|h| !h.is_empty()),
                                    c.description.clone().unwrap_or_default(),
                                ),
                                CommandEntry::Item(i) => (None, i.description.clone().unwrap_or_default()),
                            };
                            let full = match hint {
                                Some(h) => {
                                    if desc.is_empty() {
                                        h
                                    } else {
                                        format!("{h} — {desc}")
                                    }
                                }
                                None => desc,
                            };
                            Arc::new(Cmd { name, description: if full.is_empty() { None } else { Some(full) } })
                        })
                        .collect();
                    let bare = fuzzy_filter(&items, prefix, |i| {
                        i.name.strip_prefix("skill:").unwrap_or(&i.name).to_string()
                    });
                    let full_only_candidates: Vec<Arc<Cmd>> = items
                        .iter()
                        .filter(|i| i.name.starts_with("skill:") && !bare.iter().any(|b| Arc::ptr_eq(b, i)))
                        .cloned()
                        .collect();
                    let full_only = fuzzy_filter(&full_only_candidates, prefix, |i| i.name.clone());
                    let filtered: Vec<AutocompleteItem> = bare
                        .iter()
                        .chain(full_only.iter())
                        .map(|i| AutocompleteItem {
                            value: i.name.clone(),
                            label: i.name.clone(),
                            description: i.description.clone(),
                        })
                        .collect();
                    if filtered.is_empty() {
                        return ready(None);
                    }
                    return ready(Some(AutocompleteSuggestions { items: filtered, prefix: command_text.to_string() }));
                }
                Some(space) => {
                    let command_name = &command_text[1..space];
                    let argument_text = command_text[space + 1..].to_string();
                    let command = self.commands.iter().find(|c| c.name() == command_name);
                    let Some(CommandEntry::Command(cmd)) = command else { return ready(None) };
                    let Some(f) = cmd.get_argument_completions.clone() else { return ready(None) };
                    let fut = f(&argument_text);
                    return Box::pin(async move {
                        let items = fut.await?;
                        if items.is_empty() {
                            return None;
                        }
                        Some(AutocompleteSuggestions { items, prefix: argument_text })
                    });
                }
            }
        }

        let Some(path_match) = self.extract_path_prefix(text_before, options.force) else { return ready(None) };
        let suggestions = self.get_file_suggestions(&path_match);
        if suggestions.is_empty() {
            return ready(None);
        }
        ready(Some(AutocompleteSuggestions { items: suggestions, prefix: path_match }))
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionResult {
        let current = lines.get(cursor_line).map(String::as_str).unwrap_or("");
        // Clamp to char boundaries: a stale prefix (the list was built for older text) must not
        // slice inside a multi-byte character.
        let cursor_col = current.floor_char_boundary(cursor_col);
        let before_prefix = &current[..current.floor_char_boundary(cursor_col.saturating_sub(prefix.len()))];
        let after_cursor = &current[cursor_col..];
        let is_quoted_prefix = prefix.starts_with('"') || prefix.starts_with("@\"");
        let adjusted_after = if is_quoted_prefix && item.value.ends_with('"') && after_cursor.starts_with('"') {
            &after_cursor[1..]
        } else {
            after_cursor
        };
        let set_line = |new_line: String| {
            let mut new_lines = lines.to_vec();
            if cursor_line < new_lines.len() {
                new_lines[cursor_line] = new_line;
            } else {
                new_lines.resize(cursor_line, String::new());
                new_lines.push(new_line);
            }
            new_lines
        };
        let is_dir = item.label.ends_with('/');
        let has_trailing_quote = item.value.ends_with('"');
        let cursor_offset = if is_dir && has_trailing_quote { item.value.len() - 1 } else { item.value.len() };

        let is_slash = prefix.starts_with('/') && js_trim(before_prefix).is_empty() && !prefix[1..].contains('/');
        if is_slash {
            let new_line = format!("{before_prefix}/{} {adjusted_after}", item.value);
            return CompletionResult {
                lines: set_line(new_line),
                cursor_line,
                cursor_col: before_prefix.len() + item.value.len() + 2,
            };
        }
        if prefix.starts_with('@') {
            let suffix = if is_dir { "" } else { " " };
            let new_line = format!("{before_prefix}{}{suffix}{adjusted_after}", item.value);
            return CompletionResult {
                lines: set_line(new_line),
                cursor_line,
                cursor_col: before_prefix.len() + cursor_offset + suffix.len(),
            };
        }
        let new_line = format!("{before_prefix}{}{adjusted_after}", item.value);
        CompletionResult { lines: set_line(new_line), cursor_line, cursor_col: before_prefix.len() + cursor_offset }
    }

    fn should_trigger_file_completion(&self, lines: &[String], cursor_line: usize, cursor_col: usize) -> bool {
        let current = lines.get(cursor_line).map(String::as_str).unwrap_or("");
        let before = js_trim(&current[..current.floor_char_boundary(cursor_col)]);
        !(before.starts_with('/') && !before.contains(' '))
    }

    fn has_should_trigger_file_completion(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_helpers() {
        assert_eq!(posix_join(&["/tmp", "./"]), "/tmp/");
        assert_eq!(posix_join(&["/tmp", "a/../b"]), "/tmp/b");
        assert_eq!(posix_join(&["base", "."]), "base");
        assert_eq!(posix_dirname("src/ma"), "src");
        assert_eq!(posix_dirname("/a"), "/");
        assert_eq!(posix_dirname("a"), ".");
        assert_eq!(posix_dirname("./src/x"), "./src");
        assert_eq!(posix_basename("src/ma"), "ma");
        assert_eq!(posix_basename("dir/"), "dir");
    }
}
