//! Port of `packages/coding-agent/src/modes/interactive/components/session-selector.ts`.

use crate::core::keybindings::KeybindingsManager;
use crate::core::session_manager::SessionInfo;
use crate::core::session_manager::SessionListProgress;
use crate::modes::interactive::components::session_selector_search::NameFilter;
use crate::modes::interactive::components::session_selector_search::SortMode;
use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::components::keybinding_hints::{key_hint, key_text};
use crate::modes::interactive::components::session_selector_search::{filter_and_sort_sessions, has_session_name};
use crate::modes::interactive::theme::theme::{ThemeBg, ThemeColor, theme};
use crate::utils::paths::canonicalize_path as canonicalize_path_impl;
use futures::FutureExt;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use pi_tui::components::input::Input;
use pi_tui::components::spacer::Spacer;
use pi_tui::components::text::Text;
use pi_tui::keybindings::get_keybindings;
use pi_tui::tui::{Component, ComponentRef, Container, MouseEvent, MouseResult};
use pi_tui::utils::{js_trim, truncate_to_width, visible_width};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionScope {
    #[serde(rename = "current")]
    Current,
    #[serde(rename = "all")]
    All,
}

impl SessionScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionSelectorHeaderSetStatusMessageMsgType {
    #[serde(rename = "info")]
    Info,
    #[serde(rename = "error")]
    Error,
}

impl SessionSelectorHeaderSetStatusMessageMsgType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSelectorHeaderSetStatusMessageMsg {
    pub r#type: SessionSelectorHeaderSetStatusMessageMsgType,
    pub message: String,
}


fn shorten_path(path: &str) -> String {
    if path.is_empty() {
        return path.to_string();
    }
    let home = dirs::home_dir().map(|h| h.to_string_lossy().to_string()).unwrap_or_default();
    if !home.is_empty() && path.starts_with(&home) {
        return format!("~{}", &path[home.len()..]);
    }
    path.to_string()
}

fn format_session_date(date: &chrono::DateTime<chrono::Utc>) -> String {
    let diff_ms = (chrono::Utc::now() - *date).num_milliseconds() as f64;
    let diff_mins = (diff_ms / 60000.0).floor();
    let diff_hours = (diff_ms / 3600000.0).floor();
    let diff_days = (diff_ms / 86400000.0).floor();

    if diff_mins < 1.0 {
        return "now".to_string();
    }
    if diff_mins < 60.0 {
        return format!("{diff_mins}m");
    }
    if diff_hours < 24.0 {
        return format!("{diff_hours}h");
    }
    if diff_days < 7.0 {
        return format!("{diff_days}d");
    }
    if diff_days < 30.0 {
        return format!("{}w", (diff_days / 7.0).floor());
    }
    if diff_days < 365.0 {
        return format!("{}mo", (diff_days / 30.0).floor());
    }
    format!("{}y", (diff_days / 365.0).floor())
}

fn canonicalize_path(path: Option<&str>) -> Option<String> {
    let path = path.filter(|p| !p.is_empty())?;
    Some(canonicalize_path_impl(path))
}

/// Run a future without awaiting it (TS: `void promise`).
fn spawn_detached(future: BoxFuture<'static, ()>) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        crate::utils::uncaught::spawn_on(&handle, future);
    } else {
        crate::utils::uncaught::spawn_thread(move || futures::executor::block_on(future));
    }
}

struct HeaderState {
    scope: SessionScope,
    sort_mode: SortMode,
    name_filter: NameFilter,
    loading: bool,
    load_progress: Option<(i64, i64)>,
    show_path: bool,
    confirming_delete_path: Option<String>,
    status_message: Option<SessionSelectorHeaderSetStatusMessageMsg>,
    show_rename_hint: bool,
}

/// TS: `implements Component`
pub struct SessionSelectorHeader {
    state: Arc<Mutex<HeaderState>>,
    status_generation: Arc<AtomicU64>,
    request_render: Arc<dyn Fn() + Send + Sync>,
}

impl SessionSelectorHeader {
    pub fn new(
        scope: &SessionScope,
        sort_mode: &SortMode,
        name_filter: &NameFilter,
        request_render: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(HeaderState {
                scope: *scope,
                sort_mode: *sort_mode,
                name_filter: *name_filter,
                loading: false,
                load_progress: None,
                show_path: false,
                confirming_delete_path: None,
                status_message: None,
                show_rename_hint: false,
            })),
            status_generation: Arc::new(AtomicU64::new(0)),
            request_render,
        }
    }

    pub fn set_scope(&self, scope: &SessionScope) {
        self.state.lock().scope = *scope;
    }

    pub fn set_sort_mode(&self, sort_mode: &SortMode) {
        self.state.lock().sort_mode = *sort_mode;
    }

    pub fn set_name_filter(&self, name_filter: &NameFilter) {
        self.state.lock().name_filter = *name_filter;
    }

    pub fn set_loading(&self, loading: bool) {
        let mut s = self.state.lock();
        s.loading = loading;
        // Progress is scoped to the current load; clear whenever the loading state is set
        s.load_progress = None;
    }

    pub fn set_progress(&self, loaded: i64, total: i64) {
        self.state.lock().load_progress = Some((loaded, total));
    }

    pub fn set_show_path(&self, show_path: bool) {
        self.state.lock().show_path = show_path;
    }

    pub fn set_show_rename_hint(&self, show: bool) {
        self.state.lock().show_rename_hint = show;
    }

    pub fn set_confirming_delete_path(&self, path: Option<&str>) {
        self.state.lock().confirming_delete_path = path.map(str::to_string);
    }

    fn clear_status_timeout(&self) -> u64 {
        self.status_generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn set_status_message(&self, msg: Option<&SessionSelectorHeaderSetStatusMessageMsg>, auto_hide_ms: Option<u64>) {
        let generation = self.clear_status_timeout();
        self.state.lock().status_message = msg.cloned();
        let Some(auto_hide_ms) = auto_hide_ms.filter(|ms| *ms > 0) else { return };
        if msg.is_none() {
            return;
        }
        let state = self.state.clone();
        let current = self.status_generation.clone();
        let request_render = self.request_render.clone();
        crate::utils::uncaught::spawn_thread(move || {
            std::thread::sleep(Duration::from_millis(auto_hide_ms));
            if current.load(Ordering::SeqCst) != generation {
                return;
            }
            state.lock().status_message = None;
            request_render();
        });
    }
}

