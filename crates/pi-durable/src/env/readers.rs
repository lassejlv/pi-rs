//! Stateful readers over one opened file, with retryable positional reads.
use super::decode::StreamDecoder;
use super::{filesystem::*, line_scan::LineScanner};
use crate::Context;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(crate) fn check(context: &Context, path: &std::path::Path) -> FileResult<()> {
    context
        .check()
        .map_err(|_| FileError::new(FileErrorCode::Aborted, "aborted", Some(path.into())))
}
pub(crate) fn invalid(message: &str, path: &std::path::Path) -> FileError {
    FileError::new(FileErrorCode::Invalid, message, Some(path.into()))
}
fn safe(value: f64) -> bool {
    value.is_finite()
        && value >= 0.0
        && value.fract() == 0.0
        && value <= crate::MAX_SAFE_INTEGER as f64
}

pub struct NativeBinaryReader {
    file: Arc<dyn FileHandle>,
    path: PathBuf,
    closed: AtomicBool,
}
impl NativeBinaryReader {
    pub fn new(file: Arc<dyn FileHandle>, path: PathBuf) -> Self {
        Self {
            file,
            path,
            closed: AtomicBool::new(false),
        }
    }
    fn check(&self, context: &Context) -> FileResult<()> {
        check(context, &self.path)?;
        if self.closed.load(Ordering::Acquire) {
            return Err(invalid("Binary reader is closed", &self.path));
        }
        Ok(())
    }
}
impl BinaryReader for NativeBinaryReader {
    fn info(&self, context: &Context) -> FileFuture<FileInfo> {
        if let Err(error) = self.check(context) {
            return Box::pin(async { Err(error) });
        }
        self.file.info(context)
    }
    fn read(&self, offset: f64, length: f64, context: &Context) -> FileFuture<Vec<u8>> {
        let admission = self.check(context).and_then(|_| {
            if safe(offset) && safe(length) {
                Ok(())
            } else {
                Err(invalid(
                    "Offset and length must be non-negative safe integers",
                    &self.path,
                ))
            }
        });
        if let Err(error) = admission {
            return Box::pin(async { Err(error) });
        }
        let (file, path, context) = (self.file.clone(), self.path.clone(), context.clone());
        file_job(async move {
            let mut bytes = Vec::new();
            while (bytes.len() as u64) < length as u64 {
                let chunk = file
                    .read(
                        offset as u64 + bytes.len() as u64,
                        (length as u64 - bytes.len() as u64).min(1024 * 1024) as usize,
                        &context,
                    )
                    .await?;
                check(&context, &path)?;
                if chunk.is_empty() {
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        })
    }
    fn scan_lines(
        &self,
        start_line: f64,
        end_line: Option<f64>,
        context: &Context,
    ) -> FileFuture<super::LineScan> {
        let admission = self.check(context).and_then(|_| {
            LineScanner::new(start_line, end_line).map_err(|message| invalid(message, &self.path))
        });
        let mut scanner = match admission {
            Ok(scanner) => scanner,
            Err(error) => return Box::pin(async { Err(error) }),
        };
        let (file, path, context) = (self.file.clone(), self.path.clone(), context.clone());
        file_job(async move {
            let mut position = 0;
            loop {
                let chunk = file.read(position, 64 * 1024, &context).await?;
                check(&context, &path)?;
                if chunk.is_empty() {
                    return Ok(scanner.finish());
                }
                scanner.push(&chunk);
                position += chunk.len() as u64;
            }
        })
    }
    fn close(&self, context: &Context) -> FileFuture<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Box::pin(async { Ok(()) });
        }
        let close = self.file.close(context);
        file_job(async move {
            let _ = close.await;
            Ok(())
        })
    }
}

#[derive(Default)]
struct TextState {
    offset: u64,
    buffered: String,
    decoder: StreamDecoder,
    ended: bool,
}
pub struct NativeTextLineReader {
    file: Arc<dyn FileHandle>,
    path: PathBuf,
    state: Arc<tokio::sync::Mutex<TextState>>,
    closed: Arc<AtomicBool>,
}
impl NativeTextLineReader {
    pub fn new(file: Arc<dyn FileHandle>, path: PathBuf) -> Self {
        Self {
            file,
            path,
            state: Arc::default(),
            closed: Arc::default(),
        }
    }
}
impl TextLineReader for NativeTextLineReader {
    fn read_line(&self, context: &Context) -> FileFuture<Option<TextLine>> {
        let admission = check(context, &self.path).and_then(|_| {
            if self.closed.load(Ordering::Acquire) {
                Err(invalid("Text line reader is closed", &self.path))
            } else {
                Ok(())
            }
        });
        if let Err(error) = admission {
            return Box::pin(async { Err(error) });
        }
        let (file, path, state, context) = (
            self.file.clone(),
            self.path.clone(),
            self.state.clone(),
            context.clone(),
        );
        file_job(async move {
            let mut state = state.lock().await;
            loop {
                if let Some(newline) = state.buffered.find('\n') {
                    let rest = state.buffered.split_off(newline + 1);
                    state.buffered.pop();
                    let text = std::mem::replace(&mut state.buffered, rest);
                    return Ok(Some(TextLine {
                        text,
                        terminated: true,
                    }));
                }
                if state.ended {
                    return Ok((!state.buffered.is_empty()).then(|| TextLine {
                        text: std::mem::take(&mut state.buffered),
                        terminated: false,
                    }));
                }
                let bytes = file.read(state.offset, 64 * 1024, &context).await?;
                // Do not advance the offset or decoder when cancellation wins:
                // another caller must be able to retry the same bytes.
                check(&context, &path)?;
                state.offset += bytes.len() as u64;
                let decoded = state
                    .decoder
                    .decode((!bytes.is_empty()).then_some(bytes.as_slice()));
                state.buffered.push_str(&decoded);
                if bytes.is_empty() {
                    state.ended = true;
                }
            }
        })
    }
    fn close(&self, context: &Context) -> FileFuture<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Box::pin(async { Ok(()) });
        }
        let (state, close) = (self.state.clone(), self.file.close(context));
        file_job(async move {
            state.lock().await.buffered.clear();
            let _ = close.await;
            Ok(())
        })
    }
}
