//! Core TUI types: the [`Component`] trait, [`Container`], overlays, mouse events and the
//! [`Tui`] renderer. Port of `packages/tui/src/tui.ts`.
//!
//! See the crate documentation for the ownership and threading model.

use std::any::Any;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use crate::event_loop::{self, Deferred, EventLoop, Resolver, TimerId};
use crate::keys::{is_key_release, matches_key};
use crate::terminal::Terminal;
use crate::terminal_colors::{
    OscColorTarget, RgbColor, TerminalColorScheme, TerminalColors, parse_osc_color_response,
    parse_terminal_color_scheme_report,
};
use crate::terminal_image::{CellDimensions, get_capabilities, is_image_line, set_cell_dimensions};
use crate::utils::{extract_segments, normalize_terminal_output, slice_by_column, slice_with_width, visible_width};

// =============================================================================
// Component trait
// =============================================================================

/// Upcast helper so `dyn Component` values can be downcast to their concrete type.
pub trait AsAny: Any + Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync>;
}

impl<T: Any + Send + Sync> AsAny for T {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// A shared, thread-safe component handle.
pub type ComponentRef = Arc<dyn Component>;

/// A UI component that renders itself to lines for a given width.
///
/// All methods take `&self`: components use interior mutability (e.g. a `Mutex` around
/// their state) and must **not** hold their own lock while calling callbacks, other
/// components, or TUI methods, so that callbacks may call back into the component.
pub trait Component: AsAny {
    /// Render the component to lines for the given viewport width. Every line must have
    /// a visible width of at most `width`.
    fn render(&self, width: usize) -> Vec<String>;

    /// Keyboard input when the component has focus.
    fn handle_input(&self, _data: &str) {}

    /// Whether this component implements [`handle_input`](Self::handle_input)
    /// (the TypeScript `handleInput` property exists). Override together with it.
    fn handles_input(&self) -> bool {
        false
    }

    /// Normalized mouse handler.
    fn handle_mouse(&self, _event: &MouseEvent) -> Option<MouseResult> {
        None
    }

    /// If true, the component receives key release events (Kitty protocol).
    fn wants_key_release(&self) -> bool {
        false
    }

    /// Invalidate any cached rendering state.
    fn invalidate(&self) {}

    /// Whether the component implements focus tracking (`Focusable` in TypeScript).
    fn is_focusable(&self) -> bool {
        false
    }

    /// Set by the TUI when focus changes. Focused components emit [`CURSOR_MARKER`].
    fn set_focused(&self, _focused: bool) {}

    /// Current focus state for focusable components.
    fn focused(&self) -> bool {
        false
    }

    /// Child components for container-like components (`instanceof Container`).
    fn child_components(&self) -> Option<Vec<ComponentRef>> {
        None
    }

    /// Layout node for components that participate in fullscreen layout.
    fn layout_node(&self) -> Option<crate::layout_node::LayoutNode> {
        None
    }

    /// True when the component's mouse handling is the plain [`Container`] implementation.
    fn uses_container_mouse(&self) -> bool {
        false
    }
}

/// Address of the component data, ignoring vtable metadata.
pub fn component_ptr(c: &ComponentRef) -> *const () {
    Arc::as_ptr(c) as *const ()
}

/// Identity comparison of two components.
pub fn same_component(a: &ComponentRef, b: &ComponentRef) -> bool {
    component_ptr(a) == component_ptr(b)
}

fn same_opt(a: &Option<ComponentRef>, b: &Option<ComponentRef>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => same_component(a, b),
        (None, None) => true,
        _ => false,
    }
}

/// Downcast a component handle to a concrete type.
pub fn downcast_component<T: Component>(c: &ComponentRef) -> Option<Arc<T>> {
    c.clone().as_any_arc().downcast::<T>().ok()
}

/// Whether a component implements focus tracking.
pub fn is_focusable(component: Option<&ComponentRef>) -> bool {
    component.is_some_and(|c| c.is_focusable())
}

// =============================================================================
// Mouse events
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEventType {
    Press,
    Release,
    Move,
    Drag,
    Click,
    Wheel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    None,
}

/// Normalized cell-based mouse event. Coordinates are zero-based.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseEvent {
    pub kind: MouseEventType,
    pub button: MouseButton,
    /// Coordinates local to the receiving component.
    pub x: i64,
    pub y: i64,
    /// Absolute terminal coordinates.
    pub screen_x: i64,
    pub screen_y: i64,
    /// Current component bounds.
    pub width: i64,
    pub height: i64,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    /// Logical lines. Negative values scroll up.
    pub wheel_delta: Option<i64>,
    /// Consecutive click count when the type is click.
    pub click_count: Option<u32>,
}

/// Where a mouse event was delivered and its coordinate transform.
#[derive(Clone)]
pub struct MouseDispatchTarget {
    pub component: ComponentRef,
    pub origin_x: i64,
    pub origin_y: i64,
    pub width: i64,
    pub height: i64,
}

/// Result of a mouse handler.
#[derive(Clone, Default)]
pub struct MouseResult {
    /// Stop propagation and suppress renderer-level fallback behavior.
    pub handled: bool,
    /// Route subsequent drag/release events to this component. Implies handled.
    pub capture: bool,
    /// Give keyboard focus to this component. Implies handled.
    pub focus: bool,
    /// Explicitly request or suppress a render.
    pub render: Option<bool>,
    /// Set when the event was forwarded to a nested component (a dispatch result).
    pub target: Option<MouseDispatchTarget>,
    /// Keyboard focus target, which may be a delegating parent container.
    pub focus_target: Option<ComponentRef>,
}

impl MouseResult {
    pub fn handled() -> Self {
        Self { handled: true, ..Default::default() }
    }
}

/// Dispatch an event to a component and retain the exact target and coordinate transform.
/// Returns a result with `target` set, or `None` when the component did not handle it.
pub fn dispatch_mouse_event(component: &ComponentRef, event: &MouseEvent) -> Option<MouseResult> {
    let result = component.handle_mouse(event)?;
    if result.target.is_some() {
        let mut forwarded = result;
        if forwarded.focus && component.handles_input() {
            forwarded.focus_target = Some(component.clone());
        }
        return Some(forwarded);
    }
    if !result.handled && !result.capture && !result.focus {
        return None;
    }
    let focus = result.focus;
    Some(MouseResult {
        handled: true,
        capture: result.capture,
        focus,
        render: result.render,
        focus_target: if focus { Some(component.clone()) } else { result.focus_target },
        target: Some(MouseDispatchTarget {
            component: component.clone(),
            origin_x: event.screen_x - event.x,
            origin_y: event.screen_y - event.y,
            width: event.width,
            height: event.height,
        }),
    })
}

/// Recreate local coordinates for a previously dispatched mouse target.
pub fn retarget_mouse_event(event: &MouseEvent, target: &MouseDispatchTarget) -> MouseEvent {
    MouseEvent {
        x: event.screen_x - target.origin_x,
        y: event.screen_y - target.origin_y,
        width: target.width,
        height: target.height,
        ..*event
    }
}

// =============================================================================
// Cursor markers
// =============================================================================

/// Cursor position marker (zero-width APC sequence). Focused components emit it at the
/// cursor position; the TUI strips it and positions the hardware cursor there.
pub const CURSOR_MARKER: &str = "\x1b_pi:c\x07";
/// Fake cursor start marker.
pub const FAKE_CURSOR_START: &str = "\x1b_pi:fc\x07";
/// Fake cursor end marker.
pub const FAKE_CURSOR_END: &str = "\x1b_pi:/fc\x07";

/// Wrap text in fake cursor markers.
pub fn render_fake_cursor(text: &str) -> String {
    format!("{FAKE_CURSOR_START}{text}{FAKE_CURSOR_END}")
}

