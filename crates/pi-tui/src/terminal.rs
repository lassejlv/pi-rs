//! Terminal abstraction and the real process terminal. Port of `packages/tui/src/terminal.ts`.

use std::sync::Arc;
use std::time::Duration;

use crate::program_status::ProgramStatus;

const NATIVE_SHIFT_ENTER_SEQUENCE: &str = "\x1b[13;2u";

/// Called with each complete input sequence (key, paste, mouse event, terminal reply).
pub type InputHandler = Arc<dyn Fn(String) + Send + Sync>;
/// Called when the terminal was resized.
pub type ResizeHandler = Arc<dyn Fn() + Send + Sync>;

/// A sequence that answers the keyboard protocol negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardProtocolNegotiationSequence {
    KittyFlags(u32),
    DeviceAttributes,
}

/// Parse a Kitty flags reply (`CSI ? flags u`) or a DA1 reply (`CSI ? ... c`).
pub fn parse_keyboard_protocol_negotiation_sequence(sequence: &str) -> Option<KeyboardProtocolNegotiationSequence> {
    let body = sequence.strip_prefix("\x1b[?")?;
    if let Some(digits) = body.strip_suffix('u')
        && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
    {
        return Some(KeyboardProtocolNegotiationSequence::KittyFlags(digits.parse().unwrap_or(u32::MAX)));
    }
    if let Some(params) = body.strip_suffix('c')
        && params.bytes().all(|b| b.is_ascii_digit() || b == b';')
    {
        return Some(KeyboardProtocolNegotiationSequence::DeviceAttributes);
    }
    None
}

fn is_keyboard_protocol_negotiation_sequence_prefix(sequence: &str) -> bool {
    if sequence == "\x1b[" {
        return true;
    }
    match sequence.strip_prefix("\x1b[?") {
        Some(rest) => rest.bytes().all(|b| b.is_ascii_digit() || b == b';'),
        None => false,
    }
}

/// Descriptor of the process's real stdout for [`Terminal::restore_on_exit`]. Node's
/// `restoreOnExit` writes with `fs.writeSync(process.stdout.fd)`, which bypasses an app that
/// redirects `process.stdout.write`; apps that move fd 1 (pi's stdout takeover) register the
/// descriptor holding the real stdout here.
#[cfg_attr(not(unix), allow(dead_code))]
static EXIT_RESTORE_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);

/// Set the descriptor exit-time terminal restoration writes to (1 by default).
pub fn set_exit_restore_fd(fd: i32) {
    EXIT_RESTORE_FD.store(fd, std::sync::atomic::Ordering::SeqCst);
}

/// Called when the process's stdin reaches end of file while a `ProcessTerminal` reads it
/// (Node's `process.stdin` `"end"` event).
pub type StdinEndHandler = Arc<dyn Fn() + Send + Sync>;

static STDIN_END_HANDLER: std::sync::Mutex<Option<StdinEndHandler>> = std::sync::Mutex::new(None);

/// Install (or clear) the handler run when stdin reaches end of file during terminal input.
pub fn set_stdin_end_handler(handler: Option<StdinEndHandler>) {
    *STDIN_END_HANDLER.lock().unwrap_or_else(|e| e.into_inner()) = handler;
}

/// Called when reading the process's stdin fails while a `ProcessTerminal` reads it (Node's
/// `process.stdin` `"error"` event), e.g. `EIO` for an orphaned background process group or
/// `ENOTTY` for a revoked tty. The reader stops after the handler returns.
pub type StdinErrorHandler = Arc<dyn Fn(&std::io::Error) + Send + Sync>;

static STDIN_ERROR_HANDLER: std::sync::Mutex<Option<StdinErrorHandler>> = std::sync::Mutex::new(None);

/// Install (or clear) the handler run when reading stdin fails during terminal input.
pub fn set_stdin_error_handler(handler: Option<StdinErrorHandler>) {
    *STDIN_ERROR_HANDLER.lock().unwrap_or_else(|e| e.into_inner()) = handler;
}

/// Report a stdin read error to the [`set_stdin_error_handler`] handler. Returns whether a
/// handler was installed.
pub fn notify_stdin_error(error: &std::io::Error) -> bool {
    let handler = STDIN_ERROR_HANDLER.lock().unwrap_or_else(|e| e.into_inner()).clone();
    match handler {
        Some(handler) => {
            handler(error);
            true
        }
        None => false,
    }
}

