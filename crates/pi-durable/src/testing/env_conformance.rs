//! Runner-independent `ExecutionEnv` cases, executable against any native environment.

use super::{
    ConformanceFuture, EnvConformanceCase, EnvConformanceOptions, EnvConformanceProvider,
    assertions::create_expect_assertions,
};
use crate::{Context, Error, JsonString, Result, env::*};
use parking_lot::Mutex;
use pi_chord::context::with_cancel;
use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

type Env = Arc<dyn ExecutionEnv>;
type Changes = Arc<Mutex<Vec<WatchChange>>>;

const WATCH_TIMEOUT: Duration = Duration::from_secs(30);

fn context() -> Context {
    Context::default()
}
fn aborted() -> Context {
    let (context, controller) = with_cancel(&Context::default());
    controller.abort();
    context
}
fn file<T>(result: FileResult<T>) -> Result<T> {
    result.map_err(Error::file)
}
fn run<T>(result: ExecutionResult<T>) -> Result<T> {
    result.map_err(Error::execution)
}
fn file_code<T>(result: &FileResult<T>) -> Option<FileErrorCode> {
    result.as_ref().err().map(FileError::code)
}
fn exec_code<T>(result: &ExecutionResult<T>) -> Option<ExecutionErrorCode> {
    result.as_ref().err().map(ExecutionError::code)
}
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
fn name(info: &FileInfo) -> String {
    info.name.to_string_lossy().into_owned()
}
async fn write(env: &Env, path: &str, content: impl AsRef<[u8]>) -> Result<()> {
    file(
        env.write_file(Path::new(path), content.as_ref(), &context())
            .await,
    )
}
async fn rename(env: &Env, from: &str, to: &str) -> Result<()> {
    file(
        env.rename_file(Path::new(from), Path::new(to), &context())
            .await,
    )
}
async fn make_dir(env: &Env, path: &str) -> Result<()> {
    file(env.create_dir(Path::new(path), false, &context()).await)
}
async fn remove_file(env: &Env, path: &str) -> Result<()> {
    file(env.remove(Path::new(path), false, false, &context()).await)
}
async fn absolute(env: &Env, path: &str) -> Result<PathBuf> {
    file(env.absolute_path(Path::new(path), &context()).await)
}

/// Decode like a WHATWG `TextDecoder`: a leading byte-order mark is dropped
/// unless `ignore_bom` is set.
fn decode(bytes: &[u8], ignore_bom: bool) -> String {
    let decoded = text(bytes);
    match decoded.strip_prefix('\u{feff}') {
        Some(rest) if !ignore_bom => rest.to_owned(),
        _ => decoded,
    }
}

async fn read_all(env: &Env, path: &str, max_entries: f64) -> Result<(Vec<Vec<FileInfo>>, bool)> {
    let reader = file(env.open_dir_reader(Path::new(path), &context()).await)?;
    let mut pages = Vec::new();
    let mut done = false;
    for _ in 0..1000 {
        match reader.next(max_entries, &context()).await {
            Ok(page) => {
                done = page.done;
                pages.push(page.entries);
                if done {
                    break;
                }
            }
            Err(error) => {
                let _ = reader.close(&context()).await;
                return Err(Error::file(error));
            }
        }
    }
    let _ = reader.close(&context()).await;
    Ok((pages, done))
}

/// Whether a change reports `path`: an overflow, or a reported path at or above it.
fn covers(change: &WatchChange, path: &Path) -> bool {
    match change {
        WatchChange::Overflow => true,
        WatchChange::Paths(reported) => reported.iter().any(|reported| path.starts_with(reported)),
        WatchChange::Error(_) => false,
    }
}

/// Watch `targets`, run the body, and always close the watcher.
async fn watching<F, Fut>(env: &Env, targets: Vec<WatchTarget>, body: F) -> Result<()>
where
    F: FnOnce(Changes) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let changes = Changes::default();
    let sink = changes.clone();
    let watcher = file(
        env.watch(
            &targets,
            Arc::new(move |change| sink.lock().push(change)),
            &context(),
        )
        .await,
    )?;
    let result = body(changes).await;
    let closed = file(watcher.close(&context()).await);
    result.and(closed)
}

/// Wait up to three seconds for a change, after `change` runs, that reports `path`.
async fn expect_change(
    env: &Env,
    changes: &Changes,
    path: &str,
    change: impl Future<Output = Result<()>>,
) -> Result<()> {
    let target = absolute(env, path).await?;
    let from = changes.lock().len();
    change.await?;
    let deadline = pi_chord::rt::Instant::now() + Duration::from_secs(3);
    loop {
        {
            let seen = changes.lock();
            if seen[from..].iter().any(|entry| covers(entry, &target)) {
                return Ok(());
            }
            if pi_chord::rt::Instant::now() > deadline {
                return Err(Error::message(format!(
                    "No change reported {}; got {:?}",
                    target.display(),
                    &seen[from..]
                )));
            }
        }
        pi_chord::rt::sleep(Duration::from_millis(20)).await;
    }
}

