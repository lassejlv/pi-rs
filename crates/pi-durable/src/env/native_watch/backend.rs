use super::{NativeWatchCallback, NativeWatchEvent, NativeWatchHandle};
use crate::env::{FileError, FileErrorCode, FileResult};
use notify::{
    Config, EventKind, RecursiveMode, Watcher,
    event::{AccessKind, AccessMode},
};
use std::path::PathBuf;

pub(super) fn open(
    path: PathBuf,
    recursive: bool,
    callback: NativeWatchCallback,
) -> FileResult<Box<dyn NativeWatchHandle>> {
    let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    let logical = path.clone();
    let mut watcher = notify::RecommendedWatcher::new(
        move |event: notify::Result<notify::Event>| {
            let event = match event {
                Ok(event) => event,
                Err(_) => {
                    callback(NativeWatchEvent::Error);
                    return;
                }
            };
            if event.need_rescan() {
                callback(NativeWatchEvent::Overflow);
                return;
            }
            if matches!(
                event.kind,
                EventKind::Access(
                    AccessKind::Open(_) | AccessKind::Read | AccessKind::Close(AccessMode::Read)
                )
            ) {
                return;
            }
            if event.paths.is_empty() {
                callback(NativeWatchEvent::Change(None));
            }
            for path in event.paths {
                if let Ok(relative) = path
                    .strip_prefix(&canonical)
                    .or_else(|_| path.strip_prefix(&logical))
                {
                    callback(NativeWatchEvent::Change(
                        (!relative.as_os_str().is_empty()).then(|| relative.into()),
                    ));
                }
            }
        },
        Config::default(),
    )
    .map_err(|error| convert(error, &path))?;
    watcher
        .watch(
            &path,
            if recursive {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            },
        )
        .map_err(|error| convert(error, &path))?;
    Ok(Box::new(watcher))
}
fn convert(error: notify::Error, path: &std::path::Path) -> FileError {
    match error.kind {
        notify::ErrorKind::PathNotFound => FileError::new(
            FileErrorCode::NotFound,
            "No path was found",
            Some(path.into()),
        ),
        notify::ErrorKind::Io(error) => FileError::from_io(error, Some(path.into())),
        _ => FileError::new(FileErrorCode::Unknown, error.to_string(), Some(path.into())),
    }
}