/// Called when writing terminal output (stdout/stderr) fails (Node's `process.stdout` /
/// `process.stderr` `"error"` event), e.g. `EIO` after a hangup or `EPIPE`/`ENOTCONN` for a
/// closed pty. The write is abandoned after the handler returns.
pub type OutputErrorHandler = Arc<dyn Fn(&std::io::Error) + Send + Sync>;

static OUTPUT_ERROR_HANDLER: std::sync::Mutex<Option<OutputErrorHandler>> = std::sync::Mutex::new(None);

/// Install (or clear) the handler run when writing terminal output fails.
pub fn set_output_error_handler(handler: Option<OutputErrorHandler>) {
    *OUTPUT_ERROR_HANDLER.lock().unwrap_or_else(|e| e.into_inner()) = handler;
}

/// Report a terminal output write error to the [`set_output_error_handler`] handler. Returns
/// whether a handler was installed.
pub fn notify_output_error(error: &std::io::Error) -> bool {
    let handler = OUTPUT_ERROR_HANDLER.lock().unwrap_or_else(|e| e.into_inner()).clone();
    match handler {
        Some(handler) => {
            handler(error);
            true
        }
        None => false,
    }
}

#[cfg_attr(not(unix), allow(dead_code))]
fn notify_stdin_end() {
    let handler = STDIN_END_HANDLER.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(handler) = handler {
        handler();
    }
}

/// Whether we run in macOS Terminal.app.
pub fn is_apple_terminal_session() -> bool {
    cfg!(target_os = "macos") && std::env::var("TERM_PROGRAM").as_deref() == Ok("Apple_Terminal")
}

/// Refresh terminal dimensions by sending SIGWINCH to this process (best-effort).
pub fn refresh_terminal_dimensions() {
    #[cfg(unix)]
    unsafe {
        let pid = libc::getpid();
        if pid > 0 {
            libc::kill(pid, libc::SIGWINCH);
        }
    }
}

pub fn normalize_native_shift_enter_input(data: &str, should_detect: bool, is_shift_pressed: bool) -> String {
    if should_detect && data == "\r" && is_shift_pressed {
        return NATIVE_SHIFT_ENTER_SEQUENCE.to_string();
    }
    data.to_string()
}

pub fn normalize_apple_terminal_input(data: &str, is_apple_terminal: bool, is_shift_pressed: bool) -> String {
    normalize_native_shift_enter_input(data, is_apple_terminal, is_shift_pressed)
}

/// Minimal terminal interface for the TUI.
pub trait Terminal: Send + Sync {
    /// Start the terminal with input and resize handlers.
    fn start(&self, on_input: InputHandler, on_resize: ResizeHandler);
    /// Stop the terminal and restore its state.
    fn stop(&self);
    /// Synchronously restore terminal modes when the process exits without `stop()`.
    /// Writes `before`, then this terminal's own mode resets, then `after`.
    fn restore_on_exit(&self, _before: &str, _after: &str) {}
    /// Drain stdin before exiting so Kitty key release events do not leak to the shell.
    fn drain_input(&self, _max: Duration, _idle: Duration) {}
    /// Write output to the terminal.
    fn write(&self, data: &str);
    fn columns(&self) -> usize;
    fn rows(&self) -> usize;
    /// Whether the Kitty keyboard protocol is active.
    fn kitty_protocol_active(&self) -> bool;
    /// Move the cursor up (negative) or down (positive) by N lines.
    fn move_by(&self, lines: i64) {
        if lines > 0 {
            self.write(&format!("\x1b[{lines}B"));
        } else if lines < 0 {
            self.write(&format!("\x1b[{}A", -lines));
        }
    }
    fn hide_cursor(&self) {
        self.write("\x1b[?25l");
    }
    fn show_cursor(&self) {
        self.write("\x1b[?25h");
    }
    fn clear_line(&self) {
        self.write("\x1b[K");
    }
    fn clear_from_cursor(&self) {
        self.write("\x1b[J");
    }
    fn clear_screen(&self) {
        self.write("\x1b[2J\x1b[H");
    }
    fn set_title(&self, title: &str) {
        self.write(&format!("\x1b]0;{title}\x07"));
    }
    /// Progress indicator (OSC 9;4).
    fn set_progress(&self, _active: bool) {}
    /// Report what the program is doing (OSC 7501).
    fn set_program_status(&self, _status: &ProgramStatus) {}
}

const DEFAULT_ESCAPE_TIMEOUT_MS: f64 = 10.0;
const DEFAULT_SSH_ESCAPE_TIMEOUT_MS: f64 = 100.0;

