use super::line_scan::LineScan;
use crate::Context;
use std::{
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileErrorCode {
    Aborted,
    NotFound,
    PermissionDenied,
    NotDirectory,
    IsDirectory,
    Invalid,
    NotSupported,
    Unknown,
}
impl FileErrorCode {
    pub const ALL: [Self; 8] = [
        Self::Aborted,
        Self::NotFound,
        Self::PermissionDenied,
        Self::NotDirectory,
        Self::IsDirectory,
        Self::Invalid,
        Self::NotSupported,
        Self::Unknown,
    ];
    /// The reference's code string (`FileErrorCode` in `env/index.ts`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aborted => "aborted",
            Self::NotFound => "not_found",
            Self::PermissionDenied => "permission_denied",
            Self::NotDirectory => "not_directory",
            Self::IsDirectory => "is_directory",
            Self::Invalid => "invalid",
            Self::NotSupported => "not_supported",
            Self::Unknown => "unknown",
        }
    }
    /// The code for a reference code string; anything else is `Unknown`.
    pub fn from_name(name: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|code| code.as_str() == name)
            .unwrap_or(Self::Unknown)
    }
}
#[derive(Debug)]
struct Failure {
    code: FileErrorCode,
    message: String,
    path: Option<PathBuf>,
    cause: Option<Arc<dyn std::error::Error + Send + Sync>>,
}
#[derive(Clone, Debug)]
pub struct FileError(Arc<Failure>);
impl FileError {
    pub fn new(code: FileErrorCode, message: impl Into<String>, path: Option<PathBuf>) -> Self {
        Self(Arc::new(Failure {
            code,
            message: message.into(),
            path,
            cause: None,
        }))
    }
    pub fn from_io(error: std::io::Error, path: Option<PathBuf>) -> Self {
        use std::io::ErrorKind::*;
        let code = match error.kind() {
            NotFound => FileErrorCode::NotFound,
            PermissionDenied => FileErrorCode::PermissionDenied,
            NotADirectory => FileErrorCode::NotDirectory,
            IsADirectory => FileErrorCode::IsDirectory,
            InvalidInput | InvalidData => FileErrorCode::Invalid,
            Unsupported => FileErrorCode::NotSupported,
            _ => FileErrorCode::Unknown,
        };
        Self(Arc::new(Failure {
            code,
            message: error.to_string(),
            path,
            cause: Some(Arc::new(error)),
        }))
    }
    pub fn code(&self) -> FileErrorCode {
        self.0.code
    }
    pub fn path(&self) -> Option<&Path> {
        self.0.path.as_deref()
    }
    pub fn same_error(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.message)
    }
}
impl std::error::Error for FileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.cause.as_ref().map(|e| e.as_ref() as _)
    }
}
pub type FileResult<T> = std::result::Result<T, FileError>;
pub type FileFuture<T> = Pin<Box<dyn Future<Output = FileResult<T>> + Send + 'static>>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
}
#[derive(Clone, Debug)]
pub struct FileInfo {
    pub name: OsString,
    pub path: PathBuf,
    pub kind: FileKind,
    pub size: u64,
    pub mtime_ms: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextLine {
    pub text: String,
    pub terminated: bool,
}
pub trait TextLineReader: Send + Sync {
    fn read_line(&self, context: &Context) -> FileFuture<Option<TextLine>>;
    fn close(&self, context: &Context) -> FileFuture<()>;
}
pub trait BinaryReader: Send + Sync {
    fn info(&self, context: &Context) -> FileFuture<FileInfo>;
    fn read(&self, offset: f64, length: f64, context: &Context) -> FileFuture<Vec<u8>>;
    fn scan_lines(
        &self,
        start_line: f64,
        end_line: Option<f64>,
        context: &Context,
    ) -> FileFuture<LineScan>;
    fn close(&self, context: &Context) -> FileFuture<()>;
}
/// What `FileSystem::list_files` lists. Bounds are `f64` so that negative and
/// fractional values are rejected as `invalid` like the reference.
#[derive(Clone, Debug, Default)]
pub struct ListFilesOptions {
    /// Include names starting with `.`; default true. `.git` is never listed.
    pub hidden: Option<bool>,
    /// Inside a git work tree skip what git ignores; default true.
    pub gitignore: Option<bool>,
    /// Names of entries to skip with everything below them.
    pub exclude: Vec<OsString>,
    /// Most entries to list; default 50 000.
    pub max_entries: Option<f64>,
    /// Most bytes of the listing, each entry's UTF-8 path plus two; default 4 MiB, at most 15.9 MiB.
    pub max_bytes: Option<f64>,
    /// Time after which the listing stops; default 10 000 ms.
    pub timeout_ms: Option<f64>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileListEntry {
    /// Relative to the listed directory, with `/` separators on every system.
    pub path: String,
    /// A symbolic link is listed as such and not followed.
    pub kind: FileKind,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileListing {
    /// Depth first, parents before their entries, siblings in byte order of their names.
    pub entries: Vec<FileListEntry>,
    /// A bound in `ListFilesOptions` ended the listing early.
    pub truncated: bool,
}
pub struct DirPage {
    pub entries: Vec<FileInfo>,
    pub done: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WatchMode {
    Native,
    Polling,
}
#[derive(Clone, Debug, Default)]
pub struct WatchExclude {
    pub hidden: bool,
    pub names: Vec<OsString>,
}
#[derive(Clone, Debug)]
pub struct WatchTarget {
    pub path: PathBuf,
    pub recursive: bool,
    pub exclude: WatchExclude,
}
impl WatchTarget {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            recursive: false,
            exclude: WatchExclude::default(),
        }
    }
}
#[derive(Clone, Debug)]
pub enum WatchChange {
    Paths(Vec<PathBuf>),
    Overflow,
    Error(FileError),
}
pub type WatchCallback = Arc<dyn Fn(WatchChange) + Send + Sync>;
pub trait FileWatcher: Send + Sync {
    fn mode(&self) -> WatchMode;
    /// Seal delivery immediately and join any scan or callback already running.
    fn close(&self, context: &Context) -> FileFuture<()>;
}
pub trait DirReader: Send + Sync {
    fn next(&self, max_entries: f64, context: &Context) -> FileFuture<DirPage>;
    fn close(&self, context: &Context) -> FileFuture<()>;
}

/// Opened-file I/O can be local or a host capability. Reader state, offsets,
/// cancellation and decoding remain in the native reader implementation.
pub trait FileHandle: Send + Sync {
    fn info(&self, context: &Context) -> FileFuture<FileInfo>;
    fn read(&self, offset: u64, length: usize, context: &Context) -> FileFuture<Vec<u8>>;
    fn sync(&self, context: &Context) -> FileFuture<()>;
    fn close(&self, context: &Context) -> FileFuture<()>;
}
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenFileOptions {
    pub write: bool,
    pub regular: bool,
    pub no_follow: bool,
}
pub type FileOpener =
    Arc<dyn Fn(PathBuf, OpenFileOptions, Context) -> FileFuture<Arc<dyn FileHandle>> + Send + Sync>;

pub(crate) fn file_job<T: Send + 'static>(
    future: impl Future<Output = FileResult<T>> + Send + 'static,
) -> FileFuture<T> {
    let task = pi_chord::rt::spawn(future);
    Box::pin(async move {
        task.await
            .map_err(|error| FileError::new(FileErrorCode::Unknown, error.to_string(), None))?
    })
}
pub(crate) fn unsupported<T: Send + 'static>(path: Option<PathBuf>) -> FileFuture<T> {
    Box::pin(async move {
        Err(FileError::new(
            FileErrorCode::NotSupported,
            "Filesystem capability is not supported",
            path,
        ))
    })
}

