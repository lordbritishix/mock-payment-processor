//! The reducer: folds [`Transaction`] events into per-client [`Account`]
//! projections.
//!
//! Holds the accounts and a ledger of value events (deposits/withdrawals). The
//! ledger is both the dispute-lookup table and the dedup applied-set. `apply`
//! validates fully before mutating, so an account is never half-updated.

use std::collections::HashMap;
use std::fmt;

use crate::account::{Account, BalanceError};
use crate::types::{Amount, ClientId, Transaction, TxId, ValueKind};

/// Why a well-formed event was declined. These are *no-ops*, not errors — the
/// input was valid but the rules don't act on it. Structured (rather than a
/// string) so callers and tests can match on the specific reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// Deposit/withdrawal amount was zero or negative.
    NonPositiveAmount,
    /// A value event reused an already-seen tx id.
    DuplicateTx,
    /// The account is frozen by a prior chargeback.
    AccountLocked,
    /// A withdrawal exceeded available funds.
    InsufficientFunds,
    /// The balance operation would overflow the `Decimal` range.
    Overflow,
    /// The referenced tx id is not in the ledger.
    UnknownTx,
    /// The referenced tx exists but belongs to a different client.
    ClientMismatch,
    /// The referenced tx is not a deposit (only deposits are disputable).
    NotDisputable,
    /// A resolve/chargeback referenced a tx that is not under dispute.
    NotDisputed,
    /// A dispute referenced a tx that is already under dispute.
    AlreadyDisputed,
    /// Defensive only: a ledger entry without a matching account. Unreachable
    /// given that every recorded tx first creates its account (see `record`).
    UnknownAccount,
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            RejectReason::NonPositiveAmount => "amount is not positive",
            RejectReason::DuplicateTx => "duplicate tx id",
            RejectReason::AccountLocked => "account is locked",
            RejectReason::InsufficientFunds => "insufficient funds",
            RejectReason::Overflow => "arithmetic overflow",
            RejectReason::UnknownTx => "references unknown tx",
            RejectReason::ClientMismatch => "client mismatch",
            RejectReason::NotDisputable => "tx is not disputable (not a deposit)",
            RejectReason::NotDisputed => "tx is not under dispute",
            RejectReason::AlreadyDisputed => "tx already disputed",
            RejectReason::UnknownAccount => "references unknown account",
        };
        f.write_str(s)
    }
}

impl From<BalanceError> for RejectReason {
    fn from(e: BalanceError) -> Self {
        match e {
            BalanceError::InsufficientFunds => RejectReason::InsufficientFunds,
            BalanceError::Overflow => RejectReason::Overflow,
        }
    }
}

/// What happened when an event was applied.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// State changed.
    Applied,
    /// Well-formed input the rules declined (a no-op); carries the reason.
    Rejected(RejectReason),
}

/// A retained value event: its owner, amount, kind, and dispute state.
struct LedgerEntry {
    client: ClientId,
    amount: Amount,
    kind: ValueKind,
    disputed: bool,
}

/// The payments engine. Build with [`Engine::new`], feed events with
/// [`Engine::apply`], then read results with [`Engine::accounts`].
#[derive(Default)]
pub struct Engine {
    accounts: HashMap<ClientId, Account>,
    ledger: HashMap<TxId, LedgerEntry>,
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    /// The resulting accounts as `(client, account)` pairs. Order is unspecified.
    pub fn accounts(&self) -> impl Iterator<Item = (ClientId, &Account)> {
        self.accounts
            .iter()
            .map(|(&client, account)| (client, account))
    }

    /// Apply one event to the projection.
    pub fn apply(&mut self, event: Transaction) -> Outcome {
        match event {
            Transaction::Deposit { client, tx, amount } => self.deposit(client, tx, amount),
            Transaction::Withdrawal { client, tx, amount } => self.withdrawal(client, tx, amount),
            Transaction::Dispute { client, tx } => self.dispute(client, tx),
            Transaction::Resolve { client, tx } => self.resolve(client, tx),
            Transaction::Chargeback { client, tx } => self.chargeback(client, tx),
        }
    }

    fn deposit(&mut self, client: ClientId, tx: TxId, amount: Amount) -> Outcome {
        if amount <= Amount::ZERO {
            return Outcome::Rejected(RejectReason::NonPositiveAmount);
        }
        if self.ledger.contains_key(&tx) {
            return Outcome::Rejected(RejectReason::DuplicateTx);
        }
        let account = self.accounts.entry(client).or_default();
        if account.is_locked() {
            return Outcome::Rejected(RejectReason::AccountLocked);
        }
        if let Err(e) = account.credit(amount) {
            return Outcome::Rejected(e.into());
        }
        self.record(client, tx, amount, ValueKind::Deposit);
        Outcome::Applied
    }