/// How long to wait for the rest of an escape sequence before dispatching a lone ESC.
/// `get_env` looks up environment variables (use [`std::env::var`] for the process env).
pub fn resolve_escape_timeout_ms(get_env: &dyn Fn(&str) -> Option<String>) -> f64 {
    if let Some(v) = get_env("PI_TUI_ESC_TIMEOUT") {
        let t = v.trim();
        let configured = if t.is_empty() { 0.0 } else { t.parse::<f64>().unwrap_or(f64::NAN) };
        if configured.is_finite() && configured > 0.0 {
            return configured;
        }
    }
    let set = |k: &str| get_env(k).is_some_and(|v| !v.is_empty());
    if set("SSH_CONNECTION") || set("SSH_TTY") {
        return DEFAULT_SSH_ESCAPE_TIMEOUT_MS;
    }
    DEFAULT_ESCAPE_TIMEOUT_MS
}

#[cfg(unix)]
pub use process::ProcessTerminal;

#[cfg(unix)]
mod process {
    use super::*;
    use crate::keys::set_kitty_protocol_active;
    use crate::program_status::{
        PROGRAM_STATUS_QUERY, ProgramState, format_program_status, is_program_status_reply,
    };
    use crate::stdin_buffer::{StdinBuffer, StdinBufferOptions, StdinEvent};
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
    use std::sync::{Mutex, MutexGuard};
    use std::thread::JoinHandle;
    use std::time::Instant;

    const TERMINAL_PROGRESS_KEEPALIVE: Duration = Duration::from_millis(1000);
    const TERMINAL_PROGRESS_ACTIVE_SEQUENCE: &str = "\x1b]9;4;3\x07";
    const TERMINAL_PROGRESS_CLEAR_SEQUENCE: &str = "\x1b]9;4;0\x07";
    const DESIRED_KITTY_KEYBOARD_PROTOCOL_FLAGS: u32 = 7;
    const KEYBOARD_PROTOCOL_RESPONSE_FRAGMENT_TIMEOUT: Duration = Duration::from_millis(150);
    const DEVICE_ATTRIBUTES_QUERY: &str = "\x1b[c";

    fn kitty_keyboard_protocol_query() -> String {
        format!("\x1b[>{DESIRED_KITTY_KEYBOARD_PROTOCOL_FLAGS}u\x1b[?u")
    }

    static WINCH_WRITE_FD: AtomicI32 = AtomicI32::new(-1);
    static WINCH_READ_FD: AtomicI32 = AtomicI32::new(-1);

    /// The SIGWINCH handler installed before ours (e.g. pi-jsrt's for an extension's
    /// `process.on('SIGWINCH')`), chained like libuv chains to a previous handler.
    static PREVIOUS_WINCH: std::sync::OnceLock<libc::sigaction> = std::sync::OnceLock::new();

