//! Crate error type.
//!
//! Malformed input is recoverable — the offending row is skipped and the stream
//! continues. The variants distinguish a row we couldn't parse from an error
//! surfaced by the underlying CSV reader.

use thiserror::Error;

/// An error encountered while turning a raw record into a [`crate::types::Transaction`].
#[derive(Debug, Error)]
pub enum EngineError {
    /// A record that could not be parsed into a transaction (bad field, bad
    /// type, amount beyond 4 dp, etc.).
    #[error("malformed record: {0}")]
    Malformed(String),

    /// An error from the underlying CSV reader (e.g. a torn record).
    #[error("csv error: {0}")]
    Csv(#[from] csv::Error),
}

/// Convenience alias for results carrying an [`EngineError`].
pub type Result<T> = std::result::Result<T, EngineError>;
