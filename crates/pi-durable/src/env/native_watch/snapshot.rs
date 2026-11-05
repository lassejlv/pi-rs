use crate::env::{FileError, FileErrorCode, FileResult, WatchMode, WatchTarget};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::Metadata,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    File,
    Directory,
    Symlink,
    Other,
}
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Entry {
    pub kind: Kind,
    pub dev: u64,
    pub ino: u64,
    size: u64,
    mtime_ms: f64,
    hash: Option<[u8; 32]>,
}
pub(super) type Snapshot = BTreeMap<PathBuf, Entry>;
pub(super) struct Scan {
    pub snapshot: Snapshot,
    pub linked_files: HashSet<PathBuf>,
}
pub(super) fn ancestors(path: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut parent = path.parent();
    while let Some(path) = parent {
        result.push(path.into());
        parent = path.parent();
    }
    result
}
fn kind(metadata: &Metadata) -> Kind {
    if metadata.is_file() {
        Kind::File
    } else if metadata.is_dir() {
        Kind::Directory
    } else if metadata.is_symlink() {
        Kind::Symlink
    } else {
        Kind::Other
    }
}
fn milliseconds(time: SystemTime) -> f64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(time) => time.as_secs_f64() * 1000.0,
        Err(time) => -time.duration().as_secs_f64() * 1000.0,
    }
}
fn observe(path: &Path, follow: bool) -> std::io::Result<(Metadata, (u64, u64))> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = if follow {
            std::fs::metadata(path)?
        } else {
            std::fs::symlink_metadata(path)?
        };
        let identity = (metadata.dev(), metadata.ino());
        Ok((metadata, identity))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Query identity briefly, allowing both rename and deletion. A snapshot
        // must never retain directory handles on Windows.
        let file = std::fs::OpenOptions::new()
            .access_mode(0x80)
            .share_mode(7)
            .custom_flags(0x02000000 | if follow { 0 } else { 0x00200000 })
            .open(path)?;
        // Metadata and identity must refer to the same object if the path is
        // replaced while this scan is in progress.
        let metadata = file.metadata()?;
        let info = winapi_util::file::information(&file)?;
        Ok((metadata, (info.volume_serial_number(), info.file_index())))
    }
}
fn record(path: &Path, metadata: &Metadata, (dev, ino): (u64, u64), mode: WatchMode) -> Entry {
    let kind = kind(metadata);
    let mtime_ms = metadata.modified().map_or(0.0, milliseconds);
    let hash = if mode == WatchMode::Polling
        && kind == Kind::File
        && metadata.len() <= 256 * 1024
        && milliseconds(SystemTime::now()) - mtime_ms < 5000.0
    {
        std::fs::read(path)
            .ok()
            .map(|bytes| Sha256::digest(bytes).into())
    } else {
        None
    };
    Entry {
        kind,
        dev,
        ino,
        size: if kind == Kind::Directory {
            0
        } else {
            metadata.len()
        },
        mtime_ms: if kind == Kind::Directory {
            0.0
        } else {
            mtime_ms
        },
        hash,
    }
}
pub(super) fn excluded(target: &WatchTarget, name: &std::ffi::OsStr) -> bool {
    (target.exclude.hidden && name.to_string_lossy().starts_with('.'))
        || target.exclude.names.iter().any(|excluded| excluded == name)
}
pub(super) fn capture(
    targets: &[WatchTarget],
    mode: WatchMode,
    max_directories: f64,
) -> FileResult<Scan> {
    let mut snapshot = Snapshot::new();
    let mut linked_files = HashSet::new();
    let mut counted = HashSet::<PathBuf>::new();
    let mut traversed = HashSet::<(usize, PathBuf)>::new();
    let mut listed = HashMap::<PathBuf, Kind>::new();
    let count = |counted: &mut HashSet<PathBuf>, path: &Path| -> FileResult<()> {
        counted.insert(path.into());
        if counted.len() as f64 > max_directories {
            Err(FileError::new(
                FileErrorCode::Invalid,
                format!(
                    "Watched paths exceed {} directories",
                    pi_json::number_to_string(max_directories)
                ),
                None,
            ))
        } else {
            Ok(())
        }
    };
    for (index, target) in targets.iter().enumerate() {
        for ancestor in ancestors(&target.path) {
            if snapshot.contains_key(&ancestor) {
                continue;
            }
            if let Ok((metadata, identity)) = observe(&ancestor, false) {
                let mut entry = record(&ancestor, &metadata, identity, WatchMode::Native);
                entry.size = 0;
                entry.mtime_ms = 0.0;
                snapshot.insert(ancestor, entry);
            }
        }
        let (metadata, identity) = match observe(&target.path, true) {
            Ok(observed) => observed,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(FileError::from_io(error, Some(target.path.clone())));
            }
            Err(_) => continue,
        };
        snapshot.insert(
            target.path.clone(),
            record(&target.path, &metadata, identity, mode),
        );
        if metadata.is_file()
            && std::fs::symlink_metadata(&target.path).is_ok_and(|metadata| metadata.is_symlink())
        {
            linked_files.insert(target.path.clone());
        }
        if !metadata.is_dir() {
            continue;
        }
        count(&mut counted, &target.path)?;
        let mut pending = vec![target.path.clone()];
        while let Some(directory) = pending.pop() {
            if !traversed.insert((index, directory.clone())) {
                continue;
            }
            let entries = match std::fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error)
                    if directory == target.path
                        && error.kind() == std::io::ErrorKind::PermissionDenied =>
                {
                    return Err(FileError::from_io(error, Some(directory)));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound
                            | std::io::ErrorKind::PermissionDenied
                            | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(FileError::from_io(error, Some(directory))),
            };
            for entry in entries {
                let entry =
                    entry.map_err(|error| FileError::from_io(error, Some(directory.clone())))?;
                if excluded(target, &entry.file_name()) {
                    continue;
                }
                let path = entry.path();
                let entry_kind = if let Some(kind) = listed.get(&path) {
                    *kind
                } else {
                    let Ok((metadata, identity)) = observe(&path, false) else {
                        continue;
                    };
                    let kind = kind(&metadata);
                    listed.insert(path.clone(), kind);
                    if !snapshot.contains_key(&path) {
                        snapshot.insert(path.clone(), record(&path, &metadata, identity, mode));
                    }
                    kind
                };
                if target.recursive && entry_kind == Kind::Directory {
                    count(&mut counted, &path)?;
                    pending.push(path);
                }
            }
        }
    }
    Ok(Scan {
        snapshot,
        linked_files,
    })
}
pub(super) fn unreliable(targets: &[WatchTarget]) -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::unix::ffi::OsStrExt;
        const UNRELIABLE: &[u64] = &[
            0x6969, 0x517b, 0xff534d42, 0xfe534d42, 0x65735546, 0x01021997, 0x0bd00bd0, 0x47504653,
            0x00c36400, 0x5346414f, 0x6b414653, 0x5dca2df5,
        ];
        for target in targets {
            for candidate in std::iter::once(target.path.clone()).chain(ancestors(&target.path)) {
                let Ok(path) = std::ffi::CString::new(candidate.as_os_str().as_bytes()) else {
                    continue;
                };
                let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
                if unsafe { libc::statfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
                    continue;
                }
                if UNRELIABLE.contains(&(unsafe { info.assume_init() }.f_type as u64)) {
                    return true;
                }
                break;
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = targets;
    false
}
