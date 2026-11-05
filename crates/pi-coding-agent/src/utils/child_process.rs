//! Port of `packages/coding-agent/src/utils/child-process.ts`.
//!
//! `spawnProcess`/`spawnProcessSync` become command builders (callers set cwd, env and stdio);
//! `waitForChildProcess` drives the child's stdout/stderr pipes and reports their chunks through a
//! callback while it waits, since Rust has no stream event listeners.

use std::process::ExitStatus;
use std::time::Duration;
use tokio::io::AsyncReadExt;

const EXIT_STDIO_GRACE_MS: u64 = 100;

/// `spawnProcess(command, args, options)`: a command builder for `command args...`.
///
/// On Windows Node uses cross-spawn to resolve `.cmd`/`.bat` shims; Rust's `Command` resolves
/// `.exe` files on PATH, and batch files must be named with their extension.
pub fn spawn_process(command: &str, args: &[String]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(args);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// `spawnProcessSync(command, args, options)`: a blocking command builder.
pub fn spawn_process_sync(command: &str, args: &[String]) -> std::process::Command {
    let mut cmd = std::process::Command::new(command);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Which pipe a chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildStream {
    Stdout,
    Stderr,
}

/// How the child terminated (`exit` event arguments).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildExit {
    /// Exit code, `None` when the process was terminated by a signal.
    pub code: Option<i32>,
    /// Terminating signal number (Unix), when there is no exit code.
    pub signal: Option<i32>,
}

impl From<ExitStatus> for ChildExit {
    fn from(status: ExitStatus) -> Self {
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        Self {
            code: status.code(),
            signal,
        }
    }
}

/// Wait for a child process to terminate without hanging on inherited stdio handles.
///
/// A short-lived child can `exit` while a detached descendant keeps its stdout/stderr
/// pipe open. We must not resolve and destroy the streams on a fixed deadline measured
/// from `exit`, or output still being written past that deadline is silently lost
/// (earendil-works/pi#5303). Instead, after `exit` we wait for the pipes to fall idle:
/// the grace timer is re-armed on every chunk, so an actively writing descendant keeps
/// us reading, while a quiet inherited handle (e.g. a Windows daemonized descendant
/// that never lets `close` fire) still releases us after the grace elapses.
///
/// The child's piped `stdout`/`stderr` are taken from `child` and every chunk is passed to
/// `on_data` as it arrives.
pub async fn wait_for_child_process(
    child: &mut tokio::process::Child,
    on_data: &mut (dyn FnMut(ChildStream, &[u8]) + Send),
) -> anyhow::Result<ChildExit> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(ChildStream, Option<Vec<u8>>)>();
    let mut readers = Vec::new();
    let mut stdout_ended = true;
    let mut stderr_ended = true;

    fn spawn_reader<R>(
        mut reader: R,
        stream: ChildStream,
        tx: tokio::sync::mpsc::UnboundedSender<(ChildStream, Option<Vec<u8>>)>,
    ) -> tokio::task::JoinHandle<()>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        crate::utils::uncaught::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => {
                        let _ = tx.send((stream, None));
                        break;
                    }
                    Ok(n) => {
                        if tx.send((stream, Some(buf[..n].to_vec()))).is_err() {
                            break;
                        }
                    }
                }
            }
        })
    }

    // Duplicates of the pipes' read ends, only to ask the kernel whether output (or EOF) is still
    // waiting to be read when the grace period after `exit` runs out (see `pipes_pending`).
    #[cfg(unix)]
    let mut watched = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        stdout_ended = false;
        #[cfg(unix)]
        watched.extend(dup_pipe(&stdout).map(|fd| (ChildStream::Stdout, fd)));
        readers.push(spawn_reader(stdout, ChildStream::Stdout, tx.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        stderr_ended = false;
        #[cfg(unix)]
        watched.extend(dup_pipe(&stderr).map(|fd| (ChildStream::Stderr, fd)));
        readers.push(spawn_reader(stderr, ChildStream::Stderr, tx.clone()));
    }
    drop(tx);

    let mut exit: Option<ChildExit> = None;
    let idle = tokio::time::sleep(Duration::from_millis(EXIT_STDIO_GRACE_MS));
    tokio::pin!(idle);

    let result = loop {
        if let Some(exit) = exit
            && stdout_ended
            && stderr_ended
        {
            break Ok(exit);
        }
        tokio::select! {
            message = rx.recv() => match message {
                Some((stream, Some(data))) => {
                    on_data(stream, &data);
                    // Output is still arriving after exit; defer finalizing so we don't
                    // drop the stream mid-write and truncate the tail.
                    if exit.is_some() {
                        idle.as_mut().reset(tokio::time::Instant::now() + Duration::from_millis(EXIT_STDIO_GRACE_MS));
                    }
                }
                Some((ChildStream::Stdout, None)) => stdout_ended = true,
                Some((ChildStream::Stderr, None)) => stderr_ended = true,
                None => {
                    stdout_ended = true;
                    stderr_ended = true;
                }
            },
            status = child.wait(), if exit.is_none() => match status {
                Ok(status) => {
                    exit = Some(ChildExit::from(status));
                    idle.as_mut().reset(tokio::time::Instant::now() + Duration::from_millis(EXIT_STDIO_GRACE_MS));
                }
                Err(err) => break Err(anyhow::Error::from(err)),
            },
            _ = &mut idle, if exit.is_some() => {
                // The pipes are idle only if nothing is left to read. Under load the reader tasks
                // can lag behind the exit by more than the grace period; output (or EOF) already
                // sitting in a pipe is not an idle inherited handle, so keep reading.
                #[cfg(unix)]
                if pipes_pending(watched.iter().filter_map(|(stream, fd)| {
                    let ended = match stream {
                        ChildStream::Stdout => stdout_ended,
                        ChildStream::Stderr => stderr_ended,
                    };
                    (!ended).then(|| std::os::fd::AsFd::as_fd(fd))
                })) {
                    idle.as_mut().reset(tokio::time::Instant::now() + Duration::from_millis(EXIT_STDIO_GRACE_MS));
                    continue;
                }
                break Ok(exit.expect("exit status"));
            }
        }
    };

    // Deliver chunks that were already read, then stop reading (TS destroys the streams).
    while let Ok((stream, data)) = rx.try_recv() {
        if let Some(data) = data {
            on_data(stream, &data);
        }
    }
    for reader in readers {
        reader.abort();
    }
    result
}