struct Collected {
    result: ExecutionResult<ShellExecResult>,
    stdout: String,
    stderr: String,
}
async fn exec_collect(env: &Env, command: ShellCommand, cwd: Option<&str>) -> Collected {
    let output = Arc::new(Mutex::new((String::new(), String::new())));
    let sink = output.clone();
    let result = env
        .exec(
            command,
            ShellExecOptions {
                cwd: cwd.map(PathBuf::from),
                on_output: Some(Arc::new(move |text, _context, info| {
                    let mut output = sink.lock();
                    match info.stream {
                        ShellStream::Stdout => output.0.push_str(&text.to_string_lossy()),
                        ShellStream::Stderr => output.1.push_str(&text.to_string_lossy()),
                    }
                    Ok(())
                })),
                ..Default::default()
            },
            &context(),
        )
        .await;
    let (stdout, stderr) = output.lock().clone();
    Collected {
        result,
        stdout,
        stderr,
    }
}
fn argv(shell: &[String], rest: &[&str]) -> ShellCommand {
    ShellCommand::Argv(
        shell
            .iter()
            .map(String::as_str)
            .chain(rest.iter().copied())
            .map(JsonString::from)
            .collect(),
    )
}

type CaseFuture = std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send>>;
type CaseBody = Arc<dyn Fn(Env, EnvConformanceOptions) -> CaseFuture + Send + Sync>;

fn case<F, Fut>(
    provider: &EnvConformanceProvider,
    options: &EnvConformanceOptions,
    name: &'static str,
    timeout: Option<Duration>,
    body: F,
) -> EnvConformanceCase
where
    F: Fn(Env, EnvConformanceOptions) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let body: CaseBody = Arc::new(move |env, options| Box::pin(body(env, options)));
    let (provider, options) = (provider.clone(), options.clone());
    EnvConformanceCase {
        name,
        timeout,
        run: Arc::new(move || -> ConformanceFuture {
            let (body, options) = (body.clone(), options.clone());
            provider(Box::new(move |env| body(env, options)))
        }),
    }
}

/// Creates runner-independent cases for an `ExecutionEnv`. `with_env` must call
/// and await its callback exactly once per case with an environment whose `cwd`
/// is a fresh, empty, writable directory.
pub fn create_env_conformance(
    with_env: EnvConformanceProvider,
    options: EnvConformanceOptions,
) -> Vec<EnvConformanceCase> {
    let p = &with_env;
    let o = &options;
    let plain =
        |name, body: fn(Env, EnvConformanceOptions) -> CaseFuture| case(p, o, name, None, body);
    // Watch cases wait up to three seconds per step, longer than a runner's default.
    let watch = |name, body: fn(Env, EnvConformanceOptions) -> CaseFuture| {
        case(p, o, name, Some(WATCH_TIMEOUT), body)
    };
    let mut cases = vec![
        plain(
            "binary reader reads byte ranges of the opened file",
            |env, _| Box::pin(binary_reader_ranges(env)),
        ),
        plain(
            "binary reader scans lines like decoding the whole file",
            |env, _| Box::pin(binary_reader_lines(env)),
        ),
        plain(
            "binary reader keeps reading the file it opened after a rename",
            |env, _| Box::pin(binary_reader_rename(env)),
        ),
        plain(
            "binary reader refuses directories, missing files and aborted opens",
            |env, _| Box::pin(binary_reader_refusals(env)),
        ),
        plain(
            "directory reader pages every entry exactly once",
            |env, _| Box::pin(dir_reader_pages(env)),
        ),
        plain(
            "directory reader reports the end and refuses use after close",
            |env, _| Box::pin(dir_reader_end(env)),
        ),
        plain(
            "directory reader refuses missing paths and files",
            |env, _| Box::pin(dir_reader_refusals(env)),
        ),
        plain(
            "directory reader skips entries removed during enumeration",
            |env, _| Box::pin(dir_reader_removed(env)),
        ),
        watch(
            "watch reports a missing file's creation, changes, replacement and removal",
            |env, _| Box::pin(watch_file_lifecycle(env)),
        ),
        watch(
            "watch reports a missing target whose ancestors are created",
            |env, _| Box::pin(watch_missing_ancestors(env)),
        ),
        watch(
            "watch follows directories created together with their contents",
            |env, _| Box::pin(watch_new_directories(env)),
        ),
        watch(
            "watch keeps watching a path whose parent is renamed and recreated",
            |env, _| Box::pin(watch_renamed_parent(env)),
        ),
        watch(
            "watch skips excluded entries and reports a rename out of them",
            |env, _| Box::pin(watch_exclusions(env)),
        ),
        watch(
            "watch keeps recursive coverage where a non-recursive target overlaps",
            |env, _| Box::pin(watch_overlap(env)),
        ),
        watch(
            "watch follows a directory replaced at the same path",
            |env, _| Box::pin(watch_replaced_directory(env)),
        ),
        watch("watch stops reporting once closed", |env, _| {
            Box::pin(watch_closed(env))
        }),
        plain(
            "argv exec passes arguments to the program without shell parsing",
            |env, o| Box::pin(exec_argv_hostile(env, o)),
        ),
        plain(
            "exec reports the stream of every chunk in both forms",
            |env, o| Box::pin(exec_streams(env, o)),
        ),
        plain("argv exec honors cwd and exit codes", |env, o| {
            Box::pin(exec_cwd_and_exit(env, o))
        }),
        plain(
            "argv exec reports missing programs and empty argv as spawn errors",
            |env, _| Box::pin(exec_spawn_errors(env)),
        ),
        plain(
            "windowed exec keeps the exact tail and counts what it skips",
            |env, o| Box::pin(exec_window(env, o)),
        ),
        plain("argv exec distinguishes timeout from abort", |env, o| {
            Box::pin(exec_timeout_and_abort(env, o))
        }),
    ];
    if options.list_files {
        cases.push(plain(
            "listFiles lists files and directories depth first in name order",
            |env, _| Box::pin(list_order(env)),
        ));
        cases.push(plain(
            "listFiles skips what git ignores in work trees, .git, and excluded names",
            |env, _| Box::pin(list_ignores(env)),
        ));
        cases.push(plain(
            "listFiles stops at its bounds and reports truncated",
            |env, _| Box::pin(list_bounds(env)),
        ));
        cases.push(plain(
            "listFiles fails on missing directories, files, and aborts",
            |env, _| Box::pin(list_failures(env)),
        ));
        if options.symlinks {
            cases.push(plain(
                "listFiles lists symbolic links without following them",
                |env, o| Box::pin(list_symlinks(env, o)),
            ));
        }
    }
    if options.symlinks {
        cases.push(plain(
            "binary reader follows symlinks unless noFollow refuses the final one",
            |env, o| Box::pin(binary_reader_symlinks(env, o)),
        ));
        cases.push(watch(
            "watch reports changes to the file a watched symbolic link points to",
            |env, o| Box::pin(watch_symlink(env, o)),
        ));
    }
    cases
}

