//! File system ops backing `node:fs` and `node:fs/promises`.
//!
//! JS calls `ops.fsSync(name, ...args)` or `ops.fsAsync(name, ...args)`; both build
//! an [`FsOp`] on the JS thread and execute it either inline or on tokio's blocking pool.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::js::{Context, JsArgs, JsResult, JsValue};

use super::{array, bytes_arg, bytes_to_js, io_error, object};
use crate::engine::{self, OpFn, arg_f64, arg_string, js_str, state};
use crate::error::Error;

pub(crate) enum FsOp {
    ReadFile(String),
    WriteFile { path: String, data: Vec<u8>, flag: String, mode: u32 },
    Stat(String, bool),
    Access(String, i32),
    Exists(String),
    Mkdir { path: String, recursive: bool, mode: u32 },
    Readdir(String),
    Rm { path: String, recursive: bool, force: bool },
    Rmdir { path: String, recursive: bool },
    Unlink(String),
    Rename(String, String),
    CopyFile { from: String, to: String, excl: bool },
    Cp { from: String, to: String, recursive: bool, force: bool, error_on_exist: bool },
    Realpath(String),
    Readlink(String),
    Symlink(String, String),
    Link(String, String),
    Chmod(String, u32),
    Utimes(String, f64, f64),
    Truncate(String, u64),
    Mkdtemp(String),
    Open { path: String, flags: String, mode: u32 },
    Close(i32),
    Read { fd: i32, len: usize, pos: Option<u64> },
    Write { fd: i32, data: Vec<u8>, pos: Option<u64> },
    Fstat(i32),
    Ftruncate(i32, u64),
    Fsync(i32),
    Futimes(i32, f64, f64),
    Fchmod(i32, u32),
}

pub(crate) struct StatData {
    dev: u64,
    ino: u64,
    mode: u32,
    nlink: u64,
    uid: u32,
    gid: u32,
    rdev: u64,
    size: u64,
    blksize: u64,
    blocks: u64,
    atime_ms: f64,
    mtime_ms: f64,
    ctime_ms: f64,
    birthtime_ms: f64,
}

pub(crate) enum FsOut {
    Unit,
    Bytes(Vec<u8>),
    Bool(bool),
    Str(String),
    OptStr(Option<String>),
    Stat(StatData),
    Entries(Vec<(String, u8)>),
    Num(f64),
}

/// Open files keyed by their raw fd (shared across threads for async fd ops).
static FILES: std::sync::LazyLock<parking_lot::Mutex<HashMap<i32, std::sync::Arc<parking_lot::Mutex<fs::File>>>>> =
    std::sync::LazyLock::new(Default::default);

