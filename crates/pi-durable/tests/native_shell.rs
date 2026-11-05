use pi_chord::{AbortController, context::with_abort_signal};
use pi_durable::{Context, env::*};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

fn output() -> (Arc<Mutex<String>>, ShellOutput) {
    let text = Arc::new(Mutex::new(String::new()));
    let collected = text.clone();
    (
        text,
        Arc::new(move |chunk, _, _| {
            collected.lock().unwrap().push_str(&chunk.to_string_lossy());
            Ok(())
        }),
    )
}
#[cfg(unix)]
#[tokio::test]
async fn script_argv_and_signal_status_use_native_processes() {
    let directory = tempfile::tempdir().unwrap();
    let env = NativeExecutionEnv::new(directory.path()).unwrap();
    let (text, on_output) = output();
    let result = env
        .exec(
            ShellCommand::Script("printf out; printf err >&2".into()),
            ShellExecOptions {
                on_output: Some(on_output),
                ..Default::default()
            },
            &Context::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert!(text.lock().unwrap().contains("out"));
    assert!(text.lock().unwrap().contains("err"));
    let (text, on_output) = output();
    let result = env
        .exec(
            ShellCommand::Argv(vec!["/bin/echo".into(), "one; two $HOME".into()]),
            ShellExecOptions {
                on_output: Some(on_output),
                ..Default::default()
            },
            &Context::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(*text.lock().unwrap(), "one; two $HOME\n");
    #[cfg(unix)]
    {
        let result = env
            .exec(
                ShellCommand::Script("kill -9 $$".into()),
                ShellExecOptions::default(),
                &Context::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.exit_code, 137);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_timeouts_are_side_effect_free_and_cancellation_is_command_local() {
    let directory = tempfile::tempdir().unwrap();
    let env = NativeExecutionEnv::new(directory.path()).unwrap();
    for timeout in [0.0, -1.0, f64::NAN, f64::INFINITY, 2_147_484.0] {
        let error = env
            .exec(
                ShellCommand::Script("touch invalid".into()),
                ShellExecOptions {
                    timeout: Some(timeout),
                    ..Default::default()
                },
                &Context::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), ExecutionErrorCode::Timeout);
    }
    assert!(!directory.path().join("invalid").exists());
    let controller = AbortController::new();
    let first = env.exec(
        ShellCommand::Script("touch first; sleep 60".into()),
        ShellExecOptions::default(),
        &with_abort_signal(controller.signal(), &Context::default()),
    );
    let second = env.exec(
        ShellCommand::Script("sleep 0.1; printf preserved".into()),
        ShellExecOptions::default(),
        &Context::default(),
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while !directory.path().join("first").exists() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    controller.abort();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), first)
            .await
            .unwrap()
            .unwrap_err()
            .code(),
        ExecutionErrorCode::Aborted
    );
    assert_eq!(second.await.unwrap().exit_code, 0);
}
#[cfg(unix)]
struct SlowSpill {
    path: std::path::PathBuf,
    bytes: Mutex<Vec<u8>>,
}
#[cfg(unix)]
impl ShellSpillWriter for SlowSpill {
    fn write(&self, bytes: Vec<u8>) -> ExecutionFuture<bool> {
        self.bytes.lock().unwrap().extend(bytes);
        Box::pin(async { Ok(false) })
    }
    fn drain(&self) -> ExecutionFuture<()> {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(600)).await;
            Ok(())
        })
    }
    fn finish(&self) -> ExecutionFuture<()> {
        let (path, bytes) = (self.path.clone(), self.bytes.lock().unwrap().clone());
        Box::pin(async move {
            tokio::fs::write(path, bytes).await.map_err(|error| {
                ExecutionError::new(ExecutionErrorCode::Unknown, error.to_string())
            })
        })
    }
    fn settled(&self) -> ExecutionFuture<()> {
        Box::pin(std::future::pending())
    }
}
#[cfg(unix)]
#[tokio::test]
async fn post_exit_stdio_remains_open_while_the_spill_is_backpressured() {
    let directory = tempfile::tempdir().unwrap();
    let env = NativeExecutionEnv::new(directory.path())
        .unwrap()
        .with_shell_options(NativeShellOptions {
            spill_opener: Some(Arc::new(|path| {
                Box::pin(async move {
                    Ok(Arc::new(SlowSpill {
                        path,
                        bytes: Mutex::new(Vec::new()),
                    }) as Arc<dyn ShellSpillWriter>)
                })
            })),
            ..Default::default()
        });
    let (text, on_output) = output();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        env.exec(
            ShellCommand::Script(
                "printf '%020d' 0 | tr 0 a; (sleep 0.2; printf '%01000d' 0 | tr 0 b) &".into(),
            ),
            ShellExecOptions {
                spill: Some(ShellSpillOptions {
                    after_bytes: 10.0,
                    after_lines: 10.0,
                }),
                on_output: Some(on_output),
                ..Default::default()
            },
            &Context::default(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let expected = format!("{}{}", "a".repeat(20), "b".repeat(1000));
    assert_eq!(*text.lock().unwrap(), expected);
    let path = result.spill_path.unwrap();
    assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), expected);
    tokio::fs::remove_dir_all(path.parent().unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn every_shell_line_is_delivered_and_spilled_in_order() {
    let directory = tempfile::tempdir().unwrap();
    let env = NativeExecutionEnv::new(directory.path()).unwrap();
    let (text, on_output) = output();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        env.exec(
            ShellCommand::Script(
                "i=1; while [ $i -le 15000 ]; do echo line-$i; i=$((i+1)); done".into(),
            ),
            ShellExecOptions {
                on_output: Some(on_output),
                spill: Some(ShellSpillOptions {
                    after_bytes: 1024.0 * 1024.0,
                    after_lines: 100.0,
                }),
                ..Default::default()
            },
            &Context::default(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let expected = (1..=15000)
        .map(|i| format!("line-{i}\n"))
        .collect::<String>();
    assert_eq!(*text.lock().unwrap(), expected);
    let path = result.spill_path.unwrap();
    assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), expected);
    tokio::fs::remove_dir_all(path.parent().unwrap())
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abort_kills_a_shells_background_descendants() {
    let directory = tempfile::tempdir().unwrap();
    let env = NativeExecutionEnv::new(directory.path()).unwrap();
    let controller = AbortController::new();
    let running = env.exec(
        ShellCommand::Script("sleep 60 & echo $! > descendant; touch begun; wait".into()),
        ShellExecOptions::default(),
        &with_abort_signal(controller.signal(), &Context::default()),
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while !directory.path().join("begun").exists() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    let pid: u32 = tokio::fs::read_to_string(directory.path().join("descendant"))
        .await
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    controller.abort();
    assert_eq!(
        running.await.unwrap_err().code(),
        ExecutionErrorCode::Aborted
    );
    let ended = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let status = tokio::process::Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "stat="])
                .output()
                .await
                .unwrap();
            if !status.status.success()
                || String::from_utf8_lossy(&status.stdout)
                    .trim()
                    .starts_with('Z')
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    if ended.is_err() {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    assert!(
        ended.is_ok(),
        "the shell exited but its descendant survived cancellation"
    );
}

#[tokio::test]
async fn spill_thresholds_preserve_javascript_number_comparisons() {
    let directory = tempfile::tempdir().unwrap();
    let env = NativeExecutionEnv::new(directory.path()).unwrap();
    // Independently executed against the pinned NodeExecutionEnv.
    for (after_bytes, spilled) in [
        (f64::INFINITY, false),
        (f64::NAN, true),
        (4.5, true),
        (-1.0, true),
    ] {
        let result = env
            .exec(
                ShellCommand::Script("printf short".into()),
                ShellExecOptions {
                    spill: Some(ShellSpillOptions {
                        after_bytes,
                        after_lines: f64::INFINITY,
                    }),
                    ..Default::default()
                },
                &Context::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.spill_path.is_some(),
            spilled,
            "threshold {after_bytes}"
        );
        if let Some(path) = result.spill_path {
            assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "short");
            tokio::fs::remove_dir_all(path.parent().unwrap())
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn command_cwd_is_captured_at_admission_including_relative_overrides() {
    let directory = tempfile::tempdir().unwrap();
    let a = directory.path().join("a");
    let b = directory.path().join("b");
    tokio::fs::create_dir_all(a.join("child")).await.unwrap();
    tokio::fs::create_dir_all(b.join("child")).await.unwrap();
    let env = NativeExecutionEnv::new(&a).unwrap();
    for relative in [false, true] {
        env.set_cwd(&a);
        let running = env.exec(
            ShellCommand::Script("printf x > location".into()),
            ShellExecOptions {
                cwd: relative.then(|| "child".into()),
                ..Default::default()
            },
            &Context::default(),
        );
        env.set_cwd(&b);
        running.await.unwrap();
        let suffix = if relative {
            "child/location"
        } else {
            "location"
        };
        assert!(
            a.join(suffix).exists(),
            "execution observed a later cwd mutation"
        );
        assert!(!b.join(suffix).exists());
    }
}