impl Component for SessionSelectorHeader {
    fn render(&self, width: usize) -> Vec<String> {
        let t = theme();
        let s = self.state.lock();
        let title = if s.scope == SessionScope::Current { "Resume Session (Current Folder)" } else { "Resume Session (All)" };
        let left_text = t.bold(title);

        let sort_label = match s.sort_mode {
            SortMode::Threaded => "Threaded",
            SortMode::Recent => "Recent",
            SortMode::Relevance => "Fuzzy",
        };
        let sort_text = t.fg(&ThemeColor::Muted, "Sort: ") + &t.fg(&ThemeColor::Accent, sort_label);

        let name_label = if s.name_filter == NameFilter::All { "All" } else { "Named" };
        let name_text = t.fg(&ThemeColor::Muted, "Name: ") + &t.fg(&ThemeColor::Accent, name_label);

        let scope_text = if s.loading {
            let progress_text = match s.load_progress {
                Some((loaded, total)) => format!("{loaded}/{total}"),
                None => "...".to_string(),
            };
            format!(
                "{}{}",
                t.fg(&ThemeColor::Muted, "○ Current Folder | "),
                t.fg(&ThemeColor::Accent, &format!("Loading {progress_text}"))
            )
        } else if s.scope == SessionScope::Current {
            format!("{}{}", t.fg(&ThemeColor::Accent, "◉ Current Folder"), t.fg(&ThemeColor::Muted, " | ○ All"))
        } else {
            format!("{}{}", t.fg(&ThemeColor::Muted, "○ Current Folder | "), t.fg(&ThemeColor::Accent, "◉ All"))
        };

        let right_text = truncate_to_width(&format!("{scope_text}  {name_text}  {sort_text}"), width, "", false);
        let available_left = width.saturating_sub(visible_width(&right_text) + 1);
        let left = truncate_to_width(&left_text, available_left, "", false);
        let spacing = width.saturating_sub(visible_width(&left) + visible_width(&right_text));

        // Build hint lines - changes based on state (all branches truncate to width)
        let (hint_line1, hint_line2) = if s.confirming_delete_path.is_some() {
            let confirm_hint = format!(
                "Delete session? {} · {}",
                key_hint("tui.select.confirm".into(), "confirm"),
                key_hint("tui.select.cancel".into(), "cancel")
            );
            (t.fg(&ThemeColor::Error, &truncate_to_width(&confirm_hint, width, "…", false)), String::new())
        } else if let Some(status) = &s.status_message {
            let color = if status.r#type == SessionSelectorHeaderSetStatusMessageMsgType::Error {
                ThemeColor::Error
            } else {
                ThemeColor::Accent
            };
            (t.fg(&color, &truncate_to_width(&status.message, width, "…", false)), String::new())
        } else {
            let path_state = if s.show_path { "(on)" } else { "(off)" };
            let sep = t.fg(&ThemeColor::Muted, " · ");
            let hint1 = key_hint("tui.input.tab".into(), "scope")
                + &sep
                + &t.fg(&ThemeColor::Muted, "re:<pattern> regex · \"phrase\" exact");
            let mut hint2_parts = vec![
                key_hint("app.session.toggleSort".into(), "sort"),
                key_hint("app.session.toggleNamedFilter".into(), "named"),
                key_hint("app.session.delete".into(), "delete"),
                key_hint("app.session.togglePath".into(), &format!("path {path_state}")),
            ];
            if s.show_rename_hint {
                hint2_parts.push(key_hint("app.session.rename".into(), "rename"));
            }
            let hint2 = hint2_parts.join(&sep);
            (truncate_to_width(&hint1, width, "…", false), truncate_to_width(&hint2, width, "…", false))
        };

        vec![format!("{left}{}{right_text}", " ".repeat(spacing)), hint_line1, hint_line2]
    }

    fn invalidate(&self) {}
}

/// A session tree node for hierarchical display
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTreeNode {
    pub session: SessionInfo,
    pub children: Vec<SessionTreeNode>,
    #[serde(with = "pi_ai::serde_util::js_f64")]
    pub latest_activity: f64,
}

/// Flattened node for display with tree structure info
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlatSessionNode {
    pub session: SessionInfo,
    pub depth: i64,
    pub is_last: bool,
    /// For each ancestor level, whether there are more siblings after it
    pub ancestor_continues: Vec<bool>,
}


/// Build a tree structure from sessions based on parentSessionPath.
/// Returns root nodes sorted by modified date (descending).
fn build_session_tree(sessions: &[SessionInfo]) -> Vec<SessionTreeNode> {
    let keys: Vec<String> =
        sessions.iter().map(|s| canonicalize_path(Some(&s.path)).unwrap_or_else(|| s.path.clone())).collect();
    let index_by_path: HashMap<&str, usize> = keys.iter().enumerate().map(|(i, k)| (k.as_str(), i)).collect();

    // Parent index for each session (if the parent is in the list).
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); sessions.len()];
    let mut roots: Vec<usize> = Vec::new();
    for (i, session) in sessions.iter().enumerate() {
        let parent_path = canonicalize_path(session.parent_session_path.as_deref());
        match parent_path.as_deref().and_then(|p| index_by_path.get(p)) {
            Some(&parent) => children[parent].push(i),
            None => roots.push(i),
        }
    }

    fn build(i: usize, sessions: &[SessionInfo], children: &[Vec<usize>], depth: usize) -> SessionTreeNode {
        let mut kids: Vec<SessionTreeNode> = if depth > sessions.len() {
            Vec::new() // cycle guard
        } else {
            children[i].iter().map(|&c| build(c, sessions, children, depth + 1)).collect()
        };
        let mut latest_activity = sessions[i].modified.timestamp_millis() as f64;
        for kid in &kids {
            latest_activity = latest_activity.max(kid.latest_activity);
        }
        // Sort children by latest activity in each subtree (descending)
        kids.sort_by(|a, b| b.latest_activity.partial_cmp(&a.latest_activity).unwrap_or(std::cmp::Ordering::Equal));
        SessionTreeNode { session: sessions[i].clone(), children: kids, latest_activity }
    }

    let mut root_nodes: Vec<SessionTreeNode> = roots.iter().map(|&r| build(r, sessions, &children, 0)).collect();
    root_nodes.sort_by(|a, b| b.latest_activity.partial_cmp(&a.latest_activity).unwrap_or(std::cmp::Ordering::Equal));
    root_nodes
}

/// Flatten tree into display list with tree structure metadata.
fn flatten_session_tree(roots: &[SessionTreeNode]) -> Vec<FlatSessionNode> {
    fn walk(
        node: &SessionTreeNode,
        depth: i64,
        ancestor_continues: Vec<bool>,
        is_last: bool,
        result: &mut Vec<FlatSessionNode>,
    ) {
        result.push(FlatSessionNode {
            session: node.session.clone(),
            depth,
            is_last,
            ancestor_continues: ancestor_continues.clone(),
        });
        let count = node.children.len();
        for (i, child) in node.children.iter().enumerate() {
            let child_is_last = i == count - 1;
            // Only show continuation line for non-root ancestors
            let continues = if depth > 0 { !is_last } else { false };
            let mut next = ancestor_continues.clone();
            next.push(continues);
            walk(child, depth + 1, next, child_is_last, result);
        }
    }
    let mut result = Vec::new();
    let count = roots.len();
    for (i, root) in roots.iter().enumerate() {
        walk(root, 0, Vec::new(), i == count - 1, &mut result);
    }
    result
}