thread_local! {
    static OWNED_FDS: std::cell::RefCell<Vec<i32>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn stat_data(m: &fs::Metadata) -> StatData {
    let ms = |s: i64, ns: i64| s as f64 * 1000.0 + ns as f64 / 1e6;
    let birth = m
        .created()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or_else(|| ms(m.ctime(), m.ctime_nsec()));
    StatData {
        dev: m.dev(),
        ino: m.ino(),
        mode: m.mode(),
        nlink: m.nlink(),
        uid: m.uid(),
        gid: m.gid(),
        rdev: m.rdev(),
        size: m.size(),
        blksize: m.blksize(),
        blocks: m.blocks(),
        atime_ms: ms(m.atime(), m.atime_nsec()),
        mtime_ms: ms(m.mtime(), m.mtime_nsec()),
        ctime_ms: ms(m.ctime(), m.ctime_nsec()),
        birthtime_ms: birth,
    }
}

fn file_type_code(ft: &fs::FileType) -> u8 {
    use std::os::unix::fs::FileTypeExt;
    if ft.is_file() {
        1
    } else if ft.is_dir() {
        2
    } else if ft.is_symlink() {
        3
    } else if ft.is_fifo() {
        4
    } else if ft.is_socket() {
        5
    } else if ft.is_char_device() {
        6
    } else if ft.is_block_device() {
        7
    } else {
        0
    }
}

fn open_options(flags: &str, mode: u32) -> fs::OpenOptions {
    let mut o = fs::OpenOptions::new();
    o.mode(mode);
    match flags {
        "r" | "rs" | "sr" => {
            o.read(true);
        }
        "r+" | "rs+" | "sr+" => {
            o.read(true).write(true);
        }
        "w" => {
            o.write(true).create(true).truncate(true);
        }
        "wx" | "xw" => {
            o.write(true).create_new(true);
        }
        "w+" => {
            o.read(true).write(true).create(true).truncate(true);
        }
        "wx+" | "xw+" => {
            o.read(true).write(true).create_new(true);
        }
        "a" | "as" | "sa" => {
            o.append(true).create(true);
        }
        "ax" | "xa" => {
            o.append(true).create_new(true);
        }
        "a+" | "as+" | "sa+" => {
            o.read(true).append(true).create(true);
        }
        "ax+" | "xa+" => {
            o.read(true).append(true).create_new(true);
        }
        _ => {
            o.read(true);
        }
    }
    o
}

fn copy_recursive(from: &Path, to: &Path, force: bool, error_on_exist: bool) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &to.join(entry.file_name()), force, error_on_exist)?;
        }
        Ok(())
    } else if meta.file_type().is_symlink() {
        let target = fs::read_link(from)?;
        if to.symlink_metadata().is_ok() {
            if !force {
                return if error_on_exist { Err(std::io::ErrorKind::AlreadyExists.into()) } else { Ok(()) };
            }
            fs::remove_file(to)?;
        }
        std::os::unix::fs::symlink(target, to)
    } else {
        if to.exists() && !force {
            return if error_on_exist { Err(std::io::ErrorKind::AlreadyExists.into()) } else { Ok(()) };
        }
        fs::copy(from, to).map(|_| ())
    }
}

fn file_for(fd: i32) -> std::io::Result<std::sync::Arc<parking_lot::Mutex<fs::File>>> {
    let mut files = FILES.lock();
    if let Some(f) = files.get(&fd) {
        return Ok(f.clone());
    }
    // The standard streams are open in every process (`fs.readFileSync(0)`, `fs.writeSync(1, ..)`):
    // use a duplicate, so closing it here leaves the process's own descriptor alone.
    if (0..=2).contains(&fd) {
        use std::os::fd::FromRawFd;
        // SAFETY: dup of a standard descriptor; the duplicate is owned by the File.
        let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if dup >= 0 {
            // SAFETY: `dup` is a fresh descriptor.
            let f = std::sync::Arc::new(parking_lot::Mutex::new(unsafe { fs::File::from_raw_fd(dup) }));
            files.insert(fd, f.clone());
            return Ok(f);
        }
    }
    Err(std::io::Error::from_raw_os_error(libc::EBADF))
}