/// A listing as `<kind letter> <path>` lines (`f`, `d`, `l`), with its truncation.
async fn listed(env: &Env, path: &str, options: ListFilesOptions) -> Result<(Vec<String>, bool)> {
    let listing = file(env.list_files(Path::new(path), &options, &context()).await)?;
    let entries = listing
        .entries
        .iter()
        .map(|entry| {
            let letter = match entry.kind {
                FileKind::File => 'f',
                FileKind::Directory => 'd',
                FileKind::Symlink => 'l',
            };
            format!("{letter} {}", entry.path)
        })
        .collect();
    Ok((entries, listing.truncated))
}
fn lines(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}
async fn write_all(env: &Env, files: &[&str]) -> Result<()> {
    for entry in files {
        write(env, entry, "").await?;
    }
    Ok(())
}

async fn list_order(env: Env) -> Result<()> {
    let a = create_expect_assertions();
    write_all(
        &env,
        &["b.txt", "a/z.txt", "a/b/c.txt", ".hidden/x", "A.txt"],
    )
    .await?;
    make_dir(&env, "empty").await?;
    let all = lines(&[
        "d .hidden",
        "f .hidden/x",
        "f A.txt",
        "d a",
        "d a/b",
        "f a/b/c.txt",
        "f a/z.txt",
        "f b.txt",
        "d empty",
    ]);
    a.deep_equal(&listed(&env, ".", Default::default()).await?, &(all, false))?;
    let visible = ListFilesOptions {
        hidden: Some(false),
        ..Default::default()
    };
    a.deep_equal(
        &listed(&env, ".", visible).await?.0,
        &lines(&[
            "f A.txt",
            "d a",
            "d a/b",
            "f a/b/c.txt",
            "f a/z.txt",
            "f b.txt",
            "d empty",
        ]),
    )?;
    let expected = (lines(&["d b", "f b/c.txt", "f z.txt"]), false);
    a.deep_equal(&listed(&env, "a", Default::default()).await?, &expected)?;
    let absolute_a = absolute(&env, "a").await?;
    a.deep_equal(
        &listed(&env, &absolute_a.to_string_lossy(), Default::default()).await?,
        &expected,
    )?;
    a.deep_equal(
        &listed(&env, "empty", Default::default()).await?,
        &(Vec::new(), false),
    )
}

async fn list_ignores(env: Env) -> Result<()> {
    let a = create_expect_assertions();
    write_all(
        &env,
        &[
            "keep.txt",
            "out/x.js",
            "sub/keep.txt",
            "sub/drop.log",
            "node_modules/m/i.js",
        ],
    )
    .await?;
    write(&env, ".gitignore", "out/\n*.log\n").await?;
    write(&env, "sub/.gitignore", "!keep.txt\n").await?;
    // Not a work tree yet: `.gitignore` files have no effect.
    let before = listed(&env, ".", Default::default()).await?.0;
    a.ok(
        before.contains(&"f out/x.js".to_owned()),
        "ignore files need a work tree",
    )?;
    write(&env, ".git/HEAD", "ref: refs/heads/main\n").await?;
    write(&env, ".git/info/exclude", "keep.txt\n").await?;
    let excluded = ListFilesOptions {
        exclude: vec!["node_modules".into()],
        ..Default::default()
    };
    a.deep_equal(
        &listed(&env, ".", excluded).await?.0,
        &lines(&[
            "f .gitignore",
            "d sub",
            "f sub/.gitignore",
            "f sub/keep.txt",
        ]),
    )?;
    // A directory inside the work tree applies its parents' rules.
    a.deep_equal(
        &listed(&env, "sub", Default::default()).await?.0,
        &lines(&["f .gitignore", "f keep.txt"]),
    )?;
    let unfiltered = ListFilesOptions {
        gitignore: Some(false),
        ..Default::default()
    };
    let all = listed(&env, ".", unfiltered).await?.0;
    a.ok(
        all.contains(&"f out/x.js".to_owned())
            && all.contains(&"f node_modules/m/i.js".to_owned())
            && all.contains(&"f keep.txt".to_owned()),
        "gitignore: false lists ignored files",
    )?;
    a.ok(
        !all.iter()
            .any(|entry| entry.contains(".git/") || entry == "d .git"),
        ".git is never listed",
    )
}

