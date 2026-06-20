//! Core domain types: the event ([`Transaction`]) and its identifiers.
//!
//! `Transaction` is a data-carrying enum, so the amount exists only on the
//! variants that have one — a dispute literally cannot hold an amount, and a
//! deposit cannot lack one. Parsing a raw CSV record into a `Transaction` lives
//! here (the `TryFrom` impl), so the `engine` never touches CSV.

use std::str;

use csv::ByteRecord;
use rust_decimal::Decimal;

use crate::error::EngineError;

/// The maximum decimal precision the spec allows for an amount.
const MAX_SCALE: u32 = 4;

/// A client identifier. A distinct newtype so it can't be confused with a
/// [`TxId`] or used as an arithmetic operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(pub u16);

/// A globally-unique transaction identifier. Used purely as an identity key
/// (dedup, ledger lookup) — never for ordering, since ids are unordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TxId(pub u32);

/// A monetary amount. Fixed-point decimal, never a float.
pub type Amount = Decimal;

/// The kind of a recorded value event — the only events a dispute can
/// reference. Narrower than [`Transaction`]: the dispute lifecycle is never
/// recorded, so it can't appear here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Deposit,
    Withdrawal,
}

/// A single event from the input stream. Deposits and withdrawals carry an
/// amount; the dispute lifecycle references a prior transaction by id only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transaction {
    Deposit {
        client: ClientId,
        tx: TxId,
        amount: Amount,
    },
    Withdrawal {
        client: ClientId,
        tx: TxId,
        amount: Amount,
    },
    Dispute {
        client: ClientId,
        tx: TxId,
    },
    Resolve {
        client: ClientId,
        tx: TxId,
    },
    Chargeback {
        client: ClientId,
        tx: TxId,
    },
}

impl TryFrom<&ByteRecord> for Transaction {
    type Error = EngineError;

    /// Parse a `type, client, tx, amount` record. The `type` column is compared
    /// as raw bytes (no allocation); deposits/withdrawals require an amount,
    /// the dispute lifecycle ignores the column.
    fn try_from(record: &ByteRecord) -> Result<Self, Self::Error> {
        let client = ClientId(parse_field(record.get(1), "client")?);
        let tx = TxId(parse_field(record.get(2), "tx")?);

        Ok(match record.get(0) {
            Some(b"deposit") => Transaction::Deposit {
                client,
                tx,
                amount: require_amount(record.get(3))?,
            },
            Some(b"withdrawal") => Transaction::Withdrawal {
                client,
                tx,
                amount: require_amount(record.get(3))?,
            },
            Some(b"dispute") => Transaction::Dispute { client, tx },
            Some(b"resolve") => Transaction::Resolve { client, tx },
            Some(b"chargeback") => Transaction::Chargeback { client, tx },
            other => {
                let shown = String::from_utf8_lossy(other.unwrap_or(b"<missing>"));
                return Err(EngineError::Malformed(format!("unknown type: {shown}")));
            }
        })
    }
}

/// Parse a required integer field.
fn parse_field<T>(field: Option<&[u8]>, name: &str) -> Result<T, EngineError>
where
    T: str::FromStr,
{
    let bytes = field.ok_or_else(|| EngineError::Malformed(format!("missing {name}")))?;
    let text = str::from_utf8(bytes)
        .map_err(|_| EngineError::Malformed(format!("{name} is not valid UTF-8")))?;
    text.parse()
        .map_err(|_| EngineError::Malformed(format!("invalid {name}: {text}")))
}

/// Parse the (required) amount column of a deposit/withdrawal. Absent/empty or
/// beyond 4 dp is malformed.
fn require_amount(field: Option<&[u8]>) -> Result<Amount, EngineError> {
    let bytes = match field {
        Some(b) if !b.is_empty() => b,
        _ => return Err(EngineError::Malformed("missing amount".to_string())),
    };
    let text = str::from_utf8(bytes)
        .map_err(|_| EngineError::Malformed("amount is not valid UTF-8".to_string()))?;
    let amount: Amount = text
        .parse()
        .map_err(|_| EngineError::Malformed(format!("invalid amount: {text}")))?;
    if amount.scale() > MAX_SCALE {
        return Err(EngineError::Malformed(format!(
            "amount exceeds {MAX_SCALE} dp: {text}"
        )));
    }
    Ok(amount)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(fields: &[&str]) -> ByteRecord {
        ByteRecord::from(fields.to_vec())
    }

    #[test]
    fn parses_a_deposit() {
        let tx = Transaction::try_from(&record(&["deposit", "1", "7", "1.5"])).unwrap();
        assert_eq!(
            tx,
            Transaction::Deposit {
                client: ClientId(1),
                tx: TxId(7),
                amount: "1.5".parse().unwrap(),
            }
        );
    }

    #[test]
    fn parses_a_dispute_without_amount() {
        let tx = Transaction::try_from(&record(&["dispute", "1", "7"])).unwrap();
        assert_eq!(
            tx,
            Transaction::Dispute {
                client: ClientId(1),
                tx: TxId(7)
            }
        );
    }

    #[test]
    fn dispute_ignores_a_trailing_empty_amount() {
        let tx = Transaction::try_from(&record(&["dispute", "1", "7", ""])).unwrap();
        assert_eq!(
            tx,
            Transaction::Dispute {
                client: ClientId(1),
                tx: TxId(7)
            }
        );
    }

    #[test]
    fn deposit_without_amount_is_malformed() {
        assert!(Transaction::try_from(&record(&["deposit", "1", "7"])).is_err());
        assert!(Transaction::try_from(&record(&["deposit", "1", "7", ""])).is_err());
    }

    #[test]
    fn unknown_type_is_malformed() {
        assert!(Transaction::try_from(&record(&["nope", "1", "7", "1.0"])).is_err());
    }

    #[test]
    fn bad_client_is_malformed() {
        assert!(Transaction::try_from(&record(&["deposit", "x", "7", "1.0"])).is_err());
    }

    #[test]
    fn missing_tx_is_malformed() {
        assert!(Transaction::try_from(&record(&["deposit", "1"])).is_err());
    }

    #[test]
    fn amount_beyond_four_dp_is_malformed() {
        assert!(Transaction::try_from(&record(&["deposit", "1", "7", "1.00001"])).is_err());
    }
}
