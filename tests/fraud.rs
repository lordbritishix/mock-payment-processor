//! End-to-end fraud-scenario tests.
//!
//! Each test drives a full CSV stream through the public API and asserts the
//! resulting account state, demonstrating that the documented attacks cannot
//! manufacture or extract funds. These complement the per-branch unit tests in
//! `engine.rs` by exercising the whole `parse -> apply` pipeline on realistic,
//! adversarial inputs.

use payments_engine::{run, Amount, ClientId, Engine, TransactionReader};

/// Process an in-memory CSV through the engine and return it for inspection.
fn process(csv: &str) -> Engine {
    let mut engine = Engine::new();
    run(TransactionReader::new(csv.as_bytes()), &mut engine);
    engine
}

fn dec(s: &str) -> Amount {
    s.parse().unwrap()
}

/// (available, held, total, locked) for a client that must exist.
fn account(engine: &Engine, client: u16) -> (Amount, Amount, Amount, bool) {
    let (_, a) = engine
        .accounts()
        .find(|(c, _)| *c == ClientId(client))
        .expect("account should exist");
    (a.available(), a.held(), a.total(), a.is_locked())
}

fn account_exists(engine: &Engine, client: u16) -> bool {
    engine.accounts().any(|(c, _)| c == ClientId(client))
}

/// The canonical attack from the prompt: deposit fiat, withdraw it (e.g. as
/// crypto), then reverse the original deposit. The chargeback removes funds that
/// are no longer there, so the balance goes negative and the account locks. We
/// must NOT clamp to zero — the negative balance is the visible realized loss.
#[test]
fn deposit_withdraw_then_reverse_leaves_negative_locked_balance() {
    let csv = "\
type, client, tx, amount
deposit, 1, 1, 100.0
withdrawal, 1, 2, 100.0
dispute, 1, 1
chargeback, 1, 1
";
    let e = process(csv);
    assert_eq!(
        account(&e, 1),
        (dec("-100.0"), dec("0"), dec("-100.0"), true)
    );
}

/// Once a chargeback freezes the account, the fraudster cannot keep operating it:
/// every later event — deposit, withdrawal, or a fresh dispute — is ignored.
#[test]
fn frozen_account_rejects_all_further_activity() {
    let csv = "\
type, client, tx, amount
deposit, 1, 1, 50.0
dispute, 1, 1
chargeback, 1, 1
deposit, 1, 2, 1000.0
withdrawal, 1, 3, 10.0
";
    let e = process(csv);
    // Balance is exactly the charged-back state; the later deposit/withdrawal no-op.
    assert_eq!(account(&e, 1), (dec("0"), dec("0"), dec("0"), true));
}

/// Replaying a deposit under an existing tx id must not double-credit the account.
#[test]
fn duplicate_tx_id_cannot_double_credit() {
    let csv = "\
type, client, tx, amount
deposit, 1, 1, 100.0
deposit, 1, 1, 100.0
";
    let e = process(csv);
    assert_eq!(account(&e, 1).0, dec("100.0"));
}

/// A client cannot dispute a transaction that belongs to someone else.
#[test]
fn cannot_dispute_another_clients_transaction() {
    let csv = "\
type, client, tx, amount
deposit, 1, 1, 100.0
dispute, 2, 1
";
    let e = process(csv);
    // Client 1 is untouched; client 2 never gains a hold (and no phantom account).
    assert_eq!(
        account(&e, 1),
        (dec("100.0"), dec("0"), dec("100.0"), false)
    );
    assert!(!account_exists(&e, 2));
}

/// A chargeback (or resolve) against a transaction that was never disputed would
/// remove or release funds that are not held — i.e. create money. It must be
/// ignored, and must not lock the account.
#[test]
fn chargeback_without_dispute_is_ignored() {
    let csv = "\
type, client, tx, amount
deposit, 1, 1, 100.0
chargeback, 1, 1
resolve, 1, 1
";
    let e = process(csv);
    assert_eq!(
        account(&e, 1),
        (dec("100.0"), dec("0"), dec("100.0"), false)
    );
}

/// A dispute can be resolved at most once: after a resolve the tx is no longer
/// disputed, so a second resolve cannot refund the funds again.
#[test]
fn resolve_cannot_double_refund() {
    let csv = "\
type, client, tx, amount
deposit, 1, 1, 100.0
dispute, 1, 1
resolve, 1, 1
resolve, 1, 1
";
    let e = process(csv);
    assert_eq!(
        account(&e, 1),
        (dec("100.0"), dec("0"), dec("100.0"), false)
    );
}

/// Withdrawals can never overdraw available funds, so a fraudster cannot push the
/// balance negative through withdrawal alone.
#[test]
fn withdrawal_cannot_overdraw() {
    let csv = "\
type, client, tx, amount
deposit, 1, 1, 5.0
withdrawal, 1, 2, 10.0
";
    let e = process(csv);
    assert_eq!(account(&e, 1).0, dec("5.0"));
}

/// A held (disputed) deposit cannot be withdrawn: the funds moved to `held` are
/// no longer available, so a withdrawal against them is rejected.
#[test]
fn held_funds_cannot_be_withdrawn() {
    let csv = "\
type, client, tx, amount
deposit, 1, 1, 100.0
dispute, 1, 1
withdrawal, 1, 2, 50.0
";
    let e = process(csv);
    assert_eq!(
        account(&e, 1),
        (dec("0"), dec("100.0"), dec("100.0"), false)
    );
}

/// Disputing a deposit whose funds were already (legitimately) withdrawn drives
/// `available` negative while `total` stays consistent — money is conserved, not
/// created, and the deficit is preserved as the visible record.
#[test]
fn dispute_after_withdrawal_conserves_money() {
    let csv = "\
type, client, tx, amount
deposit, 1, 1, 100.0
withdrawal, 1, 2, 60.0
dispute, 1, 1
";
    let e = process(csv);
    let (available, held, total, locked) = account(&e, 1);
    assert_eq!(
        (available, held, total, locked),
        (dec("-60.0"), dec("100.0"), dec("40.0"), false)
    );
    assert_eq!(total, available + held); // conservation invariant
}