async fn list_bounds(env: Env) -> Result<()> {
    let a = create_expect_assertions();
    write_all(&env, &["a", "b", "c"]).await?;
    let bounded = |entries: Option<f64>, bytes: Option<f64>| ListFilesOptions {
        max_entries: entries,
        max_bytes: bytes,
        ..Default::default()
    };
    a.deep_equal(
        &listed(&env, ".", bounded(Some(2.0), None)).await?,
        &(lines(&["f a", "f b"]), true),
    )?;
    a.deep_equal(
        &listed(&env, ".", bounded(Some(3.0), None)).await?,
        &(lines(&["f a", "f b", "f c"]), false),
    )?;
    // Each entry counts its path plus two bytes.
    a.deep_equal(
        &listed(&env, ".", bounded(None, Some(6.0))).await?,
        &(lines(&["f a", "f b"]), true),
    )?;
    a.deep_equal(
        &listed(&env, ".", bounded(Some(0.0), None)).await?,
        &(Vec::new(), true),
    )?;
    let invalid = Some(FileErrorCode::Invalid);
    a.strict_equal(
        &file_code(
            &env.list_files(Path::new("."), &bounded(Some(-1.0), None), &context())
                .await,
        ),
        &invalid,
    )?;
    let timeout = ListFilesOptions {
        timeout_ms: Some(1.5),
        ..Default::default()
    };
    a.strict_equal(
        &file_code(&env.list_files(Path::new("."), &timeout, &context()).await),
        &invalid,
    )
}

async fn list_failures(env: Env) -> Result<()> {
    let a = create_expect_assertions();
    write(&env, "file.txt", "").await?;
    let options = ListFilesOptions::default();
    let code = |path: &'static str, context: Context| {
        let env = env.clone();
        let options = options.clone();
        async move { file_code(&env.list_files(Path::new(path), &options, &context).await) }
    };
    a.strict_equal(
        &code("missing", context()).await,
        &Some(FileErrorCode::NotFound),
    )?;
    a.strict_equal(
        &code("file.txt", context()).await,
        &Some(FileErrorCode::NotDirectory),
    )?;
    a.strict_equal(&code(".", aborted()).await, &Some(FileErrorCode::Aborted))
}

async fn list_symlinks(env: Env, options: EnvConformanceOptions) -> Result<()> {
    let a = create_expect_assertions();
    write(&env, "dir/inner.txt", "").await?;
    link_with(
        &env,
        &options,
        "ln -s dir link && ln -s missing dangling && ln -s .. dir/loop",
    )
    .await?;
    a.deep_equal(
        &listed(&env, ".", Default::default()).await?.0,
        &lines(&[
            "l dangling",
            "d dir",
            "f dir/inner.txt",
            "l dir/loop",
            "l link",
        ]),
    )?;
    // The listed directory itself may be a link.
    a.deep_equal(
        &listed(&env, "link", Default::default()).await?.0,
        &lines(&["f inner.txt", "l loop"]),
    )
}

async fn binary_reader_ranges(env: Env) -> Result<()> {
    let a = create_expect_assertions();
    let c = context();
    write(&env, "data.txt", "hello world").await?;
    let reader = file(
        env.open_binary_reader(Path::new("data.txt"), false, &c)
            .await,
    )?;
    let info = file(reader.info(&c).await)?;
    a.strict_equal(&name(&info), &"data.txt".to_owned())?;
    a.strict_equal(&info.kind, &FileKind::File)?;
    a.strict_equal(&info.size, &11)?;
    a.strict_equal(
        &text(&file(reader.read(0.0, 5.0, &c).await)?),
        &"hello".to_owned(),
    )?;
    a.strict_equal(
        &text(&file(reader.read(6.0, 100.0, &c).await)?),
        &"world".to_owned(),
    )?;
    a.strict_equal(&file(reader.read(11.0, 4.0, &c).await)?.len(), &0)?;
    a.strict_equal(&file(reader.read(50.0, 1.0, &c).await)?.len(), &0)?;
    a.strict_equal(&file(reader.read(3.0, 0.0, &c).await)?.len(), &0)?;
    let invalid = Some(FileErrorCode::Invalid);
    a.strict_equal(&file_code(&reader.read(-1.0, 1.0, &c).await), &invalid)?;
    a.strict_equal(&file_code(&reader.read(0.0, 1.5, &c).await), &invalid)?;
    a.strict_equal(
        &file_code(&reader.read(0.0, 1.0, &aborted()).await),
        &Some(FileErrorCode::Aborted),
    )?;
    file(reader.close(&c).await)?;
    file(reader.close(&c).await)?;
    a.strict_equal(&file_code(&reader.read(0.0, 1.0, &c).await), &invalid)?;
    a.strict_equal(&file_code(&reader.info(&c).await), &invalid)
}