    fn withdrawal(&mut self, client: ClientId, tx: TxId, amount: Amount) -> Outcome {
        if amount <= Amount::ZERO {
            return Outcome::Rejected(RejectReason::NonPositiveAmount);
        }
        if self.ledger.contains_key(&tx) {
            return Outcome::Rejected(RejectReason::DuplicateTx);
        }
        // A reference to a new client creates a zero record (per spec); the
        // withdrawal then fails for want of funds. See README assumption.
        let account = self.accounts.entry(client).or_default();
        if account.is_locked() {
            return Outcome::Rejected(RejectReason::AccountLocked);
        }
        if let Err(e) = account.debit(amount) {
            return Outcome::Rejected(e.into());
        }
        self.record(client, tx, amount, ValueKind::Withdrawal);
        Outcome::Applied
    }

    fn dispute(&mut self, client: ClientId, tx: TxId) -> Outcome {
        let Some(entry) = self.ledger.get_mut(&tx) else {
            return Outcome::Rejected(RejectReason::UnknownTx);
        };
        if entry.client != client {
            return Outcome::Rejected(RejectReason::ClientMismatch);
        }
        if entry.kind != ValueKind::Deposit {
            return Outcome::Rejected(RejectReason::NotDisputable);
        }
        if entry.disputed {
            return Outcome::Rejected(RejectReason::AlreadyDisputed);
        }
        // Defensive: every ledger entry's client has an account (see `record`),
        // so this branch is unreachable in practice.
        let Some(account) = self.accounts.get_mut(&client) else {
            return Outcome::Rejected(RejectReason::UnknownAccount);
        };
        if account.is_locked() {
            return Outcome::Rejected(RejectReason::AccountLocked);
        }
        if let Err(e) = account.hold(entry.amount) {
            return Outcome::Rejected(e.into());
        }
        entry.disputed = true;
        Outcome::Applied
    }

    fn resolve(&mut self, client: ClientId, tx: TxId) -> Outcome {
        let Some(entry) = self.ledger.get_mut(&tx) else {
            return Outcome::Rejected(RejectReason::UnknownTx);
        };
        if entry.client != client {
            return Outcome::Rejected(RejectReason::ClientMismatch);
        }
        if !entry.disputed {
            return Outcome::Rejected(RejectReason::NotDisputed);
        }
        // Defensive: unreachable, as above.
        let Some(account) = self.accounts.get_mut(&client) else {
            return Outcome::Rejected(RejectReason::UnknownAccount);
        };
        if account.is_locked() {
            return Outcome::Rejected(RejectReason::AccountLocked);
        }
        if let Err(e) = account.release(entry.amount) {
            return Outcome::Rejected(e.into());
        }
        entry.disputed = false;
        Outcome::Applied
    }

    fn chargeback(&mut self, client: ClientId, tx: TxId) -> Outcome {
        let Some(entry) = self.ledger.get_mut(&tx) else {
            return Outcome::Rejected(RejectReason::UnknownTx);
        };
        if entry.client != client {
            return Outcome::Rejected(RejectReason::ClientMismatch);
        }
        if !entry.disputed {
            return Outcome::Rejected(RejectReason::NotDisputed);
        }
        // Defensive: unreachable, as above.
        let Some(account) = self.accounts.get_mut(&client) else {
            return Outcome::Rejected(RejectReason::UnknownAccount);
        };
        if account.is_locked() {
            return Outcome::Rejected(RejectReason::AccountLocked);
        }
        if let Err(e) = account.chargeback(entry.amount) {
            return Outcome::Rejected(e.into());
        }
        entry.disputed = false;
        Outcome::Applied
    }