// =============================================================================
// Overlays
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayAnchor {
    #[default]
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TopCenter,
    BottomCenter,
    LeftCenter,
    RightCenter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OverlayMargin {
    pub top: Option<i64>,
    pub right: Option<i64>,
    pub bottom: Option<i64>,
    pub left: Option<i64>,
}

impl OverlayMargin {
    pub fn all(v: i64) -> Self {
        Self { top: Some(v), right: Some(v), bottom: Some(v), left: Some(v) }
    }
}

/// A size that is absolute or a percentage of a reference size.
#[derive(Debug, Clone, PartialEq)]
pub enum SizeValue {
    Abs(i64),
    /// Percentage string like `"50%"`. Invalid strings are ignored.
    Percent(String),
}

impl From<i64> for SizeValue {
    fn from(v: i64) -> Self {
        SizeValue::Abs(v)
    }
}
impl From<&str> for SizeValue {
    fn from(v: &str) -> Self {
        SizeValue::Percent(v.to_string())
    }
}

fn parse_percent(s: &str) -> Option<f64> {
    // /^(\d+(?:\.\d+)?)%$/
    let num = s.strip_suffix('%')?;
    let (int, frac) = match num.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (num, None),
    };
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if let Some(f) = frac
        && (f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    num.parse().ok()
}

fn parse_size_value(value: Option<&SizeValue>, reference: i64) -> Option<i64> {
    match value? {
        SizeValue::Abs(v) => Some(*v),
        SizeValue::Percent(s) => parse_percent(s).map(|p| ((reference as f64 * p) / 100.0).floor() as i64),
    }
}

/// Visibility predicate for overlays: `(terminal_width, terminal_height) -> visible`.
pub type OverlayVisibleFn = Arc<dyn Fn(usize, usize) -> bool + Send + Sync>;

/// Options for overlay positioning and sizing.
#[derive(Clone, Default)]
pub struct OverlayOptions {
    /// Width in columns, or percentage of terminal width.
    pub width: Option<SizeValue>,
    /// Minimum width in columns.
    pub min_width: Option<i64>,
    /// Maximum height in rows, or percentage of terminal height.
    pub max_height: Option<SizeValue>,
    /// Anchor point (default: center).
    pub anchor: Option<OverlayAnchor>,
    /// Horizontal offset from the anchor position.
    pub offset_x: Option<i64>,
    /// Vertical offset from the anchor position.
    pub offset_y: Option<i64>,
    /// Row position: absolute or percentage.
    pub row: Option<SizeValue>,
    /// Column position: absolute or percentage.
    pub col: Option<SizeValue>,
    /// Margin from terminal edges.
    pub margin: Option<OverlayMargin>,
    /// Render only while this returns true.
    pub visible: Option<OverlayVisibleFn>,
    /// Don't capture keyboard focus when shown.
    pub non_capturing: bool,
}

/// Last rendered terminal-relative overlay rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayBounds {
    pub row: i64,
    pub col: i64,
    pub width: i64,
    pub height: i64,
}

struct OverlayEntryState {
    pre_focus: Option<ComponentRef>,
    hidden: bool,
    focus_order: u64,
    bounds: Option<OverlayBounds>,
}

pub(crate) struct OverlayEntry {
    pub(crate) component: ComponentRef,
    options: Option<OverlayOptions>,
    st: Mutex<OverlayEntryState>,
}

impl OverlayEntry {
    fn lock(&self) -> MutexGuard<'_, OverlayEntryState> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn non_capturing(&self) -> bool {
        self.options.as_ref().is_some_and(|o| o.non_capturing)
    }
}

type EntryRef = Arc<OverlayEntry>;

fn same_entry(a: &EntryRef, b: &EntryRef) -> bool {
    Arc::ptr_eq(a, b)
}

#[derive(Clone)]
enum BlockedResume {
    RestoreOverlay,
    FocusTarget(Option<ComponentRef>),
}

#[derive(Clone)]
enum FocusRestore {
    Inactive,
    Eligible { overlay: EntryRef },
    Blocked { overlay: EntryRef, blocked_by: ComponentRef, resume: BlockedResume },
}

