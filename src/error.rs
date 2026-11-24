use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Open(Box<crate::OpenError>),
    #[cfg(not(target_arch = "wasm32"))]
    #[error(transparent)]
    Provider(#[from] crate::providers::ProviderError),
    #[cfg(not(target_arch = "wasm32"))]
    #[error("{method} failed with native provider code {code}")]
    ProviderRpc { method: &'static str, code: i64 },
    #[cfg(not(target_arch = "wasm32"))]
    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("runtime refused request: {0}")]
    Refused(String),
    #[error("unsupported operation: {0}")]
    Unsupported(String),
    #[error("input is {bytes} UTF-8 bytes and exceeds this runtime's {limit}-byte limit")]
    InputTooLarge { bytes: usize, limit: usize },
    #[error("another turn is already running")]
    Busy,
    #[error("session closed: {0}")]
    Closed(String),
    #[error("operation timed out: {0}")]
    Timeout(String),
    #[error("invalid cursor for this stream")]
    InvalidCursor,
}
pub type Result<T> = std::result::Result<T, Error>;