/// Execute an op (blocking).
pub(crate) fn exec(op: FsOp) -> Result<FsOut, Error> {
    use FsOp::*;
    let err = |e: std::io::Error, sys: &str, p: &str| io_error(&e, sys, Some(p));
    Ok(match op {
        ReadFile(p) => FsOut::Bytes(fs::read(&p).map_err(|e| err(e, "open", &p))?),
        WriteFile { path, data, flag, mode } => {
            let mut f = open_options(&flag, mode).open(&path).map_err(|e| err(e, "open", &path))?;
            f.write_all(&data).map_err(|e| err(e, "write", &path))?;
            FsOut::Unit
        }
        Stat(p, follow) => {
            let m = if follow { fs::metadata(&p) } else { fs::symlink_metadata(&p) };
            FsOut::Stat(stat_data(&m.map_err(|e| err(e, if follow { "stat" } else { "lstat" }, &p))?))
        }
        Access(p, mode) => {
            let c = std::ffi::CString::new(p.clone()).map_err(|_| Error::host("path contains NUL"))?;
            // SAFETY: valid C string.
            if unsafe { libc::access(c.as_ptr(), mode) } != 0 {
                return Err(err(std::io::Error::last_os_error(), "access", &p));
            }
            FsOut::Unit
        }
        Exists(p) => FsOut::Bool(Path::new(&p).exists()),
        Mkdir { path, recursive, mode } => {
            if recursive {
                let pb = PathBuf::from(&path);
                let mut first = None;
                let mut cur = Some(pb.as_path());
                while let Some(c) = cur {
                    if c.exists() {
                        break;
                    }
                    first = Some(c.to_path_buf());
                    cur = c.parent();
                }
                let mut b = fs::DirBuilder::new();
                b.recursive(true).mode(mode);
                b.create(&path).map_err(|e| err(e, "mkdir", &path))?;
                FsOut::OptStr(first.map(|p| p.display().to_string()))
            } else {
                let mut b = fs::DirBuilder::new();
                b.mode(mode);
                b.create(&path).map_err(|e| err(e, "mkdir", &path))?;
                FsOut::OptStr(None)
            }
        }
        Readdir(p) => {
            let mut out = Vec::new();
            for entry in fs::read_dir(&p).map_err(|e| err(e, "scandir", &p))? {
                let entry = entry.map_err(|e| err(e, "scandir", &p))?;
                let t = entry.file_type().map(|t| file_type_code(&t)).unwrap_or(0);
                out.push((entry.file_name().to_string_lossy().into_owned(), t));
            }
            out.sort_by(|a, b| a.0.cmp(&b.0));
            FsOut::Entries(out)
        }
        Rm { path, recursive, force } => {
            match fs::symlink_metadata(&path) {
                Err(e) if force && e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(err(e, "rm", &path)),
                Ok(m) if m.is_dir() => {
                    if !recursive {
                        return Err(Error::Js(crate::error::JsException {
                            name: "SystemError".into(),
                            message: format!("Path is a directory: rm returned EISDIR (is a directory) {path}"),
                            stack: None,
                            code: Some("ERR_FS_EISDIR".into()),
                        }));
                    }
                    fs::remove_dir_all(&path).map_err(|e| err(e, "rm", &path))?;
                }
                Ok(_) => fs::remove_file(&path).map_err(|e| err(e, "rm", &path))?,
            }
            FsOut::Unit
        }
        Rmdir { path, recursive } => {
            if recursive {
                fs::remove_dir_all(&path).map_err(|e| err(e, "rmdir", &path))?;
            } else {
                fs::remove_dir(&path).map_err(|e| err(e, "rmdir", &path))?;
            }
            FsOut::Unit
        }
        Unlink(p) => {
            fs::remove_file(&p).map_err(|e| err(e, "unlink", &p))?;
            FsOut::Unit
        }
        Rename(a, b) => {
            fs::rename(&a, &b).map_err(|e| io_error(&e, "rename", Some(&format!("{a}' -> '{b}"))))?;
            FsOut::Unit
        }
        CopyFile { from, to, excl } => {
            if excl && Path::new(&to).exists() {
                return Err(err(std::io::ErrorKind::AlreadyExists.into(), "copyfile", &format!("{from}' -> '{to}")));
            }
            fs::copy(&from, &to).map_err(|e| io_error(&e, "copyfile", Some(&format!("{from}' -> '{to}"))))?;
            FsOut::Unit
        }
        Cp { from, to, recursive, force, error_on_exist } => {
            let m = fs::symlink_metadata(&from).map_err(|e| err(e, "cp", &from))?;
            if m.is_dir() && !recursive {
                return Err(Error::Js(crate::error::JsException {
                    name: "SystemError".into(),
                    message: format!("Recursive option is required to copy a directory: {from}"),
                    stack: None,
                    code: Some("ERR_FS_EISDIR".into()),
                }));
            }
            copy_recursive(Path::new(&from), Path::new(&to), force, error_on_exist).map_err(|e| err(e, "cp", &to))?;
            FsOut::Unit
        }
        Realpath(p) => FsOut::Str(fs::canonicalize(&p).map_err(|e| err(e, "realpath", &p))?.display().to_string()),
        Readlink(p) => FsOut::Str(fs::read_link(&p).map_err(|e| err(e, "readlink", &p))?.display().to_string()),
        Symlink(target, p) => {
            std::os::unix::fs::symlink(&target, &p).map_err(|e| err(e, "symlink", &p))?;
            FsOut::Unit
        }
        Link(a, b) => {
            fs::hard_link(&a, &b).map_err(|e| err(e, "link", &b))?;
            FsOut::Unit
        }
        Chmod(p, mode) => {
            fs::set_permissions(&p, fs::Permissions::from_mode(mode)).map_err(|e| err(e, "chmod", &p))?;
            FsOut::Unit
        }
        Utimes(p, atime, mtime) => {
            let f = fs::OpenOptions::new().write(true).open(&p).or_else(|_| fs::File::open(&p)).map_err(|e| err(e, "utime", &p))?;
            let to_time = |s: f64| SystemTime::UNIX_EPOCH + Duration::from_secs_f64(s.max(0.0));
            let times = fs::FileTimes::new().set_accessed(to_time(atime)).set_modified(to_time(mtime));
            f.set_times(times).map_err(|e| err(e, "utime", &p))?;
            FsOut::Unit
        }
        Truncate(p, len) => {
            let f = fs::OpenOptions::new().write(true).open(&p).map_err(|e| err(e, "open", &p))?;
            f.set_len(len).map_err(|e| err(e, "ftruncate", &p))?;
            FsOut::Unit
        }
        Mkdtemp(prefix) => {
            use rand::Rng;
            const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
            for _ in 0..100 {
                let suffix: String = (0..6)
                    .map(|_| CHARS[rand::thread_rng().gen_range(0..CHARS.len())] as char)
                    .collect();
                let p = format!("{prefix}{suffix}");
                match fs::create_dir(&p) {
                    Ok(()) => return Ok(FsOut::Str(p)),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(err(e, "mkdtemp", &format!("{prefix}XXXXXX"))),
                }
            }
            return Err(Error::host("mkdtemp: too many attempts"));
        }
        Open { path, flags, mode } => {
            use std::os::fd::AsRawFd;
            let f = open_options(&flags, mode).open(&path).map_err(|e| err(e, "open", &path))?;
            let fd = f.as_raw_fd();
            FILES.lock().insert(fd, std::sync::Arc::new(parking_lot::Mutex::new(f)));
            FsOut::Num(fd as f64)
        }
        Close(fd) => {
            if FILES.lock().remove(&fd).is_none() {
                return Err(io_error(&std::io::Error::from_raw_os_error(libc::EBADF), "close", None));
            }
            FsOut::Unit
        }
        Read { fd, len, pos } => {
            let f = file_for(fd).map_err(|e| io_error(&e, "read", None))?;
            let mut f = f.lock();
            if let Some(p) = pos {
                f.seek(SeekFrom::Start(p)).map_err(|e| io_error(&e, "read", None))?;
            }
            let mut buf = vec![0u8; len];
            let n = f.read(&mut buf).map_err(|e| io_error(&e, "read", None))?;
            buf.truncate(n);
            FsOut::Bytes(buf)
        }
        Write { fd, data, pos } => {
            let f = file_for(fd).map_err(|e| io_error(&e, "write", None))?;
            let mut f = f.lock();
            if let Some(p) = pos {
                f.seek(SeekFrom::Start(p)).map_err(|e| io_error(&e, "write", None))?;
            }
            f.write_all(&data).map_err(|e| io_error(&e, "write", None))?;
            FsOut::Num(data.len() as f64)
        }
        Fstat(fd) => {
            let f = file_for(fd).map_err(|e| io_error(&e, "fstat", None))?;
            let m = f.lock().metadata().map_err(|e| io_error(&e, "fstat", None))?;
            FsOut::Stat(stat_data(&m))
        }
        Ftruncate(fd, len) => {
            let f = file_for(fd).map_err(|e| io_error(&e, "ftruncate", None))?;
            f.lock().set_len(len).map_err(|e| io_error(&e, "ftruncate", None))?;
            FsOut::Unit
        }
        Fsync(fd) => {
            let f = file_for(fd).map_err(|e| io_error(&e, "fsync", None))?;
            f.lock().sync_all().map_err(|e| io_error(&e, "fsync", None))?;
            FsOut::Unit
        }
        Futimes(fd, atime, mtime) => {
            let f = file_for(fd).map_err(|e| io_error(&e, "futime", None))?;
            let to_time = |s: f64| SystemTime::UNIX_EPOCH + Duration::from_secs_f64(s.max(0.0));
            let times = fs::FileTimes::new().set_accessed(to_time(atime)).set_modified(to_time(mtime));
            f.lock().set_times(times).map_err(|e| io_error(&e, "futime", None))?;
            FsOut::Unit
        }
        Fchmod(fd, mode) => {
            let f = file_for(fd).map_err(|e| io_error(&e, "fchmod", None))?;
            f.lock().set_permissions(fs::Permissions::from_mode(mode)).map_err(|e| io_error(&e, "fchmod", None))?;
            FsOut::Unit
        }
    })
}

