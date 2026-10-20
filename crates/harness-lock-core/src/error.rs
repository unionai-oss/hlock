//! Errors.

use std::path::Path;

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
    /// `hlock pull`: a source that can't be read, or running `git` or fetching failed.
    #[error("{0}")]
    Pull(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// An I/O error about `path`, with the path in its message (and its kind kept).
pub fn io_at(path: &Path, e: std::io::Error) -> Error {
    Error::Io(std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

/// `e` with `path` added to it when it's an I/O error; other errors pass through.
pub fn at(path: &Path, e: Error) -> Error {
    match e {
        Error::Io(io) => io_at(path, io),
        e => e,
    }
}
