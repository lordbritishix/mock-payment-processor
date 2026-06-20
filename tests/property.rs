//! Property-based tests: whatever arbitrary sequence of events we throw at the
//! engine, the core invariants must hold for every resulting account —
//! `total == available + held` and `held >= 0`. This covers event orderings we
//! would never enumerate by hand.

use std::collections::HashMap;

use proptest::prelude::*;
use rust_decimal::Decimal;

use payments_engine::{ClientId, Engine, Transaction, TxId};

/// A bounded alphabet of clients/txs: small enough that disputes and
/// chargebacks frequently hit existing transactions (exercising the lifecycle),
/// wide enough that long sequences keep producing fresh deposits rather than
/// saturating into all-duplicate rejections.
fn arb_event() -> impl Strategy<Value = Transaction> {
    let client = 1u16..=5;
    let tx = 1u32..=100;
    // Amounts up to 4 dp, the spec's precision.
    let amount = (0i64..=1_000_000).prop_map(|n| Decimal::new(n, 4));

    prop_oneof![
        (client.clone(), tx.clone(), amount.clone()).prop_map(|(c, t, a)| Transaction::Deposit {
            client: ClientId(c),
            tx: TxId(t),
            amount: a
        }),
        (client.clone(), tx.clone(), amount).prop_map(|(c, t, a)| Transaction::Withdrawal {
            client: ClientId(c),
            tx: TxId(t),
            amount: a
        }),
        (client.clone(), tx.clone()).prop_map(|(c, t)| Transaction::Dispute {
            client: ClientId(c),
            tx: TxId(t)
        }),
        (client.clone(), tx.clone()).prop_map(|(c, t)| Transaction::Resolve {
            client: ClientId(c),
            tx: TxId(t)
        }),
        (client, tx).prop_map(|(c, t)| Transaction::Chargeback {
            client: ClientId(c),
            tx: TxId(t)
        }),
    ]
}

proptest! {
    #[test]
    fn invariants_hold_for_any_event_sequence(events in prop::collection::vec(arb_event(), 0..1000)) {
        let mut engine = Engine::new();
        for event in events {
            engine.apply(event);
            for (_, account) in engine.accounts() {
                // total is derived as available + held — must never desync.
                prop_assert_eq!(account.total(), account.available() + account.held());
                // held funds can only ever be released back, never go negative.
                prop_assert!(account.held() >= Decimal::ZERO);
            }
        }
    }
}

// --- Differential test against an independent reference model -----------------

/// Expected state of one account in the reference model.
#[derive(Clone, Default, PartialEq, Eq)]
struct RefAccount {
    available: Decimal,
    held: Decimal,
    locked: bool,
}

/// A ledger entry in the reference model.
#[derive(Clone)]
struct RefEntry {
    client: u16,
    amount: Decimal,
    is_deposit: bool,
    disputed: bool,
}

/// A deliberately naive, independent reimplementation of the rules — plain
/// `+=`/`-=` on raw maps, no `Account` abstraction, no checked arithmetic. It
/// shares no code with the engine, so any divergence in *how* the rules are
/// applied (guard order, a flipped sign, a wrong condition) shows up as a
/// mismatch. Amounts are bounded by the generator, so unchecked arithmetic is
/// safe here.
fn reference(events: &[Transaction]) -> HashMap<u16, RefAccount> {
    let mut accounts: HashMap<u16, RefAccount> = HashMap::new();
    let mut ledger: HashMap<u32, RefEntry> = HashMap::new();

    for event in events {
        match *event {
            Transaction::Deposit { client, tx, amount } => {
                if amount <= Decimal::ZERO || ledger.contains_key(&tx.0) {
                    continue;
                }
                let acct = accounts.entry(client.0).or_default();
                if acct.locked {
                    continue;
                }
                acct.available += amount;
                ledger.insert(
                    tx.0,
                    RefEntry {
                        client: client.0,
                        amount,
                        is_deposit: true,
                        disputed: false,
                    },
                );
            }
            Transaction::Withdrawal { client, tx, amount } => {
                if amount <= Decimal::ZERO || ledger.contains_key(&tx.0) {
                    continue;
                }
                let acct = accounts.entry(client.0).or_default();
                if acct.locked || acct.available < amount {
                    continue;
                }
                acct.available -= amount;
                ledger.insert(
                    tx.0,
                    RefEntry {
                        client: client.0,
                        amount,
                        is_deposit: false,
                        disputed: false,
                    },
                );
            }
            Transaction::Dispute { client, tx } => {
                let Some(entry) = ledger.get_mut(&tx.0) else {
                    continue;
                };
                if entry.client != client.0 || !entry.is_deposit || entry.disputed {
                    continue;
                }
                let Some(acct) = accounts.get_mut(&client.0) else {
                    continue;
                };
                if acct.locked {
                    continue;
                }
                acct.available -= entry.amount;
                acct.held += entry.amount;
                entry.disputed = true;
            }
            Transaction::Resolve { client, tx } => {
                let Some(entry) = ledger.get_mut(&tx.0) else {
                    continue;
                };
                if entry.client != client.0 || !entry.disputed {
                    continue;
                }
                let Some(acct) = accounts.get_mut(&client.0) else {
                    continue;
                };
                if acct.locked {
                    continue;
                }
                acct.held -= entry.amount;
                acct.available += entry.amount;
                entry.disputed = false;
            }
            Transaction::Chargeback { client, tx } => {
                let Some(entry) = ledger.get_mut(&tx.0) else {
                    continue;
                };
                if entry.client != client.0 || !entry.disputed {
                    continue;
                }
                let Some(acct) = accounts.get_mut(&client.0) else {
                    continue;
                };
                if acct.locked {
                    continue;
                }
                acct.held -= entry.amount;
                acct.locked = true;
                entry.disputed = false;
            }
        }
    }
    accounts
}

proptest! {
    #[test]
    fn engine_matches_reference_model(events in prop::collection::vec(arb_event(), 0..1000)) {
        let mut engine = Engine::new();
        for event in &events {
            engine.apply(*event);
        }

        let expected = reference(&events);

        let got: HashMap<u16, RefAccount> = engine
            .accounts()
            .map(|(client, a)| {
                (client.0, RefAccount { available: a.available(), held: a.held(), locked: a.is_locked() })
            })
            .collect();

        // Same set of clients...
        prop_assert_eq!(got.len(), expected.len());
        // ...and identical balances + lock state for each.
        for (client, want) in &expected {
            let have = got.get(client).expect("engine is missing a client the model produced");
            prop_assert_eq!(have.available, want.available);
            prop_assert_eq!(have.held, want.held);
            prop_assert_eq!(have.locked, want.locked);
        }
    }
}
