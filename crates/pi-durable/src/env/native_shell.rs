//! Native process execution and bounded, ordered spill I/O.
use super::{decode::StreamDecoder, filesystem::*, native::NativeFileSystem, shell::*};
use crate::{Context, JsonString};
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, HashSet},
    ffi::OsString,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, watch},
};

const MAX_TIMEOUT_MS: f64 = 2_147_483_647.0;
const SPILL_HIGH_WATER_MARK: usize = 1024 * 1024;
const EXIT_STDIO_GRACE: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellPlatform {
    Unix,
    Windows,
}
#[derive(Clone)]
pub struct ShellHost {
    pub platform: ShellPlatform,
    pub cwd: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
}
impl Default for ShellHost {
    fn default() -> Self {
        Self {
            platform: if cfg!(windows) {
                ShellPlatform::Windows
            } else {
                ShellPlatform::Unix
            },
            cwd: std::env::current_dir().unwrap_or_default(),
            environment: std::env::vars_os().collect(),
        }
    }
}
/// I/O capabilities preserve the policy in Rust while allowing remote or
/// embedded hosts to supply opened spill streams and temporary paths.
pub trait ShellSpillWriter: Send + Sync {
    /// Enqueue bytes in order. False pauses both process streams until drain.
    fn write(&self, bytes: Vec<u8>) -> ExecutionFuture<bool>;
    fn drain(&self) -> ExecutionFuture<()>;
    fn finish(&self) -> ExecutionFuture<()>;
    /// Resolves on close, or rejects as soon as an asynchronous write fails.
    fn settled(&self) -> ExecutionFuture<()>;
}
pub type ShellSpillOpener =
    Arc<dyn Fn(PathBuf) -> ExecutionFuture<Arc<dyn ShellSpillWriter>> + Send + Sync>;