fn to_js(out: FsOut, ctx: &mut Context) -> JsResult<JsValue> {
    Ok(match out {
        FsOut::Unit => JsValue::undefined(),
        FsOut::Bytes(b) => bytes_to_js(&b, ctx)?,
        FsOut::Bool(b) => JsValue::from(b),
        FsOut::Str(s) => js_str(&s),
        FsOut::OptStr(s) => s.map(|s| js_str(&s)).unwrap_or_default(),
        FsOut::Num(n) => JsValue::from(n),
        FsOut::Stat(s) => object(
            ctx,
            vec![
                ("dev", JsValue::from(s.dev as f64)),
                ("ino", JsValue::from(s.ino as f64)),
                ("mode", JsValue::from(s.mode)),
                ("nlink", JsValue::from(s.nlink as f64)),
                ("uid", JsValue::from(s.uid)),
                ("gid", JsValue::from(s.gid)),
                ("rdev", JsValue::from(s.rdev as f64)),
                ("size", JsValue::from(s.size as f64)),
                ("blksize", JsValue::from(s.blksize as f64)),
                ("blocks", JsValue::from(s.blocks as f64)),
                ("atimeMs", JsValue::from(s.atime_ms)),
                ("mtimeMs", JsValue::from(s.mtime_ms)),
                ("ctimeMs", JsValue::from(s.ctime_ms)),
                ("birthtimeMs", JsValue::from(s.birthtime_ms)),
            ],
        )?,
        FsOut::Entries(entries) => {
            let items: Vec<JsValue> = entries
                .into_iter()
                .map(|(name, t)| array(ctx, vec![js_str(&name), JsValue::from(t as u32)]))
                .collect();
            array(ctx, items)
        }
    })
}

