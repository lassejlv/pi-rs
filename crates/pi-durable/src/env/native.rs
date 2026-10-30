use super::filesystem::*;
use crate::Context;
use parking_lot::RwLock;
use std::{
    ffi::OsString,
    io::Write,
    path::{Component, Path, PathBuf},
    time::UNIX_EPOCH,
};

#[derive(Clone)]
pub struct NativeFileSystem {
    cwd: std::sync::Arc<RwLock<PathBuf>>,
    opener: Option<FileOpener>,
    shell: super::native_shell::NativeShell,
    watch: super::native_watch::NativeWatchOptions,
}
impl NativeFileSystem {
    pub fn new(cwd: impl Into<PathBuf>) -> FileResult<Self> {
        let cwd = cwd.into();
        let cwd = if cwd.is_absolute() {
            cwd
        } else {
            std::env::current_dir()
                .map_err(|e| FileError::from_io(e, None))?
                .join(cwd)
        };
        Ok(Self {
            cwd: std::sync::Arc::new(RwLock::new(normalize(&cwd))),
            opener: None,
            shell: Default::default(),
            watch: Default::default(),
        })
    }
    pub fn with_file_opener(cwd: impl Into<PathBuf>, opener: FileOpener) -> FileResult<Self> {
        let mut filesystem = Self::new(cwd)?;
        filesystem.opener = Some(opener);
        Ok(filesystem)
    }
    pub fn with_shell_options(mut self, options: super::native_shell::NativeShellOptions) -> Self {
        self.shell.options = options;
        self
    }
    pub fn with_watch_options(mut self, options: super::native_watch::NativeWatchOptions) -> Self {
        self.watch = options;
        self
    }
    pub fn with_shell_host(mut self, host: super::native_shell::ShellHost) -> Self {
        self.shell.options.host = Some(std::sync::Arc::new(move || host.clone()));
        self
    }
    pub fn set_cwd(&self, cwd: impl AsRef<Path>) {
        let cwd = self.resolve(cwd.as_ref());
        *self.cwd.write() = cwd;
    }
    pub(super) fn resolve(&self, path: &Path) -> PathBuf {
        let mut path = path.to_path_buf();
        if path == Path::new("~") {
            if let Some(home) = dirs::home_dir() {
                path = home;
            }
        } else if let Ok(rest) = path.strip_prefix("~")
            && let Some(home) = dirs::home_dir()
        {
            path = home.join(rest);
        } else if let Some(text) = path.to_str().filter(|text| text.starts_with("file://"))
            && let Ok(url) = url::Url::parse(text)
            && !url.path().to_ascii_lowercase().contains("%2f")
            && let Ok(decoded) = url.to_file_path()
            && decoded.to_str().is_some()
        {
            path = decoded;
        }
        normalize(&self.cwd.read().join(path))
    }
    fn open_handle(
        &self,
        path: PathBuf,
        options: OpenFileOptions,
        context: &Context,
    ) -> FileFuture<std::sync::Arc<dyn FileHandle>> {
        if let Err(error) = check(context, &path) {
            return Box::pin(async { Err(error) });
        }
        let error_path = path.clone();
        let opened = match &self.opener {
            Some(open) => open(path, options, context.clone()),
            None => NativeFileHandle::open(path, options, context),
        };
        let context = context.clone();
        file_job(async move {
            let file = opened.await?;
            let result = async {
                if options.regular {
                    let info = file.info(&context).await?;
                    if info.kind != FileKind::File {
                        return Err(FileError::new(
                            if info.kind == FileKind::Directory {
                                FileErrorCode::IsDirectory
                            } else {
                                FileErrorCode::Invalid
                            },
                            "Not a regular file",
                            Some(info.path),
                        ));
                    }
                }
                context.check().map_err(|_| {
                    FileError::new(FileErrorCode::Aborted, "aborted", Some(error_path))
                })?;
                Ok(())
            }
            .await;
            if let Err(error) = result {
                let _ = file.close(&context).await;
                return Err(error);
            }
            Ok(file)
        })
    }
    fn io<T: Send + 'static>(
        &self,
        path: PathBuf,
        context: &Context,
        operation: impl FnOnce(&Path) -> std::io::Result<T> + Send + 'static,
    ) -> FileFuture<T> {
        let context = context.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let result = (|| {
                check(&context, &path)?;
                let value =
                    operation(&path).map_err(|e| FileError::from_io(e, Some(path.clone())))?;
                check(&context, &path)?;
                Ok(value)
            })();
            let _ = send.send(result);
        });
        Box::pin(async move {
            receive
                .await
                .map_err(|e| FileError::new(FileErrorCode::Unknown, e.to_string(), None))?
        })
    }
}
impl super::shell::Shell for NativeFileSystem {
    fn exec(
        &self,
        command: super::shell::ShellCommand,
        options: super::shell::ShellExecOptions,
        context: &Context,
    ) -> super::shell::ExecutionFuture<super::shell::ShellExecResult> {
        self.shell.exec(self.clone(), command, options, context)
    }
    fn cleanup(&self, _context: &Context) -> super::shell::ExecutionFuture<()> {
        self.shell.cleanup()
    }
}
/// The local filesystem and shell share one environment authority.
pub type NativeExecutionEnv = NativeFileSystem;
fn check(context: &Context, path: &Path) -> FileResult<()> {
    context
        .check()
        .map_err(|_| FileError::new(FileErrorCode::Aborted, "aborted", Some(path.into())))
}
fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(result.components().next_back(), Some(Component::Normal(_))) {
                    result.pop();
                } else if !result.has_root() {
                    result.push("..");
                }
            }
            component => result.push(component.as_os_str()),
        }
    }
    if result.as_os_str().is_empty() {
        result.push(".");
    }
    result
}
impl FileSystem for NativeFileSystem {
    fn watch(
        &self,
        targets: &[WatchTarget],
        on_change: WatchCallback,
        context: &Context,
    ) -> FileFuture<std::sync::Arc<dyn FileWatcher>> {
        if context.check().is_err() {
            return Box::pin(async {
                Err(FileError::new(FileErrorCode::Aborted, "aborted", None))
            });
        }
        let targets = targets
            .iter()
            .map(|target| WatchTarget {
                path: self.resolve(&target.path),
                ..target.clone()
            })
            .collect();
        let opened =
            super::native_watch::NativeFileWatcher::open(targets, on_change, self.watch.clone());
        let context = context.clone();
        file_job(async move {
            let watcher = opened.await?;
            if context.check().is_err() {
                watcher.close(&context).await?;
                return Err(FileError::new(FileErrorCode::Aborted, "aborted", None));
            }
            Ok(watcher as std::sync::Arc<dyn FileWatcher>)
        })
    }
    fn id(&self) -> &str {
        "node:local"
    }
    fn cwd(&self) -> PathBuf {
        self.cwd.read().clone()
    }
    fn absolute_path(&self, path: &Path, _context: &Context) -> FileFuture<PathBuf> {
        let path = self.resolve(path);
        Box::pin(async { Ok(path) })
    }
    fn join_path(&self, parts: &[PathBuf], _context: &Context) -> FileFuture<PathBuf> {
        let mut joined = OsString::new();
        for part in parts.iter().filter(|p| !p.as_os_str().is_empty()) {
            if !joined.is_empty() {
                joined.push(std::path::MAIN_SEPARATOR_STR);
            }
            joined.push(part.as_os_str());
        }
        let path = normalize(Path::new(&joined));
        Box::pin(async { Ok(path) })
    }
    fn read_binary_file(&self, path: &Path, context: &Context) -> FileFuture<Vec<u8>> {
        self.io(self.resolve(path), context, |path| std::fs::read(path))
    }
    fn open_text_line_reader(
        &self,
        path: &Path,
        context: &Context,
    ) -> FileFuture<std::sync::Arc<dyn TextLineReader>> {
        let path = self.resolve(path);
        let opened = self.open_handle(path.clone(), OpenFileOptions::default(), context);
        file_job(async move {
            Ok(
                std::sync::Arc::new(super::readers::NativeTextLineReader::new(
                    opened.await?,
                    path,
                )) as _,
            )
        })
    }
    fn open_binary_reader(
        &self,
        path: &Path,
        no_follow: bool,
        context: &Context,
    ) -> FileFuture<std::sync::Arc<dyn BinaryReader>> {
        let path = self.resolve(path);
        let opened = self.open_handle(
            path.clone(),
            OpenFileOptions {
                regular: true,
                no_follow,
                ..Default::default()
            },
            context,
        );
        file_job(async move {
            Ok(
                std::sync::Arc::new(super::readers::NativeBinaryReader::new(opened.await?, path))
                    as _,
            )
        })
    }
    fn file_info(&self, path: &Path, context: &Context) -> FileFuture<FileInfo> {
        let path = self.resolve(path);
        let read = self.io(path.clone(), context, |path| {
            std::fs::symlink_metadata(path)
        });
        file_job(async move { metadata_info(&path, &read.await?) })
    }
    fn canonical_path(&self, path: &Path, context: &Context) -> FileFuture<PathBuf> {
        self.io(self.resolve(path), context, |path| {
            std::fs::canonicalize(path)
        })
    }
    fn list_files(
        &self,
        path: &Path,
        options: &ListFilesOptions,
        context: &Context,
    ) -> FileFuture<FileListing> {
        super::list_files::list_files(self.resolve(path), options.clone(), context.clone())
    }
    fn open_dir_reader(
        &self,
        path: &Path,
        context: &Context,
    ) -> FileFuture<std::sync::Arc<dyn DirReader>> {
        let path = self.resolve(path);
        let opened = self.io(path.clone(), context, |path| std::fs::read_dir(path));
        file_job(async move {
            Ok(std::sync::Arc::new(NativeDirReader {
                path,
                dir: std::sync::Arc::new(parking_lot::Mutex::new(Some(opened.await?))),
                done: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                closed: std::sync::atomic::AtomicBool::new(false),
            }) as _)
        })
    }
    fn create_temp_dir(&self, prefix: Option<&str>, context: &Context) -> FileFuture<PathBuf> {
        let prefix = prefix.unwrap_or("tmp-").to_owned();
        let context = context.clone();
        file_job(async move {
            check(&context, &std::env::temp_dir())?;
            tokio::task::spawn_blocking(move || {
                loop {
                    let suffix = uuid::Uuid::new_v4().simple().to_string();
                    let path = normalize(Path::new(&format!(
                        "{}/{}{}",
                        std::env::temp_dir().display(),
                        prefix,
                        &suffix[..6]
                    )));
                    #[cfg(unix)]
                    let created = {
                        use std::os::unix::fs::DirBuilderExt;
                        std::fs::DirBuilder::new().mode(0o700).create(&path)
                    };
                    #[cfg(not(unix))]
                    let created = std::fs::create_dir(&path);
                    match created {
                        Ok(()) => return Ok(path),
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                        Err(error) => return Err(FileError::from_io(error, Some(path))),
                    }
                }
            })
            .await
            .map_err(|error| FileError::new(FileErrorCode::Unknown, error.to_string(), None))?
        })
    }
    fn create_temp_file(
        &self,
        prefix: Option<&str>,
        suffix: Option<&str>,
        context: &Context,
    ) -> FileFuture<PathBuf> {
        let directory = self.create_temp_dir(Some("tmp-"), context);
        let (prefix, suffix) = (
            prefix.unwrap_or_default().to_owned(),
            suffix.unwrap_or_default().to_owned(),
        );
        file_job(async move {
            let directory = directory.await?;
            let path = normalize(Path::new(&format!(
                "{}/{prefix}{}{suffix}",
                directory.display(),
                uuid::Uuid::new_v4()
            )));
            let write = path.clone();
            tokio::task::spawn_blocking(move || {
                std::fs::write(&write, []).map_err(|error| FileError::from_io(error, Some(write)))
            })
            .await
            .map_err(|error| {
                FileError::new(
                    FileErrorCode::Unknown,
                    error.to_string(),
                    Some(path.clone()),
                )
            })??;
            Ok(path)
        })
    }
    fn write_file(&self, path: &Path, content: &[u8], context: &Context) -> FileFuture<()> {
        let content = content.to_vec();
        self.io(self.resolve(path), context, move |path| {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, content)
        })
    }
    fn append_file(&self, path: &Path, content: &[u8], context: &Context) -> FileFuture<()> {
        let content = content.to_vec();
        self.io(self.resolve(path), context, move |path| {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(path)?;
            file.write_all(&content)
        })
    }
    fn truncate_file(&self, path: &Path, size: u64, context: &Context) -> FileFuture<()> {
        if let Err(error) = check(context, &self.resolve(path)) {
            return Box::pin(async { Err(error) });
        }
        if size > crate::MAX_SAFE_INTEGER as u64 {
            let path = self.resolve(path);
            return Box::pin(async move {
                Err(FileError::new(
                    FileErrorCode::Invalid,
                    "File size must be a non-negative safe integer",
                    Some(path),
                ))
            });
        }
        self.io(self.resolve(path), context, move |path| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?
                .set_len(size)
        })
    }
    fn flush_file(&self, path: &Path, context: &Context) -> FileFuture<()> {
        let path = self.resolve(path);
        let opened = self.open_handle(
            path.clone(),
            OpenFileOptions {
                write: true,
                ..Default::default()
            },
            context,
        );
        let context = context.clone();
        file_job(async move {
            let file = opened.await?;
            let result = async {
                if file.info(&context).await?.kind == FileKind::Directory {
                    return Err(FileError::new(
                        FileErrorCode::IsDirectory,
                        "Is a directory",
                        Some(path.clone()),
                    ));
                }
                file.sync(&context).await?;
                check(&context, &path)
            }
            .await;
            let _ = file.close(&context).await;
            result
        })
    }
    fn rename_file(&self, source: &Path, destination: &Path, context: &Context) -> FileFuture<()> {
        let destination = self.resolve(destination);
        self.io(self.resolve(source), context, move |source| {
            std::fs::rename(source, destination)
        })
    }
    fn list_dir(&self, path: &Path, context: &Context) -> FileFuture<Vec<FileInfo>> {
        let (path, context) = (self.resolve(path), context.clone());
        file_job(async move {
            tokio::task::spawn_blocking(move || {
                check(&context, &path)?;
                let mut result = Vec::new();
                for entry in std::fs::read_dir(&path)
                    .map_err(|error| FileError::from_io(error, Some(path.clone())))?
                {
                    let entry =
                        entry.map_err(|error| FileError::from_io(error, Some(path.clone())))?;
                    check(&context, &path)?;
                    let entry_path = entry.path();
                    let metadata = std::fs::symlink_metadata(&entry_path)
                        .map_err(|error| FileError::from_io(error, Some(entry_path.clone())))?;
                    match metadata_info(&entry_path, &metadata) {
                        Ok(info) => result.push(info),
                        Err(error) if error.code() == FileErrorCode::Invalid => {}
                        Err(error) => return Err(error),
                    }
                }
                Ok(result)
            })
            .await
            .map_err(|error| FileError::new(FileErrorCode::Unknown, error.to_string(), None))?
        })
    }
    fn create_dir(&self, path: &Path, recursive: bool, context: &Context) -> FileFuture<()> {
        self.io(self.resolve(path), context, move |path| {
            if recursive {
                std::fs::create_dir_all(path)
            } else {
                std::fs::create_dir(path)
            }
        })
    }
    fn remove(
        &self,
        path: &Path,
        recursive: bool,
        force: bool,
        context: &Context,
    ) -> FileFuture<()> {
        self.io(self.resolve(path), context, move |path| {
            let result = if recursive {
                match std::fs::symlink_metadata(path) {
                    Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
                    _ => std::fs::remove_file(path),
                }
            } else {
                std::fs::remove_file(path)
            };
            match result {
                Err(e) if force && e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                result => result,
            }
        })
    }
}

