//! Native platform helpers. Port of `native-platform.ts` and `native-modifiers.ts`.
//!
//! The TypeScript package loads prebuilt N-API modules (`*.node`) for clipboard access on
//! macOS/Windows and X11 clipboard access on Linux. The Rust port implements the same helper with
//! the pure-Rust [`arboard`] crate and offers it exactly where TS would load its addon:
//! [`get_native_clipboard`] returns it on macOS and Windows, and on Linux only when `DISPLAY` is
//! set (the X11 helper; Wayland-only sessions use `wl-paste`/`wl-copy` in the callers, like TS).
//! As in TS, the Linux helper cannot write (`set_text` is `None`): callers use command-line tools
//! there because they keep owning the clipboard selection after pi returns.
//!
//! Modifier-key state (`isModifierPressed`, macOS/Windows only in TS) uses the same system calls
//! as the TS addons ([`is_native_modifier_pressed`]).

use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};

use crate::event_loop::BoxFuture;

/// Modifier keys that can be queried natively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifierKey {
    Shift,
    Command,
    Control,
    Option,
}

/// Result of a clipboard read: `Unavailable` (no access), `Empty` (no data of that kind) or the data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardRead<T> {
    Unavailable,
    Empty,
    Data(T),
}

/// System clipboard access.
pub trait NativeClipboard: Send + Sync {
    fn get_text(&self) -> BoxFuture<Result<ClipboardRead<String>, String>>;
    /// PNG-encoded image data.
    fn get_image(&self) -> BoxFuture<Result<ClipboardRead<Vec<u8>>, String>>;
    /// POSIX paths of file URLs on the clipboard.
    fn get_file_paths(&self) -> Option<BoxFuture<Result<ClipboardRead<Vec<String>>, String>>> {
        None
    }
    fn set_text(&self, _text: &str) -> Option<BoxFuture<Result<(), String>>> {
        None
    }
    /// Whether [`NativeClipboard::set_text`] is supported (TS: `setText` is defined), without writing.
    fn can_set_text(&self) -> bool {
        false
    }
}

/// The native clipboard helper for this platform, where TS would load its native addon: macOS and
/// Windows always, Linux only with an X11 `DISPLAY`. The display is opened on each request, so a
/// disconnected display can recover (TS caches the module, not display availability).
pub fn get_native_clipboard() -> Option<Arc<dyn NativeClipboard>> {
    match std::env::consts::OS {
        "macos" | "windows" => Some(Arc::new(ArboardClipboard { can_write: true })),
        "linux" if std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty()) => {
            Some(Arc::new(ArboardClipboard { can_write: false }))
        }
        _ => None,
    }
}

/// Whether `key` is currently held, as the TS native helpers report it: macOS asks the combined
/// session's event source (`CGEventSourceFlagsState`), Windows `GetAsyncKeyState`; other
/// platforms have no helper (`false`).
pub fn is_native_modifier_pressed(key: ModifierKey) -> bool {
    modifier_state::is_pressed(key)
}

#[cfg(target_os = "macos")]
mod modifier_state {
    use super::ModifierKey;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceFlagsState(state_id: i32) -> u64;
    }

    const COMBINED_SESSION_STATE: i32 = 0;

    pub(super) fn is_pressed(key: ModifierKey) -> bool {
        let mask: u64 = match key {
            ModifierKey::Shift => 0x0002_0000,
            ModifierKey::Control => 0x0004_0000,
            ModifierKey::Option => 0x0008_0000,
            ModifierKey::Command => 0x0010_0000,
        };
        // SAFETY: a pure query of the window server's modifier flags.
        let flags = unsafe { CGEventSourceFlagsState(COMBINED_SESSION_STATE) };
        flags & mask != 0
    }
}

