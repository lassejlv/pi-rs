use pi_durable::{Context, env::*};
use std::path::Path;

#[cfg(unix)]
#[tokio::test]
async fn binary_no_follow_refuses_only_the_final_link_and_keeps_the_opened_inode() {
    let directory = tempfile::tempdir().unwrap();
    let fs = NativeFileSystem::new(directory.path()).unwrap();
    let context = Context::default();
    fs.write_file(Path::new("sub/first"), b"one", &context)
        .await
        .unwrap();
    std::os::unix::fs::symlink("sub", directory.path().join("linked-directory")).unwrap();
    std::os::unix::fs::symlink("sub/first", directory.path().join("linked-file")).unwrap();
    let error = fs
        .open_binary_reader(Path::new("linked-file"), true, &context)
        .await
        .err()
        .unwrap();
    assert_eq!(error.code(), FileErrorCode::Invalid);
    let reader = fs
        .open_binary_reader(Path::new("linked-directory/first"), true, &context)
        .await
        .unwrap();
    fs.rename_file(Path::new("sub/first"), Path::new("sub/second"), &context)
        .await
        .unwrap();
    fs.write_file(Path::new("sub/first"), b"replacement", &context)
        .await
        .unwrap();
    assert_eq!(reader.read(0.0, 100.0, &context).await.unwrap(), b"one");
    assert_eq!(reader.info(&context).await.unwrap().size, 3);
    reader.close(&context).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_directory_reader_seals_at_the_close_call_even_before_it_is_awaited() {
    let directory = tempfile::tempdir().unwrap();
    let fs = NativeFileSystem::new(directory.path()).unwrap();
    let context = Context::default();
    fs.write_file(Path::new("file"), b"x", &context)
        .await
        .unwrap();
    for _ in 0..128 {
        let reader = fs.open_dir_reader(Path::new("."), &context).await.unwrap();
        let closing = reader.close(&context);
        let after = reader.next(1.0, &context).await;
        closing.await.unwrap();
        assert!(
            matches!(after,Err(error) if error.code()==FileErrorCode::Invalid),
            "close must seal admission at the call"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn temporary_directories_are_private_before_their_paths_are_published() {
    use std::os::unix::fs::PermissionsExt;
    let fs = NativeFileSystem::new(".").unwrap();
    let path = fs
        .create_temp_dir(Some("pi-durable-private-"), &Context::default())
        .await
        .unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    std::fs::remove_dir(path).unwrap();
    assert_eq!(mode, 0o700);
}
