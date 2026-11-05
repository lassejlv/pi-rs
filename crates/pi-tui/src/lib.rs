//! Terminal UI framework with differential rendering (Rust port of `@earendil-works/pi-tui`).
//!
//! The port emits the same escape sequences, uses the same render algorithm and handles keys
//! the same way as the TypeScript package.
//!
//! # Ownership and threading
//!
//! Components are shared trait objects: [`tui::ComponentRef`] is `Arc<dyn Component>`.
//! Every [`tui::Component`] method takes `&self`, and components keep mutable state behind
//! interior mutexes. The trait is object safe and `Send + Sync`. A container keeps
//! `ComponentRef`s to its children, and the [`tui::Tui`] keeps the root children, the overlay
//! stack and the focused component. Use [`tui::same_component`] to compare components by
//! identity and [`tui::downcast_component`] to get the concrete type back. Constructors that
//! need a back reference (such as `Editor::new(&Arc<Tui>, ..)`) take an `Arc` and keep a
//! `Weak`. A component must not hold its own lock while it calls user callbacks or the TUI,
//! because callbacks may call back into the component.
//!
//! # Render loop
//!
//! The TS package depends on the Node.js event loop. The Rust port uses
//! [`event_loop::EventLoop`] for this: it serializes input handling, timers and render
//! scheduling. [`tui::Tui::main_screen`] (inline, scrollback-preserving) and
//! [`tui::Tui::alt_screen`] (fullscreen, with mouse selection, search and wheel scrolling)
//! create a TUI over any [`terminal::Terminal`], such as [`terminal::ProcessTerminal`] or
//! [`testing::VirtualTerminal`]. Call `start()`, and then `request_render(false)` after any
//! state change. Frames are throttled and diffed against the previous frame. Input from the
//! terminal is posted to the loop and sent to the focused component (`handle_input`), or to
//! `handle_mouse` in alt-screen mode. Tests use a manual loop with a virtual clock
//! ([`event_loop::EventLoop::new_manual`]).
//!
//! # Keys and theming
//!
//! [`keys::matches_key`] and [`keys::parse_key`] decode legacy, xterm modifyOtherKeys and
//! Kitty CSI-u sequences. Named actions (`tui.editor.cursorUp`, ...) are resolved through
//! [`keybindings::get_keybindings`]. Theme hooks are [`StyleFn`] closures
//! (`Arc<dyn Fn(&str) -> String + Send + Sync>`).

pub mod alt_screen_search;
pub mod autocomplete;
pub mod collation;
pub mod colors;
pub mod components;
pub mod event_loop;
pub mod fuzzy;
pub mod keybindings;
pub mod keys;
pub mod kill_ring;
pub mod latex;
pub mod layout;
pub mod layout_node;
pub mod marked;
pub mod native_platform;
pub mod oklab;
pub mod program_status;
pub mod stdin_buffer;
pub mod terminal;
pub mod terminal_colors;
pub mod terminal_image;
pub mod testing;
pub mod tui;
pub mod tui_alt_screen;
pub mod tui_main_screen;
pub mod ui_lock;
pub mod undo_stack;
#[rustfmt::skip]
mod unicode_tables;
pub mod utils;
pub mod wheel_scroll;
pub mod word_navigation;

/// A styling function, e.g. a theme color: takes text and returns it wrapped in ANSI codes.
pub type StyleFn = std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>;
