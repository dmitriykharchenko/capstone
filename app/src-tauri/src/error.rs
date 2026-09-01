//! Error type shared by every Tauri command.
//!
//! `CommandError` is serialized to the frontend as a plain string message, so
//! sagas can surface a readable failure without depending on Rust internals.

use serde::{Serialize, Serializer};

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("paper not found: {0}")]
    PaperNotFound(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("python bridge failed: {0}")]
    Bridge(String),

    #[error("invalid response from python bridge: {0}")]
    InvalidResponse(String),
}

impl Serialize for CommandError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