fn opt_pos(v: &JsValue, ctx: &mut Context) -> JsResult<Option<u64>> {
    if v.is_null_or_undefined() {
        return Ok(None);
    }
    let n = v.to_number(ctx)?;
    Ok(if n >= 0.0 { Some(n as u64) } else { None })
}

fn build(args: &[JsValue], ctx: &mut Context) -> JsResult<FsOp> {
    let name = arg_string(args, 0, ctx)?;
    let a = &args[1.min(args.len())..];
    let s = |i: usize, ctx: &mut Context| arg_string(a, i, ctx);
    let b = |i: usize| a.get_or_undefined(i).to_boolean();
    let n = |i: usize, ctx: &mut Context| arg_f64(a, i, ctx);
    Ok(match name.as_str() {
        "readFile" => FsOp::ReadFile(s(0, ctx)?),
        "writeFile" => FsOp::WriteFile {
            path: s(0, ctx)?,
            data: bytes_arg(a.get_or_undefined(1), ctx)?,
            flag: s(2, ctx)?,
            mode: n(3, ctx)? as u32,
        },
        "stat" => FsOp::Stat(s(0, ctx)?, true),
        "lstat" => FsOp::Stat(s(0, ctx)?, false),
        "access" => FsOp::Access(s(0, ctx)?, n(1, ctx)? as i32),
        "exists" => FsOp::Exists(s(0, ctx)?),
        "mkdir" => FsOp::Mkdir {
            path: s(0, ctx)?,
            recursive: b(1),
            mode: n(2, ctx)? as u32,
        },
        "readdir" => FsOp::Readdir(s(0, ctx)?),
        "rm" => FsOp::Rm {
            path: s(0, ctx)?,
            recursive: b(1),
            force: b(2),
        },
        "rmdir" => FsOp::Rmdir {
            path: s(0, ctx)?,
            recursive: b(1),
        },
        "unlink" => FsOp::Unlink(s(0, ctx)?),
        "rename" => FsOp::Rename(s(0, ctx)?, s(1, ctx)?),
        "copyFile" => FsOp::CopyFile {
            from: s(0, ctx)?,
            to: s(1, ctx)?,
            excl: (n(2, ctx)? as i64) & 1 == 1,
        },
        "cp" => FsOp::Cp {
            from: s(0, ctx)?,
            to: s(1, ctx)?,
            recursive: b(2),
            force: b(3),
            error_on_exist: b(4),
        },
        "realpath" => FsOp::Realpath(s(0, ctx)?),
        "readlink" => FsOp::Readlink(s(0, ctx)?),
        "symlink" => FsOp::Symlink(s(0, ctx)?, s(1, ctx)?),
        "link" => FsOp::Link(s(0, ctx)?, s(1, ctx)?),
        "chmod" => FsOp::Chmod(s(0, ctx)?, n(1, ctx)? as u32),
        "utimes" => FsOp::Utimes(s(0, ctx)?, n(1, ctx)?, n(2, ctx)?),
        "truncate" => FsOp::Truncate(s(0, ctx)?, n(1, ctx)?.max(0.0) as u64),
        "mkdtemp" => FsOp::Mkdtemp(s(0, ctx)?),
        "open" => FsOp::Open {
            path: s(0, ctx)?,
            flags: s(1, ctx)?,
            mode: n(2, ctx)? as u32,
        },
        "close" => FsOp::Close(n(0, ctx)? as i32),
        "read" => FsOp::Read {
            fd: n(0, ctx)? as i32,
            len: n(1, ctx)?.max(0.0) as usize,
            pos: opt_pos(a.get_or_undefined(2), ctx)?,
        },
        "write" => FsOp::Write {
            fd: n(0, ctx)? as i32,
            data: bytes_arg(a.get_or_undefined(1), ctx)?,
            pos: opt_pos(a.get_or_undefined(2), ctx)?,
        },
        "fstat" => FsOp::Fstat(n(0, ctx)? as i32),
        "ftruncate" => FsOp::Ftruncate(n(0, ctx)? as i32, n(1, ctx)?.max(0.0) as u64),
        "fsync" => FsOp::Fsync(n(0, ctx)? as i32),
        "futimes" => FsOp::Futimes(n(0, ctx)? as i32, n(1, ctx)?, n(2, ctx)?),
        "fchmod" => FsOp::Fchmod(n(0, ctx)? as i32, n(1, ctx)? as u32),
        other => return Err(engine::type_error(format!("unknown fs op {other}"))),
    })
}