pub(crate) fn metadata_info(path: &Path, metadata: &std::fs::Metadata) -> FileResult<FileInfo> {
    let kind = if metadata.is_file() {
        FileKind::File
    } else if metadata.is_dir() {
        FileKind::Directory
    } else if metadata.is_symlink() {
        FileKind::Symlink
    } else {
        return Err(FileError::new(
            FileErrorCode::Invalid,
            "Unsupported file type",
            Some(path.into()),
        ));
    };
    Ok(FileInfo {
        name: path.file_name().unwrap_or_default().to_owned(),
        path: path.into(),
        kind,
        size: metadata.len(),
        mtime_ms: metadata
            .modified()
            .map_err(|error| FileError::from_io(error, Some(path.into())))?
            .duration_since(UNIX_EPOCH)
            .map_or_else(
                |error| -(error.duration().as_secs_f64() * 1000.0),
                |time| time.as_secs_f64() * 1000.0,
            ),
    })
}

struct NativeFileHandle {
    file: std::sync::Arc<parking_lot::Mutex<Option<std::fs::File>>>,
    path: PathBuf,
}
impl NativeFileHandle {
    fn open(
        path: PathBuf,
        options: OpenFileOptions,
        context: &Context,
    ) -> FileFuture<std::sync::Arc<dyn FileHandle>> {
        let context = context.clone();
        file_job(async move {
            tokio::task::spawn_blocking(move || {
                check(&context, &path)?;
                let mut open = std::fs::OpenOptions::new();
                open.read(true).write(options.write);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    let flags = if options.regular { libc::O_NONBLOCK } else { 0 }
                        | if options.no_follow {
                            libc::O_NOFOLLOW
                        } else {
                            0
                        };
                    open.custom_flags(flags);
                }
                #[cfg(windows)]
                if options.no_follow
                    && std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_symlink())
                {
                    return Err(FileError::new(
                        FileErrorCode::Invalid,
                        "Refusing to follow a symbolic link",
                        Some(path),
                    ));
                }
                let file = open.open(&path).map_err(|error| {
                    #[cfg(unix)]
                    if options.no_follow
                        && matches!(error.raw_os_error(), Some(libc::ELOOP) | Some(libc::EMLINK))
                    {
                        return FileError::new(
                            FileErrorCode::Invalid,
                            "Refusing to follow a symbolic link",
                            Some(path.clone()),
                        );
                    }
                    FileError::from_io(error, Some(path.clone()))
                })?;
                if options.regular {
                    let metadata = file
                        .metadata()
                        .map_err(|error| FileError::from_io(error, Some(path.clone())))?;
                    if !metadata.is_file() {
                        return Err(if metadata.is_dir() {
                            FileError::new(
                                FileErrorCode::IsDirectory,
                                "EISDIR: illegal operation on a directory, read",
                                Some(path),
                            )
                        } else {
                            FileError::new(FileErrorCode::Invalid, "Not a regular file", Some(path))
                        });
                    }
                }
                check(&context, &path)?;
                Ok(std::sync::Arc::new(Self {
                    file: std::sync::Arc::new(parking_lot::Mutex::new(Some(file))),
                    path,
                }) as std::sync::Arc<dyn FileHandle>)
            })
            .await
            .map_err(|error| FileError::new(FileErrorCode::Unknown, error.to_string(), None))?
        })
    }
    fn operation<T: Send + 'static>(
        &self,
        context: &Context,
        operation: impl FnOnce(&std::fs::File, &Path) -> FileResult<T> + Send + 'static,
    ) -> FileFuture<T> {
        let (file, path, context) = (self.file.clone(), self.path.clone(), context.clone());
        file_job(async move {
            tokio::task::spawn_blocking(move || {
                check(&context, &path)?;
                let file = file.lock();
                let file = file
                    .as_ref()
                    .ok_or_else(|| super::readers::invalid("File handle is closed", &path))?;
                operation(file, &path)
            })
            .await
            .map_err(|error| FileError::new(FileErrorCode::Unknown, error.to_string(), None))?
        })
    }
}
impl FileHandle for NativeFileHandle {
    fn info(&self, context: &Context) -> FileFuture<FileInfo> {
        self.operation(context, |file, path| {
            metadata_info(
                path,
                &file
                    .metadata()
                    .map_err(|error| FileError::from_io(error, Some(path.into())))?,
            )
        })
    }
    fn read(&self, offset: u64, length: usize, context: &Context) -> FileFuture<Vec<u8>> {
        self.operation(context, move |file, path| {
            let mut bytes = vec![0; length];
            #[cfg(unix)]
            let count = {
                use std::os::unix::fs::FileExt;
                file.read_at(&mut bytes, offset)
            };
            #[cfg(windows)]
            let count = {
                use std::os::windows::fs::FileExt;
                file.seek_read(&mut bytes, offset)
            };
            let count = count.map_err(|error| FileError::from_io(error, Some(path.into())))?;
            bytes.truncate(count);
            Ok(bytes)
        })
    }
    fn sync(&self, context: &Context) -> FileFuture<()> {
        self.operation(context, |file, path| {
            file.sync_all()
                .map_err(|error| FileError::from_io(error, Some(path.into())))
        })
    }
    fn close(&self, _context: &Context) -> FileFuture<()> {
        let file = self.file.clone();
        file_job(async move {
            tokio::task::spawn_blocking(move || {
                file.lock().take();
            })
            .await
            .map_err(|error| FileError::new(FileErrorCode::Unknown, error.to_string(), None))?;
            Ok(())
        })
    }
}