async fn binary_reader_lines(env: Env) -> Result<()> {
    let a = create_expect_assertions();
    let c = context();
    // A byte-order mark, an invalid sequence before a newline, an empty line, a later U+FEFF, and no final newline.
    let bytes: Vec<u8> = vec![
        0xef, 0xbb, 0xbf, 0x61, 0x0a, 0xe2, 0x82, 0x0a, 0x0a, 0xef, 0xbb, 0xbf, 0x62, 0x0a, 0xc3,
        0xa9,
    ];
    write(&env, "lines.txt", &bytes).await?;
    let whole = decode(&bytes, false);
    let lines: Vec<&str> = whole.split('\n').collect();
    let reader = file(
        env.open_binary_reader(Path::new("lines.txt"), false, &c)
            .await,
    )?;
    let result = async {
        let ranges: [(usize, Option<usize>); 6] = [
            (0, None),
            (0, Some(1)),
            (1, Some(3)),
            (2, Some(3)),
            (3, None),
            (4, Some(9)),
        ];
        for (start_line, end_line) in ranges {
            let scan = file(
                reader
                    .scan_lines(start_line as f64, end_line.map(|end| end as f64), &c)
                    .await,
            )?;
            let selected = &lines
                [start_line.min(lines.len())..end_line.unwrap_or(lines.len()).min(lines.len())];
            let joined = selected.join("\n");
            let range = |from: u64, to: u64| decode(&bytes[from as usize..to as usize], from > 0);
            a.strict_equal(&scan.newlines, &(lines.len() as u64 - 1))?;
            a.strict_equal(&range(scan.start, scan.end), &joined)?;
            a.strict_equal(&scan.selected_bytes, &(joined.len() as u64))?;
            a.strict_equal(
                &range(scan.start, scan.first_line_end),
                &lines[start_line].to_owned(),
            )?;
            a.strict_equal(&scan.first_line_bytes, &(lines[start_line].len() as u64))?;
        }
        let tail = file(reader.scan_lines(9.0, None, &c).await)?;
        a.strict_equal(&tail.start, &(bytes.len() as u64))?;
        a.strict_equal(&tail.end, &(bytes.len() as u64))?;
        a.strict_equal(&tail.selected_bytes, &0)?;
        a.strict_equal(
            &file_code(&reader.scan_lines(2.0, Some(2.0), &c).await),
            &Some(FileErrorCode::Invalid),
        )
    }
    .await;
    let _ = reader.close(&c).await;
    result
}

async fn binary_reader_rename(env: Env) -> Result<()> {
    let c = context();
    write(&env, "a.txt", "one").await?;
    let reader = file(env.open_binary_reader(Path::new("a.txt"), false, &c).await)?;
    let result = async {
        rename(&env, "a.txt", "b.txt").await?;
        write(&env, "a.txt", "two").await?;
        create_expect_assertions().strict_equal(
            &text(&file(reader.read(0.0, 10.0, &c).await)?),
            &"one".to_owned(),
        )
    }
    .await;
    let _ = reader.close(&c).await;
    result
}

async fn binary_reader_refusals(env: Env) -> Result<()> {
    let a = create_expect_assertions();
    let c = context();
    make_dir(&env, "dir").await?;
    write(&env, "file.txt", "x").await?;
    a.strict_equal(
        &file_code(&env.open_binary_reader(Path::new("dir"), false, &c).await),
        &Some(FileErrorCode::IsDirectory),
    )?;
    a.strict_equal(
        &file_code(
            &env.open_binary_reader(Path::new("missing.txt"), false, &c)
                .await,
        ),
        &Some(FileErrorCode::NotFound),
    )?;
    a.strict_equal(
        &file_code(
            &env.open_binary_reader(Path::new("file.txt"), false, &aborted())
                .await,
        ),
        &Some(FileErrorCode::Aborted),
    )
}

async fn dir_reader_pages(env: Env) -> Result<()> {
    let a = create_expect_assertions();
    let names = ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"];
    for entry in names {
        write(&env, entry, entry).await?;
    }
    make_dir(&env, "sub").await?;
    let (pages, done) = read_all(&env, ".", 2.0).await?;
    a.ok(done, "directory reader reached the end")?;
    for page in &pages {
        a.ok(page.len() <= 2, "page within maxEntries")?;
    }
    let entries: Vec<&FileInfo> = pages.iter().flatten().collect();
    let mut found: Vec<String> = entries.iter().map(|entry| name(entry)).collect();
    found.sort();
    let mut expected: Vec<String> = names.iter().map(|entry| entry.to_string()).collect();
    expected.push("sub".into());
    expected.sort();
    a.deep_equal(&found, &expected)?;
    let sub = entries.iter().find(|entry| name(entry) == "sub");
    a.strict_equal(&sub.map(|entry| entry.kind), &Some(FileKind::Directory))?;
    let first = entries.iter().find(|entry| name(entry) == "a.txt");
    a.strict_equal(
        &first.map(|entry| (entry.kind, entry.size)),
        &Some((FileKind::File, 5)),
    )
}

async fn dir_reader_end(env: Env) -> Result<()> {
    let a = create_expect_assertions();
    let c = context();
    make_dir(&env, "empty").await?;
    let reader = file(env.open_dir_reader(Path::new("empty"), &c).await)?;
    for _ in 0..2 {
        let page = file(reader.next(10.0, &c).await)?;
        a.ok(
            page.entries.is_empty() && page.done,
            "empty directory is done",
        )?;
    }
    a.strict_equal(
        &file_code(&reader.next(0.0, &c).await),
        &Some(FileErrorCode::Invalid),
    )?;
    a.strict_equal(
        &file_code(&reader.next(1.0, &aborted()).await),
        &Some(FileErrorCode::Aborted),
    )?;
    file(reader.close(&c).await)?;
    file(reader.close(&c).await)?;
    a.strict_equal(
        &file_code(&reader.next(1.0, &c).await),
        &Some(FileErrorCode::Invalid),
    )
}