type DeleteSessionFn = Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

#[derive(Default)]
struct ListCallbacks {
    on_select: Option<Arc<dyn Fn(String) + Send + Sync>>,
    on_cancel: Option<Arc<dyn Fn() + Send + Sync>>,
    on_exit: Option<Arc<dyn Fn() + Send + Sync>>,
    on_toggle_scope: Option<Arc<dyn Fn() + Send + Sync>>,
    on_toggle_sort: Option<Arc<dyn Fn() + Send + Sync>>,
    on_toggle_name_filter: Option<Arc<dyn Fn() + Send + Sync>>,
    on_toggle_path: Option<Arc<dyn Fn(bool) + Send + Sync>>,
    on_delete_confirmation_change: Option<Arc<dyn Fn(Option<String>) + Send + Sync>>,
    on_delete_session: Option<DeleteSessionFn>,
    on_rename_session: Option<Arc<dyn Fn(String) + Send + Sync>>,
    on_error: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

struct ListState {
    all_sessions: Vec<SessionInfo>,
    filtered_sessions: Vec<FlatSessionNode>,
    selected_index: usize,
    selection_touched: bool,
    show_cwd: bool,
    sort_mode: SortMode,
    name_filter: NameFilter,
    show_path: bool,
    confirming_delete_path: Option<String>,
    focused: bool,
}

impl ListState {
    fn filter_sessions(&mut self, query: &str) {
        let trimmed = js_trim(query);
        let name_filtered: Vec<SessionInfo> = if self.name_filter == NameFilter::All {
            self.all_sessions.clone()
        } else {
            self.all_sessions.iter().filter(|s| has_session_name(s)).cloned().collect()
        };

        if self.sort_mode == SortMode::Threaded && trimmed.is_empty() {
            // Threaded mode without search: show tree structure
            let roots = build_session_tree(&name_filtered);
            self.filtered_sessions = flatten_session_tree(&roots);
        } else {
            // Other modes or with search: flat list
            let filtered = filter_and_sort_sessions(&name_filtered, query, &self.sort_mode, Some(&NameFilter::All));
            self.filtered_sessions = filtered
                .into_iter()
                .map(|session| FlatSessionNode { session, depth: 0, is_last: true, ancestor_continues: Vec::new() })
                .collect();
        }
        self.selected_index = self.selected_index.min(self.filtered_sessions.len().saturating_sub(1));
    }

    fn selected_path(&self) -> Option<String> {
        self.filtered_sessions.get(self.selected_index).map(|n| n.session.path.clone())
    }
}

fn build_tree_prefix(node: &FlatSessionNode) -> String {
    if node.depth == 0 {
        return String::new();
    }
    let parts: String = node.ancestor_continues.iter().map(|c| if *c { "│  " } else { "   " }).collect();
    let branch = if node.is_last { "└─ " } else { "├─ " };
    format!("{parts}{branch}")
}

/// Custom session list component with multi-line items and search
/// TS: `implements Component, Focusable`
pub struct SessionList {
    state: Arc<Mutex<ListState>>,
    callbacks: Arc<Mutex<ListCallbacks>>,
    search_input: Arc<Input>,
    keybindings: Arc<KeybindingsManager>,
    current_session_canonical_path: Option<String>,
    /// Max sessions visible (one line each)
    max_visible: usize,
}

impl SessionList {
    pub fn new(
        sessions: &[SessionInfo],
        show_cwd: bool,
        sort_mode: &SortMode,
        name_filter: &NameFilter,
        keybindings: Arc<KeybindingsManager>,
        current_session_file_path: Option<&str>,
    ) -> Self {
        let mut state = ListState {
            all_sessions: sessions.to_vec(),
            filtered_sessions: Vec::new(),
            selected_index: 0,
            selection_touched: false,
            show_cwd,
            sort_mode: *sort_mode,
            name_filter: *name_filter,
            show_path: false,
            confirming_delete_path: None,
            focused: false,
        };
        state.filter_sessions("");
        let state = Arc::new(Mutex::new(state));
        let callbacks: Arc<Mutex<ListCallbacks>> = Arc::default();
        let search_input = Arc::new(Input::default());

        // Handle Enter in search input - select current item
        {
            let state = state.clone();
            let callbacks = callbacks.clone();
            search_input.set_on_submit(Some(Arc::new(move |_v: String| {
                let selected = state.lock().selected_path();
                let on_select = callbacks.lock().on_select.clone();
                if let (Some(path), Some(on_select)) = (selected, on_select) {
                    on_select(path);
                }
            })));
        }

        Self {
            state,
            callbacks,
            search_input,
            keybindings,
            current_session_canonical_path: canonicalize_path(current_session_file_path),
            max_visible: 10,
        }
    }

    pub fn get_selected_session_path(&self) -> Option<String> {
        self.state.lock().selected_path()
    }

    pub fn on_select(&self) -> Option<Arc<dyn Fn(String) + Send + Sync>> {
        self.callbacks.lock().on_select.clone()
    }

    pub fn set_on_select(&self, value: Option<Arc<dyn Fn(String) + Send + Sync>>) {
        self.callbacks.lock().on_select = value;
    }

    pub fn on_cancel(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        self.callbacks.lock().on_cancel.clone()
    }

    pub fn set_on_cancel(&self, value: Option<Arc<dyn Fn() + Send + Sync>>) {
        self.callbacks.lock().on_cancel = value;
    }

    /// Invoke the exit callback (TS: `onExit()`, a no-op by default).
    pub fn on_exit(&self) {
        let cb = self.callbacks.lock().on_exit.clone();
        if let Some(cb) = cb {
            cb();
        }
    }

    pub fn set_on_exit(&self, value: Option<Arc<dyn Fn() + Send + Sync>>) {
        self.callbacks.lock().on_exit = value;
    }

    pub fn on_toggle_scope(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        self.callbacks.lock().on_toggle_scope.clone()
    }

    pub fn set_on_toggle_scope(&self, value: Option<Arc<dyn Fn() + Send + Sync>>) {
        self.callbacks.lock().on_toggle_scope = value;
    }

    pub fn on_toggle_sort(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        self.callbacks.lock().on_toggle_sort.clone()
    }

    pub fn set_on_toggle_sort(&self, value: Option<Arc<dyn Fn() + Send + Sync>>) {
        self.callbacks.lock().on_toggle_sort = value;
    }

    pub fn on_toggle_name_filter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        self.callbacks.lock().on_toggle_name_filter.clone()
    }