struct NativeDirReader {
    path: PathBuf,
    dir: std::sync::Arc<parking_lot::Mutex<Option<std::fs::ReadDir>>>,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    closed: std::sync::atomic::AtomicBool,
}
impl DirReader for NativeDirReader {
    fn next(&self, max_entries: f64, context: &Context) -> FileFuture<DirPage> {
        let admission = check(context, &self.path).and_then(|_| {
            if self.closed.load(std::sync::atomic::Ordering::Acquire) {
                Err(super::readers::invalid(
                    "Directory reader is closed",
                    &self.path,
                ))
            } else {
                Ok(())
            }
        });
        if let Err(error) = admission {
            return Box::pin(async { Err(error) });
        }
        let (path, dir, done, context) = (
            self.path.clone(),
            self.dir.clone(),
            self.done.clone(),
            context.clone(),
        );
        file_job(async move {
            tokio::task::spawn_blocking(move || {
                use std::sync::atomic::Ordering;
                check(&context, &path)?;
                let mut guard = dir.lock();
                let dir = guard
                    .as_mut()
                    .ok_or_else(|| super::readers::invalid("Directory reader is closed", &path))?;
                if !max_entries.is_finite()
                    || max_entries <= 0.0
                    || max_entries.fract() != 0.0
                    || max_entries > crate::MAX_SAFE_INTEGER as f64
                {
                    return Err(super::readers::invalid(
                        "maxEntries must be a positive safe integer",
                        &path,
                    ));
                }
                let mut entries = Vec::new();
                while !done.load(Ordering::Acquire) && (entries.len() as f64) < max_entries {
                    let Some(entry) = dir.next() else {
                        done.store(true, Ordering::Release);
                        break;
                    };
                    let entry =
                        entry.map_err(|error| FileError::from_io(error, Some(path.clone())))?;
                    check(&context, &path)?;
                    let path = entry.path();
                    let metadata = match std::fs::symlink_metadata(&path) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(FileError::from_io(error, Some(path))),
                    };
                    if let Ok(info) = metadata_info(&path, &metadata) {
                        entries.push(info);
                    }
                }
                Ok(DirPage {
                    entries,
                    done: done.load(Ordering::Acquire),
                })
            })
            .await
            .map_err(|error| FileError::new(FileErrorCode::Unknown, error.to_string(), None))?
        })
    }
    fn close(&self, _context: &Context) -> FileFuture<()> {
        if self.closed.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return Box::pin(async { Ok(()) });
        }
        let dir = self.dir.clone();
        file_job(async move {
            tokio::task::spawn_blocking(move || {
                dir.lock().take();
            })
            .await
            .map_err(|error| FileError::new(FileErrorCode::Unknown, error.to_string(), None))?;
            Ok(())
        })
    }
}
