//! Port of `packages/coding-agent/src/core/output-guard.ts`.
//!
//! TS monkey-patches `process.stdout.write` so stray writes go to stderr while a mode (RPC,
//! JSON) owns stdout. The Rust port does the same at the file-descriptor level: taking over
//! duplicates the real stdout to a private descriptor and points fd 1 at stderr, so any
//! `println!` from libraries lands on stderr. [`write_raw_stdout`] writes to the private
//! descriptor (the real stdout).

use parking_lot::Mutex;
use std::io::Write;

/// Saved state while stdout is taken over.
#[derive(Debug, Clone, Copy)]
pub struct StdoutTakeoverState {
    /// Duplicate of the original stdout descriptor; raw writes go here.
    pub raw_stdout_fd: i32,
}

static STDOUT_TAKEOVER_STATE: Mutex<Option<StdoutTakeoverState>> = Mutex::new(None);

/// Serializes raw writes (TS `rawStdoutWriteTail` promise chain).
static RAW_STDOUT_WRITE_LOCK: Mutex<()> = Mutex::new(());

const RAW_STDOUT_RETRY_DELAY_MS: u64 = 10;

#[cfg(unix)]
fn write_all_fd(fd: i32, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        // SAFETY: writing a valid buffer to a descriptor we own or inherited.
        let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if written < 0 {
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(code)
                    if code == libc::EAGAIN || code == libc::EWOULDBLOCK || code == libc::ENOBUFS =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(RAW_STDOUT_RETRY_DELAY_MS));
                    continue;
                }
                Some(code) if code == libc::EINTR => continue,
                _ => return Err(error),
            }
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

fn write_raw_stdout_chunk(text: &str) -> std::io::Result<()> {
    let state = *STDOUT_TAKEOVER_STATE.lock();
    #[cfg(unix)]
    {
        let fd = state.map(|s| s.raw_stdout_fd).unwrap_or(1);
        write_all_fd(fd, text.as_bytes())
    }
    #[cfg(not(unix))]
    {
        let _ = state;
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(text.as_bytes())?;
        stdout.flush()
    }
}

pub fn take_over_stdout() {
    let mut state = STDOUT_TAKEOVER_STATE.lock();
    if state.is_some() {
        return;
    }
    // Flush anything Rust buffered for the real stdout before redirecting it.
    let _ = std::io::stdout().flush();
    #[cfg(unix)]
    {
        // SAFETY: dup/dup2 on the standard descriptors.
        unsafe {
            let raw = libc::dup(1);
            if raw < 0 {
                return;
            }
            libc::dup2(2, 1);
            *state = Some(StdoutTakeoverState { raw_stdout_fd: raw });
            // Exit-time terminal restoration writes to the real stdout like TS `restoreOnExit`
            // (`fs.writeSync(process.stdout.fd)` bypasses the patched `process.stdout.write`).
            pi_tui::terminal::set_exit_restore_fd(raw);
        }
    }
    #[cfg(not(unix))]
    {
        *state = Some(StdoutTakeoverState { raw_stdout_fd: 1 });
    }
}

pub fn restore_stdout() {
    let mut state = STDOUT_TAKEOVER_STATE.lock();
    let Some(saved) = state.take() else {
        return;
    };
    let _ = std::io::stdout().flush();
    #[cfg(unix)]
    {
        // SAFETY: restore fd 1 from the duplicate taken in `take_over_stdout`.
        pi_tui::terminal::set_exit_restore_fd(1);
        unsafe {
            libc::dup2(saved.raw_stdout_fd, 1);
            libc::close(saved.raw_stdout_fd);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = saved;
    }
}

pub fn is_stdout_taken_over() -> bool {
    STDOUT_TAKEOVER_STATE.lock().is_some()
}

/// Write `text` to the real stdout. Writes are serialized; a write error exits the process
/// with status 1, like the TS version.
pub fn write_raw_stdout(text: &str) {
    if text.is_empty() {
        return;
    }
    let _guard = RAW_STDOUT_WRITE_LOCK.lock();
    if write_raw_stdout_chunk(text).is_err() {
        std::process::exit(1);
    }
}

/// Raw writes complete synchronously, so there is never pending backpressure.
pub async fn wait_for_raw_stdout_backpressure() -> anyhow::Result<()> {
    let _guard = RAW_STDOUT_WRITE_LOCK.lock();
    Ok(())
}

pub async fn flush_raw_stdout() -> anyhow::Result<()> {
    wait_for_raw_stdout_backpressure().await?;
    let _guard = RAW_STDOUT_WRITE_LOCK.lock();
    write_raw_stdout_chunk("")?;
    Ok(())
}