    pub fn set_on_toggle_name_filter(&self, value: Option<Arc<dyn Fn() + Send + Sync>>) {
        self.callbacks.lock().on_toggle_name_filter = value;
    }

    pub fn on_toggle_path(&self) -> Option<Arc<dyn Fn(bool) + Send + Sync>> {
        self.callbacks.lock().on_toggle_path.clone()
    }

    pub fn set_on_toggle_path(&self, value: Option<Arc<dyn Fn(bool) + Send + Sync>>) {
        self.callbacks.lock().on_toggle_path = value;
    }

    pub fn on_delete_confirmation_change(&self) -> Option<Arc<dyn Fn(Option<String>) + Send + Sync>> {
        self.callbacks.lock().on_delete_confirmation_change.clone()
    }

    pub fn set_on_delete_confirmation_change(&self, value: Option<Arc<dyn Fn(Option<String>) + Send + Sync>>) {
        self.callbacks.lock().on_delete_confirmation_change = value;
    }

    pub fn on_delete_session(
        &self,
    ) -> Option<Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>> {
        self.callbacks.lock().on_delete_session.clone()
    }

    pub fn set_on_delete_session(
        &self,
        value: Option<Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>>,
    ) {
        self.callbacks.lock().on_delete_session = value;
    }

    pub fn on_rename_session(&self) -> Option<Arc<dyn Fn(String) + Send + Sync>> {
        self.callbacks.lock().on_rename_session.clone()
    }

    pub fn set_on_rename_session(&self, value: Option<Arc<dyn Fn(String) + Send + Sync>>) {
        self.callbacks.lock().on_rename_session = value;
    }

    pub fn on_error(&self) -> Option<Arc<dyn Fn(String) + Send + Sync>> {
        self.callbacks.lock().on_error.clone()
    }

    pub fn set_on_error(&self, value: Option<Arc<dyn Fn(String) + Send + Sync>>) {
        self.callbacks.lock().on_error = value;
    }

    pub fn set_sort_mode(&self, sort_mode: &SortMode) {
        let query = self.search_input.get_value();
        let mut s = self.state.lock();
        s.sort_mode = *sort_mode;
        s.filter_sessions(&query);
    }

    pub fn set_name_filter(&self, name_filter: &NameFilter) {
        let query = self.search_input.get_value();
        let mut s = self.state.lock();
        s.name_filter = *name_filter;
        s.filter_sessions(&query);
    }

    pub fn set_sessions(&self, sessions: &[SessionInfo], show_cwd: bool) {
        let query = self.search_input.get_value();
        let mut s = self.state.lock();
        let selected_path = if s.selection_touched { s.selected_path() } else { None };
        s.all_sessions = sessions.to_vec();
        s.show_cwd = show_cwd;
        s.filter_sessions(&query);
        if !s.selection_touched {
            s.selected_index = 0;
        } else if let Some(selected_path) = selected_path
            && let Some(index) = s.filtered_sessions.iter().position(|n| n.session.path == selected_path)
        {
            s.selected_index = index;
        }
    }

    /// The search input.
    pub fn search_input(&self) -> Arc<Input> {
        self.search_input.clone()
    }

    fn set_confirming_delete_path(&self, path: Option<String>) {
        self.state.lock().confirming_delete_path = path.clone();
        let cb = self.callbacks.lock().on_delete_confirmation_change.clone();
        if let Some(cb) = cb {
            cb(path);
        }
    }

    fn start_delete_confirmation_for_selected_session(&self) {
        let Some(path) = self.state.lock().selected_path() else { return };

        // Prevent deleting current session
        if self.is_current_session_path(&path) {
            let cb = self.callbacks.lock().on_error.clone();
            if let Some(cb) = cb {
                cb("Cannot delete the currently active session".to_string());
            }
            return;
        }

        self.set_confirming_delete_path(Some(path));
    }

    fn is_current_session_path(&self, path: &str) -> bool {
        let Some(current) = &self.current_session_canonical_path else { return false };
        canonicalize_path(Some(path)).unwrap_or_else(|| path.to_string()) == *current
    }

    fn filter_from_input(&self) {
        let query = self.search_input.get_value();
        self.state.lock().filter_sessions(&query);
    }
}

impl Component for SessionList {
    fn render(&self, width: usize) -> Vec<String> {
        let t = theme();
        let mut lines = Vec::new();

        // Render search input
        lines.extend(self.search_input.render(width));
        lines.push(String::new()); // Blank line after search

        let s = self.state.lock();
        if s.filtered_sessions.is_empty() {
            let empty_message = if s.name_filter == NameFilter::Named {
                let toggle_key = key_text("app.session.toggleNamedFilter".into());
                if s.show_cwd {
                    format!("  No named sessions found. Press {toggle_key} to show all.")
                } else {
                    format!("  No named sessions in current folder. Press {toggle_key} to show all, or Tab to view all.")
                }
            } else if s.show_cwd {
                // "All" scope - no sessions anywhere that match filter
                "  No sessions found".to_string()
            } else {
                // "Current folder" scope - hint to try "all"
                "  No sessions in current folder. Press Tab to view all.".to_string()
            };
            lines.push(t.fg(&ThemeColor::Muted, &truncate_to_width(&empty_message, width, "…", false)));
            return lines;
        }

        // Calculate visible range with scrolling
        let len = s.filtered_sessions.len() as i64;
        let max_visible = self.max_visible as i64;
        let selected = s.selected_index as i64;
        let start_index = (selected - max_visible / 2).min(len - max_visible).max(0);
        let end_index = (start_index + max_visible).min(len);

        // Render visible sessions (one line each with tree structure)
        for i in start_index..end_index {
            let node = &s.filtered_sessions[i as usize];
            let session = &node.session;
            let is_selected = i == selected;
            let is_confirming_delete = s.confirming_delete_path.as_deref() == Some(session.path.as_str());
            let is_current = self.is_current_session_path(&session.path);

            // Build tree prefix
            let prefix = build_tree_prefix(node);

            // Session display text (name or first message)
            let has_name = session.name.as_deref().is_some_and(|n| !n.is_empty());
            let display_text = session.name.clone().unwrap_or_else(|| session.first_message.clone());
            let normalized: String =
                display_text.chars().map(|c| if (c as u32) < 0x20 || c as u32 == 0x7f { ' ' } else { c }).collect();
            let normalized_message = js_trim(&normalized).to_string();

            // Right side: message count and age
            let age = format_session_date(&session.modified);
            let msg_count = session.message_count.to_string();
            let mut right_part = format!("{msg_count} {age}");
            if s.show_cwd && !session.cwd.is_empty() {
                right_part = format!("{} {right_part}", shorten_path(&session.cwd));
            }
            if s.show_path {
                right_part = format!("{} {right_part}", shorten_path(&session.path));
            }

            // Cursor
            let cursor = if is_selected { t.fg(&ThemeColor::Accent, "› ") } else { "  ".to_string() };

            // Calculate available width for message
            let prefix_width = visible_width(&prefix) as i64;
            let right_width = visible_width(&right_part) as i64 + 2; // +2 for spacing
            let available_for_msg = width as i64 - 2 - prefix_width - right_width; // -2 for cursor

            let truncated_msg = truncate_to_width(&normalized_message, available_for_msg.max(10) as usize, "…", false);

            // Style message
            let message_color = if is_confirming_delete {
                Some(ThemeColor::Error)
            } else if is_current {
                Some(ThemeColor::Accent)
            } else if has_name {
                Some(ThemeColor::Warning)
            } else {
                None
            };
            let mut styled_msg = match message_color {
                Some(color) => t.fg(&color, &truncated_msg),
                None => truncated_msg,
            };
            if is_selected {
                styled_msg = t.bold(&styled_msg);
            }

            // Build line
            let left_part = format!("{cursor}{}{styled_msg}", t.fg(&ThemeColor::Dim, &prefix));
            let left_width = visible_width(&left_part);
            let spacing = (width as i64 - left_width as i64 - visible_width(&right_part) as i64).max(1) as usize;
            let styled_right =
                t.fg(if is_confirming_delete { &ThemeColor::Error } else { &ThemeColor::Dim }, &right_part);

            let mut line = format!("{left_part}{}{styled_right}", " ".repeat(spacing));
            if is_selected {
                line = t.bg(&ThemeBg::SelectedBg, &line);
            }
            lines.push(truncate_to_width(&line, width, "...", false));
        }

        // Add scroll indicator if needed
        if start_index > 0 || end_index < len {
            let scroll_text = format!("  ({}/{})", selected + 1, len);
            lines.push(t.fg(&ThemeColor::Muted, &truncate_to_width(&scroll_text, width, "", false)));
        }

        lines
    }

