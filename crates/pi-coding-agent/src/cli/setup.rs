//! Port of `packages/coding-agent/src/cli/setup.ts`.

use std::sync::OnceLock;

use crate::config::APP_NAME;
use crate::core::http_dispatcher::configure_http_dispatcher;

static PROCESS_ARGS: OnceLock<Vec<String>> = OnceLock::new();

/// The process arguments (`process.argv` without the runtime and script), captured before
/// [`setup_cli`] replaces the command line with the process title.
pub fn process_args() -> &'static [String] {
    PROCESS_ARGS.get_or_init(|| std::env::args().collect())
}

/// Process-level setup run by the `pi` binary before `main()`.
///
/// TS also silences `process.emitWarning` (Rust has no runtime warnings to silence).
pub fn setup_cli() {
    process_args();
    set_process_title(APP_NAME);
    // SAFETY: called at startup from the binary's main thread before other threads exist.
    unsafe {
        std::env::set_var("PI_CODING_AGENT", "true");
        std::env::set_var("AI_AGENT", "pi");
    }
    // Configure the HTTP client before provider requests. Settings are applied
    // once SettingsManager has loaded global/project configuration.
    let _ = configure_http_dispatcher(None);
}

/// `process.title = title` as libuv implements it on Linux: the thread name (`PR_SET_NAME`, what
/// `top` and `ps -o comm` show) and the command line (`/proc/<pid>/cmdline`, what `ps` shows),
/// which is overwritten in place, hiding the arguments. Other platforms keep the command line.
pub fn set_process_title(title: &str) {
    #[cfg(target_os = "linux")]
    {
        if let Ok(name) = std::ffi::CString::new(title) {
            // SAFETY: PR_SET_NAME reads a NUL-terminated string (truncated to 15 bytes).
            unsafe { libc::prctl(libc::PR_SET_NAME, name.as_ptr() as libc::c_ulong, 0, 0, 0) };
        }
        overwrite_command_line(title);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = title;
}

/// Write `title` over the argument strings (`arg_start..arg_end` of `/proc/self/stat`), like
/// libuv's `uv_set_process_title`, zero-filling the rest of the area.
#[cfg(target_os = "linux")]
fn overwrite_command_line(title: &str) {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else { return };
    // Fields after the parenthesized command name start with field 3 (state).
    let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else { return };
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let field = |n: usize| fields.get(n - 3).and_then(|f| f.parse::<usize>().ok());
    let (Some(start), Some(end)) = (field(48), field(49)) else { return };
    if start == 0 || end <= start {
        return;
    }
    let capacity = end - start;
    let bytes = title.as_bytes();
    let len = bytes.len().min(capacity - 1);
    // SAFETY: `arg_start..arg_end` is this process's own writable argument area (the initial
    // stack); Rust's copies of the arguments were taken first (`process_args`).
    unsafe {
        let area = std::slice::from_raw_parts_mut(start as *mut u8, capacity);
        area[..len].copy_from_slice(&bytes[..len]);
        area[len..].fill(0);
    }
}