async fn dir_reader_refusals(env: Env) -> Result<()> {
    let a = create_expect_assertions();
    let c = context();
    write(&env, "file.txt", "x").await?;
    a.strict_equal(
        &file_code(&env.open_dir_reader(Path::new("missing"), &c).await),
        &Some(FileErrorCode::NotFound),
    )?;
    a.strict_equal(
        &file_code(&env.open_dir_reader(Path::new("file.txt"), &c).await),
        &Some(FileErrorCode::NotDirectory),
    )?;
    a.strict_equal(
        &file_code(&env.open_dir_reader(Path::new("."), &aborted()).await),
        &Some(FileErrorCode::Aborted),
    )
}

async fn dir_reader_removed(env: Env) -> Result<()> {
    let c = context();
    make_dir(&env, "dir").await?;
    for entry in ["x", "y", "z"] {
        write(&env, &format!("dir/{entry}"), entry).await?;
    }
    let reader = file(env.open_dir_reader(Path::new("dir"), &c).await)?;
    let result = async {
        for entry in ["x", "y", "z"] {
            remove_file(&env, &format!("dir/{entry}")).await?;
        }
        let mut entries = Vec::new();
        for _ in 0..10 {
            let page = file(reader.next(10.0, &c).await)?;
            entries.extend(page.entries.iter().map(name));
            if page.done {
                break;
            }
        }
        create_expect_assertions().deep_equal(&entries, &Vec::new())
    }
    .await;
    let _ = reader.close(&c).await;
    result
}

async fn watch_file_lifecycle(env: Env) -> Result<()> {
    let e = env.clone();
    watching(
        &env,
        vec![WatchTarget::new("AGENTS.md")],
        |changes| async move {
            expect_change(&e, &changes, "AGENTS.md", write(&e, "AGENTS.md", "one")).await?;
            expect_change(&e, &changes, "AGENTS.md", write(&e, "AGENTS.md", "two!")).await?;
            // Editors replace a file by renaming a new one over it.
            expect_change(&e, &changes, "AGENTS.md", async {
                write(&e, "AGENTS.md.tmp", "three").await?;
                rename(&e, "AGENTS.md.tmp", "AGENTS.md").await
            })
            .await?;
            expect_change(&e, &changes, "AGENTS.md", write(&e, "AGENTS.md", "four")).await?;
            expect_change(&e, &changes, "AGENTS.md", remove_file(&e, "AGENTS.md")).await
        },
    )
    .await
}

async fn watch_missing_ancestors(env: Env) -> Result<()> {
    let e = env.clone();
    watching(
        &env,
        vec![WatchTarget::new("a/b/c/AGENTS.md")],
        |changes| async move {
            expect_change(
                &e,
                &changes,
                "a/b/c/AGENTS.md",
                write(&e, "a/b/c/AGENTS.md", "x"),
            )
            .await
        },
    )
    .await
}

fn recursive(path: &str) -> WatchTarget {
    WatchTarget {
        recursive: true,
        ..WatchTarget::new(path)
    }
}

async fn watch_new_directories(env: Env) -> Result<()> {
    make_dir(&env, "skills").await?;
    let e = env.clone();
    watching(&env, vec![recursive("skills")], |changes| async move {
        // Written before any watcher on the new directories can exist.
        let target = "skills/a/b/SKILL.md";
        expect_change(&e, &changes, target, write(&e, target, "one")).await?;
        expect_change(&e, &changes, target, write(&e, target, "two!")).await?;
        let deeper = "skills/a/b/c/SKILL.md";
        expect_change(&e, &changes, deeper, write(&e, deeper, "deeper")).await
    })
    .await
}

async fn watch_renamed_parent(env: Env) -> Result<()> {
    write(&env, "proj/.pi/skills/x.md", "x").await?;
    let e = env.clone();
    watching(
        &env,
        vec![recursive("proj/.pi/skills")],
        |changes| async move {
            expect_change(
                &e,
                &changes,
                "proj/.pi/skills",
                rename(&e, "proj/.pi", "proj/old"),
            )
            .await?;
            let target = "proj/.pi/skills/y.md";
            expect_change(&e, &changes, target, write(&e, target, "y")).await?;
            expect_change(&e, &changes, target, write(&e, target, "yy")).await
        },
    )
    .await
}

async fn watch_exclusions(env: Env) -> Result<()> {
    let a = create_expect_assertions();
    make_dir(&env, "skills").await?;
    let target = WatchTarget {
        recursive: true,
        exclude: WatchExclude {
            hidden: true,
            names: vec!["node_modules".into()],
        },
        ..WatchTarget::new("skills")
    };
    let e = env.clone();
    watching(&env, vec![target], |changes| async move {
        write(&e, "skills/node_modules/dep/SKILL.md", "dep").await?;
        write(&e, "skills/.SKILL.md.tmp", "draft").await?;
        expect_change(
            &e,
            &changes,
            "skills/SKILL.md",
            rename(&e, "skills/.SKILL.md.tmp", "skills/SKILL.md"),
        )
        .await?;
        let hidden = [
            absolute(&e, "skills/node_modules").await?,
            absolute(&e, "skills/.SKILL.md.tmp").await?,
        ];
        for change in changes.lock().iter() {
            if let WatchChange::Paths(paths) = change {
                for path in paths {
                    let shown = path.to_string_lossy();
                    a.ok(
                        !hidden
                            .iter()
                            .any(|excluded| shown.starts_with(excluded.to_string_lossy().as_ref())),
                        &format!("excluded {shown}"),
                    )?;
                }
            }
        }
        Ok(())
    })
    .await
}

