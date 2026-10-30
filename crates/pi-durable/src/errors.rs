use crate::types::ConversationId;
use std::sync::Arc;

#[derive(Debug)]
pub enum ErrorKind {
    File(crate::env::FileError),
    Execution(crate::env::ExecutionError),
    Caused {
        message: crate::JsonString,
        cause: Error,
    },
    Sqlite(pi_chord::sqlite::SqliteError),
    JsonlCorruption {
        message: String,
        cause: Option<Error>,
    },
    JsonlPoisoned {
        cause: Error,
    },
    JsonlIo {
        action: String,
        cause: crate::env::FileError,
    },
    Aborted(pi_chord::context::AbortError),
    Delta(pi_chord::delta::Error),
    Message(String),
    Host {
        name: String,
        message: crate::JsonString,
    },
    StorageRequest(String),
    ReadAfterWrite(String),
    SessionFailed {
        cause: Error,
    },
    ReadOnly(String),
    ConversationBusy(ConversationId),
    DocumentCopy {
        id: crate::types::DocumentId,
        cause: Error,
    },
}
#[derive(Clone, Debug)]
pub struct Error(Arc<ErrorKind>);
pub type Result<T> = std::result::Result<T, Error>;
impl Error {
    pub fn file(error: crate::env::FileError) -> Self {
        Self(Arc::new(ErrorKind::File(error)))
    }
    pub fn execution(error: crate::env::ExecutionError) -> Self {
        Self(Arc::new(ErrorKind::Execution(error)))
    }
    pub fn caused(message: crate::JsonString, cause: Error) -> Self {
        Self(Arc::new(ErrorKind::Caused { message, cause }))
    }
    pub fn jsonl_corruption(message: impl Into<String>, cause: Option<Error>) -> Self {
        Self(Arc::new(ErrorKind::JsonlCorruption {
            message: message.into(),
            cause,
        }))
    }
    pub fn jsonl_poisoned(cause: Error) -> Self {
        Self(Arc::new(ErrorKind::JsonlPoisoned { cause }))
    }
    pub fn jsonl_io(action: impl Into<String>, cause: crate::env::FileError) -> Self {
        Self(Arc::new(ErrorKind::JsonlIo {
            action: action.into(),
            cause,
        }))
    }
    pub fn message(message: impl Into<String>) -> Self {
        Self(Arc::new(ErrorKind::Message(message.into())))
    }
    pub fn host(name: impl Into<String>, message: crate::JsonString) -> Self {
        Self(Arc::new(ErrorKind::Host {
            name: name.into(),
            message,
        }))
    }
    pub fn message_text(&self) -> crate::JsonString {
        match self.kind() {
            ErrorKind::Host { message, .. } | ErrorKind::Caused { message, .. } => message.clone(),
            _ => self.to_string().into(),
        }
    }
    pub fn request(message: impl Into<String>) -> Self {
        Self(Arc::new(ErrorKind::StorageRequest(message.into())))
    }
    pub fn read_after_write(method: impl Into<String>) -> Self {
        Self(Arc::new(ErrorKind::ReadAfterWrite(method.into())))
    }
    pub fn failed(cause: Self) -> Self {
        Self(Arc::new(ErrorKind::SessionFailed { cause }))
    }
    pub fn read_only(what: impl Into<String>) -> Self {
        Self(Arc::new(ErrorKind::ReadOnly(what.into())))
    }
    pub fn busy(id: ConversationId) -> Self {
        Self(Arc::new(ErrorKind::ConversationBusy(id)))
    }
    pub fn document_copy(id: crate::types::DocumentId, cause: Self) -> Self {
        Self(Arc::new(ErrorKind::DocumentCopy { id, cause }))
    }
    pub fn kind(&self) -> &ErrorKind {
        &self.0
    }
    pub fn name(&self) -> &str {
        match self.kind() {
            ErrorKind::File(_) => "FileError",
            ErrorKind::Execution(_) => "ExecutionError",
            ErrorKind::Host { name, .. } => name,
            ErrorKind::JsonlCorruption { .. } => "JsonlCorruptionError",
            ErrorKind::JsonlPoisoned { .. } => "JsonlStoragePoisonedError",
            ErrorKind::Aborted(_) => "AbortError",
            ErrorKind::StorageRequest(_) => "StorageRequestError",
            ErrorKind::ReadAfterWrite(_) => "ReadAfterWrite",
            ErrorKind::SessionFailed { .. } => "SessionFailed",
            ErrorKind::ReadOnly(_) => "ReadOnlySession",
            ErrorKind::ConversationBusy(_) => "ConversationBusy",
            ErrorKind::Delta(pi_chord::delta::Error::Type(_)) => "TypeError",
            ErrorKind::Delta(pi_chord::delta::Error::UnsafePath(_)) => "UnsafePathError",
            ErrorKind::Delta(pi_chord::delta::Error::Path(_)) => "PathError",
            _ => "Error",
        }
    }
    pub fn is_request(&self) -> bool {
        matches!(self.kind(), ErrorKind::StorageRequest(_))
    }
    pub fn same_error(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind() {
            ErrorKind::File(error) => error.fmt(f),
            ErrorKind::Execution(error) => error.fmt(f),
            ErrorKind::Caused { message, .. } => f.write_str(&message.to_string_lossy()),
            ErrorKind::Sqlite(error) => error.fmt(f),
            ErrorKind::JsonlCorruption { message, .. } => f.write_str(message),
            ErrorKind::JsonlPoisoned { .. } => {
                f.write_str("JSONL storage is poisoned and must be reopened")
            }
            ErrorKind::JsonlIo { action, cause } => write!(f, "JSONL {action} failed: {cause}"),
            ErrorKind::Aborted(error) => error.fmt(f),
            ErrorKind::Delta(error) => error.fmt(f),
            ErrorKind::Message(m) | ErrorKind::StorageRequest(m) => f.write_str(m),
            ErrorKind::Host { message, .. } => f.write_str(&message.to_string_lossy()),
            ErrorKind::ReadAfterWrite(method) => write!(
                f,
                "Tx.{method}() cannot read tables after the first table write"
            ),
            ErrorKind::SessionFailed { .. } => {
                f.write_str("Session failed after a storage error; close and reopen it")
            }
            ErrorKind::ReadOnly(what) => write!(f, "Read-only Harness: {what} refused"),
            ErrorKind::ConversationBusy(id) => write!(f, "Conversation {id} is busy"),
            ErrorKind::DocumentCopy { id, .. } => write!(f, "Document copy {id} was rejected"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self.kind() {
            ErrorKind::File(error) => Some(error),
            ErrorKind::Execution(error) => Some(error),
            ErrorKind::Caused { cause, .. } => Some(cause),
            ErrorKind::Sqlite(error) => Some(error),
            ErrorKind::JsonlCorruption { cause, .. } => cause.as_ref().map(|e| e as _),
            ErrorKind::JsonlPoisoned { cause } => Some(cause),
            ErrorKind::JsonlIo { cause, .. } => Some(cause),
            ErrorKind::Aborted(error) => Some(error),
            ErrorKind::Delta(error) => Some(error),
            ErrorKind::SessionFailed { cause } | ErrorKind::DocumentCopy { cause, .. } => {
                Some(cause)
            }
            _ => None,
        }
    }
}
impl From<pi_chord::sqlite::SqliteError> for Error {
    fn from(error: pi_chord::sqlite::SqliteError) -> Self {
        if let pi_chord::sqlite::SqliteErrorKind::External(cause) = error.kind()
            && let Some(error) = cause.downcast_ref::<Self>()
        {
            return error.clone();
        }
        Self(Arc::new(ErrorKind::Sqlite(error)))
    }
}
impl From<pi_chord::context::AbortError> for Error {
    fn from(error: pi_chord::context::AbortError) -> Self {
        Self(Arc::new(ErrorKind::Aborted(error)))
    }
}
impl From<pi_chord::delta::Error> for Error {
    fn from(error: pi_chord::delta::Error) -> Self {
        Self(Arc::new(ErrorKind::Delta(error)))
    }
}
impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::message(error.to_string())
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self::message(message)
    }
}

impl From<Error> for pi_chord::WatchError {
    fn from(error: Error) -> Self {
        Self::with_cause(error.to_string(), error)
    }
}
impl From<pi_chord::WatchError> for Error {
    fn from(error: pi_chord::WatchError) -> Self {
        error
            .cause::<Error>()
            .cloned()
            .unwrap_or_else(|| Error::message(error.to_string()))
    }
}