pub type ShellTempFile = Arc<dyn Fn(Context) -> FileFuture<PathBuf> + Send + Sync>;
#[derive(Clone, Default)]
pub struct NativeShellOptions {
    pub shell_path: Option<PathBuf>,
    pub environment: BTreeMap<OsString, Option<OsString>>,
    pub host: Option<Arc<dyn Fn() -> ShellHost + Send + Sync>>,
    pub spill_opener: Option<ShellSpillOpener>,
    pub temp_file: Option<ShellTempFile>,
}
#[derive(Clone, Default)]
pub(crate) struct NativeShell {
    pub options: NativeShellOptions,
    active: Arc<Mutex<HashSet<u32>>>,
}
fn failure(
    code: ExecutionErrorCode,
    error: impl std::error::Error + Send + Sync + 'static,
) -> ExecutionError {
    ExecutionError::new(code, error.to_string()).with_cause(Arc::new(error))
}
fn aborted() -> ExecutionError {
    ExecutionError::new(ExecutionErrorCode::Aborted, "aborted")
}
fn spill_failure(error: ExecutionError) -> ExecutionError {
    ExecutionError::new(
        ExecutionErrorCode::Unknown,
        format!("Failed to preserve complete shell output: {error}"),
    )
    .with_cause(Arc::new(error))
}
fn windows_normalize(path: &str) -> String {
    let path = path.replace('/', "\\").to_lowercase();
    let rooted = path.as_bytes().get(2) == Some(&b'\\');
    let drive = if path.as_bytes().get(1) == Some(&b':') {
        &path[..2]
    } else {
        ""
    };
    let mut parts = Vec::new();
    for part in path[drive.len()..].split('\\') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|part| *part != "..") => {
                parts.pop();
            }
            ".." if !rooted => parts.push(part),
            ".." => {}
            part => parts.push(part),
        }
    }
    format!(
        "{drive}{}{tail}",
        if rooted { "\\" } else { "" },
        tail = parts.join("\\")
    )
}
fn legacy_wsl(path: &str, host: &ShellHost) -> bool {
    let path = windows_normalize(path);
    if let Some(root) = host
        .environment
        .get(&OsString::from("SystemRoot"))
        .filter(|root| !root.is_empty())
    {
        let root = root.to_string_lossy();
        let bytes = root.as_bytes();
        if bytes.len() < 3
            || !bytes[0].is_ascii_alphabetic()
            || bytes[1] != b':'
            || !matches!(bytes[2], b'\\' | b'/')
        {
            return false;
        }
        return ["System32", "Sysnative"]
            .iter()
            .any(|directory| path == windows_normalize(&format!("{root}\\{directory}\\bash.exe")));
    }
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && [
            "\\windows\\system32\\bash.exe",
            "\\windows\\sysnative\\bash.exe",
        ]
        .contains(&&path[2..])
}
async fn exists(path: &std::path::Path, host: &ShellHost) -> bool {
    tokio::fs::metadata(host.cwd.join(path)).await.is_ok()
}
async fn shell_path(options: &NativeShellOptions, host: &ShellHost) -> ExecutionResult<PathBuf> {
    if let Some(path) = options
        .shell_path
        .as_ref()
        .filter(|path| !path.as_os_str().is_empty())
    {
        if exists(path, host).await {
            return Ok(path.clone());
        }
        return Err(ExecutionError::new(
            ExecutionErrorCode::ShellUnavailable,
            format!("Custom shell path not found: {}", path.display()),
        ));
    }
    let mut candidates = Vec::new();
    if host.platform == ShellPlatform::Windows {
        for key in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(root) = host.environment.get(&OsString::from(key)) {
                let path = PathBuf::from(format!("{}\\Git\\bin\\bash.exe", root.to_string_lossy()));
                if exists(&path, host).await {
                    return Ok(path);
                }
                candidates.push(path);
            }
        }
    } else if exists(std::path::Path::new("/bin/bash"), host).await {
        return Ok("/bin/bash".into());
    }
    let lookup = if host.platform == ShellPlatform::Windows {
        "where"
    } else {
        "which"
    };
    let mut command = tokio::process::Command::new(lookup);
    command
        .arg(if host.platform == ShellPlatform::Windows {
            "bash.exe"
        } else {
            "bash"
        })
        .current_dir(&host.cwd)
        .env_clear()
        .envs(&host.environment)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(5), command.output()).await
        && output.status.success()
        && let Some(first) = String::from_utf8_lossy(&output.stdout)
            .trim()
            .lines()
            .next()
        && !first.is_empty()
        && exists(std::path::Path::new(first), host).await
    {
        return Ok(first.into());
    }
    if host.platform == ShellPlatform::Unix {
        return Ok("sh".into());
    }
    Err(ExecutionError::new(
        ExecutionErrorCode::ShellUnavailable,
        format!(
            "No bash shell found. Options:\n  1. Install Git for Windows: https://git-scm.com/download/win\n  2. Add your bash to PATH (Cygwin, MSYS2, etc.)\n  3. Configure an explicit shellPath\n\nSearched Git Bash in:\n{}",
            candidates
                .iter()
                .map(|path| format!("  {}", path.display()))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    ))
}
fn kill_tree(pid: u32, host: &ShellHost) {
    if host.platform == ShellPlatform::Windows {
        let root = host
            .environment
            .get(&OsString::from("SystemRoot"))
            .filter(|root| !root.is_empty())
            .cloned()
            .unwrap_or_else(|| "C:\\Windows".into());
        let mut command =
            std::process::Command::new(PathBuf::from(root).join("System32").join("taskkill.exe"));
        command
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        if let Ok(mut child) = command.spawn() {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        return;
    }
    #[cfg(unix)]
    unsafe {
        if libc::kill(-(pid as i32), libc::SIGKILL) != 0 {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
}
struct ProcessLease {
    pid: u32,
    host: ShellHost,
    active: Arc<Mutex<HashSet<u32>>>,
    kill_on_drop: bool,
}
impl Drop for ProcessLease {
    fn drop(&mut self) {
        self.active.lock().remove(&self.pid);
        if self.kill_on_drop {
            kill_tree(self.pid, &self.host);
        }
    }
}
impl NativeShell {
    pub fn cleanup(&self) -> ExecutionFuture<()> {
        let host = self
            .options
            .host
            .as_ref()
            .map(|host| host())
            .unwrap_or_default();
        for pid in self.active.lock().drain() {
            kill_tree(pid, &host);
        }
        Box::pin(async { Ok(()) })
    }
    pub fn exec(
        &self,
        filesystem: NativeFileSystem,
        command: ShellCommand,
        options: ShellExecOptions,
        context: &Context,
    ) -> ExecutionFuture<ShellExecResult> {
        if context.check().is_err() {
            return Box::pin(async { Err(aborted()) });
        }
        let cwd = options
            .cwd
            .as_ref()
            .filter(|path| !path.as_os_str().is_empty())
            .map_or_else(|| filesystem.cwd(), |path| filesystem.resolve(path));
        let (shell, context) = (self.clone(), context.clone());
        let host = shell
            .options
            .host
            .as_ref()
            .map(|host| host())
            .unwrap_or_default();
        let job = tokio::spawn(async move {
            shell
                .run(filesystem, command, options, context, host, cwd)
                .await
        });
        Box::pin(async move {
            job.await
                .map_err(|error| failure(ExecutionErrorCode::Unknown, error))?
        })
    }
    async fn run(
        &self,
        filesystem: NativeFileSystem,
        command: ShellCommand,
        options: ShellExecOptions,
        context: Context,
        host: ShellHost,
        cwd: PathBuf,
    ) -> ExecutionResult<ShellExecResult> {
        let timeout = match options.timeout {
            None => None,
            Some(value) if !value.is_finite() || value <= 0.0 => {
                return Err(ExecutionError::new(
                    ExecutionErrorCode::Timeout,
                    "Invalid timeout: must be a finite number of seconds",
                ));
            }
            Some(value) if value * 1000.0 > MAX_TIMEOUT_MS => {
                return Err(ExecutionError::new(
                    ExecutionErrorCode::Timeout,
                    "Invalid timeout: maximum is 2147483.647 seconds",
                ));
            }
            Some(value) => Some(Duration::from_secs_f64(value)),
        };
        let mut stdin_command = None;
        let (program, args) = match command {
            ShellCommand::Script(script) => {
                let path = shell_path(&self.options, &host).await?;
                let stdin = legacy_wsl(&path.to_string_lossy(), &host);
                let script = script.to_string_lossy();
                let args = if stdin {
                    stdin_command = Some(script);
                    vec!["-s".into()]
                } else {
                    vec!["-c".into(), script]
                };
                (
                    path.into_os_string(),
                    args.into_iter().map(OsString::from).collect::<Vec<_>>(),
                )
            }
            ShellCommand::Argv(args) => {
                let mut args = args.into_iter();
                let program = args.next().ok_or_else(|| {
                    ExecutionError::new(
                        ExecutionErrorCode::SpawnError,
                        "Empty argv: no program to run",
                    )
                })?;
                (
                    program.to_string_lossy().into(),
                    args.map(|arg| arg.to_string_lossy().into()).collect(),
                )
            }
        };
        if let Err(error) = tokio::fs::metadata(&cwd).await {
            return Err(ExecutionError::new(
                ExecutionErrorCode::SpawnError,
                format!(
                    "Working directory does not exist: {}\nCannot execute bash commands.",
                    cwd.display()
                ),
            )
            .with_cause(Arc::new(error)));
        }
        let mut environment = if options.inherit_env == Some(false) {
            BTreeMap::new()
        } else {
            host.environment.clone()
        };
        if options.inherit_env != Some(false) {
            for (key, value) in &self.options.environment {
                if let Some(value) = value {
                    environment.insert(key.clone(), value.clone());
                } else {
                    environment.remove(key);
                }
            }
        }
        if let Some(values) = &options.env {
            for (key, value) in values.iter() {
                if let Some(value) = value.as_string() {
                    environment
                        .insert(key.to_string_lossy().into(), value.to_string_lossy().into());
                }
            }
        }
        let mut process = tokio::process::Command::new(program);
        process
            .args(args)
            .current_dir(&cwd)
            .env_clear()
            .envs(environment)
            .stdin(if stdin_command.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        unsafe {
            process.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        #[cfg(windows)]
        {
            process.creation_flags(0x08000000);
        }
        let mut child = process
            .spawn()
            .map_err(|error| failure(ExecutionErrorCode::SpawnError, error))?;
        let pid = child.id().expect("spawned child has a pid");
        self.active.lock().insert(pid);
        let mut lease = ProcessLease {
            pid,
            host: host.clone(),
            active: self.active.clone(),
            kill_on_drop: true,
        };
        if let Some(script) = stdin_command {
            let mut stdin = child.stdin.take().unwrap();
            tokio::spawn(async move {
                let _ = stdin.write_all(script.as_bytes()).await;
            });
        }
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let mut out = [0; 64 * 1024];
        let mut err = [0; 64 * 1024];
        let (mut out_ended, mut err_ended) = (false, false);
        let (mut out_decoder, mut err_decoder) =
            (StreamDecoder::default(), StreamDecoder::default());
        let mut status = None;
        let mut idle = None;
        let mut timer = timeout.map(|timeout| Box::pin(tokio::time::sleep(timeout)));
        let signal = context.abort_signal();
        let mut interrupted = false;
        let mut timed_out = false;
        let mut callback_error = None;
        let mut spill_error = None;
        let mut spill = Spill::default();
        let mut draining: Option<ExecutionFuture<()>> = None;
        let mut spill_settled: Option<ExecutionFuture<()>> = None;
        loop {
            if status.is_some() && out_ended && err_ended {
                break;
            }
            enum Event {
                Exit(std::io::Result<std::process::ExitStatus>),
                Read(ShellStream, std::io::Result<usize>),
                Abort,
                Timeout,
                Idle,
                Drain(ExecutionResult<()>),
                Spill(ExecutionResult<()>),
            }
            let event = tokio::select! {
                result=child.wait(), if status.is_none()=>Event::Exit(result),
                result=stdout.read(&mut out), if !out_ended && draining.is_none()=>Event::Read(ShellStream::Stdout,result),
                result=stderr.read(&mut err), if !err_ended && draining.is_none()=>Event::Read(ShellStream::Stderr,result),
                _=async { match &signal {Some(signal)=>signal.cancelled().await,None=>std::future::pending().await} }, if !interrupted=>Event::Abort,
                _=async { match &mut timer {Some(timer)=>timer.await,None=>std::future::pending().await} }=>Event::Timeout,
                _=async { match idle {Some(at)=>tokio::time::sleep_until(at).await,None=>std::future::pending().await} }=>Event::Idle,
                result=async { match &mut draining {Some(wait)=>wait.await,None=>std::future::pending().await} }=>Event::Drain(result),
                result=async { match &mut spill_settled {Some(wait)=>wait.await,None=>std::future::pending().await} }=>Event::Spill(result),
            };
            match event {
                Event::Exit(result) => {
                    status = Some(
                        result.map_err(|error| failure(ExecutionErrorCode::SpawnError, error))?,
                    );
                    idle = Some(tokio::time::Instant::now() + EXIT_STDIO_GRACE);
                }
                Event::Abort => {
                    interrupted = true;
                    kill_tree(pid, &host);
                }
                Event::Timeout => {
                    timed_out = true;
                    timer = None;
                    kill_tree(pid, &host);
                }
                Event::Idle if draining.is_some() => {
                    idle = Some(tokio::time::Instant::now() + EXIT_STDIO_GRACE);
                }
                Event::Idle => break,
                Event::Drain(result) => {
                    draining = None;
                    if let Err(error) = result
                        && spill_error.is_none()
                    {
                        spill_error = Some(spill_failure(error));
                        kill_tree(pid, &host);
                    }
                    if status.is_some() {
                        idle = Some(tokio::time::Instant::now() + EXIT_STDIO_GRACE);
                    }
                }
                Event::Spill(result) => {
                    spill_settled = None;
                    if let Err(error) = result
                        && spill_error.is_none()
                    {
                        spill_error = Some(spill_failure(error));
                        draining = None;
                        kill_tree(pid, &host);
                    }
                }
                Event::Read(stream, result) => {
                    let count =
                        result.map_err(|error| failure(ExecutionErrorCode::Unknown, error))?;
                    if count == 0 {
                        match stream {
                            ShellStream::Stdout => out_ended = true,
                            ShellStream::Stderr => err_ended = true,
                        };
                        continue;
                    }
                    if status.is_some() {
                        idle = Some(tokio::time::Instant::now() + EXIT_STDIO_GRACE);
                    }
                    let (bytes, decoder) = match stream {
                        ShellStream::Stdout => (&out[..count], &mut out_decoder),
                        ShellStream::Stderr => (&err[..count], &mut err_decoder),
                    };
                    emit(
                        &options,
                        &context,
                        decoder.decode(Some(bytes)),
                        stream,
                        &mut callback_error,
                    );
                    if callback_error.is_some() {
                        kill_tree(pid, &host);
                    }
                    if spill_error.is_none()
                        && let Some(threshold) = options.spill
                    {
                        match spill
                            .feed(bytes, threshold, &filesystem, &self.options, &context)
                            .await
                        {
                            Ok(pressure) => {
                                if spill_settled.is_none()
                                    && let Some(writer) = &spill.writer
                                {
                                    spill_settled = Some(writer.settled());
                                }
                                if pressure {
                                    draining = spill.writer.as_ref().map(|writer| writer.drain());
                                }
                            }
                            Err(error) => {
                                spill_error = Some(spill_failure(error));
                                kill_tree(pid, &host);
                            }
                        }
                    }
                }
            }
        }
        drop(stdout);
        drop(stderr);
        if let Some(writer) = &spill.writer
            && let Err(error) = writer.finish().await
            && spill_error.is_none()
        {
            spill_error = Some(spill_failure(error));
        }
        emit(
            &options,
            &context,
            out_decoder.decode(None),
            ShellStream::Stdout,
            &mut callback_error,
        );
        emit(
            &options,
            &context,
            err_decoder.decode(None),
            ShellStream::Stderr,
            &mut callback_error,
        );
        lease.kill_on_drop = false;
        if let Some(error) = callback_error {
            return Err(error);
        }
        if timed_out || context.check().is_err() {
            let error = if timed_out {
                ExecutionError::new(
                    ExecutionErrorCode::Timeout,
                    format!("timeout:{}", options.timeout.unwrap()),
                )
            } else {
                aborted()
            };
            return Err(match spill.path {
                Some(path) => error.with_spill_path(path),
                None => error,
            });
        }
        if let Some(error) = spill_error {
            return Err(error);
        }
        let status = status.expect("output completes only after child exit");
        #[cfg(unix)]
        let exit_code = {
            use std::os::unix::process::ExitStatusExt;
            status
                .code()
                .unwrap_or_else(|| status.signal().map_or(1, |signal| 128 + signal))
        };
        #[cfg(not(unix))]
        let exit_code = status.code().unwrap_or(1);
        Ok(ShellExecResult {
            exit_code,
            spill_path: spill.path,
        })
    }
}
fn emit(
    options: &ShellExecOptions,
    context: &Context,
    text: String,
    stream: ShellStream,
    error: &mut Option<ExecutionError>,
) {
    if text.is_empty() || error.is_some() {
        return;
    }
    if let Some(callback) = &options.on_output {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            callback(
                JsonString::from(text),
                context.clone(),
                ShellOutputInfo {
                    stream,
                    skipped: None,
                },
            )
        }));
        let result = result.unwrap_or_else(|panic| {
            Err(ExecutionError::new(
                ExecutionErrorCode::CallbackError,
                crate::session::panic_error(panic).to_string(),
            ))
        });
        if let Err(cause) = result {
            *error = Some(
                ExecutionError::new(ExecutionErrorCode::CallbackError, cause.to_string())
                    .with_cause(Arc::new(cause)),
            );
        }
    }
}
#[derive(Default)]
struct Spill {
    prefix: Vec<u8>,
    bytes: usize,
    newlines: usize,
    path: Option<PathBuf>,
    writer: Option<Arc<dyn ShellSpillWriter>>,
}
impl Spill {
    async fn feed(
        &mut self,
        bytes: &[u8],
        threshold: ShellSpillOptions,
        filesystem: &NativeFileSystem,
        options: &NativeShellOptions,
        context: &Context,
    ) -> ExecutionResult<bool> {
        if self.writer.is_none() {
            self.bytes += bytes.len();
            self.newlines += bytes.iter().filter(|byte| **byte == b'\n').count();
            self.prefix.extend_from_slice(bytes);
            let lines = self.newlines + usize::from(bytes.last() != Some(&b'\n'));
            if (self.bytes as f64) <= threshold.after_bytes
                && (lines as f64) <= threshold.after_lines
            {
                return Ok(false);
            }
            let path = match &options.temp_file {
                Some(create) => create(context.clone()).await,
                None => {
                    filesystem
                        .create_temp_file(Some("pi-output-"), Some(".log"), context)
                        .await
                }
            }
            .map_err(|error| failure(ExecutionErrorCode::Unknown, error))?;
            self.path = Some(path.clone());
            self.writer = Some(match &options.spill_opener {
                Some(open) => open(path).await?,
                None => LocalSpillWriter::open(path).await?,
            });
            return self
                .writer
                .as_ref()
                .unwrap()
                .write(std::mem::take(&mut self.prefix))
                .await
                .map(|accepted| !accepted);
        }
        self.writer
            .as_ref()
            .unwrap()
            .write(bytes.to_vec())
            .await
            .map(|accepted| !accepted)
    }
}
#[derive(Clone)]
enum SpillStatus {
    Open,
    Closed,
    Failed(ExecutionError),
}
struct SpillState {
    queued: AtomicUsize,
    changed: watch::Sender<SpillStatus>,
}
enum SpillMessage {
    Write(Vec<u8>),
    Finish,
}
struct LocalSpillWriter {
    send: mpsc::UnboundedSender<SpillMessage>,
    state: Arc<SpillState>,
}
impl LocalSpillWriter {
    async fn open(path: PathBuf) -> ExecutionResult<Arc<dyn ShellSpillWriter>> {
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .await
            .map_err(|error| failure(ExecutionErrorCode::Unknown, error))?;
        let (send, mut receive) = mpsc::unbounded_channel();
        let (changed, _) = watch::channel(SpillStatus::Open);
        let state = Arc::new(SpillState {
            queued: AtomicUsize::new(0),
            changed,
        });
        let writer = state.clone();
        tokio::spawn(async move {
            let result = async {
                while let Some(message) = receive.recv().await {
                    match message {
                        SpillMessage::Write(bytes) => {
                            file.write_all(&bytes).await?;
                            writer.queued.fetch_sub(bytes.len(), Ordering::AcqRel);
                            writer.changed.send_modify(|_| {});
                        }
                        SpillMessage::Finish => break,
                    }
                }
                file.flush().await
            }
            .await;
            drop(file);
            writer.changed.send_replace(match result {
                Ok(()) => SpillStatus::Closed,
                Err(error) => SpillStatus::Failed(failure(ExecutionErrorCode::Unknown, error)),
            });
        });
        Ok(Arc::new(Self { send, state }))
    }
    fn wait(&self, drain: bool) -> ExecutionFuture<()> {
        let state = self.state.clone();
        let mut changed = state.changed.subscribe();
        Box::pin(async move {
            loop {
                match changed.borrow_and_update().clone() {
                    SpillStatus::Failed(error) => return Err(error),
                    SpillStatus::Closed => return Ok(()),
                    SpillStatus::Open if drain && state.queued.load(Ordering::Acquire) == 0 => {
                        return Ok(());
                    }
                    SpillStatus::Open => {}
                }
                changed
                    .changed()
                    .await
                    .map_err(|error| failure(ExecutionErrorCode::Unknown, error))?;
            }
        })
    }
}
impl ShellSpillWriter for LocalSpillWriter {
    fn write(&self, bytes: Vec<u8>) -> ExecutionFuture<bool> {
        let count = self.state.queued.fetch_add(bytes.len(), Ordering::AcqRel) + bytes.len();
        let result = self
            .send
            .send(SpillMessage::Write(bytes))
            .map_err(|_| ExecutionError::new(ExecutionErrorCode::Unknown, "Spill stream is closed"))
            .map(|_| count < SPILL_HIGH_WATER_MARK);
        Box::pin(async move { result })
    }
    fn drain(&self) -> ExecutionFuture<()> {
        self.wait(true)
    }
    fn finish(&self) -> ExecutionFuture<()> {
        let _ = self.send.send(SpillMessage::Finish);
        self.wait(false)
    }
    fn settled(&self) -> ExecutionFuture<()> {
        self.wait(false)
    }
}