/// Persistence capability shared by local, remote and embedded environments.
/// Every call returns an expected filesystem failure instead of panicking.
pub trait FileSystem: Send + Sync {
    fn id(&self) -> &str;
    fn cwd(&self) -> PathBuf;
    fn absolute_path(&self, path: &Path, context: &Context) -> FileFuture<PathBuf>;
    fn join_path(&self, parts: &[PathBuf], context: &Context) -> FileFuture<PathBuf>;
    fn read_binary_file(&self, path: &Path, context: &Context) -> FileFuture<Vec<u8>>;
    fn watch(
        &self,
        _targets: &[WatchTarget],
        _on_change: WatchCallback,
        _context: &Context,
    ) -> FileFuture<Arc<dyn FileWatcher>> {
        unsupported(None)
    }
    fn read_text_file(&self, path: &Path, context: &Context) -> FileFuture<String> {
        let read = self.read_binary_file(path, context);
        file_job(async move { Ok(String::from_utf8_lossy(&read.await?).into_owned()) })
    }
    fn open_text_line_reader(
        &self,
        path: &Path,
        _context: &Context,
    ) -> FileFuture<Arc<dyn TextLineReader>> {
        unsupported(Some(path.into()))
    }
    fn read_text_lines(
        &self,
        path: &Path,
        max_lines: Option<f64>,
        context: &Context,
    ) -> FileFuture<Vec<String>> {
        if max_lines.is_some_and(|max| max <= 0.0) {
            return Box::pin(async { Ok(Vec::new()) });
        }
        let opened = self.open_text_line_reader(path, context);
        let context = context.clone();
        file_job(async move {
            let reader = opened.await?;
            let result = async {
                let mut lines = Vec::new();
                while max_lines.is_none_or(|max| (lines.len() as f64) < max) {
                    let Some(line) = reader.read_line(&context).await? else {
                        break;
                    };
                    lines.push(line.text);
                }
                Ok(lines)
            }
            .await;
            let _ = reader.close(&context).await;
            result
        })
    }
    fn open_binary_reader(
        &self,
        path: &Path,
        _no_follow: bool,
        _context: &Context,
    ) -> FileFuture<Arc<dyn BinaryReader>> {
        unsupported(Some(path.into()))
    }
    fn file_info(&self, path: &Path, _context: &Context) -> FileFuture<FileInfo> {
        unsupported(Some(path.into()))
    }
    fn canonical_path(&self, path: &Path, _context: &Context) -> FileFuture<PathBuf> {
        unsupported(Some(path.into()))
    }
    fn exists(&self, path: &Path, context: &Context) -> FileFuture<bool> {
        let info = self.file_info(path, context);
        file_job(async move {
            match info.await {
                Ok(_) => Ok(true),
                Err(error) if error.code() == FileErrorCode::NotFound => Ok(false),
                Err(error) => Err(error),
            }
        })
    }
    /// Optional: the files, directories and symbolic links below a directory at
    /// any depth. Fails with `NotSupported` when the environment cannot list.
    fn list_files(
        &self,
        path: &Path,
        _options: &ListFilesOptions,
        _context: &Context,
    ) -> FileFuture<FileListing> {
        unsupported(Some(path.into()))
    }
    fn open_dir_reader(&self, path: &Path, _context: &Context) -> FileFuture<Arc<dyn DirReader>> {
        unsupported(Some(path.into()))
    }
    fn create_temp_dir(&self, _prefix: Option<&str>, _context: &Context) -> FileFuture<PathBuf> {
        unsupported(None)
    }
    fn create_temp_file(
        &self,
        _prefix: Option<&str>,
        _suffix: Option<&str>,
        _context: &Context,
    ) -> FileFuture<PathBuf> {
        unsupported(None)
    }
    fn write_file(&self, path: &Path, content: &[u8], context: &Context) -> FileFuture<()>;
    /// Preserve a text argument across hosted environments; local files encode
    /// it as UTF8, replacing unpaired UTF16 units as Node's writeFile does.
    fn write_text_file(
        &self,
        path: &Path,
        content: &crate::JsonString,
        context: &Context,
    ) -> FileFuture<()> {
        self.write_file(path, content.to_string_lossy().as_bytes(), context)
    }
    fn append_file(&self, path: &Path, content: &[u8], context: &Context) -> FileFuture<()>;
    fn truncate_file(&self, path: &Path, size: u64, context: &Context) -> FileFuture<()>;
    fn flush_file(&self, path: &Path, context: &Context) -> FileFuture<()>;
    fn rename_file(&self, source: &Path, destination: &Path, context: &Context) -> FileFuture<()>;
    fn list_dir(&self, path: &Path, context: &Context) -> FileFuture<Vec<FileInfo>>;
    fn create_dir(&self, path: &Path, recursive: bool, context: &Context) -> FileFuture<()>;
    fn remove(
        &self,
        path: &Path,
        recursive: bool,
        force: bool,
        context: &Context,
    ) -> FileFuture<()>;
}