impl FocusRestore {
    fn overlay(&self) -> Option<&EntryRef> {
        match self {
            FocusRestore::Inactive => None,
            FocusRestore::Eligible { overlay } | FocusRestore::Blocked { overlay, .. } => Some(overlay),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FocusRestorePolicy {
    Clear,
    Preserve,
}

#[derive(Clone)]
pub(crate) struct RenderedOverlayLayout {
    pub(crate) entry: EntryRef,
    pub(crate) row: i64,
    pub(crate) col: i64,
    pub(crate) width: i64,
    pub(crate) height: i64,
}

/// Handle returned by [`Tui::show_overlay`].
#[derive(Clone)]
pub struct OverlayHandle {
    tui: Weak<Tui>,
    entry: EntryRef,
}

/// Options for [`OverlayHandle::unfocus`].
#[derive(Clone)]
pub struct OverlayUnfocusOptions {
    /// Explicit target to focus after releasing this overlay.
    pub target: Option<ComponentRef>,
}

impl OverlayHandle {
    /// Permanently remove the overlay.
    pub fn hide(&self) {
        if let Some(tui) = self.tui.upgrade() {
            tui.overlay_hide(&self.entry);
        }
    }
    /// Temporarily hide or show the overlay.
    pub fn set_hidden(&self, hidden: bool) {
        if let Some(tui) = self.tui.upgrade() {
            tui.overlay_set_hidden(&self.entry, hidden);
        }
    }
    pub fn is_hidden(&self) -> bool {
        self.entry.lock().hidden
    }
    /// Focus this overlay and bring it to the visual front.
    pub fn focus(&self) {
        if let Some(tui) = self.tui.upgrade() {
            tui.overlay_focus(&self.entry);
        }
    }
    /// Release focus to the next visible capturing overlay or previous target, or to an
    /// explicit target when provided.
    pub fn unfocus(&self, options: Option<OverlayUnfocusOptions>) {
        if let Some(tui) = self.tui.upgrade() {
            tui.overlay_unfocus(&self.entry, options);
        }
    }
    pub fn is_focused(&self) -> bool {
        match self.tui.upgrade() {
            Some(tui) => same_opt(&tui.state().focused, &Some(self.entry.component.clone())),
            None => false,
        }
    }
    /// The most recent rendered bounds for a visible overlay.
    pub fn get_bounds(&self) -> Option<OverlayBounds> {
        let tui = self.tui.upgrade()?;
        let in_stack = tui.state().overlay_stack.iter().any(|e| same_entry(e, &self.entry));
        if !in_stack || !tui.is_overlay_visible(&self.entry) {
            return None;
        }
        self.entry.lock().bounds
    }
    pub fn component(&self) -> &ComponentRef {
        &self.entry.component
    }
}

// =============================================================================
// Container
// =============================================================================

/// A component that contains other components and renders them vertically.
#[derive(Default)]
pub struct Container {
    children: Mutex<Vec<ComponentRef>>,
    mouse_layout: Mutex<Option<(usize, Vec<(ComponentRef, usize)>)>>,
}

impl Container {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new container wrapped in an `Arc`.
    pub fn new_ref() -> Arc<Self> {
        Arc::new(Self::new())
    }

    pub fn add_child(&self, component: ComponentRef) {
        self.children.lock().unwrap_or_else(|e| e.into_inner()).push(component);
    }

    /// Insert a child at `index` (clamped).
    pub fn insert_child(&self, index: usize, component: ComponentRef) {
        let mut c = self.children.lock().unwrap_or_else(|e| e.into_inner());
        let i = index.min(c.len());
        c.insert(i, component);
    }

    pub fn remove_child(&self, component: &ComponentRef) {
        let mut c = self.children.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = c.iter().position(|x| same_component(x, component)) {
            c.remove(i);
        }
    }

    pub fn clear(&self) {
        self.children.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Snapshot of the children.
    pub fn children(&self) -> Vec<ComponentRef> {
        self.children.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replace all children.
    pub fn set_children(&self, children: Vec<ComponentRef>) {
        *self.children.lock().unwrap_or_else(|e| e.into_inner()) = children;
    }

    pub fn len(&self) -> usize {
        self.children.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn invalidate_children(&self) {
        for child in self.children() {
            child.invalidate();
        }
    }

    /// Render all children (same as [`Component::render`]).
    pub fn render_children(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        let mut layout = Vec::new();
        for child in self.children() {
            let child_lines = child.render(width);
            layout.push((child.clone(), child_lines.len()));
            lines.extend(child_lines);
        }
        *self.mouse_layout.lock().unwrap_or_else(|e| e.into_inner()) = Some((width, layout));
        lines
    }

    /// Container mouse dispatch. `focus_owner` is the component that should keep keyboard
    /// focus when a child requests focus (a container subclass that handles input).
    pub fn handle_mouse_with_owner(&self, event: &MouseEvent, focus_owner: Option<ComponentRef>) -> Option<MouseResult> {
        if event.y < 0 || event.y >= event.height {
            return None;
        }
        let width = event.width.max(0) as usize;
        let cached = self.mouse_layout.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mouse_children = match cached {
            Some((w, layout)) if w == width => layout,
            _ => self.children().into_iter().map(|c| { let h = c.render(width).len(); (c, h) }).collect(),
        };
        let mut child_y = 0i64;
        for (child, h) in mouse_children {
            let h = h as i64;
            if event.y >= child_y && event.y < child_y + h {
                let child_event = MouseEvent { y: event.y - child_y, height: h, ..*event };
                let result = dispatch_mouse_event(&child, &child_event);
                if let (Some(mut r), Some(owner)) = (result.clone(), focus_owner)
                    && r.focus
                {
                    r.focus_target = Some(owner);
                    return Some(r);
                }
                return result;
            }
            child_y += h;
        }
        None
    }
}

impl Component for Container {
    fn render(&self, width: usize) -> Vec<String> {
        self.render_children(width)
    }
    fn handle_mouse(&self, event: &MouseEvent) -> Option<MouseResult> {
        self.handle_mouse_with_owner(event, None)
    }
    fn invalidate(&self) {
        self.invalidate_children();
    }
    fn child_components(&self) -> Option<Vec<ComponentRef>> {
        Some(self.children())
    }
    fn uses_container_mouse(&self) -> bool {
        true
    }
}

// =============================================================================
// Line compositing
// =============================================================================

pub(crate) const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";

/// Composite overlay content into a terminal line at a fixed column.
pub fn composite_tui_line(
    base_line: &str,
    overlay_line: &str,
    start_col: usize,
    overlay_width: usize,
    total_width: usize,
) -> String {
    if is_image_line(base_line) {
        return base_line.to_string();
    }
    let after_start = start_col + overlay_width;
    let base = extract_segments(base_line, start_col, after_start, total_width.saturating_sub(after_start), true);
    let (overlay_text, overlay_w) = slice_with_width(overlay_line, 0, overlay_width, true);
    let before_pad = start_col.saturating_sub(base.before_width);
    let overlay_pad = overlay_width.saturating_sub(overlay_w);
    let actual_before = start_col.max(base.before_width);
    let actual_overlay = overlay_width.max(overlay_w);
    let after_target = total_width.saturating_sub(actual_before + actual_overlay);
    let after_pad = after_target.saturating_sub(base.after_width);
    let result = format!(
        "{}{}{SEGMENT_RESET}{}{}{SEGMENT_RESET}{}{}",
        base.before,
        " ".repeat(before_pad),
        overlay_text,
        " ".repeat(overlay_pad),
        base.after,
        " ".repeat(after_pad)
    );
    if visible_width(&result) <= total_width { result } else { slice_by_column(&result, 0, total_width, true) }
}

/// Apply per-line resets (normalized output + SGR/OSC 8 reset) to non-image lines.
pub(crate) fn apply_line_resets(lines: &mut [String]) {
    for line in lines.iter_mut() {
        if !is_image_line(line) {
            *line = normalize_terminal_output(line) + SEGMENT_RESET;
        }
    }
}

/// Find and strip CURSOR_MARKER from the bottom `height` lines.
pub(crate) fn extract_cursor_position(lines: &mut [String], height: usize) -> Option<(usize, usize)> {
    let viewport_top = lines.len().saturating_sub(height);
    for row in (viewport_top..lines.len()).rev() {
        if let Some(idx) = lines[row].find(CURSOR_MARKER) {
            let col = visible_width(&lines[row][..idx]);
            let line = &lines[row];
            lines[row] = format!("{}{}", &line[..idx], &line[idx + CURSOR_MARKER.len()..]);
            return Some((row, col));
        }
    }
    None
}

/// Resolve fake cursor markers to reverse video.
pub(crate) fn resolve_fake_cursors(lines: Vec<String>, show_hardware_cursor: bool) -> Vec<String> {
    lines
        .into_iter()
        .map(|line| {
            if !line.contains(FAKE_CURSOR_START) {
                return line;
            }
            let mut resolved = line;
            if show_hardware_cursor {
                let pattern = format!("{CURSOR_MARKER}{FAKE_CURSOR_START}");
                if let Some(idx) = resolved.find(&pattern) {
                    let after_start = idx + pattern.len();
                    let rest = &resolved[after_start..];
                    let (text, tail) = match rest.find(FAKE_CURSOR_END) {
                        Some(e) => (&rest[..e], &rest[e + FAKE_CURSOR_END.len()..]),
                        None => (rest, ""),
                    };
                    resolved = format!("{}{CURSOR_MARKER}{text}{tail}", &resolved[..idx]);
                }
            }
            resolved.replace(FAKE_CURSOR_START, "\x1b[7m").replace(FAKE_CURSOR_END, "\x1b[27m")
        })
        .collect()
}

// =============================================================================
// TUI
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiMode {
    Regular,
    Fullscreen,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TuiStopOptions {
    /// Leave renderer output in place for another TUI taking over the same terminal.
    pub preserve_screen: bool,
}

/// Result of an input listener.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputListenerResult {
    /// Stop processing this input.
    pub consume: bool,
    /// Replace the input passed to later listeners and the focused component.
    pub data: Option<String>,
}

/// An input listener; runs before the focused component receives input.
pub type InputListener = Arc<dyn Fn(&str) -> Option<InputListenerResult> + Send + Sync>;
/// Terminal color scheme listener.
pub type ColorSchemeListener = Arc<dyn Fn(TerminalColorScheme) + Send + Sync>;

const TERMINAL_PALETTE_SIZE: usize = 16;
const TERMINAL_COLOR_REPLY_COUNT: usize = 2 + TERMINAL_PALETTE_SIZE;

fn terminal_color_query() -> String {
    let mut s = String::from("\x1b]10;?\x07\x1b]11;?\x07");
    for i in 0..TERMINAL_PALETTE_SIZE {
        s.push_str(&format!("\x1b]4;{i};?\x07"));
    }
    s.push_str("\x1b[c");
    s
}

fn is_device_attributes_response(data: &str) -> bool {
    match data.strip_prefix("\x1b[?").and_then(|r| r.strip_suffix('c')) {
        Some(params) => params.bytes().all(|b| b.is_ascii_digit() || b == b';'),
        None => false,
    }
}

type Deliver = Arc<dyn Fn(TerminalColors) + Send + Sync>;

struct PendingColorQuery {
    foreground: Option<RgbColor>,
    background: Option<RgbColor>,
    palette: Vec<Option<RgbColor>>,
    replied: Vec<String>,
    deliver: Option<Deliver>,
    timer: Option<TimerId>,
}

impl PendingColorQuery {
    fn result(&self) -> TerminalColors {
        let palette = if self.palette.iter().all(Option::is_some) {
            Some(self.palette.iter().map(|c| c.unwrap()).collect())
        } else {
            None
        };
        TerminalColors { foreground: self.foreground, background: self.background, palette }
    }
}

pub(crate) struct TuiState {
    pub(crate) focused: Option<ComponentRef>,
    input_listeners: Vec<(u64, InputListener)>,
    next_listener_id: u64,
    pub(crate) show_hardware_cursor: bool,
    clear_on_shrink: bool,
    pub(crate) stopped: bool,
    pending_color_queries: Vec<Arc<Mutex<PendingColorQuery>>>,
    color_scheme_listeners: Vec<(u64, ColorSchemeListener)>,
    color_scheme_notifications: bool,
    focus_order_counter: u64,
    pub(crate) overlay_stack: Vec<EntryRef>,
    pub(crate) rendered_overlay_layouts: Vec<RenderedOverlayLayout>,
    focus_restore: FocusRestore,
}

#[derive(Default)]
struct SchedState {
    render_requested: bool,
    immediate_scheduled: bool,
    render_timer: Option<TimerId>,
    last_render_at: f64,
}

/// Options for constructing a [`Tui`].
#[derive(Clone, Default)]
pub struct TuiOptions {
    pub show_hardware_cursor: Option<bool>,
    /// Directory for debug/crash logs.
    pub log_directory: Option<PathBuf>,
    /// Event loop for render scheduling and timers (default: [`event_loop::current`]).
    pub event_loop: Option<EventLoop>,
}

pub(crate) enum Screen {
    Main(Mutex<crate::tui_main_screen::MainRenderState>),
    Alt(Box<crate::tui_alt_screen::AltScreen>),
}

/// Debug key callback.
pub type DebugCallback = Arc<dyn Fn() + Send + Sync>;

/// The terminal UI: a container of root components plus overlays, focus management,
/// input dispatch and differential rendering.
///
/// Create with [`Tui::main_screen`] (regular mode, renders into the terminal's scrollback)
/// or [`Tui::alt_screen`] (fullscreen, application-owned viewport).
pub struct Tui {
    pub(crate) weak_self: Weak<Tui>,
    mode: TuiMode,
    terminal: Arc<dyn Terminal>,
    event_loop: EventLoop,
    pub(crate) container: Container,
    state: Mutex<TuiState>,
    sched: Mutex<SchedState>,
    pub(crate) log_directory: Option<PathBuf>,
    on_debug: Mutex<Option<DebugCallback>>,
    pub(crate) full_redraw_count: std::sync::atomic::AtomicUsize,
    pub(crate) screen: Screen,
    /// The renderer that replaced this one (see [`Tui::redirect_to`]).
    redirect: Mutex<Option<Weak<Tui>>>,
}

const MIN_RENDER_INTERVAL_MS: f64 = 16.0;

impl Tui {
    pub(crate) fn new_with_screen(terminal: Arc<dyn Terminal>, options: TuiOptions, mode: TuiMode, screen: Screen) -> Arc<Tui> {
        Self::new_with_screen_fn(terminal, options, mode, move |_| screen)
    }

    pub(crate) fn new_with_screen_fn(
        terminal: Arc<dyn Terminal>,
        options: TuiOptions,
        mode: TuiMode,
        make_screen: impl FnOnce(&Weak<Tui>) -> Screen,
    ) -> Arc<Tui> {
        let event_loop = options.event_loop.unwrap_or_else(event_loop::current);
        Arc::new_cyclic(|weak| Tui {
            weak_self: weak.clone(),
            mode,
            terminal,
            event_loop,
            container: Container::new(),
            state: Mutex::new(TuiState {
                focused: None,
                input_listeners: Vec::new(),
                next_listener_id: 1,
                show_hardware_cursor: options.show_hardware_cursor.unwrap_or(false),
                clear_on_shrink: false,
                stopped: false,
                pending_color_queries: Vec::new(),
                color_scheme_listeners: Vec::new(),
                color_scheme_notifications: false,
                focus_order_counter: 0,
                overlay_stack: Vec::new(),
                rendered_overlay_layouts: Vec::new(),
                focus_restore: FocusRestore::Inactive,
            }),
            sched: Mutex::new(SchedState::default()),
            log_directory: options.log_directory,
            on_debug: Mutex::new(None),
            full_redraw_count: std::sync::atomic::AtomicUsize::new(0),
            screen: make_screen(weak),
            redirect: Mutex::new(None),
        })
    }

    /// Forward this retired renderer's lifecycle, focus, overlay and render calls to `next`.
    ///
    /// TS hands components a proxy that always resolves to the active renderer
    /// (`createInteractiveTuiReference`); a Rust component keeps the `Arc<Tui>` it was built with,
    /// so a host that replaces its renderer points the old one at the new one instead.
    pub fn redirect_to(&self, next: &Arc<Tui>) {
        if std::ptr::eq(self, Arc::as_ptr(next)) {
            *self.redirect.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return;
        }
        *self.redirect.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(next));
    }

    /// The renderer this one forwards to, if it was retired with [`Tui::redirect_to`].
    pub fn redirect_target(&self) -> Option<Arc<Tui>> {
        self.redirect.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(Weak::upgrade)
    }

    /// A TUI that renders into the terminal's main screen and scrollback.
    pub fn main_screen(terminal: Arc<dyn Terminal>) -> Arc<Tui> {
        Self::main_screen_with(terminal, TuiOptions::default())
    }

    pub fn main_screen_with(terminal: Arc<dyn Terminal>, options: TuiOptions) -> Arc<Tui> {
        Self::new_with_screen(
            terminal,
            options,
            TuiMode::Regular,
            Screen::Main(Mutex::new(crate::tui_main_screen::MainRenderState::default())),
        )
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, TuiState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn sched(&self) -> MutexGuard<'_, SchedState> {
        self.sched.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn mode(&self) -> TuiMode {
        self.mode
    }

    /// Whether this is a fullscreen (viewport) TUI.
    pub fn is_viewport_tui(&self) -> bool {
        self.mode == TuiMode::Fullscreen
    }

    pub fn terminal(&self) -> &Arc<dyn Terminal> {
        &self.terminal
    }

    pub fn event_loop(&self) -> &EventLoop {
        &self.event_loop
    }

    /// Number of full redraws performed so far.
    pub fn full_redraws(&self) -> usize {
        self.full_redraw_count.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Global debug key (Shift+Ctrl+D) callback.
    pub fn set_on_debug(&self, callback: Option<DebugCallback>) {
        *self.on_debug.lock().unwrap_or_else(|e| e.into_inner()) = callback;
    }

    // ---- container API ----

    pub fn add_child(&self, component: ComponentRef) {
        self.container.add_child(component);
    }

    pub fn remove_child(&self, component: &ComponentRef) {
        self.container.remove_child(component);
    }

    pub fn clear(&self) {
        self.container.clear();
    }

    pub fn children(&self) -> Vec<ComponentRef> {
        self.container.children()
    }

    pub fn get_show_hardware_cursor(&self) -> bool {
        self.state().show_hardware_cursor
    }

    pub fn set_show_hardware_cursor(&self, enabled: bool) {
        {
            let mut st = self.state();
            if st.show_hardware_cursor == enabled {
                return;
            }
            st.show_hardware_cursor = enabled;
        }
        if !enabled {
            self.hide_terminal_cursor();
        }
        self.request_render(false);
    }

    pub fn get_clear_on_shrink(&self) -> bool {
        self.state().clear_on_shrink
    }

    /// Whether to trigger a full re-render when content shrinks.
    pub fn set_clear_on_shrink(&self, enabled: bool) {
        self.state().clear_on_shrink = enabled;
    }

    pub fn get_focused_component(&self) -> Option<ComponentRef> {
        self.state().focused.clone()
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.state().stopped
    }

    // ---- focus ----

    pub fn set_focus(&self, component: Option<ComponentRef>) {
        if let Some(target) = self.redirect_target() {
            return target.set_focus(component);
        }
        self.set_focus_internal(component, FocusRestorePolicy::Clear);
    }

    fn set_focus_internal(&self, component: Option<ComponentRef>, policy: FocusRestorePolicy) {
        // Overlay `visible` callbacks and the component-tree walk of `is_component_mounted` call
        // into user code, so they are evaluated before taking the state lock (never under it).
        let (stack_snapshot, restore_snapshot) = {
            let st = self.state();
            (st.overlay_stack.clone(), st.focus_restore.clone())
        };
        let visibility: Vec<(EntryRef, bool)> =
            stack_snapshot.iter().map(|e| (e.clone(), self.is_overlay_visible(e))).collect();
        let blocked_by_mounted = match &restore_snapshot {
            FocusRestore::Blocked { blocked_by, .. } => self.is_component_mounted(blocked_by),
            _ => true,
        };
        let is_visible = |e: &EntryRef| -> bool {
            match visibility.iter().find(|(v, _)| same_entry(v, e)) {
                Some((_, visible)) => *visible,
                // Pushed after the snapshot: fall back to the hidden flag (no user callback).
                None => !e.lock().hidden,
            }
        };
        let (prev, next) = {
            let mut st = self.state();
            let previous_focus = st.focused.clone();
            let mut next_focus = component;
            let previous_focused_overlay = previous_focus.as_ref().and_then(|pf| {
                st.overlay_stack.iter().find(|e| same_component(&e.component, pf) && is_visible(e)).cloned()
            });
            let next_is_overlay = next_focus
                .as_ref()
                .is_some_and(|nf| st.overlay_stack.iter().any(|e| same_component(&e.component, nf)));
            let restore_state = Self::visible_focus_restore_with(&st, &is_visible);
            if let Some(nf) = next_focus.clone().filter(|_| !next_is_overlay) {
                match &restore_state {
                    FocusRestore::Blocked { overlay, blocked_by, resume }
                        if previous_focus.as_ref().is_some_and(|p| same_component(blocked_by, p)) =>
                    {
                        let resume_is_target = matches!(resume, BlockedResume::FocusTarget(_));
                        let mounted = match &restore_snapshot {
                            FocusRestore::Blocked { blocked_by: snap, .. } if same_component(snap, blocked_by) => {
                                blocked_by_mounted
                            }
                            _ => true,
                        };
                        if resume_is_target || !mounted {
                            next_focus = self.resolve_blocked_resume(&mut st, overlay, resume);
                        } else {
                            st.focus_restore = FocusRestore::Blocked {
                                overlay: overlay.clone(),
                                blocked_by: nf.clone(),
                                resume: resume.clone(),
                            };
                        }
                    }
                    _ => {
                        if let (Some(pfo), Some(ro)) = (&previous_focused_overlay, restore_state.overlay())
                            && same_entry(ro, pfo) && !self.is_overlay_focus_ancestor(&st, pfo, &nf)
                        {
                            st.focus_restore = FocusRestore::Blocked {
                                overlay: pfo.clone(),
                                blocked_by: nf.clone(),
                                resume: BlockedResume::RestoreOverlay,
                            };
                        }
                    }
                }
            } else if next_focus.is_none() {
                match &restore_state {
                    FocusRestore::Blocked { overlay, blocked_by, resume }
                        if previous_focus.as_ref().is_some_and(|p| same_component(blocked_by, p)) =>
                    {
                        next_focus = self.resolve_blocked_resume(&mut st, overlay, resume);
                    }
                    _ => {
                        if policy == FocusRestorePolicy::Clear {
                            st.focus_restore = FocusRestore::Inactive;
                        }
                    }
                }
            }
            let prev = st.focused.take();
            st.focused = next_focus.clone();
            if let Some(nf) = &next_focus {
                let focused_overlay =
                    st.overlay_stack.iter().find(|e| same_component(&e.component, nf) && is_visible(e)).cloned();
                if let Some(fo) = focused_overlay {
                    st.focus_restore = FocusRestore::Eligible { overlay: fo };
                }
            }
            (prev, next_focus)
        };
        if let Some(p) = prev.filter(|p| p.is_focusable()) {
            p.set_focused(false);
        }
        if let Some(n) = next.filter(|n| n.is_focusable()) {
            n.set_focused(true);
        }
    }

    fn resolve_blocked_resume(&self, st: &mut TuiState, overlay: &EntryRef, resume: &BlockedResume) -> Option<ComponentRef> {
        match resume {
            BlockedResume::RestoreOverlay => Some(overlay.component.clone()),
            BlockedResume::FocusTarget(target) => {
                st.focus_restore = FocusRestore::Inactive;
                target.clone()
            }
        }
    }

    /// The focus-restore state, or `Inactive` when its overlay is gone or invisible. Takes the state
    /// lock only for the snapshot: overlay `visible` callbacks run without it.
    fn visible_focus_restore(&self) -> FocusRestore {
        let (rs, in_stack) = {
            let st = self.state();
            let rs = st.focus_restore.clone();
            let in_stack = rs.overlay().is_some_and(|o| st.overlay_stack.iter().any(|e| same_entry(e, o)));
            (rs, in_stack)
        };
        if let Some(overlay) = rs.overlay()
            && (!in_stack || !self.is_overlay_visible(overlay))
        {
            return FocusRestore::Inactive;
        }
        rs
    }

    fn visible_focus_restore_with(st: &TuiState, is_visible: &dyn Fn(&EntryRef) -> bool) -> FocusRestore {
        let rs = st.focus_restore.clone();
        if let Some(overlay) = rs.overlay()
            && (!st.overlay_stack.iter().any(|e| same_entry(e, overlay)) || !is_visible(overlay))
        {
            return FocusRestore::Inactive;
        }
        rs
    }

    fn clear_focus_restore_for(st: &mut TuiState, overlay: &EntryRef) {
        if st.focus_restore.overlay().is_some_and(|o| same_entry(o, overlay)) {
            st.focus_restore = FocusRestore::Inactive;
        }
    }

    fn is_overlay_focus_ancestor(&self, st: &TuiState, entry: &EntryRef, component: &ComponentRef) -> bool {
        let mut visited: Vec<*const ()> = Vec::new();
        let mut current = entry.lock().pre_focus.clone();
        while let Some(c) = current {
            let ptr = component_ptr(&c);
            if visited.contains(&ptr) {
                break;
            }
            visited.push(ptr);
            if same_component(&c, component) {
                return true;
            }
            current = st
                .overlay_stack
                .iter()
                .find(|o| same_component(&o.component, &c))
                .and_then(|o| o.lock().pre_focus.clone());
        }
        false
    }

    fn retarget_overlay_pre_focus(st: &TuiState, removed: &EntryRef) {
        let removed_pre = removed.lock().pre_focus.clone();
        for overlay in &st.overlay_stack {
            if same_entry(overlay, removed) {
                continue;
            }
            let mut o = overlay.lock();
            if o.pre_focus.as_ref().is_some_and(|p| same_component(p, &removed.component)) {
                o.pre_focus = removed_pre.clone();
            }
        }
    }

    /// Root components that are considered mounted.
    pub(crate) fn mounted_roots(&self) -> Vec<ComponentRef> {
        match &self.screen {
            Screen::Alt(alt) => alt.mounted_roots(self),
            Screen::Main(_) => self.container.children(),
        }
    }

    fn is_component_mounted(&self, component: &ComponentRef) -> bool {
        self.mounted_roots().iter().any(|root| contains_component(root, component))
    }

    // ---- overlays ----

    /// Show an overlay component. Returns a handle to control its visibility.
    pub fn show_overlay(&self, component: ComponentRef, options: Option<OverlayOptions>) -> OverlayHandle {
        if let Some(target) = self.redirect_target() {
            return target.show_overlay(component, options);
        }
        let non_capturing = options.as_ref().is_some_and(|o| o.non_capturing);
        let entry = {
            let mut st = self.state();
            st.focus_order_counter += 1;
            let entry = Arc::new(OverlayEntry {
                component: component.clone(),
                options,
                st: Mutex::new(OverlayEntryState {
                    pre_focus: st.focused.clone(),
                    hidden: false,
                    focus_order: st.focus_order_counter,
                    bounds: None,
                }),
            });
            st.overlay_stack.push(entry.clone());
            entry
        };
        if !non_capturing && self.is_overlay_visible(&entry) {
            self.set_focus(Some(component));
        }
        self.hide_terminal_cursor();
        self.request_render(false);
        OverlayHandle { tui: self.weak_self.clone(), entry }
    }

    fn overlay_hide(&self, entry: &EntryRef) {
        let refocus = {
            let mut st = self.state();
            let Some(index) = st.overlay_stack.iter().position(|e| same_entry(e, entry)) else {
                return;
            };
            Self::clear_focus_restore_for(&mut st, entry);
            Self::retarget_overlay_pre_focus(&st, entry);
            st.overlay_stack.remove(index);
            let had_focus = st.focused.as_ref().is_some_and(|f| same_component(f, &entry.component));
            let empty = st.overlay_stack.is_empty();
            (had_focus, empty)
        };
        if refocus.0 {
            let top = self.topmost_visible_overlay();
            let target = top.map(|t| t.component.clone()).or_else(|| entry.lock().pre_focus.clone());
            self.set_focus(target);
        }
        if refocus.1 {
            self.hide_terminal_cursor();
        }
        self.request_render(false);
    }

    fn overlay_set_hidden(&self, entry: &EntryRef, hidden: bool) {
        {
            let mut e = entry.lock();
            if e.hidden == hidden {
                return;
            }
            e.hidden = hidden;
        }
        if hidden {
            let had_focus = {
                let mut st = self.state();
                Self::clear_focus_restore_for(&mut st, entry);
                st.focused.as_ref().is_some_and(|f| same_component(f, &entry.component))
            };
            if had_focus {
                let top = self.topmost_visible_overlay();
                let target = top.map(|t| t.component.clone()).or_else(|| entry.lock().pre_focus.clone());
                self.set_focus(target);
            }
        } else if !entry.non_capturing() && self.is_overlay_visible(entry) {
            {
                let mut st = self.state();
                st.focus_order_counter += 1;
                entry.lock().focus_order = st.focus_order_counter;
            }
            self.set_focus(Some(entry.component.clone()));
        }
        self.request_render(false);
    }

    fn overlay_focus(&self, entry: &EntryRef) {
        // The `visible` callback is user code: evaluate it without the state lock.
        if !self.is_overlay_visible(entry) {
            return;
        }
        {
            let mut st = self.state();
            if !st.overlay_stack.iter().any(|e| same_entry(e, entry)) {
                return;
            }
            st.focus_order_counter += 1;
            entry.lock().focus_order = st.focus_order_counter;
        }
        self.set_focus(Some(entry.component.clone()));
        self.request_render(false);
    }

    fn overlay_unfocus(&self, entry: &EntryRef, options: Option<OverlayUnfocusOptions>) {
        let fallback = {
            let mut st = self.state();
            let is_focused = st.focused.as_ref().is_some_and(|f| same_component(f, &entry.component));
            let restore = st.focus_restore.clone();
            let has_pending = restore.overlay().is_some_and(|o| same_entry(o, entry));
            if !is_focused && !has_pending {
                return;
            }
            if let FocusRestore::Blocked { overlay, blocked_by, .. } = &restore
                && same_entry(overlay, entry) && st.focused.as_ref().is_some_and(|f| same_component(f, blocked_by))
            {
                st.focus_restore = match &options {
                    Some(opts) => FocusRestore::Blocked {
                        overlay: entry.clone(),
                        blocked_by: blocked_by.clone(),
                        resume: BlockedResume::FocusTarget(opts.target.clone()),
                    },
                    None => FocusRestore::Inactive,
                };
                drop(st);
                self.request_render(false);
                return;
            }
            Self::clear_focus_restore_for(&mut st, entry);
            is_focused || options.is_some()
        };
        if fallback {
            let target = match options {
                Some(o) => o.target,
                None => {
                    let top = self.topmost_visible_overlay();
                    match top {
                        Some(t) if !same_entry(&t, entry) => Some(t.component.clone()),
                        _ => entry.lock().pre_focus.clone(),
                    }
                }
            };
            self.set_focus(target);
        }
        self.request_render(false);
    }

    /// Hide the topmost overlay and restore previous focus.
    pub fn hide_overlay(&self) {
        if let Some(target) = self.redirect_target() {
            return target.hide_overlay();
        }
        let (overlay, had_focus, empty) = {
            let mut st = self.state();
            let Some(overlay) = st.overlay_stack.last().cloned() else {
                return;
            };
            Self::clear_focus_restore_for(&mut st, &overlay);
            Self::retarget_overlay_pre_focus(&st, &overlay);
            st.overlay_stack.pop();
            let had_focus = st.focused.as_ref().is_some_and(|f| same_component(f, &overlay.component));
            (overlay, had_focus, st.overlay_stack.is_empty())
        };
        if had_focus {
            let top = self.topmost_visible_overlay();
            let target = top.map(|t| t.component.clone()).or_else(|| overlay.lock().pre_focus.clone());
            self.set_focus(target);
        }
        if empty {
            self.hide_terminal_cursor();
        }
        self.request_render(false);
    }

    /// Hide the cursor while running. After stop(), the shell owns the cursor.
    pub(crate) fn hide_terminal_cursor(&self) {
        if !self.is_stopped() {
            self.terminal.hide_cursor();
        }
    }

    /// Whether any overlay is visible.
    pub fn has_overlay(&self) -> bool {
        if let Some(target) = self.redirect_target() {
            return target.has_overlay();
        }
        let stack = self.state().overlay_stack.clone();
        stack.iter().any(|o| self.is_overlay_visible(o))
    }

    pub(crate) fn has_overlay_entries(&self) -> bool {
        !self.state().overlay_stack.is_empty()
    }

    /// Whether the focused component is a visible overlay.
    pub(crate) fn is_overlay_focused(&self) -> bool {
        let (stack, focused) = {
            let st = self.state();
            (st.overlay_stack.clone(), st.focused.clone())
        };
        let Some(f) = focused else { return false };
        stack.iter().any(|e| same_component(&e.component, &f) && self.is_overlay_visible(e))
    }

    /// Keep overlay containers as keyboard focus owners when a nested control is clicked.
    pub(crate) fn resolve_mouse_focus_target(&self, component: &ComponentRef) -> ComponentRef {
        let stack = self.state().overlay_stack.clone();
        for overlay in stack.iter().rev() {
            if self.is_overlay_visible(overlay) && contains_component(&overlay.component, component) {
                return overlay.component.clone();
            }
        }
        component.clone()
    }

    /// Dispatch to the visually topmost overlay under the pointer. Returns (hit, result).
    pub(crate) fn dispatch_mouse_to_overlay(&self, event: &MouseEvent) -> (bool, Option<MouseResult>) {
        let layouts = self.state().rendered_overlay_layouts.clone();
        for layout in layouts.iter().rev() {
            if event.screen_x < layout.col
                || event.screen_x >= layout.col + layout.width
                || event.screen_y < layout.row
                || event.screen_y >= layout.row + layout.height
            {
                continue;
            }
            let local = MouseEvent {
                x: event.screen_x - layout.col,
                y: event.screen_y - layout.row,
                width: layout.width,
                height: layout.height,
                ..*event
            };
            let result = dispatch_mouse_event(&layout.entry.component, &local);
            return match result {
                Some(mut r) => {
                    if r.focus {
                        r.focus_target = Some(layout.entry.component.clone());
                    }
                    (true, Some(r))
                }
                None => (true, None),
            };
        }
        (false, None)
    }

    pub(crate) fn is_overlay_visible(&self, entry: &EntryRef) -> bool {
        if entry.lock().hidden {
            return false;
        }
        if let Some(visible) = entry.options.as_ref().and_then(|o| o.visible.clone()) {
            return visible(self.terminal.columns(), self.terminal.rows());
        }
        true
    }

    fn topmost_visible_overlay(&self) -> Option<EntryRef> {
        let stack = self.state().overlay_stack.clone();
        let mut topmost: Option<EntryRef> = None;
        for overlay in stack {
            if overlay.non_capturing() || !self.is_overlay_visible(&overlay) {
                continue;
            }
            let order = overlay.lock().focus_order;
            if topmost.as_ref().is_none_or(|t| order > t.lock().focus_order) {
                topmost = Some(overlay);
            }
        }
        topmost
    }

    /// Invalidate all mounted roots and overlays.
    pub fn invalidate(&self) {
        if let Some(target) = self.redirect_target() {
            return target.invalidate();
        }
        for root in self.mounted_roots() {
            root.invalidate();
        }
        let stack = self.state().overlay_stack.clone();
        for overlay in stack {
            overlay.component.invalidate();
        }
    }

    // ---- lifecycle ----

    /// Start the terminal and begin rendering.
    pub fn start(&self) {
        if let Some(target) = self.redirect_target() {
            return target.start();
        }
        self.state().stopped = false;
        crate::tui_main_screen::register_exit_restore(self.weak_self.clone());
        crate::marked::warm_up_in_background();
        self.before_terminal_start();
        let weak_input = self.weak_self.clone();
        let weak_resize = self.weak_self.clone();
        let lp_input = self.event_loop.clone();
        let lp_resize = self.event_loop.clone();
        self.terminal.start(
            Arc::new(move |data: String| {
                let weak = weak_input.clone();
                lp_input.post(move || {
                    if let Some(tui) = weak.upgrade() {
                        tui.handle_terminal_input(&data);
                    }
                });
            }),
            Arc::new(move || {
                let weak = weak_resize.clone();
                lp_resize.post(move || {
                    if let Some(tui) = weak.upgrade() {
                        tui.request_render(false);
                    }
                });
            }),
        );
        self.after_terminal_start();
        self.terminal.hide_cursor();
        if self.state().color_scheme_notifications {
            self.terminal.write("\x1b[?2031h");
        }
        self.query_cell_size();
        self.request_render(false);
    }

    fn before_terminal_start(&self) {
        if let Screen::Alt(alt) = &self.screen {
            alt.before_terminal_start(self);
        }
    }

    fn after_terminal_start(&self) {}

    /// Mode resets written before the terminal's own resets when exiting without stop().
    pub(crate) fn exit_restore_prefix(&self) -> String {
        let base = format!(
            "{}\x1b[?25h",
            if self.state().color_scheme_notifications { "\x1b[?2031l" } else { "" }
        );
        match &self.screen {
            Screen::Alt(alt) => alt.exit_restore_prefix(base),
            Screen::Main(_) => base,
        }
    }

    pub(crate) fn exit_restore_suffix(&self) -> String {
        match &self.screen {
            Screen::Alt(alt) => alt.exit_restore_suffix(),
            Screen::Main(_) => String::new(),
        }
    }

    /// Restore the terminal from a process exit hook when stop() did not run.
    pub fn restore_on_exit(&self) {
        if self.is_stopped() {
            return;
        }
        self.terminal.restore_on_exit(&self.exit_restore_prefix(), &self.exit_restore_suffix());
    }

    /// Add an input listener; returns its id for [`remove_input_listener`](Self::remove_input_listener).
    pub fn add_input_listener(&self, listener: InputListener) -> u64 {
        let mut st = self.state();
        let id = st.next_listener_id;
        st.next_listener_id += 1;
        st.input_listeners.push((id, listener));
        id
    }

    pub fn remove_input_listener(&self, id: u64) {
        self.state().input_listeners.retain(|(i, _)| *i != id);
    }

    /// Listen for terminal color scheme changes (DEC 2031 reports). Returns an id for removal.
    pub fn on_terminal_color_scheme_change(&self, listener: ColorSchemeListener) -> u64 {
        let mut st = self.state();
        let id = st.next_listener_id;
        st.next_listener_id += 1;
        st.color_scheme_listeners.push((id, listener));
        id
    }

    pub fn remove_terminal_color_scheme_listener(&self, id: u64) {
        self.state().color_scheme_listeners.retain(|(i, _)| *i != id);
    }

    pub fn set_terminal_color_scheme_notifications(&self, enabled: bool) {
        let write = {
            let mut st = self.state();
            if st.color_scheme_notifications == enabled {
                return;
            }
            st.color_scheme_notifications = enabled;
            !st.stopped
        };
        if write {
            self.terminal.write(if enabled { "\x1b[?2031h" } else { "\x1b[?2031l" });
        }
    }

    fn query_cell_size(&self) {
        if get_capabilities().images.is_none() {
            return;
        }
        self.terminal.write("\x1b[16t");
    }

    /// Stop rendering and restore the terminal.
    pub fn stop(&self, options: TuiStopOptions) {
        if let Some(target) = self.redirect_target() {
            return target.stop(options);
        }
        let notifications = {
            let mut st = self.state();
            st.stopped = true;
            st.color_scheme_notifications
        };
        self.cancel_render_timer();
        if notifications {
            self.terminal.write("\x1b[?2031l");
        }
        self.before_terminal_stop(options);
        self.terminal.show_cursor();
        self.terminal.stop();
        self.after_terminal_stop(options);
    }

    fn before_terminal_stop(&self, options: TuiStopOptions) {
        match &self.screen {
            Screen::Main(_) => self.main_before_terminal_stop(options),
            Screen::Alt(alt) => alt.before_terminal_stop(self, options),
        }
    }

    fn after_terminal_stop(&self, options: TuiStopOptions) {
        if let Screen::Alt(alt) = &self.screen {
            alt.after_terminal_stop(self, options);
        }
    }

    // ---- rendering ----

    fn reset_render_state(&self) {
        match &self.screen {
            Screen::Main(_) => self.main_reset_render_state(),
            Screen::Alt(alt) => alt.reset_render_state(),
        }
    }

    pub(crate) fn do_render(&self) {
        // Frames must not observe a multi-step component update from another thread half done.
        let _ui = crate::ui_lock::lock_ui();
        match &self.screen {
            Screen::Main(_) => self.main_do_render(),
            Screen::Alt(alt) => alt.do_render(self),
        }
    }

    /// Render the root components (children, or the layout root in fullscreen mode).
    pub fn render_root(&self, width: usize) -> Vec<String> {
        match &self.screen {
            Screen::Alt(alt) => alt.render_root(self, width),
            Screen::Main(_) => self.container.render_children(width),
        }
    }

    /// Whether a requested render has not run yet.
    pub fn has_pending_render(&self) -> bool {
        if let Some(target) = self.redirect_target() {
            return target.has_pending_render();
        }
        let s = self.sched();
        s.render_requested || s.immediate_scheduled
    }

    /// Render immediately (synchronously), optionally forcing a full redraw.
    pub fn render_now(&self, force: bool) {
        if let Some(target) = self.redirect_target() {
            return target.render_now(force);
        }
        if force {
            self.reset_render_state();
        }
        {
            let mut s = self.sched();
            s.render_requested = false;
            if let Some(t) = s.render_timer.take() {
                self.event_loop.clear_timer(t);
            }
            s.last_render_at = self.event_loop.now_ms();
        }
        self.do_render();
    }

    /// Request a render. Renders are coalesced and throttled to ~60fps; `force` resets the
    /// render state (full redraw) and renders on the next loop turn.
    pub fn request_render(&self, force: bool) {
        if let Some(target) = self.redirect_target() {
            return target.request_render(force);
        }
        if force {
            self.reset_render_state();
            self.request_immediate_render();
            return;
        }
        {
            let mut s = self.sched();
            if s.render_requested {
                return;
            }
            s.render_requested = true;
        }
        let weak = self.weak_self.clone();
        self.event_loop.post(move || {
            if let Some(tui) = weak.upgrade() {
                tui.schedule_render();
            }
        });
    }

    pub(crate) fn request_immediate_render(&self) {
        {
            let mut s = self.sched();
            if let Some(t) = s.render_timer.take() {
                self.event_loop.clear_timer(t);
            }
            s.render_requested = true;
            if s.immediate_scheduled {
                return;
            }
            s.immediate_scheduled = true;
        }
        let weak = self.weak_self.clone();
        self.event_loop.post(move || {
            let Some(tui) = weak.upgrade() else { return };
            {
                let mut s = tui.sched();
                s.immediate_scheduled = false;
                if !s.render_requested {
                    return;
                }
                if tui.is_stopped() {
                    return;
                }
                if let Some(t) = s.render_timer.take() {
                    tui.event_loop.clear_timer(t);
                }
                s.render_requested = false;
                s.last_render_at = tui.event_loop.now_ms();
            }
            tui.do_render();
        });
    }

    fn cancel_render_timer(&self) {
        let mut s = self.sched();
        if let Some(t) = s.render_timer.take() {
            self.event_loop.clear_timer(t);
        }
    }

    fn schedule_render(&self) {
        if self.is_stopped() {
            return;
        }
        let delay = {
            let s = self.sched();
            if s.render_timer.is_some() || !s.render_requested {
                return;
            }
            let elapsed = self.event_loop.now_ms() - s.last_render_at;
            (MIN_RENDER_INTERVAL_MS - elapsed).max(0.0)
        };
        let weak = self.weak_self.clone();
        let timer = self.event_loop.set_timeout(Duration::from_secs_f64(delay / 1000.0), move || {
            let Some(tui) = weak.upgrade() else { return };
            {
                let mut s = tui.sched();
                s.render_timer = None;
                if !s.render_requested {
                    return;
                }
                if tui.is_stopped() {
                    return;
                }
                s.render_requested = false;
                s.last_render_at = tui.event_loop.now_ms();
            }
            tui.do_render();
            if tui.sched().render_requested {
                tui.schedule_render();
            }
        });
        self.sched().render_timer = Some(timer);
    }

    // ---- input ----

    /// Process one input sequence (as delivered by the terminal).
    pub fn handle_terminal_input(&self, data: &str) {
        if self.consume_terminal_color_response(data) {
            return;
        }
        if self.consume_terminal_color_scheme_report(data) {
            return;
        }
        let mut data = data.to_string();
        let listeners: Vec<InputListener> = self.state().input_listeners.iter().map(|(_, l)| l.clone()).collect();
        if !listeners.is_empty() {
            let mut current = data.clone();
            for listener in listeners {
                if let Some(result) = listener(&current) {
                    if result.consume {
                        return;
                    }
                    if let Some(d) = result.data {
                        current = d;
                    }
                }
            }
            if current.is_empty() {
                return;
            }
            data = current;
        }

        if self.consume_cell_size_response(&data) {
            return;
        }

        let on_debug = self.on_debug.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(cb) = on_debug
            && matches_key(&data, "shift+ctrl+d")
        {
            cb();
            return;
        }

        // If the focused component is an overlay, verify it's still visible.
        let (focused, stack) = {
            let st = self.state();
            (st.focused.clone(), st.overlay_stack.clone())
        };
        let focused_overlay =
            focused.as_ref().and_then(|f| stack.iter().find(|o| same_component(&o.component, f)).cloned());
        if let Some(fo) = focused_overlay
            && !self.is_overlay_visible(&fo)
        {
            match self.topmost_visible_overlay() {
                Some(top) => self.set_focus(Some(top.component.clone())),
                None => {
                    let pre = fo.lock().pre_focus.clone();
                    self.set_focus_internal(pre, FocusRestorePolicy::Preserve);
                }
            }
        }

        let (focused, stack) = {
            let st = self.state();
            (st.focused.clone(), st.overlay_stack.clone())
        };
        let focus_is_overlay = focused.as_ref().is_some_and(|f| stack.iter().any(|o| same_component(&o.component, f)));
        if !focus_is_overlay {
            let restore = self.visible_focus_restore();
            match restore {
                FocusRestore::Eligible { overlay } => self.set_focus(Some(overlay.component.clone())),
                FocusRestore::Blocked { overlay, blocked_by, resume } => {
                    if !focused.as_ref().is_some_and(|f| same_component(f, &blocked_by)) {
                        match resume {
                            BlockedResume::RestoreOverlay => self.set_focus(Some(overlay.component.clone())),
                            BlockedResume::FocusTarget(target) => {
                                self.state().focus_restore = FocusRestore::Inactive;
                                self.set_focus(target);
                            }
                        }
                    }
                }
                FocusRestore::Inactive => {}
            }
        }

        let focused = self.state().focused.clone();
        if let Some(f) = focused
            && f.handles_input()
        {
            if is_key_release(&data) && !f.wants_key_release() {
                return;
            }
            f.handle_input(&data);
            self.request_immediate_render();
        }
    }

    fn consume_terminal_color_response(&self, data: &str) -> bool {
        let query = {
            let st = self.state();
            match st.pending_color_queries.first() {
                Some(q) => q.clone(),
                None => return false,
            }
        };
        if is_device_attributes_response(data) {
            self.state().pending_color_queries.remove(0);
            self.complete_color_query(&query);
            return true;
        }
        let Some(response) = parse_osc_color_response(data) else {
            return false;
        };
        let key = match response.target {
            OscColorTarget::Foreground => "foreground".to_string(),
            OscColorTarget::Background => "background".to_string(),
            OscColorTarget::Palette(n) => n.to_string(),
        };
        let complete = {
            let mut q = query.lock().unwrap_or_else(|e| e.into_inner());
            if q.deliver.is_none() || q.replied.contains(&key) {
                return true;
            }
            q.replied.push(key);
            match response.target {
                OscColorTarget::Foreground => q.foreground = response.rgb,
                OscColorTarget::Background => q.background = response.rgb,
                OscColorTarget::Palette(n) => {
                    if (n as usize) < TERMINAL_PALETTE_SIZE {
                        q.palette[n as usize] = response.rgb;
                    }
                }
            }
            q.replied.len() == TERMINAL_COLOR_REPLY_COUNT
        };
        if complete {
            self.complete_color_query(&query);
        }
        true
    }

    fn complete_color_query(&self, query: &Arc<Mutex<PendingColorQuery>>) {
        let (deliver, result) = {
            let mut q = query.lock().unwrap_or_else(|e| e.into_inner());
            let deliver = q.deliver.take();
            if let Some(t) = q.timer.take() {
                self.event_loop.clear_timer(t);
            }
            (deliver, q.result())
        };
        if let Some(d) = deliver {
            d(result);
        }
    }

    fn consume_terminal_color_scheme_report(&self, data: &str) -> bool {
        let Some(scheme) = parse_terminal_color_scheme_report(data) else {
            return false;
        };
        let listeners: Vec<ColorSchemeListener> =
            self.state().color_scheme_listeners.iter().map(|(_, l)| l.clone()).collect();
        for l in listeners {
            l(scheme);
        }
        true
    }

    fn consume_cell_size_response(&self, data: &str) -> bool {
        // ESC [ 6 ; height ; width t
        let Some(body) = data.strip_prefix("\x1b[6;").and_then(|b| b.strip_suffix('t')) else {
            return false;
        };
        let Some((h, w)) = body.split_once(';') else { return false };
        if h.is_empty() || w.is_empty() || !h.bytes().all(|b| b.is_ascii_digit()) || !w.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        let height_px: u64 = h.parse().unwrap_or(0);
        let width_px: u64 = w.parse().unwrap_or(0);
        if height_px == 0 || width_px == 0 {
            return true;
        }
        set_cell_dimensions(CellDimensions { width_px: width_px as u32, height_px: height_px as u32 });
        self.invalidate();
        self.request_render(false);
        true
    }

    /// Query the terminal's theme colors (OSC 10/11/4 followed by DA1).
    ///
    /// The returned value resolves when the DA1 reply or all color replies arrive, or when
    /// the timeout expires. Replies after the timeout are passed to `on_late_reply`.
    pub fn query_terminal_colors(
        &self,
        timeout: Duration,
        on_late_reply: Option<Arc<dyn Fn(TerminalColors) + Send + Sync>>,
    ) -> Deferred<TerminalColors> {
        let (resolver, deferred): (Resolver<TerminalColors>, Deferred<TerminalColors>) = event_loop::deferred();
        let r1 = resolver.clone();
        let query = Arc::new(Mutex::new(PendingColorQuery {
            foreground: None,
            background: None,
            palette: vec![None; TERMINAL_PALETTE_SIZE],
            replied: Vec::new(),
            deliver: Some(Arc::new(move |c| r1.resolve(c))),
            timer: None,
        }));
        let q2 = query.clone();
        let timer = self.event_loop.set_timeout(timeout, move || {
            let result = {
                let mut q = q2.lock().unwrap_or_else(|e| e.into_inner());
                q.timer = None;
                if q.deliver.is_none() {
                    return;
                }
                q.deliver = on_late_reply.clone().map(|f| -> Deliver { f });
                q.result()
            };
            resolver.resolve(result);
        });
        query.lock().unwrap_or_else(|e| e.into_inner()).timer = Some(timer);
        self.state().pending_color_queries.push(query);
        self.terminal.write(&terminal_color_query());
        deferred
    }

    // ---- overlay layout & compositing ----

    pub(crate) fn resolve_overlay_layout(
        options: Option<&OverlayOptions>,
        overlay_height: i64,
        term_width: i64,
        term_height: i64,
    ) -> (i64, i64, i64, Option<i64>) {
        let default = OverlayOptions::default();
        let opt = options.unwrap_or(&default);
        let margin = opt.margin.unwrap_or_default();
        let margin_top = margin.top.unwrap_or(0).max(0);
        let margin_right = margin.right.unwrap_or(0).max(0);
        let margin_bottom = margin.bottom.unwrap_or(0).max(0);
        let margin_left = margin.left.unwrap_or(0).max(0);
        let avail_width = (term_width - margin_left - margin_right).max(1);
        let avail_height = (term_height - margin_top - margin_bottom).max(1);

        let mut width = parse_size_value(opt.width.as_ref(), term_width).unwrap_or(80.min(avail_width));
        if let Some(min) = opt.min_width {
            width = width.max(min);
        }
        width = width.min(avail_width).max(1);

        let max_height = parse_size_value(opt.max_height.as_ref(), term_height).map(|m| m.min(avail_height).max(1));
        let effective_height = match max_height {
            Some(m) => overlay_height.min(m),
            None => overlay_height,
        };
        let anchor = opt.anchor.unwrap_or_default();

        let mut row = match &opt.row {
            Some(SizeValue::Percent(s)) => match parse_percent(s) {
                Some(p) => {
                    let max_row = (avail_height - effective_height).max(0);
                    margin_top + (max_row as f64 * (p / 100.0)).floor() as i64
                }
                None => resolve_anchor_row(OverlayAnchor::Center, effective_height, avail_height, margin_top),
            },
            Some(SizeValue::Abs(v)) => *v,
            None => resolve_anchor_row(anchor, effective_height, avail_height, margin_top),
        };
        let mut col = match &opt.col {
            Some(SizeValue::Percent(s)) => match parse_percent(s) {
                Some(p) => {
                    let max_col = (avail_width - width).max(0);
                    margin_left + (max_col as f64 * (p / 100.0)).floor() as i64
                }
                None => resolve_anchor_col(OverlayAnchor::Center, width, avail_width, margin_left),
            },
            Some(SizeValue::Abs(v)) => *v,
            None => resolve_anchor_col(anchor, width, avail_width, margin_left),
        };
        if let Some(dy) = opt.offset_y {
            row += dy;
        }
        if let Some(dx) = opt.offset_x {
            col += dx;
        }
        row = margin_top.max(row.min(term_height - margin_bottom - effective_height));
        col = margin_left.max(col.min(term_width - margin_right - width));
        (width, row, col, max_height)
    }

    /// Composite all overlays into content lines (sorted by focus order, higher on top).
    pub(crate) fn composite_overlays(&self, lines: Vec<String>, term_width: usize, term_height: usize) -> Vec<String> {
        let stack = self.state().overlay_stack.clone();
        if stack.is_empty() {
            self.state().rendered_overlay_layouts.clear();
            return lines;
        }
        let mut result = lines;
        for entry in &stack {
            entry.lock().bounds = None;
        }
        let show_hw = self.get_show_hardware_cursor();
        let mut visible: Vec<EntryRef> = stack.iter().filter(|e| self.is_overlay_visible(e)).cloned().collect();
        visible.sort_by_key(|e| e.lock().focus_order);
        let mut rendered: Vec<(EntryRef, Vec<String>, i64, i64, i64)> = Vec::new();
        let mut min_lines_needed = result.len() as i64;
        let tw = term_width as i64;
        let th = term_height as i64;
        for entry in &visible {
            let (width, _, _, max_height) = Self::resolve_overlay_layout(entry.options.as_ref(), 0, tw, th);
            let mut overlay_lines = resolve_fake_cursors(entry.component.render(width.max(0) as usize), show_hw);
            if let Some(mh) = max_height
                && overlay_lines.len() as i64 > mh
            {
                overlay_lines.truncate(mh.max(0) as usize);
            }
            let (_, row, col, _) =
                Self::resolve_overlay_layout(entry.options.as_ref(), overlay_lines.len() as i64, tw, th);
            entry.lock().bounds = Some(OverlayBounds { row, col, width, height: overlay_lines.len() as i64 });
            min_lines_needed = min_lines_needed.max(row + overlay_lines.len() as i64);
            rendered.push((entry.clone(), overlay_lines, row, col, width));
        }
        self.state().rendered_overlay_layouts = rendered
            .iter()
            .map(|(entry, lines, row, col, w)| RenderedOverlayLayout {
                entry: entry.clone(),
                row: *row,
                col: *col,
                width: *w,
                height: lines.len() as i64,
            })
            .collect();

        let working_height = (result.len() as i64).max(th).max(min_lines_needed);
        while (result.len() as i64) < working_height {
            result.push(String::new());
        }
        let viewport_start = (working_height - th).max(0);
        for (_, overlay_lines, row, col, w) in &rendered {
            for (i, ol) in overlay_lines.iter().enumerate() {
                let idx = viewport_start + row + i as i64;
                if idx >= 0 && (idx as usize) < result.len() {
                    let w_us = (*w).max(0) as usize;
                    let truncated =
                        if visible_width(ol) > w_us { slice_by_column(ol, 0, w_us, true) } else { ol.clone() };
                    let idx = idx as usize;
                    result[idx] = composite_tui_line(&result[idx], &truncated, (*col).max(0) as usize, w_us, term_width);
                }
            }
        }
        result
    }
}

fn resolve_anchor_row(anchor: OverlayAnchor, height: i64, avail_height: i64, margin_top: i64) -> i64 {
    use OverlayAnchor::*;
    match anchor {
        TopLeft | TopCenter | TopRight => margin_top,
        BottomLeft | BottomCenter | BottomRight => margin_top + avail_height - height,
        LeftCenter | Center | RightCenter => margin_top + (avail_height - height).div_euclid(2),
    }
}

fn resolve_anchor_col(anchor: OverlayAnchor, width: i64, avail_width: i64, margin_left: i64) -> i64 {
    use OverlayAnchor::*;
    match anchor {
        TopLeft | LeftCenter | BottomLeft => margin_left,
        TopRight | RightCenter | BottomRight => margin_left + avail_width - width,
        TopCenter | Center | BottomCenter => margin_left + (avail_width - width).div_euclid(2),
    }
}

/// Whether `target` is `root` or nested inside it (through container children).
pub fn contains_component(root: &ComponentRef, target: &ComponentRef) -> bool {
    if same_component(root, target) {
        return true;
    }
    match root.child_components() {
        Some(children) => children.iter().any(|c| contains_component(c, target)),
        None => false,
    }
}

impl Component for Tui {
    fn render(&self, width: usize) -> Vec<String> {
        self.render_root(width)
    }
    fn invalidate(&self) {
        Tui::invalidate(self);
    }
    fn handle_mouse(&self, event: &MouseEvent) -> Option<MouseResult> {
        self.container.handle_mouse_with_owner(event, None)
    }
    fn child_components(&self) -> Option<Vec<ComponentRef>> {
        Some(self.container.children())
    }
    fn uses_container_mouse(&self) -> bool {
        true
    }
}