#[cfg(windows)]
mod modifier_state {
    use super::ModifierKey;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetAsyncKeyState(virtual_key: i32) -> i16;
    }

    fn key_pressed(virtual_key: i32) -> bool {
        // SAFETY: a pure query of the asynchronous key state.
        (unsafe { GetAsyncKeyState(virtual_key) } as u16 & 0x8000) != 0
    }

    pub(super) fn is_pressed(key: ModifierKey) -> bool {
        let keys: &[i32] = match key {
            ModifierKey::Shift => &[0x10, 0xA0, 0xA1],
            ModifierKey::Control => &[0x11, 0xA2, 0xA3],
            ModifierKey::Option => &[0x12, 0xA4, 0xA5],
            ModifierKey::Command => &[0x5B, 0x5C],
        };
        keys.iter().any(|k| key_pressed(*k))
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod modifier_state {
    use super::ModifierKey;

    pub(super) fn is_pressed(_key: ModifierKey) -> bool {
        false
    }
}

/// [`NativeClipboard`] over `arboard`. Each call opens the clipboard on a short-lived thread
/// (arboard is blocking; X11 reads wait for the selection owner).
struct ArboardClipboard {
    can_write: bool,
}

/// Run `f` on its own thread and resolve with its result (a panic or a failed spawn is an error).
fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> BoxFuture<Result<T, String>> {
    type Slot<T> = Arc<Mutex<(Option<Result<T, String>>, Option<Waker>)>>;
    let slot: Slot<T> = Arc::new(Mutex::new((None, None)));
    let writer = slot.clone();
    let spawned = std::thread::Builder::new().name("pi-clipboard".to_string()).spawn(move || {
        let value = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
            .unwrap_or_else(|_| Err("clipboard helper panicked".to_string()));
        let mut guard = writer.lock().unwrap_or_else(|e| e.into_inner());
        guard.0 = Some(value);
        if let Some(waker) = guard.1.take() {
            waker.wake();
        }
    });
    if let Err(error) = spawned {
        slot.lock().unwrap_or_else(|e| e.into_inner()).0 = Some(Err(error.to_string()));
    }
    Box::pin(std::future::poll_fn(move |cx| {
        let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(value) = guard.0.take() {
            return Poll::Ready(value);
        }
        guard.1 = Some(cx.waker().clone());
        Poll::Pending
    }))
}

/// `arboard::Clipboard::new()`, `None` when the clipboard cannot be opened (no display access).
fn open() -> Option<arboard::Clipboard> {
    arboard::Clipboard::new().ok()
}

fn read_error<T>(error: arboard::Error) -> Result<ClipboardRead<T>, String> {
    match error {
        arboard::Error::ContentNotAvailable => Ok(ClipboardRead::Empty),
        arboard::Error::ClipboardNotSupported | arboard::Error::ClipboardOccupied => Ok(ClipboardRead::Unavailable),
        other => Err(other.to_string()),
    }
}

/// Encode arboard's RGBA image as PNG (the TS helpers return PNG bytes).
fn encode_png(image: arboard::ImageData<'_>) -> Result<Vec<u8>, String> {
    let width = u32::try_from(image.width).map_err(|e| e.to_string())?;
    let height = u32::try_from(image.height).map_err(|e| e.to_string())?;
    let buffer = image::RgbaImage::from_raw(width, height, image.bytes.into_owned())
        .ok_or_else(|| "clipboard image has an invalid size".to_string())?;
    let mut out = std::io::Cursor::new(Vec::new());
    buffer.write_to(&mut out, image::ImageFormat::Png).map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

impl NativeClipboard for ArboardClipboard {
    fn get_text(&self) -> BoxFuture<Result<ClipboardRead<String>, String>> {
        blocking(|| {
            let Some(mut clipboard) = open() else { return Ok(ClipboardRead::Unavailable) };
            match clipboard.get_text() {
                Ok(text) if text.is_empty() => Ok(ClipboardRead::Empty),
                Ok(text) => Ok(ClipboardRead::Data(text)),
                Err(error) => read_error(error),
            }
        })
    }

    fn get_image(&self) -> BoxFuture<Result<ClipboardRead<Vec<u8>>, String>> {
        blocking(|| {
            let Some(mut clipboard) = open() else { return Ok(ClipboardRead::Unavailable) };
            match clipboard.get_image() {
                Ok(image) if image.width == 0 || image.height == 0 => Ok(ClipboardRead::Empty),
                Ok(image) => encode_png(image).map(ClipboardRead::Data),
                Err(error) => read_error(error),
            }
        })
    }

    fn get_file_paths(&self) -> Option<BoxFuture<Result<ClipboardRead<Vec<String>>, String>>> {
        Some(blocking(|| {
            let Some(mut clipboard) = open() else { return Ok(ClipboardRead::Unavailable) };
            match clipboard.get().file_list() {
                Ok(paths) if paths.is_empty() => Ok(ClipboardRead::Empty),
                Ok(paths) => Ok(ClipboardRead::Data(paths.into_iter().map(|p| p.display().to_string()).collect())),
                Err(error) => read_error(error),
            }
        }))
    }

    fn can_set_text(&self) -> bool {
        self.can_write
    }

    fn set_text(&self, text: &str) -> Option<BoxFuture<Result<(), String>>> {
        if !self.can_write {
            return None;
        }
        let text = text.to_string();
        Some(blocking(move || {
            let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
            clipboard.set_text(text).map_err(|e| e.to_string())
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_encoding_round_trips() {
        let image = arboard::ImageData { width: 2, height: 1, bytes: vec![255, 0, 0, 255, 0, 255, 0, 255].into() };
        let png = encode_png(image).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        let decoded = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (2, 1));
        assert_eq!(decoded.get_pixel(1, 0).0, [0, 255, 0, 255]);
    }

    #[test]
    fn blocking_future_resolves() {
        assert_eq!(futures_lite_block_on(blocking(|| Ok(41 + 1))), Ok(42));
        let panicked: Result<(), String> = futures_lite_block_on(blocking(|| panic!("boom")));
        assert_eq!(panicked, Err("clipboard helper panicked".to_string()));
    }

    fn futures_lite_block_on<T>(mut fut: BoxFuture<T>) -> T {
        struct ThreadWaker(std::thread::Thread);
        impl std::task::Wake for ThreadWaker {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
        let mut cx = std::task::Context::from_waker(&waker);
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::park();
        }
    }
}