    extern "C" fn on_sigwinch(sig: libc::c_int, info: *mut libc::siginfo_t, uctx: *mut libc::c_void) {
        let fd = WINCH_WRITE_FD.load(Ordering::Relaxed);
        if fd >= 0 {
            let b = [1u8];
            unsafe {
                libc::write(fd, b.as_ptr() as *const libc::c_void, 1);
            }
        }
        if let Some(old) = PREVIOUS_WINCH.get()
            && old.sa_sigaction != libc::SIG_DFL
            && old.sa_sigaction != libc::SIG_IGN
        {
            // SAFETY: calling the previously installed handler with the convention its flags declare.
            unsafe {
                if old.sa_flags & libc::SA_SIGINFO != 0 {
                    let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) = std::mem::transmute(old.sa_sigaction);
                    f(sig, info, uctx);
                } else {
                    let f: extern "C" fn(libc::c_int) = std::mem::transmute(old.sa_sigaction);
                    f(sig);
                }
            }
        }
    }

    fn make_pipe() -> Option<(i32, i32)> {
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return None;
        }
        for fd in fds {
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        Some((fds[0], fds[1]))
    }

    fn install_sigwinch_handler() -> i32 {
        let existing = WINCH_READ_FD.load(Ordering::SeqCst);
        if existing >= 0 {
            return existing;
        }
        let Some((r, w)) = make_pipe() else { return -1 };
        WINCH_WRITE_FD.store(w, Ordering::SeqCst);
        WINCH_READ_FD.store(r, Ordering::SeqCst);
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_sigwinch as *const () as usize;
            action.sa_flags = libc::SA_RESTART | libc::SA_SIGINFO;
            libc::sigemptyset(&mut action.sa_mask);
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(libc::SIGWINCH, std::ptr::null(), &mut old) == 0 {
                let _ = PREVIOUS_WINCH.set(old);
            }
            libc::sigaction(libc::SIGWINCH, &action, std::ptr::null_mut());
        }
        r
    }

    fn drain_fd(fd: i32) {
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }
        }
    }

    /// Write all bytes to a file descriptor, retrying on EINTR/EAGAIN.
    pub(crate) fn write_all_fd(fd: i32, data: &[u8]) {
        let mut off = 0;
        while off < data.len() {
            let n = unsafe { libc::write(fd, data[off..].as_ptr() as *const libc::c_void, data.len() - off) };
            if n > 0 {
                off += n as usize;
                continue;
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => {
                    let mut pfd = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
                    unsafe { libc::poll(&mut pfd, 1, 100) };
                }
                _ => {
                    // Like a Node stream "error" event on stdout/stderr.
                    if n < 0 && (fd == libc::STDOUT_FILENO || fd == libc::STDERR_FILENO) {
                        super::notify_output_error(&err);
                    }
                    return;
                }
            }
        }
    }

    fn write_log_path_from_env() -> Option<PathBuf> {
        let env = std::env::var("PI_TUI_WRITE_LOG").unwrap_or_default();
        if env.is_empty() {
            return None;
        }
        let p = PathBuf::from(&env);
        if p.is_dir() {
            let now = chrono_like_timestamp();
            return Some(p.join(format!("tui-{now}-{}.log", std::process::id())));
        }
        Some(p)
    }

    fn chrono_like_timestamp() -> String {
        // Local time formatted as YYYY-MM-DD_HH-MM-SS.
        unsafe {
            let t = libc::time(std::ptr::null_mut());
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&t, &mut tm);
            format!(
                "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
                tm.tm_year + 1900,
                tm.tm_mon + 1,
                tm.tm_mday,
                tm.tm_hour,
                tm.tm_min,
                tm.tm_sec
            )
        }
    }

    #[derive(Default)]
    struct State {
        saved_termios: Option<libc::termios>,
        input_handler: Option<InputHandler>,
        resize_handler: Option<ResizeHandler>,
        kitty_active: bool,
        modify_other_keys_active: bool,
        keyboard_protocol_pushed: bool,
        pending_keyboard_protocol_device_attributes: u32,
        negotiation_buffer: String,
        negotiation_deadline: Option<Instant>,
        stdin_buffer: Option<StdinBuffer>,
        program_status: Option<ProgramStatus>,
        program_status_supported: bool,
        program_status_query_pending: bool,
        progress: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
        reader: Option<(i32, JoinHandle<()>)>,
    }

    struct Inner {
        state: Mutex<State>,
        write_log_path: Option<PathBuf>,
        last_data_ms: AtomicU64,
        epoch: Instant,
    }

    /// Real terminal using the process's stdin/stdout (Unix).
    ///
    /// `start()` puts stdin into raw mode, enables bracketed paste, negotiates the Kitty
    /// keyboard protocol (falling back to xterm modifyOtherKeys), and spawns a reader thread
    /// that splits input into sequences and calls the input handler from that thread.
    #[derive(Clone)]
    pub struct ProcessTerminal {
        inner: Arc<Inner>,
    }

    impl Default for ProcessTerminal {
        fn default() -> Self {
            Self::new()
        }
    }

    enum Negotiation {
        Pending,
        Parsed(KeyboardProtocolNegotiationSequence, String),
    }

    impl ProcessTerminal {
        pub fn new() -> Self {
            Self {
                inner: Arc::new(Inner {
                    state: Mutex::new(State::default()),
                    write_log_path: write_log_path_from_env(),
                    last_data_ms: AtomicU64::new(0),
                    epoch: Instant::now(),
                }),
            }
        }

        fn lock(&self) -> MutexGuard<'_, State> {
            self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
        }

        fn raw_write(&self, data: &str) {
            write_all_fd(libc::STDOUT_FILENO, data.as_bytes());
        }

        /// Whether xterm modifyOtherKeys mode was enabled as a Kitty fallback.
        pub fn modify_other_keys_active(&self) -> bool {
            self.lock().modify_other_keys_active
        }

        fn enable_raw_mode(state: &mut State) {
            unsafe {
                let mut t: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
                    return;
                }
                if state.saved_termios.is_none() {
                    state.saved_termios = Some(t);
                }
                let mut raw = t;
                raw.c_iflag &= !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
                raw.c_oflag |= libc::ONLCR;
                raw.c_cflag |= libc::CS8;
                raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
                raw.c_cc[libc::VMIN] = 1;
                raw.c_cc[libc::VTIME] = 0;
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &raw);
            }
        }

        fn restore_raw_mode(state: &mut State) {
            if let Some(t) = state.saved_termios.take() {
                unsafe {
                    libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &t);
                }
            }
        }

        fn write_program_status(&self, state: &State) {
            if state.program_status_supported
                && let Some(status) = &state.program_status
            {
                self.raw_write(&format_program_status(status));
            }
        }

        fn enable_modify_other_keys(&self, state: &mut State) {
            if state.kitty_active || state.modify_other_keys_active {
                return;
            }
            self.raw_write("\x1b[>4;2m");
            state.modify_other_keys_active = true;
        }

        fn disable_modify_other_keys(&self, state: &mut State) {
            if !state.modify_other_keys_active {
                return;
            }
            self.raw_write("\x1b[>4;0m");
            state.modify_other_keys_active = false;
        }

        fn handle_negotiation(&self, state: &mut State, seq: KeyboardProtocolNegotiationSequence) -> bool {
            state.negotiation_buffer.clear();
            state.negotiation_deadline = None;
            if seq == KeyboardProtocolNegotiationSequence::DeviceAttributes {
                if state.pending_keyboard_protocol_device_attributes == 0 {
                    return false;
                }
                state.pending_keyboard_protocol_device_attributes -= 1;
                if state.pending_keyboard_protocol_device_attributes == 0 {
                    state.program_status_query_pending = false;
                }
            }
            if let KeyboardProtocolNegotiationSequence::KittyFlags(flags) = seq {
                if flags != 0 {
                    self.disable_modify_other_keys(state);
                    if !state.kitty_active {
                        state.kitty_active = true;
                        set_kitty_protocol_active(true);
                    }
                } else {
                    self.enable_modify_other_keys(state);
                }
                return true;
            }
            if !state.kitty_active {
                self.enable_modify_other_keys(state);
            }
            true
        }

        fn read_negotiation(&self, state: &mut State, sequence: &str, forward: &mut Vec<String>) -> Option<Negotiation> {
            if !state.negotiation_buffer.is_empty() {
                let buffered = format!("{}{}", state.negotiation_buffer, sequence);
                if let Some(parsed) = parse_keyboard_protocol_negotiation_sequence(&buffered) {
                    state.negotiation_buffer.clear();
                    state.negotiation_deadline = None;
                    return Some(Negotiation::Parsed(parsed, buffered));
                }
                if is_keyboard_protocol_negotiation_sequence_prefix(&buffered) {
                    state.negotiation_buffer = buffered;
                    state.negotiation_deadline = None;
                    return Some(Negotiation::Pending);
                }
                forward.push(std::mem::take(&mut state.negotiation_buffer));
                state.negotiation_deadline = None;
            }
            if let Some(parsed) = parse_keyboard_protocol_negotiation_sequence(sequence) {
                return Some(Negotiation::Parsed(parsed, sequence.to_string()));
            }
            if is_keyboard_protocol_negotiation_sequence_prefix(sequence) {
                state.negotiation_buffer = sequence.to_string();
                state.negotiation_deadline = None;
                return Some(Negotiation::Pending);
            }
            None
        }

        /// Process stdin buffer events under the state lock; returns sequences to forward.
        fn handle_events(&self, state: &mut State, events: Vec<StdinEvent>) -> Vec<String> {
            let mut forward = Vec::new();
            for event in events {
                match event {
                    StdinEvent::Paste(content) => forward.push(format!("\x1b[200~{content}\x1b[201~")),
                    StdinEvent::Data(sequence) => {
                        if is_program_status_reply(&sequence) {
                            if state.program_status_query_pending {
                                state.program_status_query_pending = false;
                                state.program_status_supported = true;
                                self.write_program_status(state);
                            }
                            continue;
                        }
                        match self.read_negotiation(state, &sequence, &mut forward) {
                            Some(Negotiation::Pending) => {
                                if state.negotiation_deadline.is_none() {
                                    state.negotiation_deadline =
                                        Some(Instant::now() + KEYBOARD_PROTOCOL_RESPONSE_FRAGMENT_TIMEOUT);
                                }
                            }
                            Some(Negotiation::Parsed(parsed, full)) => {
                                if !self.handle_negotiation(state, parsed) {
                                    forward.push(full);
                                }
                            }
                            None => forward.push(sequence),
                        }
                    }
                }
            }
            forward
        }

        fn forward(&self, sequences: Vec<String>) {
            if sequences.is_empty() {
                return;
            }
            let handler = self.lock().input_handler.clone();
            if let Some(handler) = handler {
                for sequence in sequences {
                    // Apple Terminal (and Windows consoles) send "\r" for Shift+Enter too; ask
                    // the system whether Shift is held.
                    let should_detect = sequence == "\r" && (is_apple_terminal_session() || cfg!(windows));
                    let shift = should_detect
                        && crate::native_platform::is_native_modifier_pressed(crate::native_platform::ModifierKey::Shift);
                    let input = normalize_native_shift_enter_input(&sequence, should_detect, shift);
                    handler(input);
                }
            }
        }

        fn reader_loop(self, wake_fd: i32) {
            let winch_fd = WINCH_READ_FD.load(Ordering::SeqCst);
            let mut carry: Vec<u8> = Vec::new();
            let mut buf = vec![0u8; 8192];
            loop {
                let timeout_ms: i32 = {
                    let state = self.lock();
                    let mut deadline: Option<Instant> = state.stdin_buffer.as_ref().and_then(|b| b.deadline());
                    if let Some(nd) = state.negotiation_deadline {
                        deadline = Some(deadline.map_or(nd, |d| d.min(nd)));
                    }
                    match deadline {
                        None => -1,
                        Some(d) => d.saturating_duration_since(Instant::now()).as_millis().min(i32::MAX as u128) as i32,
                    }
                };
                let mut fds = [
                    libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 },
                    libc::pollfd { fd: wake_fd, events: libc::POLLIN, revents: 0 },
                    libc::pollfd { fd: winch_fd, events: libc::POLLIN, revents: 0 },
                ];
                let nfds = if winch_fd >= 0 { 3 } else { 2 };
                let r = unsafe { libc::poll(fds.as_mut_ptr(), nfds, timeout_ms) };
                if r < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    break;
                }
                if fds[1].revents != 0 {
                    drain_fd(wake_fd);
                    break;
                }
                if nfds == 3 && fds[2].revents != 0 {
                    drain_fd(winch_fd);
                    let handler = self.lock().resize_handler.clone();
                    if let Some(h) = handler {
                        h();
                    }
                }
                let now = Instant::now();
                // Timeouts.
                let mut forward = Vec::new();
                {
                    let mut state = self.lock();
                    if state.negotiation_deadline.is_some_and(|d| now >= d) {
                        state.negotiation_deadline = None;
                        if !state.negotiation_buffer.is_empty() {
                            forward.push(std::mem::take(&mut state.negotiation_buffer));
                        }
                    }
                    let events = match state.stdin_buffer.as_mut() {
                        Some(b) => b.poll_timeout(now),
                        None => Vec::new(),
                    };
                    let mut more = self.handle_events(&mut state, events);
                    forward.append(&mut more);
                }
                self.forward(forward);

                // POLLNVAL: macOS poll() rejects device files such as /dev/null; read() then reports EOF
                // instead of the loop spinning on an instantly-ready descriptor.
                if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                    let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                    if n == 0 {
                        // EOF: stop polling stdin.
                        notify_stdin_end();
                        break;
                    }
                    if n < 0 {
                        let err = std::io::Error::last_os_error();
                        if matches!(err.raw_os_error(), Some(libc::EINTR) | Some(libc::EAGAIN)) {
                            continue;
                        }
                        notify_stdin_error(&err);
                        break;
                    }
                    let ms = self.inner.epoch.elapsed().as_millis() as u64;
                    self.inner.last_data_ms.store(ms, Ordering::SeqCst);
                    carry.extend_from_slice(&buf[..n as usize]);
                    // Keep an incomplete trailing UTF-8 sequence for the next read.
                    let valid_up_to = match std::str::from_utf8(&carry) {
                        Ok(_) => carry.len(),
                        Err(e) => {
                            if e.error_len().is_none() {
                                e.valid_up_to()
                            } else {
                                carry.len()
                            }
                        }
                    };
                    let chunk: Vec<u8> = carry.drain(..valid_up_to).collect();
                    if chunk.is_empty() {
                        continue;
                    }
                    let forward = {
                        let mut state = self.lock();
                        if state.input_handler.is_none() {
                            // Draining: discard input.
                            Vec::new()
                        } else {
                            let events = match state.stdin_buffer.as_mut() {
                                Some(b) => {
                                    if chunk.len() == 1 && chunk[0] > 127 {
                                        b.process_bytes(&chunk)
                                    } else {
                                        b.process(&String::from_utf8_lossy(&chunk))
                                    }
                                }
                                None => Vec::new(),
                            };
                            self.handle_events(&mut state, events)
                        }
                    };
                    self.forward(forward);
                }
            }
        }

        fn clear_progress(state: &mut State) -> bool {
            if let Some((stop, handle)) = state.progress.take() {
                stop.store(true, Ordering::SeqCst);
                handle.thread().unpark();
                return true;
            }
            false
        }
    }

    impl Terminal for ProcessTerminal {
        fn start(&self, on_input: InputHandler, on_resize: ResizeHandler) {
            let mut state = self.lock();
            state.input_handler = Some(on_input);
            state.resize_handler = Some(on_resize);
            Self::enable_raw_mode(&mut state);
            self.raw_write("\x1b[?2004h");
            install_sigwinch_handler();
            refresh_terminal_dimensions();

            // Keyboard protocol negotiation.
            let escape_timeout = resolve_escape_timeout_ms(&|k| std::env::var(k).ok());
            state.stdin_buffer = Some(StdinBuffer::new(StdinBufferOptions {
                timeout: None,
                escape_timeout: Some(Duration::from_secs_f64(escape_timeout / 1000.0)),
            }));
            state.keyboard_protocol_pushed = true;
            state.pending_keyboard_protocol_device_attributes += 1;
            state.negotiation_buffer.clear();
            state.negotiation_deadline = None;
            let override_ = std::env::var("PI_PROGRAM_STATUS").ok();
            state.program_status_supported = override_.as_deref() == Some("1");
            state.program_status_query_pending = !matches!(override_.as_deref(), Some("1") | Some("0"));
            let query = if state.program_status_query_pending { PROGRAM_STATUS_QUERY } else { "" };
            self.raw_write(&format!("{}{query}{DEVICE_ATTRIBUTES_QUERY}", kitty_keyboard_protocol_query()));
            self.write_program_status(&state);

            if state.reader.is_none()
                && let Some((r, w)) = make_pipe()
            {
                let me = self.clone();
                let handle = std::thread::Builder::new()
                    .name("pi-tui-stdin".into())
                    .spawn(move || {
                        crate::event_loop::run_thread_guarded(|| me.reader_loop(r));
                        unsafe { libc::close(r) };
                    })
                    .expect("spawn stdin reader");
                state.reader = Some((w, handle));
            }
        }

        fn stop(&self) {
            let reader = {
                let mut state = self.lock();
                if Self::clear_progress(&mut state) {
                    self.raw_write(TERMINAL_PROGRESS_CLEAR_SEQUENCE);
                }
                if state.program_status_supported && state.program_status.is_some() {
                    self.raw_write(&format_program_status(&ProgramStatus::new(ProgramState::Clear)));
                }
                state.program_status_supported = false;
                state.program_status_query_pending = false;
                self.raw_write("\x1b[?2004l");
                let disable_kitty = state.keyboard_protocol_pushed || state.kitty_active;
                state.negotiation_buffer.clear();
                state.negotiation_deadline = None;
                if disable_kitty {
                    self.raw_write("\x1b[<u");
                    state.keyboard_protocol_pushed = false;
                    state.kitty_active = false;
                    set_kitty_protocol_active(false);
                }
                self.disable_modify_other_keys(&mut state);
                if let Some(b) = state.stdin_buffer.as_mut() {
                    b.destroy();
                }
                state.stdin_buffer = None;
                state.input_handler = None;
                state.resize_handler = None;
                state.reader.take()
            };
            if let Some((wake_fd, handle)) = reader {
                write_all_fd(wake_fd, &[1]);
                let _ = handle.join();
                unsafe { libc::close(wake_fd) };
            }
            let mut state = self.lock();
            Self::restore_raw_mode(&mut state);
        }

        fn restore_on_exit(&self, before: &str, after: &str) {
            let state = self.lock();
            let mut sequence = before.to_string();
            if state.progress.is_some() {
                sequence.push_str(TERMINAL_PROGRESS_CLEAR_SEQUENCE);
            }
            sequence.push_str("\x1b[?2004l");
            if state.keyboard_protocol_pushed || state.kitty_active {
                sequence.push_str("\x1b[<u");
            }
            if state.modify_other_keys_active {
                sequence.push_str("\x1b[>4;0m");
            }
            sequence.push_str(after);
            write_all_fd(EXIT_RESTORE_FD.load(std::sync::atomic::Ordering::SeqCst), sequence.as_bytes());
            if let Some(t) = state.saved_termios {
                unsafe {
                    libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &t);
                }
            }
        }

        fn drain_input(&self, max: Duration, idle: Duration) {
            let previous = {
                let mut state = self.lock();
                let disable_kitty = state.keyboard_protocol_pushed || state.kitty_active;
                state.negotiation_buffer.clear();
                state.negotiation_deadline = None;
                if disable_kitty {
                    self.raw_write("\x1b[<u");
                    state.keyboard_protocol_pushed = false;
                    state.kitty_active = false;
                    set_kitty_protocol_active(false);
                }
                self.disable_modify_other_keys(&mut state);
                state.input_handler.take()
            };
            let start_ms = self.inner.epoch.elapsed().as_millis() as u64;
            self.inner.last_data_ms.store(start_ms, Ordering::SeqCst);
            let end = Instant::now() + max;
            loop {
                let now = Instant::now();
                if now >= end {
                    break;
                }
                let now_ms = self.inner.epoch.elapsed().as_millis() as u64;
                let last = self.inner.last_data_ms.load(Ordering::SeqCst);
                if now_ms.saturating_sub(last) >= idle.as_millis() as u64 {
                    break;
                }
                std::thread::sleep(idle.min(end - now));
            }
            self.lock().input_handler = previous;
        }

        fn write(&self, data: &str) {
            self.raw_write(data);
            if let Some(path) = &self.inner.write_log_path
                && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path)
            {
                let _ = f.write_all(data.as_bytes());
            }
        }

        fn columns(&self) -> usize {
            let (cols, _) = window_size();
            if cols > 0 {
                return cols;
            }
            env_number("COLUMNS").unwrap_or(80)
        }

        fn rows(&self) -> usize {
            let (_, rows) = window_size();
            if rows > 0 {
                return rows;
            }
            env_number("LINES").unwrap_or(24)
        }

        fn kitty_protocol_active(&self) -> bool {
            self.lock().kitty_active
        }

        fn move_by(&self, lines: i64) {
            if lines > 0 {
                self.raw_write(&format!("\x1b[{lines}B"));
            } else if lines < 0 {
                self.raw_write(&format!("\x1b[{}A", -lines));
            }
        }

        fn set_program_status(&self, status: &ProgramStatus) {
            let mut state = self.lock();
            state.program_status = if status.state == ProgramState::Clear { None } else { Some(status.clone()) };
            if state.program_status_supported {
                self.raw_write(&format_program_status(status));
            }
        }

        fn set_progress(&self, active: bool) {
            let mut state = self.lock();
            if active {
                self.raw_write(TERMINAL_PROGRESS_ACTIVE_SEQUENCE);
                if state.progress.is_none() {
                    let stop = Arc::new(AtomicBool::new(false));
                    let stop2 = stop.clone();
                    let handle = std::thread::spawn(move || {
                        loop {
                            std::thread::park_timeout(TERMINAL_PROGRESS_KEEPALIVE);
                            if stop2.load(Ordering::SeqCst) {
                                break;
                            }
                            write_all_fd(libc::STDOUT_FILENO, TERMINAL_PROGRESS_ACTIVE_SEQUENCE.as_bytes());
                        }
                    });
                    state.progress = Some((stop, handle));
                }
            } else {
                Self::clear_progress(&mut state);
                self.raw_write(TERMINAL_PROGRESS_CLEAR_SEQUENCE);
            }
        }
    }

    fn env_number(name: &str) -> Option<usize> {
        let v: f64 = std::env::var(name).ok()?.trim().parse().ok()?;
        if v.is_finite() && v >= 1.0 { Some(v as usize) } else { None }
    }

    fn window_size() -> (usize, usize) {
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 {
                return (ws.ws_col as usize, ws.ws_row as usize);
            }
        }
        (0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_negotiation_sequences() {
        assert_eq!(
            parse_keyboard_protocol_negotiation_sequence("\x1b[?7u"),
            Some(KeyboardProtocolNegotiationSequence::KittyFlags(7))
        );
        assert_eq!(
            parse_keyboard_protocol_negotiation_sequence("\x1b[?62;22c"),
            Some(KeyboardProtocolNegotiationSequence::DeviceAttributes)
        );
        assert_eq!(parse_keyboard_protocol_negotiation_sequence("\x1b[?u"), None);
        assert!(is_keyboard_protocol_negotiation_sequence_prefix("\x1b["));
        assert!(is_keyboard_protocol_negotiation_sequence_prefix("\x1b[?62;"));
        assert!(!is_keyboard_protocol_negotiation_sequence_prefix("\x1b[A"));
    }

    #[test]
    fn resolves_escape_timeout() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        assert_eq!(resolve_escape_timeout_ms(&env(&[])), 10.0);
        assert_eq!(resolve_escape_timeout_ms(&env(&[("SSH_TTY", "/dev/pts/1")])), 100.0);
        assert_eq!(resolve_escape_timeout_ms(&env(&[("PI_TUI_ESC_TIMEOUT", "250")])), 250.0);
        assert_eq!(resolve_escape_timeout_ms(&env(&[("PI_TUI_ESC_TIMEOUT", "abc")])), 10.0);
    }
}
