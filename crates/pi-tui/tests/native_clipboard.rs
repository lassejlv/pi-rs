//! The arboard-backed native clipboard against a real X server. Linux only; starts its own `Xvfb`
//! (skipped when it is not installed or its display is unusable).

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use pi_tui::native_platform::{ClipboardRead, get_native_clipboard};

struct Xvfb(Child);

impl Drop for Xvfb {
    fn drop(&mut self) {
        // SIGTERM (not `kill()`'s SIGKILL) so Xvfb removes its `/tmp/.X<n>-lock` file and socket;
        // stale ones make later servers on that display fail to start.
        // SAFETY: plain kill(2) on our own child's pid.
        unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM) };
        for _ in 0..100 {
            if matches!(self.0.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts Xvfb on a free display it picks itself (`-displayfd`), and waits until it accepts
/// clients. `None` (the test skips) when Xvfb is missing or the display is unusable.
fn start_xvfb() -> Option<(Xvfb, String)> {
    let mut child = Command::new("Xvfb")
        .args(["-displayfd", "1", "-screen", "0", "64x64x24", "-nolisten", "tcp"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let server = Xvfb(child);
    // Xvfb writes the display number once it is ready to accept connections.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx.recv_timeout(Duration::from_secs(20)).ok()?;
    let number: u32 = line.trim().parse().ok()?;
    let display = format!(":{number}");
    // SAFETY: this test binary has a single test; nothing else reads the environment concurrently.
    unsafe { std::env::set_var("DISPLAY", &display) };
    // A display the clipboard cannot connect to (broken X setup) also means "skip".
    if let Err(err) = arboard::Clipboard::new() {
        eprintln!("skipping: cannot open the X clipboard on {display}: {err}");
        return None;
    }
    Some((server, display))
}

#[tokio::test]
async fn reads_text_image_and_files_from_x11() {
    let Some((_server, _display)) = start_xvfb() else {
        eprintln!("skipping: Xvfb is not available or its display is unusable");
        return;
    };
    let native = get_native_clipboard().expect("an X11 display enables the native helper");
    // Like the TS X11 helper, Linux writes go through command-line tools.
    assert!(!native.can_set_text());
    assert!(native.set_text("x").is_none());

    // Another clipboard owner (arboard keeps ownership while the handle lives).
    let mut owner = arboard::Clipboard::new().unwrap();
    assert_eq!(native.get_text().await, Ok(ClipboardRead::Empty));

    owner.set_text("héllo from x11").unwrap();
    assert_eq!(native.get_text().await, Ok(ClipboardRead::Data("héllo from x11".to_string())));

    owner
        .set_image(arboard::ImageData { width: 1, height: 1, bytes: vec![1, 2, 3, 255].into() })
        .unwrap();
    match native.get_image().await {
        Ok(ClipboardRead::Data(png)) => assert!(png.starts_with(b"\x89PNG"), "not a PNG"),
        other => panic!("expected an image, got {other:?}"),
    }

    let dir = std::env::temp_dir().join(format!("pi-tui-clip-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("a b.txt");
    std::fs::write(&file, "x").unwrap();
    let file = file.canonicalize().unwrap();
    owner.set().file_list(&[&file]).unwrap();
    let files = native.get_file_paths().expect("file paths are supported").await;
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(files, Ok(ClipboardRead::Data(vec![file.display().to_string()])));
}