async fn watch_overlap(env: Env) -> Result<()> {
    write(&env, "skills/a/one.md", "one").await?;
    let e = env.clone();
    watching(
        &env,
        vec![WatchTarget::new("skills"), recursive("skills")],
        |changes| async move {
            expect_change(
                &e,
                &changes,
                "skills/a/two.md",
                write(&e, "skills/a/two.md", "two"),
            )
            .await
        },
    )
    .await
}

async fn watch_replaced_directory(env: Env) -> Result<()> {
    write(&env, "skills/a/x.md", "x").await?;
    let e = env.clone();
    watching(&env, vec![recursive("skills")], |changes| async move {
        expect_change(&e, &changes, "skills/a", async {
            rename(&e, "skills/a", "skills-old").await?;
            make_dir(&e, "skills/a").await
        })
        .await?;
        let target = "skills/a/y.md";
        expect_change(&e, &changes, target, write(&e, target, "y")).await?;
        expect_change(&e, &changes, target, write(&e, target, "yy")).await
    })
    .await
}

async fn watch_closed(env: Env) -> Result<()> {
    let a = create_expect_assertions();
    let c = context();
    let changes = Changes::default();
    let sink = changes.clone();
    let watcher = file(
        env.watch(
            &[WatchTarget::new("file.txt")],
            Arc::new(move |change| sink.lock().push(change)),
            &c,
        )
        .await,
    )?;
    a.ok(
        matches!(watcher.mode(), WatchMode::Native | WatchMode::Polling),
        "watcher reports its mode",
    )?;
    file(watcher.close(&c).await)?;
    file(watcher.close(&c).await)?;
    write(&env, "file.txt", "x").await?;
    pi_chord::rt::sleep(Duration::from_millis(300)).await;
    a.ok(changes.lock().is_empty(), "no changes after close")
}

