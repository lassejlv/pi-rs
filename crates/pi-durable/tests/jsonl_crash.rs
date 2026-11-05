//! Real child processes are killed at publication/reclamation boundaries. No
//! storage/session destructor gets a chance to repair the interrupted write.
use pi_durable::{env::*, *};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
struct CrashFs {
    inner: Arc<NativeFileSystem>,
    point: String,
}
fn ready(point: &str) -> ! {
    println!("CRASH_READY {point}");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}
macro_rules! forward {
    ($method:ident($($arg:ident:$ty:ty),*) -> $out:ty)=>{
        fn $method(&self,$($arg:$ty,)*ctx:&Context)->FileFuture<$out>{self.inner.$method($($arg,)*ctx)}
    };
}
impl FileSystem for CrashFs {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn cwd(&self) -> PathBuf {
        self.inner.cwd()
    }
    forward!(absolute_path(path:&Path)->PathBuf);
    forward!(join_path(parts:&[PathBuf])->PathBuf);
    forward!(read_binary_file(path:&Path)->Vec<u8>);
    forward!(truncate_file(path:&Path,size:u64)->());
    forward!(list_dir(path:&Path)->Vec<FileInfo>);
    forward!(create_dir(path:&Path,recursive:bool)->());
    fn append_file(&self, path: &Path, bytes: &[u8], context: &Context) -> FileFuture<()> {
        let (fs, path, bytes, context, point) = (
            self.inner.clone(),
            path.to_owned(),
            bytes.to_vec(),
            context.clone(),
            self.point.clone(),
        );
        Box::pin(async move {
            let main = path.file_name().unwrap() == "main.jsonl";
            if point
                == if main {
                    "before_main"
                } else {
                    "before_sidecar"
                }
            {
                ready(&point)
            }
            if point
                == if main {
                    "partial_main"
                } else {
                    "partial_sidecar"
                }
            {
                fs.append_file(&path, &bytes[..bytes.len() / 2], &context)
                    .await?;
                ready(&point)
            }
            fs.append_file(&path, &bytes, &context).await?;
            if point == if main { "after_main" } else { "after_sidecar" } {
                ready(&point)
            }
            Ok(())
        })
    }
    fn flush_file(&self, path: &Path, context: &Context) -> FileFuture<()> {
        let (fs, path, context, point) = (
            self.inner.clone(),
            path.to_owned(),
            context.clone(),
            self.point.clone(),
        );
        Box::pin(async move {
            fs.flush_file(&path, &context).await?;
            let stage = if path.file_name().unwrap() == "main.jsonl" {
                "after_main_flush"
            } else if path.extension().is_some_and(|e| e == "reclaim") {
                "after_reclaim_flush"
            } else {
                "after_sidecar_flush"
            };
            if point == stage {
                ready(&point)
            }
            Ok(())
        })
    }
    fn write_file(&self, path: &Path, bytes: &[u8], context: &Context) -> FileFuture<()> {
        let (fs, path, bytes, context, point) = (
            self.inner.clone(),
            path.to_owned(),
            bytes.to_vec(),
            context.clone(),
            self.point.clone(),
        );
        Box::pin(async move {
            if point == "before_reclaim_write" {
                ready(&point)
            }
            fs.write_file(&path, &bytes, &context).await?;
            if point == "after_reclaim_write" {
                ready(&point)
            }
            Ok(())
        })
    }
    fn rename_file(&self, source: &Path, destination: &Path, context: &Context) -> FileFuture<()> {
        let (fs, source, destination, context, point) = (
            self.inner.clone(),
            source.to_owned(),
            destination.to_owned(),
            context.clone(),
            self.point.clone(),
        );
        Box::pin(async move {
            if point == "before_reclaim_rename" {
                ready(&point)
            }
            fs.rename_file(&source, &destination, &context).await?;
            if point == "after_reclaim_rename" {
                ready(&point)
            }
            Ok(())
        })
    }
    fn remove(
        &self,
        path: &Path,
        recursive: bool,
        force: bool,
        context: &Context,
    ) -> FileFuture<()> {
        let (fs, path, context, point) = (
            self.inner.clone(),
            path.to_owned(),
            context.clone(),
            self.point.clone(),
        );
        Box::pin(async move {
            if point == "before_remove" {
                ready(&point)
            }
            fs.remove(&path, recursive, force, &context).await?;
            if point == "after_remove" {
                ready(&point)
            }
            Ok(())
        })
    }
}
fn base(count: u32) -> DocumentBase {
    DocumentBase {
        version: 1,
        value: serde_json::from_str(&format!("{{\"count\":{count},\"text\":\"€\"}}")).unwrap(),
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_crash_child() {
    let Ok(directory) = std::env::var("PI_DURABLE_CRASH_DIRECTORY") else {
        return;
    };
    let point = std::env::var("PI_DURABLE_CRASH_POINT").unwrap();
    let fs = Arc::new(CrashFs {
        inner: Arc::new(NativeFileSystem::new(&directory).unwrap()),
        point: point.clone(),
    });
    let storage = JsonlStorage::open(
        directory,
        fs,
        &Context::default(),
        JsonlStorageOptions { fsync: true },
    )
    .await
    .unwrap();
    let write = if point.ends_with("remove") {
        StorageWrite::DocumentRetire {
            id: DocumentId::from_raw(64),
        }
    } else {
        let DocumentBase { version, value } = base(1);
        StorageWrite::DocumentChange {
            id: DocumentId::from_raw(64),
            content: DocumentContent::Base { version, value },
        }
    };
    storage
        .commit(
            &[write],
            Box::new(move |_| {
                if point == "adopt" {
                    ready(&point)
                }
            }),
            &Context::default(),
        )
        .await
        .unwrap();
    panic!("crash point was not reached");
}
#[tokio::test]
async fn kills_before_and_after_publication_recover_exactly_the_confirmed_prefix() {
    use tokio::io::AsyncBufReadExt;
    for (point, committed) in [
        ("before_sidecar", false),
        ("partial_sidecar", false),
        ("after_sidecar", false),
        ("after_sidecar_flush", false),
        ("before_main", false),
        ("partial_main", false),
        ("after_main", true),
        ("adopt", true),
        ("after_main_flush", true),
        ("before_reclaim_write", true),
        ("after_reclaim_write", true),
        ("after_reclaim_flush", true),
        ("before_reclaim_rename", true),
        ("after_reclaim_rename", true),
        ("before_remove", true),
        ("after_remove", true),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let context = Context::default();
        let options = JsonlStorageOptions { fsync: true };
        let storage = JsonlStorage::open_native(directory.path(), &context, options)
            .await
            .unwrap();
        storage
            .commit(
                &[StorageWrite::Conversation {
                    value: ConversationRecord {
                        id: ROOT_CONVERSATION_ID,
                        parent: None,
                        owner: None,
                    },
                }],
                Box::new(|_| {}),
                &context,
            )
            .await
            .unwrap();
        storage
            .commit(
                &[StorageWrite::DocumentCreate {
                    record: DocumentCreate {
                        id: DocumentId::from_raw(64),
                        kind: "crash".into(),
                        key: None,
                        semantics: DocumentSemantics::Session,
                    },
                    content: base(0),
                }],
                Box::new(|_| {}),
                &context,
            )
            .await
            .unwrap();
        storage.close(&context).await.unwrap();
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("jsonl_crash_child")
            .arg("--nocapture")
            .env("PI_DURABLE_CRASH_DIRECTORY", directory.path())
            .env("PI_DURABLE_CRASH_POINT", point)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let output = child.stdout.take().unwrap();
        let mut lines = tokio::io::BufReader::new(output).lines();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let line = lines
                    .next_line()
                    .await
                    .unwrap()
                    .unwrap_or_else(|| panic!("child exited before {point}"));
                if line.starts_with("CRASH_READY ") {
                    assert_eq!(line, format!("CRASH_READY {point}"));
                    break;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("child did not reach {point}"));
        child.kill().await.unwrap();
        let status = child.wait().await.unwrap();
        assert!(!status.success());
        let recovered = JsonlStorage::open_native(directory.path(), &context, options)
            .await
            .unwrap();
        let document = recovered
            .document(DocumentId::from_raw(64), DocumentPoint::Current, &context)
            .await
            .unwrap();
        if point.ends_with("remove") {
            assert!(document.is_none(), "{point}");
        } else {
            assert_eq!(
                document.unwrap().value,
                base(u32::from(committed)).value,
                "{point}"
            );
        }
        assert_eq!(
            recovered.mint_id().await.unwrap(),
            65,
            "committed IDs survived {point}"
        );
        assert_eq!(
            recovered
                .commit(&[], Box::new(|_| {}), &context)
                .await
                .unwrap()
                .get(),
            if committed { 4 } else { 3 },
            "sequence after {point}"
        );
        assert!(
            std::fs::read_dir(directory.path()).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".reclaim")),
            "temporary reclaim at {point}"
        );
        recovered.close(&context).await.unwrap();
    }
}