    fn handle_input(&self, data: &str) {
        let kb = get_keybindings();

        // Handle delete confirmation state first - intercept all keys
        let confirming = self.state.lock().confirming_delete_path.clone();
        if let Some(path_to_delete) = confirming {
            if kb.matches(data, "tui.select.confirm") {
                self.set_confirming_delete_path(None);
                let cb = self.callbacks.lock().on_delete_session.clone();
                if let Some(cb) = cb {
                    let future = cb(path_to_delete);
                    spawn_detached(
                        async move {
                            let _ = future.await;
                        }
                        .boxed(),
                    );
                }
                return;
            }
            if kb.matches(data, "tui.select.cancel") {
                self.set_confirming_delete_path(None);
                return;
            }
            // Ignore all other keys while confirming
            return;
        }

        if kb.matches(data, "tui.input.tab") {
            let cb = self.callbacks.lock().on_toggle_scope.clone();
            if let Some(cb) = cb {
                cb();
            }
            return;
        }

        if kb.matches(data, "app.session.toggleSort") {
            let cb = self.callbacks.lock().on_toggle_sort.clone();
            if let Some(cb) = cb {
                cb();
            }
            return;
        }

        if self.keybindings.matches(data, "app.session.toggleNamedFilter") {
            let cb = self.callbacks.lock().on_toggle_name_filter.clone();
            if let Some(cb) = cb {
                cb();
            }
            return;
        }

        // Ctrl+P: toggle path display
        if kb.matches(data, "app.session.togglePath") {
            let show_path = {
                let mut s = self.state.lock();
                s.show_path = !s.show_path;
                s.show_path
            };
            let cb = self.callbacks.lock().on_toggle_path.clone();
            if let Some(cb) = cb {
                cb(show_path);
            }
            return;
        }

        // Ctrl+D: initiate delete confirmation (useful on terminals that don't distinguish Ctrl+Backspace from Backspace)
        if kb.matches(data, "app.session.delete") {
            self.start_delete_confirmation_for_selected_session();
            return;
        }

        // Rename selected session
        if kb.matches(data, "app.session.rename") {
            let selected = self.state.lock().selected_path();
            if let Some(path) = selected {
                let cb = self.callbacks.lock().on_rename_session.clone();
                if let Some(cb) = cb {
                    cb(path);
                }
            }
            return;
        }

        // Ctrl+Backspace: non-invasive convenience alias for delete
        // Only triggers deletion when the query is empty; otherwise it is forwarded to the input
        if kb.matches(data, "app.session.deleteNoninvasive") {
            if !self.search_input.get_value().is_empty() {
                self.search_input.handle_input(data);
                self.filter_from_input();
                return;
            }
            self.start_delete_confirmation_for_selected_session();
            return;
        }

        self.state.lock().selection_touched = true;
        let max_visible = self.max_visible;
        if kb.matches(data, "tui.select.up") {
            let mut s = self.state.lock();
            s.selected_index = s.selected_index.saturating_sub(1);
        } else if kb.matches(data, "tui.select.down") {
            let mut s = self.state.lock();
            let max = s.filtered_sessions.len().saturating_sub(1);
            s.selected_index = (s.selected_index + 1).min(max);
        } else if kb.matches(data, "tui.select.pageUp") {
            // Page up - jump up by maxVisible items
            let mut s = self.state.lock();
            s.selected_index = s.selected_index.saturating_sub(max_visible);
        } else if kb.matches(data, "tui.select.pageDown") {
            // Page down - jump down by maxVisible items
            let mut s = self.state.lock();
            let max = s.filtered_sessions.len().saturating_sub(1);
            s.selected_index = (s.selected_index + max_visible).min(max);
        } else if kb.matches(data, "tui.select.confirm") {
            let selected = self.state.lock().selected_path();
            let cb = self.callbacks.lock().on_select.clone();
            if let (Some(path), Some(cb)) = (selected, cb) {
                cb(path);
            }
        } else if kb.matches(data, "tui.select.cancel") {
            let cb = self.callbacks.lock().on_cancel.clone();
            if let Some(cb) = cb {
                cb();
            }
        } else {
            // Pass everything else to search input
            self.search_input.handle_input(data);
            self.filter_from_input();
        }
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn invalidate(&self) {}

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&self, focused: bool) {
        self.state.lock().focused = focused;
        self.search_input.set_focused(focused);
    }

    fn focused(&self) -> bool {
        self.state.lock().focused
    }
}

pub type SessionsLoader = Arc<
    dyn Fn(
            Option<SessionListProgress>,
            Option<CancellationToken>,
        ) -> BoxFuture<'static, anyhow::Result<Vec<SessionInfo>>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct SessionSelectorComponentOptions {
    pub rename_session: Option<
        Arc<dyn Fn(String, Option<String>) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>,
    >,
    pub show_rename_hint: Option<bool>,
    pub keybindings: Option<Arc<KeybindingsManager>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeleteMethod {
    Trash,
    Unlink,
}

struct DeleteResult {
    ok: bool,
    method: DeleteMethod,
    error: Option<String>,
}