fn track_fd(out: &FsOut) {
    if let FsOut::Num(fd) = out {
        OWNED_FDS.with(|f| f.borrow_mut().push(*fd as i32));
    }
}

fn op_fs_sync(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let is_open = args.first().and_then(|v| v.as_string()).is_some_and(|s| s.to_std_string_lossy() == "open");
    let op = build(args, ctx)?;
    match exec(op) {
        Ok(out) => {
            if is_open {
                track_fd(&out);
            }
            to_js(out, ctx)
        }
        Err(e) => Err(crate::error::to_js(&e, ctx)),
    }
}

fn op_fs_async(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let op = build(args, ctx)?;
    Ok(engine::spawn_result(
        ctx,
        async move {
            tokio::task::spawn_blocking(move || exec(op))
                .await
                .map_err(|e| Error::host(e.to_string()))?
        },
        to_js,
    ))
}

/// Minimal file watcher: polls mtimes of a path (and its direct children) and calls back.
fn op_watch(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let path = arg_string(args, 0, ctx)?;
    let interval = args.get_or_undefined(1).to_number(ctx).unwrap_or(500.0).max(50.0);
    let st = state(ctx);
    let id = st.next_id();
    let poster = st.poster();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    WATCHERS.lock().insert(id, stop.clone());
    let snapshot = |p: &str| -> HashMap<String, Option<SystemTime>> {
        let mut m = HashMap::new();
        let root = Path::new(p);
        m.insert(String::new(), fs::metadata(root).and_then(|m| m.modified()).ok());
        if root.is_dir()
            && let Ok(rd) = fs::read_dir(root)
        {
            for e in rd.flatten() {
                m.insert(e.file_name().to_string_lossy().into_owned(), e.metadata().and_then(|m| m.modified()).ok());
            }
        }
        m
    };
    // The baseline is taken before `fs.watch` returns, like Node's watcher, which is active as
    // soon as it is created: changes the caller makes right after are reported.
    let mut prev = snapshot(&path);
    st.tokio.spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(interval as u64)).await;
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            let p = path.clone();
            let next = tokio::task::spawn_blocking(move || snapshot(&p)).await.unwrap_or_default();
            let mut events = Vec::new();
            for (k, v) in &next {
                match prev.get(k) {
                    None => events.push(("rename", k.clone())),
                    Some(old) if old != v => events.push(("change", k.clone())),
                    _ => {}
                }
            }
            for k in prev.keys() {
                if !next.contains_key(k) {
                    events.push(("rename", k.clone()));
                }
            }
            prev = next;
            if !events.is_empty() {
                let ok = poster.post(move |ctx| {
                    if let Some(internal) = engine::try_internal(ctx)
                        && let Ok(f) = internal.get(crate::js::js_string!("fsWatchEvent"), ctx)
                        && let Some(f) = f.as_function()
                    {
                        for (kind, name) in events {
                            let _ = f.call(&JsValue::undefined(), &[JsValue::from(id as f64), js_str(kind), js_str(&name)], ctx);
                        }
                    }
                });
                if !ok {
                    break;
                }
            }
        }
    });
    Ok(JsValue::from(id as f64))
}

static WATCHERS: std::sync::LazyLock<parking_lot::Mutex<HashMap<u64, std::sync::Arc<std::sync::atomic::AtomicBool>>>> =
    std::sync::LazyLock::new(Default::default);

fn op_unwatch(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    if let Some(stop) = WATCHERS.lock().remove(&id) {
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(JsValue::undefined())
}

pub(crate) fn teardown(ctx: &mut Context) {
    let _ = ctx;
    let fds = OWNED_FDS.with(|f| std::mem::take(&mut *f.borrow_mut()));
    let mut files = FILES.lock();
    for fd in fds {
        files.remove(&fd);
    }
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[
    ("fsSync", 1, op_fs_sync),
    ("fsAsync", 1, op_fs_async),
    ("fsWatch", 2, op_watch),
    ("fsUnwatch", 1, op_unwatch),
];