    fn record(&mut self, client: ClientId, tx: TxId, amount: Amount, kind: ValueKind) {
        self.ledger.insert(
            tx,
            LedgerEntry {
                client,
                amount,
                kind,
                disputed: false,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deposit(c: u16, tx: u32, a: &str) -> Transaction {
        Transaction::Deposit {
            client: ClientId(c),
            tx: TxId(tx),
            amount: a.parse().unwrap(),
        }
    }
    fn withdrawal(c: u16, tx: u32, a: &str) -> Transaction {
        Transaction::Withdrawal {
            client: ClientId(c),
            tx: TxId(tx),
            amount: a.parse().unwrap(),
        }
    }
    fn dispute(c: u16, tx: u32) -> Transaction {
        Transaction::Dispute {
            client: ClientId(c),
            tx: TxId(tx),
        }
    }
    fn resolve(c: u16, tx: u32) -> Transaction {
        Transaction::Resolve {
            client: ClientId(c),
            tx: TxId(tx),
        }
    }
    fn chargeback(c: u16, tx: u32) -> Transaction {
        Transaction::Chargeback {
            client: ClientId(c),
            tx: TxId(tx),
        }
    }

    fn amt(s: &str) -> Amount {
        s.parse().unwrap()
    }

    /// Snapshot of one client's balances for terse assertions.
    fn balances(engine: &Engine, client: u16) -> (Amount, Amount, Amount, bool) {
        let a = engine
            .accounts
            .get(&ClientId(client))
            .expect("account exists");
        (a.available(), a.held(), a.total(), a.is_locked())
    }

    #[test]
    fn deposit_creates_and_accumulates() {
        let mut e = Engine::new();
        assert_eq!(e.apply(deposit(1, 1, "1.0")), Outcome::Applied);
        assert_eq!(e.apply(deposit(1, 2, "2.0")), Outcome::Applied);
        assert_eq!(balances(&e, 1), (amt("3.0"), amt("0"), amt("3.0"), false));
    }

    #[test]
    fn multiple_clients_are_independent() {
        let mut e = Engine::new();
        e.apply(deposit(1, 1, "1.0"));
        e.apply(deposit(2, 2, "2.0"));
        assert_eq!(balances(&e, 1).0, amt("1.0"));
        assert_eq!(balances(&e, 2).0, amt("2.0"));
    }

    #[test]
    fn zero_or_negative_deposit_rejected() {
        let mut e = Engine::new();
        assert_eq!(
            e.apply(deposit(1, 1, "0")),
            Outcome::Rejected(RejectReason::NonPositiveAmount)
        );
        assert_eq!(
            e.apply(deposit(1, 2, "-5")),
            Outcome::Rejected(RejectReason::NonPositiveAmount)
        );
        assert!(e.accounts.is_empty());
    }

    #[test]
    fn duplicate_tx_id_ignored() {
        let mut e = Engine::new();
        e.apply(deposit(1, 1, "1.0"));
        assert_eq!(
            e.apply(deposit(1, 1, "9.0")),
            Outcome::Rejected(RejectReason::DuplicateTx)
        );
        assert_eq!(balances(&e, 1).0, amt("1.0"));
    }

    #[test]
    fn withdrawal_success_and_insufficient() {
        let mut e = Engine::new();
        e.apply(deposit(1, 1, "5.0"));
        assert_eq!(e.apply(withdrawal(1, 2, "5.0")), Outcome::Applied);
        assert_eq!(balances(&e, 1).0, amt("0"));
        assert_eq!(
            e.apply(withdrawal(1, 3, "0.01")),
            Outcome::Rejected(RejectReason::InsufficientFunds)
        );
        assert_eq!(balances(&e, 1).0, amt("0"));
    }

    #[test]
    fn withdrawal_for_unknown_client_fails_but_creates_zero_record() {
        let mut e = Engine::new();
        assert_eq!(
            e.apply(withdrawal(9, 1, "5.0")),
            Outcome::Rejected(RejectReason::InsufficientFunds)
        );
        assert_eq!(balances(&e, 9), (amt("0"), amt("0"), amt("0"), false));
    }

    #[test]
    fn dispute_holds_funds_total_unchanged() {
        let mut e = Engine::new();
        e.apply(deposit(1, 1, "10.0"));
        assert_eq!(e.apply(dispute(1, 1)), Outcome::Applied);
        assert_eq!(balances(&e, 1), (amt("0"), amt("10.0"), amt("10.0"), false));
    }

    #[test]
    fn dispute_unknown_mismatch_and_double_are_ignored() {
        let mut e = Engine::new();
        e.apply(deposit(1, 1, "10.0"));
        assert_eq!(
            e.apply(dispute(1, 99)),
            Outcome::Rejected(RejectReason::UnknownTx)
        );
        assert_eq!(
            e.apply(dispute(2, 1)),
            Outcome::Rejected(RejectReason::ClientMismatch)
        );
        assert_eq!(e.apply(dispute(1, 1)), Outcome::Applied);
        assert_eq!(
            e.apply(dispute(1, 1)),
            Outcome::Rejected(RejectReason::AlreadyDisputed)
        );
        assert_eq!(balances(&e, 1).1, amt("10.0"));
    }

    #[test]
    fn withdrawals_are_not_disputable() {
        let mut e = Engine::new();
        e.apply(deposit(1, 1, "10.0"));
        e.apply(withdrawal(1, 2, "4.0"));
        assert_eq!(
            e.apply(dispute(1, 2)),
            Outcome::Rejected(RejectReason::NotDisputable)
        );
    }

    #[test]
    fn dispute_can_drive_available_negative() {
        let mut e = Engine::new();
        e.apply(deposit(1, 1, "10.0"));
        e.apply(withdrawal(1, 2, "10.0")); // available now 0
        assert_eq!(e.apply(dispute(1, 1)), Outcome::Applied);
        assert_eq!(
            balances(&e, 1),
            (amt("-10.0"), amt("10.0"), amt("0"), false)
        );
    }

    #[test]
    fn resolve_releases_held() {
        let mut e = Engine::new();
        e.apply(deposit(1, 1, "10.0"));
        e.apply(dispute(1, 1));
        assert_eq!(e.apply(resolve(1, 1)), Outcome::Applied);
        assert_eq!(balances(&e, 1), (amt("10.0"), amt("0"), amt("10.0"), false));
        // resolving again is a no-op (no longer disputed)
        assert_eq!(
            e.apply(resolve(1, 1)),
            Outcome::Rejected(RejectReason::NotDisputed)
        );
    }

    #[test]
    fn resolve_of_non_disputed_ignored() {
        let mut e = Engine::new();
        e.apply(deposit(1, 1, "10.0"));
        assert_eq!(
            e.apply(resolve(1, 1)),
            Outcome::Rejected(RejectReason::NotDisputed)
        );
    }

    #[test]
    fn chargeback_locks_and_removes_funds() {
        let mut e = Engine::new();
        e.apply(deposit(1, 1, "10.0"));
        e.apply(deposit(1, 2, "5.0"));
        e.apply(dispute(1, 1));
        assert_eq!(e.apply(chargeback(1, 1)), Outcome::Applied);
        assert_eq!(balances(&e, 1), (amt("5.0"), amt("0"), amt("5.0"), true));
    }

    #[test]
    fn chargeback_of_non_disputed_ignored() {
        let mut e = Engine::new();
        e.apply(deposit(1, 1, "10.0"));
        assert_eq!(
            e.apply(chargeback(1, 1)),
            Outcome::Rejected(RejectReason::NotDisputed)
        );
        assert!(!balances(&e, 1).3);
    }

    #[test]
    fn locked_account_freezes_all_later_events() {
        let mut e = Engine::new();
        e.apply(deposit(1, 1, "10.0"));
        e.apply(dispute(1, 1));
        e.apply(chargeback(1, 1)); // locks
        assert_eq!(
            e.apply(deposit(1, 3, "5.0")),
            Outcome::Rejected(RejectReason::AccountLocked)
        );
        assert_eq!(
            e.apply(withdrawal(1, 4, "1.0")),
            Outcome::Rejected(RejectReason::AccountLocked)
        );
        assert!(balances(&e, 1).3);
    }

    #[test]
    fn redispute_after_resolve_is_allowed() {
        // dispute -> resolve returns the tx to "not disputed", so a second
        // dispute legitimately re-holds the funds (the state machine permits it).
        let mut e = Engine::new();
        e.apply(deposit(1, 1, "10.0"));
        assert_eq!(e.apply(dispute(1, 1)), Outcome::Applied);
        assert_eq!(e.apply(resolve(1, 1)), Outcome::Applied);
        assert_eq!(e.apply(dispute(1, 1)), Outcome::Applied);
        assert_eq!(balances(&e, 1), (amt("0"), amt("10.0"), amt("10.0"), false));
    }

    #[test]
    fn four_dp_amount_is_preserved_exactly() {
        let mut e = Engine::new();
        e.apply(deposit(1, 1, "1.2345"));
        assert_eq!(balances(&e, 1).0, amt("1.2345"));
    }

    #[test]
    fn fractional_arithmetic_has_no_float_drift() {
        // The reason for Decimal over f64: 0.1 + 0.2 must equal exactly 0.3.
        let mut e = Engine::new();
        e.apply(deposit(1, 1, "0.1"));
        e.apply(deposit(1, 2, "0.2"));
        assert_eq!(balances(&e, 1).0, amt("0.3"));
    }

    #[test]
    fn total_invariant_holds_throughout() {
        let mut e = Engine::new();
        for tx in [
            deposit(1, 1, "10.0"),
            withdrawal(1, 2, "3.0"),
            deposit(1, 3, "1.2345"),
        ] {
            e.apply(tx);
            let (avail, held, total, _) = balances(&e, 1);
            assert_eq!(total, avail + held);
        }
    }
}
