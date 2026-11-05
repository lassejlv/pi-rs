//! Port of `packages/coding-agent/src/core/exec.ts`.
//!
//! Shared command execution utilities for extensions and custom tools.

use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

/// Options for executing shell commands.
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// AbortSignal to cancel the command
    pub signal: Option<CancellationToken>,
    /// Timeout in milliseconds
    pub timeout: Option<i64>,
    /// Working directory
    pub cwd: Option<String>,
}

/// Result of executing a shell command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub code: i64,
    pub killed: bool,
}

/// After the process exits, keep reading inherited pipes until they are idle this long.
const EXIT_STDIO_GRACE_MS: u64 = 100;
/// Force kill after this long if SIGTERM doesn't work.
const FORCE_KILL_DELAY_MS: u64 = 5000;

#[cfg(unix)]
fn send_sigterm(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: plain kill(2) on a pid we spawned.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
}

#[cfg(not(unix))]
fn send_sigterm(_child: &tokio::process::Child) {
    // No SIGTERM outside unix: the force kill below terminates the process.
}

/// Execute a shell command and return stdout/stderr/code.
/// Supports timeout and abort signal.
pub async fn exec_command(
    command: &str,
    args: &[String],
    cwd: &str,
    options: Option<&ExecOptions>,
) -> anyhow::Result<ExecResult> {
    let signal = options.and_then(|o| o.signal.clone());
    let timeout = options.and_then(|o| o.timeout).filter(|t| *t > 0);

    let spawned = tokio::process::Command::new(command)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(_) => {
            return Ok(ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                code: 1,
                killed: false,
            });
        }
    };

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let mut stdout_done = stdout_pipe.is_none();
    let mut stderr_done = stderr_pipe.is_none();
    let mut out_buf = [0u8; 8192];
    let mut err_buf = [0u8; 8192];

    let mut killed = false;
    let mut force_kill_at: Option<tokio::time::Instant> = None;
    let timeout_at = timeout.map(|t| tokio::time::Instant::now() + Duration::from_millis(t as u64));
    let mut exit_code: Option<Option<i32>> = None;
    let mut idle_deadline: Option<tokio::time::Instant> = None;

    let abort_requested = signal.clone().unwrap_or_default();
    let has_signal = signal.is_some();

    loop {
        if exit_code.is_some() && stdout_done && stderr_done {
            break;
        }
        if let Some(deadline) = idle_deadline
            && tokio::time::Instant::now() >= deadline
        {
            // Output (or EOF) still waiting in a pipe means this task fell behind (e.g. under
            // load), not that a quiet descendant holds the pipe open: keep reading.
            #[cfg(unix)]
            {
                use std::os::fd::AsFd;
                let open = [
                    stdout_pipe.as_ref().filter(|_| !stdout_done).map(|p| p.as_fd()),
                    stderr_pipe.as_ref().filter(|_| !stderr_done).map(|p| p.as_fd()),
                ];
                if crate::utils::child_process::pipes_pending(open.into_iter().flatten()) {
                    idle_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(EXIT_STDIO_GRACE_MS));
                    continue;
                }
            }
            break;
        }
        let far = tokio::time::Instant::now() + Duration::from_secs(86_400);
        tokio::select! {
            read = async { stdout_pipe.as_mut().unwrap().read(&mut out_buf).await }, if !stdout_done => {
                match read {
                    Ok(0) | Err(_) => stdout_done = true,
                    Ok(n) => {
                        stdout.extend_from_slice(&out_buf[..n]);
                        if exit_code.is_some() {
                            idle_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(EXIT_STDIO_GRACE_MS));
                        }
                    }
                }
            }
            read = async { stderr_pipe.as_mut().unwrap().read(&mut err_buf).await }, if !stderr_done => {
                match read {
                    Ok(0) | Err(_) => stderr_done = true,
                    Ok(n) => {
                        stderr.extend_from_slice(&err_buf[..n]);
                        if exit_code.is_some() {
                            idle_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(EXIT_STDIO_GRACE_MS));
                        }
                    }
                }
            }
            status = child.wait(), if exit_code.is_none() => {
                exit_code = Some(status.ok().and_then(|s| s.code()));
                idle_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(EXIT_STDIO_GRACE_MS));
            }
            _ = abort_requested.cancelled(), if has_signal && !killed && exit_code.is_none() => {
                killed = true;
                send_sigterm(&child);
                force_kill_at = Some(tokio::time::Instant::now() + Duration::from_millis(FORCE_KILL_DELAY_MS));
            }
            _ = tokio::time::sleep_until(timeout_at.unwrap_or(far)), if timeout_at.is_some() && !killed && exit_code.is_none() => {
                killed = true;
                send_sigterm(&child);
                force_kill_at = Some(tokio::time::Instant::now() + Duration::from_millis(FORCE_KILL_DELAY_MS));
            }
            _ = tokio::time::sleep_until(force_kill_at.unwrap_or(far)), if force_kill_at.is_some() && exit_code.is_none() => {
                force_kill_at = None;
                let _ = child.start_kill();
            }
            _ = tokio::time::sleep_until(idle_deadline.unwrap_or(far)), if idle_deadline.is_some() => {}
        }
    }

    Ok(ExecResult {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        code: exit_code.flatten().unwrap_or(0) as i64,
        killed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_output_and_exit_code() {
        let result = exec_command(
            "sh",
            &["-c".into(), "echo out; echo err >&2; exit 3".into()],
            ".",
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.stdout, "out\n");
        assert_eq!(result.stderr, "err\n");
        assert_eq!(result.code, 3);
        assert!(!result.killed);
    }

    #[tokio::test]
    async fn timeout_kills() {
        let result = exec_command(
            "sh",
            &["-c".into(), "sleep 5".into()],
            ".",
            Some(&ExecOptions {
                timeout: Some(50),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert!(result.killed);
    }

    #[tokio::test]
    async fn missing_command_returns_code_1() {
        let result = exec_command("definitely-not-a-command-xyz", &[], ".", None)
            .await
            .unwrap();
        assert_eq!(result.code, 1);
    }
}
