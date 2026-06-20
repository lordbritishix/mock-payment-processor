//! Streaming CSV input (bytes → [`Transaction`]) and account output.
//!
//! Input is read into a single reused [`ByteRecord`], so steady-state parsing
//! does no per-row heap allocation. The reader is generic over [`Read`], so a
//! file and a socket are handled identically.

use std::io::{Read, Write};

use csv::{ByteRecord, ReaderBuilder, Trim, WriterBuilder};

use crate::account::Account;
use crate::engine::Engine;
use crate::error::EngineError;
use crate::types::{Amount, ClientId, Transaction};

/// Streams [`Transaction`] events from a CSV source. Each `next` reuses one
/// record buffer; a parse failure yields `Err` for that row without stopping
/// the stream.
pub struct TransactionReader<R: Read> {
    reader: csv::Reader<R>,
    buf: ByteRecord,
}

impl<R: Read> TransactionReader<R> {
    pub fn new(source: R) -> Self {
        let reader = ReaderBuilder::new()
            .has_headers(true)
            .trim(Trim::All)
            .flexible(true) // dispute/resolve/chargeback rows omit the amount column
            .from_reader(source);
        Self {
            reader,
            buf: ByteRecord::new(),
        }
    }
}

impl<R: Read> Iterator for TransactionReader<R> {
    type Item = Result<Transaction, EngineError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.reader.read_byte_record(&mut self.buf) {
            Ok(true) => Some(Transaction::try_from(&self.buf)),
            Ok(false) => None,
            Err(e) => Some(Err(EngineError::Csv(e))),
        }
    }
}

/// Write the engine's accounts as CSV to `out`.
pub fn write_accounts<W: Write>(engine: &Engine, out: W) -> csv::Result<()> {
    let mut writer = WriterBuilder::new().from_writer(out);
    writer.write_record(["client", "available", "held", "total", "locked"])?;
    // Sorting is not required (row order is spec-irrelevant), but it makes the
    // output deterministic and easy to diff.
    let mut accounts: Vec<(ClientId, &Account)> = engine.accounts().collect();
    accounts.sort_by_key(|(client, _)| client.0);
    for (client, account) in accounts {
        writer.write_record([
            client.0.to_string(),
            fmt_amount(account.available()),
            fmt_amount(account.held()),
            fmt_amount(account.total()),
            account.is_locked().to_string(),
        ])?;
    }
    writer.flush()?;
    Ok(())
}

/// Render an amount at up to 4 dp, trimming trailing zeros (round values such as
/// `2.0000` print as `2`). Spacing/round-value formatting is spec-irrelevant.
fn fmt_amount(amount: Amount) -> String {
    amount.round_dp(4).normalize().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_records_and_tolerates_missing_amount_column() {
        let csv = "type, client, tx, amount\ndeposit, 1, 1, 1.0\ndispute, 1, 1\n";
        let txns: Vec<Transaction> = TransactionReader::new(csv.as_bytes())
            .map(|r| r.unwrap())
            .collect();
        assert!(matches!(txns[0], Transaction::Deposit { .. }));
        assert!(matches!(txns[1], Transaction::Dispute { .. }));
    }

    #[test]
    fn malformed_row_yields_err_without_stopping() {
        let csv = "type, client, tx, amount\ndeposit, 1, 1, oops\ndeposit, 1, 2, 2.0\n";
        let results: Vec<_> = TransactionReader::new(csv.as_bytes()).collect();
        assert_eq!(results.len(), 2);
        assert!(results[0].is_err());
        assert!(results[1].is_ok());
    }

    fn amt(s: &str) -> Amount {
        s.parse().unwrap()
    }

    #[test]
    fn fmt_amount_trims_trailing_zeros_on_round_values() {
        assert_eq!(fmt_amount(amt("2.0000")), "2");
        assert_eq!(fmt_amount(amt("2.5000")), "2.5");
    }

    #[test]
    fn fmt_amount_keeps_full_precision_and_sign() {
        assert_eq!(fmt_amount(amt("1.2345")), "1.2345");
        assert_eq!(fmt_amount(amt("-10.0")), "-10");
    }

    /// Run a CSV through the engine and return the written output as a string.
    fn output_for(csv: &str) -> String {
        let mut engine = Engine::new();
        for tx in TransactionReader::new(csv.as_bytes()).flatten() {
            engine.apply(tx);
        }
        let mut out = Vec::new();
        write_accounts(&engine, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn empty_engine_writes_only_the_header() {
        assert_eq!(
            output_for("type, client, tx, amount\n"),
            "client,available,held,total,locked\n"
        );
    }

    #[test]
    fn output_is_sorted_by_client() {
        let csv = "type, client, tx, amount\n\
                   deposit, 3, 1, 1.0\n\
                   deposit, 1, 2, 1.0\n\
                   deposit, 2, 3, 1.0\n";
        let output = output_for(csv);
        let clients: Vec<&str> = output
            .lines()
            .skip(1)
            .map(|l| l.split(',').next().unwrap())
            .collect();
        assert_eq!(clients, ["1", "2", "3"]);
    }
}