/// Delete a session file, trying the `trash` CLI first, then falling back to unlink
async fn delete_session_file(session_path: &str) -> DeleteResult {
    // Try `trash` first (if installed)
    let trash_args: Vec<&str> = if session_path.starts_with('-') { vec!["--", session_path] } else { vec![session_path] };
    let trash_result = std::process::Command::new("trash").args(&trash_args).output();

    let trash_error_hint = || -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        match &trash_result {
            Err(error) => parts.push(error.to_string()),
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stderr = js_trim(&stderr);
                if !stderr.is_empty() {
                    parts.push(stderr.split('\n').next().unwrap_or(stderr).to_string());
                }
            }
        }
        if parts.is_empty() {
            return None;
        }
        let joined: String = parts.join(" · ").chars().take(200).collect();
        Some(format!("trash: {joined}"))
    };

    // If trash reports success, or the file is gone afterwards, treat it as successful
    let trash_ok = matches!(&trash_result, Ok(output) if output.status.success());
    if trash_ok || !std::path::Path::new(session_path).exists() {
        return DeleteResult { ok: true, method: DeleteMethod::Trash, error: None };
    }

    // Fallback to permanent deletion
    match tokio::fs::remove_file(session_path).await {
        Ok(()) => DeleteResult { ok: true, method: DeleteMethod::Unlink, error: None },
        Err(err) => {
            let unlink_error = err.to_string();
            let error = match trash_error_hint() {
                Some(hint) => format!("{unlink_error} ({hint})"),
                None => unlink_error,
            };
            DeleteResult { ok: false, method: DeleteMethod::Unlink, error: Some(error) }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectorMode {
    List,
    Rename,
}

type RenameSessionFn = Arc<dyn Fn(String, Option<String>) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

struct SelectorState {
    scope: SessionScope,
    sort_mode: SortMode,
    name_filter: NameFilter,
    current_sessions: Option<Vec<SessionInfo>>,
    all_sessions: Option<Vec<SessionInfo>>,
    current_load: Option<(u64, CancellationToken)>,
    all_load: Option<(u64, CancellationToken)>,
    mode: SelectorMode,
    rename_target_path: Option<String>,
    focused: bool,
}

impl SelectorState {
    fn load_slot(&mut self, scope: SessionScope) -> &mut Option<(u64, CancellationToken)> {
        match scope {
            SessionScope::Current => &mut self.current_load,
            SessionScope::All => &mut self.all_load,
        }
    }

    fn sessions_slot(&mut self, scope: SessionScope) -> &mut Option<Vec<SessionInfo>> {
        match scope {
            SessionScope::Current => &mut self.current_sessions,
            SessionScope::All => &mut self.all_sessions,
        }
    }
}

struct SelectorInner {
    container: Container,
    session_list: Arc<SessionList>,
    header: Arc<SessionSelectorHeader>,
    current_sessions_loader: SessionsLoader,
    all_sessions_loader: SessionsLoader,
    request_render: Arc<dyn Fn() + Send + Sync>,
    rename_session: Option<RenameSessionFn>,
    rename_input: Arc<Input>,
    next_load_id: AtomicU64,
    state: Mutex<SelectorState>,
}

impl SelectorInner {
    fn accent_border() -> ComponentRef {
        Arc::new(DynamicBorder::new(Some(Arc::new(|s: String| theme().fg(&ThemeColor::Accent, &s)))))
    }

    fn build_base_layout(&self, content: ComponentRef, show_header: bool) {
        let c = &self.container;
        c.clear();
        c.add_child(Arc::new(Spacer::new(1)));
        c.add_child(Self::accent_border());
        c.add_child(Arc::new(Spacer::new(1)));
        if show_header {
            c.add_child(self.header.clone());
            c.add_child(Arc::new(Spacer::new(1)));
        }
        c.add_child(content);
        c.add_child(Arc::new(Spacer::new(1)));
        c.add_child(Self::accent_border());
    }

    fn cancel_loads(&self) {
        let mut s = self.state.lock();
        if let Some((_, token)) = s.current_load.take() {
            token.cancel();
            s.current_sessions = None;
        }
        if let Some((_, token)) = s.all_load.take() {
            token.cancel();
            s.all_sessions = None;
        }
    }

    fn enter_rename_mode(&self, session_path: String, current_name: Option<String>) {
        {
            let mut s = self.state.lock();
            s.mode = SelectorMode::Rename;
            s.rename_target_path = Some(session_path);
        }
        self.rename_input.set_value(current_name.unwrap_or_default());
        self.rename_input.set_focused(true);

        let t = theme();
        let panel = Arc::new(Container::new());
        panel.add_child(Arc::new(Text::new(t.bold("Rename Session"), 1, 0, None)));
        panel.add_child(Arc::new(Spacer::new(1)));
        panel.add_child(self.rename_input.clone());
        panel.add_child(Arc::new(Spacer::new(1)));
        panel.add_child(Arc::new(Text::new(
            t.fg(
                &ThemeColor::Muted,
                &format!(
                    "{} to save · {} to cancel",
                    key_text("tui.select.confirm".into()),
                    key_text("tui.select.cancel".into())
                ),
            ),
            1,
            0,
            None,
        )));

        self.build_base_layout(panel, false);
        (self.request_render)();
    }

    fn exit_rename_mode(&self) {
        {
            let mut s = self.state.lock();
            s.mode = SelectorMode::List;
            s.rename_target_path = None;
        }
        self.build_base_layout(self.session_list.clone(), true);
        (self.request_render)();
    }

    async fn confirm_rename(self: Arc<Self>, value: String) {
        let next = js_trim(&value).to_string();
        if next.is_empty() {
            return;
        }
        let Some(target) = self.state.lock().rename_target_path.clone() else {
            self.exit_rename_mode();
            return;
        };
        let Some(rename_session) = self.rename_session.clone() else {
            self.exit_rename_mode();
            return;
        };
        if rename_session(target, Some(next)).await.is_ok() {
            self.clone().refresh_sessions_after_mutation().await;
        }
        self.exit_rename_mode();
    }

    /// Starts loading `scope`. The synchronous part (reserving the load, calling the loader) runs immediately,
    /// like the TS async function up to its first `await`; the returned future finishes the load.
    fn load_scope(self: Arc<Self>, scope: SessionScope) -> BoxFuture<'static, ()> {
        let (id, controller) = {
            let mut s = self.state.lock();
            if s.load_slot(scope).is_some() {
                return async {}.boxed();
            }
            let id = self.next_load_id.fetch_add(1, Ordering::SeqCst) + 1;
            let controller = CancellationToken::new();
            *s.load_slot(scope) = Some((id, controller.clone()));
            (id, controller)
        };
        let show_cwd = scope == SessionScope::All;
        self.header.set_scope(&scope);
        self.header.set_loading(true);
        (self.request_render)();

        let weak = Arc::downgrade(&self);
        let on_progress: SessionListProgress =
            Arc::new(move |loaded: i64, total: i64, partial_sessions: Option<Vec<SessionInfo>>| {
                let Some(inner) = weak.upgrade() else { return };
                let current_scope = {
                    let mut s = inner.state.lock();
                    if !s.load_slot(scope).as_ref().is_some_and(|(a, _)| *a == id) {
                        return;
                    }
                    if let Some(partial) = &partial_sessions {
                        *s.sessions_slot(scope) = Some(partial.clone());
                    }
                    s.scope
                };
                if let Some(partial) = partial_sessions
                    && scope == current_scope
                {
                    inner.session_list.set_sessions(&partial, show_cwd);
                }
                if scope != current_scope {
                    return;
                }
                inner.header.set_progress(loaded, total);
                (inner.request_render)();
            });

        let loader = match scope {
            SessionScope::Current => self.current_sessions_loader.clone(),
            SessionScope::All => self.all_sessions_loader.clone(),
        };
        let load = loader(Some(on_progress), Some(controller));

        async move {
            let result = load.await;
            let is_active = self.state.lock().load_slot(scope).as_ref().is_some_and(|(active, _)| *active == id);
            if !is_active {
                return;
            }
            match result {
                Ok(sessions) => {
                    let current_scope = {
                        let mut s = self.state.lock();
                        *s.sessions_slot(scope) = Some(sessions.clone());
                        *s.load_slot(scope) = None;
                        s.scope
                    };
                    if scope != current_scope {
                        return;
                    }
                    self.header.set_loading(false);
                    self.session_list.set_sessions(&sessions, show_cwd);
                    (self.request_render)();
                }
                Err(err) => {
                    let current_scope = {
                        let mut s = self.state.lock();
                        *s.load_slot(scope) = None;
                        *s.sessions_slot(scope) = None;
                        s.scope
                    };
                    if scope != current_scope {
                        return;
                    }
                    self.header.set_loading(false);
                    self.header.set_status_message(
                        Some(&SessionSelectorHeaderSetStatusMessageMsg {
                            r#type: SessionSelectorHeaderSetStatusMessageMsgType::Error,
                            message: format!("Failed to load sessions: {err}"),
                        }),
                        Some(4000),
                    );
                    self.session_list.set_sessions(&[], show_cwd);
                    (self.request_render)();
                }
            }
        }
        .boxed()
    }

    fn toggle_sort_mode(&self) {
        // Cycle: threaded -> recent -> relevance -> threaded
        let sort_mode = {
            let mut s = self.state.lock();
            s.sort_mode = match s.sort_mode {
                SortMode::Threaded => SortMode::Recent,
                SortMode::Recent => SortMode::Relevance,
                SortMode::Relevance => SortMode::Threaded,
            };
            s.sort_mode
        };
        self.header.set_sort_mode(&sort_mode);
        self.session_list.set_sort_mode(&sort_mode);
        (self.request_render)();
    }

    fn toggle_name_filter(&self) {
        let name_filter = {
            let mut s = self.state.lock();
            s.name_filter = if s.name_filter == NameFilter::All { NameFilter::Named } else { NameFilter::All };
            s.name_filter
        };
        self.header.set_name_filter(&name_filter);
        self.session_list.set_name_filter(&name_filter);
        (self.request_render)();
    }

    async fn refresh_sessions_after_mutation(self: Arc<Self>) {
        self.cancel_loads();
        let scope = {
            let mut s = self.state.lock();
            s.current_sessions = None;
            s.all_sessions = None;
            s.scope
        };
        self.load_scope(scope).await;
    }

    fn toggle_scope(self: &Arc<Self>) {
        let (scope, sessions, loading) = {
            let mut s = self.state.lock();
            s.scope = if s.scope == SessionScope::Current { SessionScope::All } else { SessionScope::Current };
            let scope = s.scope;
            let sessions = s.sessions_slot(scope).clone();
            let loading = s.load_slot(scope).is_some();
            (scope, sessions, loading)
        };
        self.header.set_scope(&scope);
        self.header.set_loading(loading);
        self.session_list.set_sessions(sessions.as_deref().unwrap_or(&[]), scope == SessionScope::All);
        (self.request_render)();
        if sessions.is_none() && !loading {
            spawn_detached(self.clone().load_scope(scope));
        }
    }

    fn current_scope_sessions(&self) -> Vec<SessionInfo> {
        let s = self.state.lock();
        match s.scope {
            SessionScope::All => s.all_sessions.clone().unwrap_or_default(),
            SessionScope::Current => s.current_sessions.clone().unwrap_or_default(),
        }
    }

    async fn delete_session(self: Arc<Self>, session_path: String) {
        let result = delete_session_file(&session_path).await;

        if result.ok {
            {
                let mut s = self.state.lock();
                if let Some(sessions) = &mut s.current_sessions {
                    sessions.retain(|x| x.path != session_path);
                }
                if let Some(sessions) = &mut s.all_sessions {
                    sessions.retain(|x| x.path != session_path);
                }
            }
            let sessions = self.current_scope_sessions();
            let show_cwd = self.state.lock().scope == SessionScope::All;
            self.session_list.set_sessions(&sessions, show_cwd);

            let msg = if result.method == DeleteMethod::Trash { "Session moved to trash" } else { "Session deleted" };
            self.header.set_status_message(
                Some(&SessionSelectorHeaderSetStatusMessageMsg {
                    r#type: SessionSelectorHeaderSetStatusMessageMsgType::Info,
                    message: msg.to_string(),
                }),
                Some(2000),
            );
            self.clone().refresh_sessions_after_mutation().await;
        } else {
            let error_message = result.error.unwrap_or_else(|| "Unknown error".to_string());
            self.header.set_status_message(
                Some(&SessionSelectorHeaderSetStatusMessageMsg {
                    r#type: SessionSelectorHeaderSetStatusMessageMsgType::Error,
                    message: format!("Failed to delete: {error_message}"),
                }),
                Some(3000),
            );
        }

        (self.request_render)();
    }
}

/// Component that renders a session selector
/// TS: `extends Container implements Focusable`
pub struct SessionSelectorComponent {
    inner: Arc<SelectorInner>,
}

impl SessionSelectorComponent {
    /// Whether a session list is still loading.
    pub fn is_loading(&self) -> bool {
        let s = self.inner.state.lock();
        s.current_load.is_some() || s.all_load.is_some()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        current_sessions_loader: &SessionsLoader,
        all_sessions_loader: &SessionsLoader,
        on_select: Arc<dyn Fn(String) + Send + Sync>,
        on_cancel: Arc<dyn Fn() + Send + Sync>,
        on_exit: Arc<dyn Fn() + Send + Sync>,
        request_render: Arc<dyn Fn() + Send + Sync>,
        options: Option<&SessionSelectorComponentOptions>,
        current_session_file_path: Option<&str>,
    ) -> Self {
        let keybindings =
            options.and_then(|o| o.keybindings.clone()).unwrap_or_else(|| KeybindingsManager::create(None));
        let scope = SessionScope::Current;
        let sort_mode = SortMode::Threaded;
        let name_filter = NameFilter::All;
        let header = Arc::new(SessionSelectorHeader::new(&scope, &sort_mode, &name_filter, request_render.clone()));
        let rename_session = options.and_then(|o| o.rename_session.clone());
        let can_rename = rename_session.is_some();
        header.set_show_rename_hint(options.and_then(|o| o.show_rename_hint).unwrap_or(can_rename));

        // Create session list (starts empty, will be populated after load)
        let session_list = Arc::new(SessionList::new(
            &[],
            false,
            &sort_mode,
            &name_filter,
            keybindings,
            current_session_file_path,
        ));

        let inner = Arc::new(SelectorInner {
            container: Container::new(),
            session_list: session_list.clone(),
            header: header.clone(),
            current_sessions_loader: current_sessions_loader.clone(),
            all_sessions_loader: all_sessions_loader.clone(),
            request_render: request_render.clone(),
            rename_session: rename_session.clone(),
            rename_input: Arc::new(Input::default()),
            next_load_id: AtomicU64::new(0),
            state: Mutex::new(SelectorState {
                scope,
                sort_mode,
                name_filter,
                current_sessions: None,
                all_sessions: None,
                current_load: None,
                all_load: None,
                mode: SelectorMode::List,
                rename_target_path: None,
                focused: false,
            }),
        });

        inner.build_base_layout(session_list.clone(), true);

        let weak = Arc::downgrade(&inner);
        inner.rename_input.set_on_submit(Some(Arc::new(move |value: String| {
            if let Some(inner) = weak.upgrade() {
                spawn_detached(inner.confirm_rename(value).boxed());
            }
        })));

        // Ensure header status timeouts are cleared when leaving the selector
        let make_leave = |cb: Arc<dyn Fn() + Send + Sync>| -> Arc<dyn Fn() + Send + Sync> {
            let weak = Arc::downgrade(&inner);
            Arc::new(move || {
                if let Some(inner) = weak.upgrade() {
                    inner.header.set_status_message(None, None);
                    inner.cancel_loads();
                }
                cb();
            })
        };
        {
            let weak = Arc::downgrade(&inner);
            session_list.set_on_select(Some(Arc::new(move |session_path: String| {
                if let Some(inner) = weak.upgrade() {
                    inner.header.set_status_message(None, None);
                    inner.cancel_loads();
                }
                on_select(session_path);
            })));
        }
        session_list.set_on_cancel(Some(make_leave(on_cancel)));
        session_list.set_on_exit(Some(make_leave(on_exit)));

        let weak = Arc::downgrade(&inner);
        session_list.set_on_toggle_scope(Some(Arc::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner.toggle_scope();
            }
        })));
        let weak = Arc::downgrade(&inner);
        session_list.set_on_toggle_sort(Some(Arc::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner.toggle_sort_mode();
            }
        })));
        let weak = Arc::downgrade(&inner);
        session_list.set_on_toggle_name_filter(Some(Arc::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner.toggle_name_filter();
            }
        })));
        let weak = Arc::downgrade(&inner);
        session_list.set_on_rename_session(Some(Arc::new(move |session_path: String| {
            let Some(inner) = weak.upgrade() else { return };
            if inner.rename_session.is_none() {
                return;
            }
            let loading = {
                let mut s = inner.state.lock();
                let scope = s.scope;
                s.load_slot(scope).is_some()
            };
            if loading {
                return;
            }
            let sessions = inner.current_scope_sessions();
            let name = sessions.iter().find(|s| s.path == session_path).and_then(|s| s.name.clone());
            inner.enter_rename_mode(session_path, name);
        })));

        // Sync list events to header
        let weak = Arc::downgrade(&inner);
        session_list.set_on_toggle_path(Some(Arc::new(move |show_path: bool| {
            if let Some(inner) = weak.upgrade() {
                inner.header.set_show_path(show_path);
                (inner.request_render)();
            }
        })));
        let weak = Arc::downgrade(&inner);
        session_list.set_on_delete_confirmation_change(Some(Arc::new(move |path: Option<String>| {
            if let Some(inner) = weak.upgrade() {
                inner.header.set_confirming_delete_path(path.as_deref());
                (inner.request_render)();
            }
        })));
        let weak = Arc::downgrade(&inner);
        session_list.set_on_error(Some(Arc::new(move |msg: String| {
            if let Some(inner) = weak.upgrade() {
                inner.header.set_status_message(
                    Some(&SessionSelectorHeaderSetStatusMessageMsg {
                        r#type: SessionSelectorHeaderSetStatusMessageMsgType::Error,
                        message: msg,
                    }),
                    Some(3000),
                );
                (inner.request_render)();
            }
        })));

        // Handle session deletion
        let weak = Arc::downgrade(&inner);
        session_list.set_on_delete_session(Some(Arc::new(move |session_path: String| {
            let weak = weak.clone();
            async move {
                if let Some(inner) = weak.upgrade() {
                    inner.delete_session(session_path).await;
                }
                Ok(())
            }
            .boxed()
        })));

        // Start loading current sessions immediately
        spawn_detached(inner.clone().load_scope(SessionScope::Current));

        Self { inner }
    }

    pub fn get_session_list(&self) -> Arc<SessionList> {
        self.inner.session_list.clone()
    }
}

impl Component for SessionSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.inner.container.render(width)
    }

    fn handle_input(&self, data: &str) {
        let mode = self.inner.state.lock().mode;
        if mode == SelectorMode::Rename {
            let kb = get_keybindings();
            if kb.matches(data, "tui.select.cancel") {
                self.inner.exit_rename_mode();
                return;
            }
            self.inner.rename_input.handle_input(data);
            return;
        }
        self.inner.session_list.handle_input(data);
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn invalidate(&self) {
        self.inner.container.invalidate();
    }

    fn handle_mouse(&self, event: &MouseEvent) -> Option<MouseResult> {
        self.inner.container.handle_mouse(event)
    }

    fn child_components(&self) -> Option<Vec<ComponentRef>> {
        self.inner.container.child_components()
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&self, focused: bool) {
        let mode = {
            let mut s = self.inner.state.lock();
            s.focused = focused;
            s.mode
        };
        self.inner.session_list.set_focused(focused);
        self.inner.rename_input.set_focused(focused);
        if focused && mode == SelectorMode::Rename {
            self.inner.rename_input.set_focused(true);
        }
    }

    fn focused(&self) -> bool {
        self.inner.state.lock().focused
    }
}