/// A duplicate of a pipe's read end (`None` if `dup` fails; then that pipe is not watched).
#[cfg(unix)]
fn dup_pipe(pipe: &impl std::os::fd::AsFd) -> Option<std::os::fd::OwnedFd> {
    pipe.as_fd().try_clone_to_owned().ok()
}

/// Whether any of `pipes` (not yet at EOF on our side) has data or EOF ready to read (`poll` with
/// a zero timeout).
#[cfg(unix)]
pub(crate) fn pipes_pending<'a>(pipes: impl Iterator<Item = std::os::fd::BorrowedFd<'a>>) -> bool {
    use std::os::fd::AsRawFd;
    let mut fds: Vec<libc::pollfd> = pipes
        .map(|fd| libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 })
        .collect();
    if fds.is_empty() {
        return false;
    }
    // SAFETY: `fds` is a valid array of `fds.len()` pollfd entries for open descriptors we own.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 0) };
    ready > 0 && fds.iter().any(|fd| fd.revents & (libc::POLLIN | libc::POLLHUP) != 0)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[tokio::test]
    async fn collects_output_and_exit_code() {
        let mut child = spawn_process("sh", &["-c".into(), "echo out; echo err >&2; exit 3".into()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let exit = wait_for_child_process(&mut child, &mut |stream, data| match stream {
            ChildStream::Stdout => out.extend_from_slice(data),
            ChildStream::Stderr => err.extend_from_slice(data),
        })
        .await
        .unwrap();
        assert_eq!(exit.code, Some(3));
        assert_eq!(out, b"out\n");
        assert_eq!(err, b"err\n");
    }

    #[tokio::test]
    async fn does_not_hang_on_inherited_pipes() {
        // The background sleep inherits stdout but stays quiet; we resolve after the grace period.
        let started = std::time::Instant::now();
        let mut child = spawn_process("sh", &["-c".into(), "sleep 5 & echo done".into()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = Vec::new();
        let exit = wait_for_child_process(&mut child, &mut |_, data| out.extend_from_slice(data))
            .await
            .unwrap();
        assert_eq!(exit.code, Some(0));
        assert_eq!(out, b"done\n");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn keeps_reading_while_descendants_write() {
        let mut child = spawn_process(
            "sh",
            &["-c".into(), "(for i in 1 2 3; do sleep 0.05; echo $i; done) & exit 0".into()],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
        let mut out = Vec::new();
        wait_for_child_process(&mut child, &mut |_, data| out.extend_from_slice(data))
            .await
            .unwrap();
        assert_eq!(out, b"1\n2\n3\n");
    }
}
