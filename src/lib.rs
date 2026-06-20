//! A streaming, event-sourced toy payments engine.
//!
//! Each CSV row is an immutable event; [`Engine::apply`] folds events into
//! per-client [`Account`] projections. Parsing ([`io`]) is separate from
//! applying ([`engine`]), with [`Transaction`] as the only contract between
//! them. See `README.md` for the design and `IMPLEMENTATION.md` for mechanics.

pub mod account;
pub mod engine;
pub mod error;
pub mod io;
pub mod types;

use std::io::Read;

pub use account::Account;
pub use engine::{Engine, Outcome, RejectReason};
pub use error::{EngineError, Result};
pub use io::{write_accounts, TransactionReader};
pub use types::{Amount, ClientId, Transaction, TxId};

/// Counts of how each input row was handled, reported to stderr at the end.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Events that changed state.
    pub processed: u64,
    /// Well-formed events the rules declined (no-ops).
    pub ignored: u64,
    /// Malformed rows skipped at the parse boundary.
    pub skipped: u64,
}

/// Fold every event from `reader` into `engine`, returning per-row counts.
///
/// This is the core processing loop, shared by the binary and the integration
/// tests. Malformed rows are skipped (warned to stderr) so the stream never
/// stops; rule-declined events are counted but not logged (they are common and
/// legitimate, e.g. a dispute of an unknown tx).
pub fn run<R: Read>(reader: TransactionReader<R>, engine: &mut Engine) -> Stats {
    let mut stats = Stats::default();
    for item in reader {
        match item {
            Ok(tx) => match engine.apply(tx) {
                Outcome::Applied => stats.processed += 1,
                Outcome::Rejected(_) => stats.ignored += 1,
            },
            Err(e) => {
                stats.skipped += 1;
                eprintln!("skipped malformed row: {e}");
            }
        }
    }
    stats
}
