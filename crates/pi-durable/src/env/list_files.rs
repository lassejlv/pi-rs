//! `list_files`: the files and directories below a directory as `fd --hidden
//! --exclude .git` lists them, without following symbolic links and skipping
//! what git ignores. Uses the `ignore` crate, as fd and ripgrep do.

use super::{filesystem::*, readers::check};
use crate::Context;
use ignore::WalkBuilder;
use std::{
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

const DEFAULT_MAX_ENTRIES: u64 = 50_000;
const DEFAULT_MAX_BYTES: u64 = 4 * 1024 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
/// The reference caps a listing at its largest daemon payload.
const MAX_BYTES: u64 = 16 * 1024 * 1024 - 64 * 1024;

fn bound(name: &str, value: Option<f64>, default: u64, path: &Path) -> FileResult<u64> {
    match value {
        None => Ok(default),
        Some(value)
            if value.is_finite()
                && value >= 0.0
                && value.fract() == 0.0
                && value <= crate::MAX_SAFE_INTEGER as f64 =>
        {
            Ok(value as u64)
        }
        Some(_) => Err(FileError::new(
            FileErrorCode::Invalid,
            format!("{name} must be a non-negative safe integer"),
            Some(path.into()),
        )),
    }
}

/// Names that are not UTF-8 get replacement characters, as `readdir` reports them.
fn relative(root: &Path, path: &Path) -> Option<String> {
    let mut text = String::new();
    for component in path.strip_prefix(root).ok()?.components() {
        let Component::Normal(name) = component else {
            return None;
        };
        if !text.is_empty() {
            text.push('/');
        }
        text.push_str(&name.to_string_lossy());
    }
    (!text.is_empty()).then_some(text)
}

pub(crate) fn list_files(
    root: PathBuf,
    options: ListFilesOptions,
    context: Context,
) -> FileFuture<FileListing> {
    let admission = (|| {
        let max_entries = bound(
            "maxEntries",
            options.max_entries,
            DEFAULT_MAX_ENTRIES,
            &root,
        )?;
        let max_bytes =
            bound("maxBytes", options.max_bytes, DEFAULT_MAX_BYTES, &root)?.min(MAX_BYTES);
        let timeout = bound("timeoutMs", options.timeout_ms, DEFAULT_TIMEOUT_MS, &root)?;
        check(&context, &root)?;
        Ok((max_entries, max_bytes, Duration::from_millis(timeout)))
    })();
    let (max_entries, max_bytes, timeout) = match admission {
        Ok(bounds) => bounds,
        Err(error) => return Box::pin(async { Err(error) }),
    };
    let (send, receive) = tokio::sync::oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let _ = send.send(walk(
            &root,
            &options,
            max_entries,
            max_bytes,
            timeout,
            &context,
        ));
    });
    Box::pin(async move {
        receive
            .await
            .map_err(|error| FileError::new(FileErrorCode::Unknown, error.to_string(), None))?
    })
}

fn walk(
    root: &Path,
    options: &ListFilesOptions,
    max_entries: u64,
    max_bytes: u64,
    timeout: Duration,
    context: &Context,
) -> FileResult<FileListing> {
    let metadata =
        std::fs::metadata(root).map_err(|error| FileError::from_io(error, Some(root.into())))?;
    if !metadata.is_dir() {
        return Err(FileError::new(
            FileErrorCode::NotDirectory,
            format!("not a directory, scandir '{}'", root.display()),
            Some(root.into()),
        ));
    }
    let gitignore = options.gitignore.unwrap_or(true);
    let exclude = options.exclude.clone();
    let walker = WalkBuilder::new(root)
        .hidden(!options.hidden.unwrap_or(true))
        .ignore(false)
        .parents(gitignore)
        .git_ignore(gitignore)
        .git_exclude(gitignore)
        .git_global(gitignore)
        .require_git(true)
        .follow_links(false)
        // The global excludes file is found relative to the listed directory.
        .current_dir(root)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(move |entry| {
            let name = entry.file_name();
            name != ".git" && !exclude.iter().any(|excluded| excluded == name)
        })
        .build();
    let started = Instant::now();
    let (mut entries, mut bytes, mut truncated) = (Vec::new(), 0u64, false);
    for entry in walker {
        check(context, root)?;
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 {
            continue;
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        let kind = if file_type.is_symlink() {
            FileKind::Symlink
        } else if file_type.is_dir() {
            FileKind::Directory
        } else if file_type.is_file() {
            FileKind::File
        } else {
            continue;
        };
        let Some(path) = relative(root, entry.path()) else {
            continue;
        };
        let size = path.len() as u64 + 2;
        if entries.len() as u64 >= max_entries
            || bytes + size > max_bytes
            || started.elapsed() > timeout
        {
            truncated = true;
            break;
        }
        bytes += size;
        entries.push(FileListEntry { path, kind });
    }
    Ok(FileListing { entries, truncated })
}