async fn exec_argv_hostile(env: Env, options: EnvConformanceOptions) -> Result<()> {
    let a = create_expect_assertions();
    let hostile = "it's $(touch pwned) `touch pwned` *; touch pwned";
    let collected = exec_collect(
        &env,
        argv(
            &options.shell,
            &[r#"printf "%s|%s" "$1" "$2""#, "argv0", hostile, "a b"],
        ),
        None,
    )
    .await;
    a.strict_equal(&run(collected.result)?.exit_code, &0)?;
    a.strict_equal(&collected.stdout, &format!("{hostile}|a b"))?;
    a.strict_equal(
        &file(env.exists(Path::new("pwned"), &context()).await)?,
        &false,
    )
}

async fn exec_streams(env: Env, options: EnvConformanceOptions) -> Result<()> {
    let a = create_expect_assertions();
    let script = "printf out; printf err >&2; printf more";
    let forms = [
        exec_collect(&env, argv(&options.shell, &[script]), None).await,
        exec_collect(&env, ShellCommand::Script(JsonString::from(script)), None).await,
    ];
    for collected in forms {
        a.strict_equal(&run(collected.result)?.exit_code, &0)?;
        a.strict_equal(&collected.stdout, &"outmore".to_owned())?;
        a.strict_equal(&collected.stderr, &"err".to_owned())?;
    }
    Ok(())
}

async fn exec_cwd_and_exit(env: Env, options: EnvConformanceOptions) -> Result<()> {
    let a = create_expect_assertions();
    make_dir(&env, "sub").await?;
    let made = exec_collect(
        &env,
        argv(&options.shell, &["printf x > made.txt; exit 3"]),
        Some("sub"),
    )
    .await;
    a.strict_equal(&run(made.result)?.exit_code, &3)?;
    a.strict_equal(
        &file(
            env.read_text_file(Path::new("sub/made.txt"), &context())
                .await,
        )?,
        &"x".to_owned(),
    )
}

async fn exec_spawn_errors(env: Env) -> Result<()> {
    let a = create_expect_assertions();
    let spawn = Some(ExecutionErrorCode::SpawnError);
    let missing = ShellCommand::Argv(vec![JsonString::from(
        "pi-durable-conformance-missing-program",
    )]);
    a.strict_equal(
        &exec_code(
            &env.exec(missing, ShellExecOptions::default(), &context())
                .await,
        ),
        &spawn,
    )?;
    a.strict_equal(
        &exec_code(
            &env.exec(
                ShellCommand::Argv(Vec::new()),
                ShellExecOptions::default(),
                &context(),
            )
            .await,
        ),
        &spawn,
    )
}

#[derive(Default)]
struct WindowTally {
    bytes: usize,
    newlines: usize,
    tail: String,
    problems: Vec<String>,
}
async fn exec_window(env: Env, options: EnvConformanceOptions) -> Result<()> {
    let a = create_expect_assertions();
    let lines = 2000usize;
    let window = ShellOutputWindow {
        max_bytes: 200,
        max_lines: 5,
        min_interval_ms: 0.0,
        bytes_per_second: 1_000_000_000.0,
    };
    let tally = Arc::new(Mutex::new(WindowTally::default()));
    let sink = tally.clone();
    let script = format!("i=0; while [ $i -lt {lines} ]; do echo line-$i; i=$((i+1)); done");
    let result = env
        .exec(
            argv(&options.shell, &[&script]),
            ShellExecOptions {
                window: Some(window),
                on_output: Some(Arc::new(move |output, _context, info| {
                    let output = output.to_string_lossy();
                    let mut tally = sink.lock();
                    if let Some(skipped) = info.skipped {
                        tally.bytes += skipped.bytes;
                        tally.newlines += skipped.newlines;
                        let after = output.len();
                        let after_newlines = output.matches('\n').count();
                        if !(after > window.max_bytes || after_newlines > window.max_lines) {
                            tally
                                .problems
                                .push("a skip is followed by more than the window".into());
                        }
                        tally.tail.clear();
                    }
                    tally.bytes += output.len();
                    tally.newlines += output.matches('\n').count();
                    tally.tail.push_str(&output);
                    Ok(())
                })),
                ..Default::default()
            },
            &context(),
        )
        .await;
    a.strict_equal(&run(result)?.exit_code, &0)?;
    let expected: Vec<String> = (0..lines).map(|index| format!("line-{index}\n")).collect();
    let tally = tally.lock();
    a.deep_equal(&tally.problems, &Vec::new())?;
    a.strict_equal(&tally.bytes, &expected.concat().len())?;
    a.strict_equal(&tally.newlines, &lines)?;
    a.ok(
        tally
            .tail
            .ends_with(&expected[lines - window.max_lines..].concat()),
        "the delivered output ends with the tail",
    )
}

async fn exec_timeout_and_abort(env: Env, options: EnvConformanceOptions) -> Result<()> {
    let a = create_expect_assertions();
    let timed_out = env
        .exec(
            argv(&options.shell, &["sleep 2"]),
            ShellExecOptions {
                timeout: Some(0.1),
                ..Default::default()
            },
            &context(),
        )
        .await;
    a.strict_equal(&exec_code(&timed_out), &Some(ExecutionErrorCode::Timeout))?;
    let (cancellable, controller) = with_cancel(&context());
    let running = env.exec(
        argv(&options.shell, &["sleep 2"]),
        ShellExecOptions::default(),
        &cancellable,
    );
    pi_chord::rt::spawn(async move {
        pi_chord::rt::sleep(Duration::from_millis(100)).await;
        controller.abort();
    });
    a.strict_equal(
        &exec_code(&running.await),
        &Some(ExecutionErrorCode::Aborted),
    )
}

async fn link_with(env: &Env, options: &EnvConformanceOptions, script: &str) -> Result<()> {
    let linked = env
        .exec(
            argv(&options.shell, &[script]),
            ShellExecOptions::default(),
            &context(),
        )
        .await;
    create_expect_assertions().strict_equal(&run(linked)?.exit_code, &0)
}

async fn binary_reader_symlinks(env: Env, options: EnvConformanceOptions) -> Result<()> {
    let a = create_expect_assertions();
    let c = context();
    write(&env, "target.txt", "target").await?;
    make_dir(&env, "sub").await?;
    write(&env, "sub/inner.txt", "inner").await?;
    link_with(
        &env,
        &options,
        "ln -s target.txt link.txt && ln -s sub dirlink",
    )
    .await?;

    let followed = file(
        env.open_binary_reader(Path::new("link.txt"), false, &c)
            .await,
    )?;
    a.strict_equal(
        &text(&file(followed.read(0.0, 10.0, &c).await)?),
        &"target".to_owned(),
    )?;
    file(followed.close(&c).await)?;

    a.strict_equal(
        &file_code(
            &env.open_binary_reader(Path::new("link.txt"), true, &c)
                .await,
        ),
        &Some(FileErrorCode::Invalid),
    )?;

    // Only the final component is refused; earlier symlinked directories still resolve.
    let inner = file(
        env.open_binary_reader(Path::new("dirlink/inner.txt"), true, &c)
            .await,
    )?;
    a.strict_equal(
        &text(&file(inner.read(0.0, 10.0, &c).await)?),
        &"inner".to_owned(),
    )?;
    file(inner.close(&c).await)
}

async fn watch_symlink(env: Env, options: EnvConformanceOptions) -> Result<()> {
    write(&env, "data/real.md", "one").await?;
    make_dir(&env, "config").await?;
    link_with(&env, &options, "ln -s ../data/real.md config/AGENTS.md").await?;
    let e = env.clone();
    watching(
        &env,
        vec![WatchTarget::new("config/AGENTS.md")],
        |changes| async move {
            expect_change(
                &e,
                &changes,
                "config/AGENTS.md",
                write(&e, "data/real.md", "two!"),
            )
            .await
        },
    )
    .await
}

/// A provider that runs each case in a fresh private directory under the system
/// temporary directory, backed by the native environment, and removes it afterwards.
#[cfg(not(target_arch = "wasm32"))]
pub fn native_env_provider() -> EnvConformanceProvider {
    Arc::new(|body| {
        Box::pin(async move {
            let scratch = file(
                file(NativeFileSystem::new("."))?
                    .create_temp_dir(Some("pi-durable-conformance-"), &context())
                    .await,
            )?;
            let root = std::fs::canonicalize(&scratch)
                .map_err(|error| Error::message(error.to_string()))?;
            let env: Env = Arc::new(file(NativeExecutionEnv::new(&root))?);
            let result = body(env.clone()).await;
            let cleaned = run(env.cleanup(&context()).await);
            let _ = std::fs::remove_dir_all(&root);
            result.and(cleaned)
        })
    })
}
