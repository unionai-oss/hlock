//! Errors.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("manifest error: {0}")]
    Manifest(String),
    #[error("lockfile error: {0}")]
    Lockfile(String),
    #[error("eval pack error: {0}")]
    EvalPack(String),
    #[error("store error: {0}")]
    Store(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("tuning error: {0}")]
    Tuning(String),
    #[error("{0}")]
    Profile(String),
    #[error("python worker error: {0}")]
    Worker(String),
}

pub type Result<T> = std::result::Result<T, Error>;
