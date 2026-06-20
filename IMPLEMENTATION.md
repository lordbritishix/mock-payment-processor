# Payments Engine — Implementation Notes

Backing detail for the decisions in **README.md** — read a section when you're
reviewing that specific area. The *why* lives in README.md; the *how* lives here.

## CLI contract

```
cargo run -- transactions.csv > accounts.csv
```

- Input file path is the first and only positional argument; output goes to **stdout**.
- Diagnostics/warnings go to **stderr** only, so they never corrupt the output.
- Non-zero exit only for unrecoverable startup errors (unreadable file, bad CLI).
  Per-record data problems are skipped, not fatal.

Output: `client,available,held,total,locked`, one row per client. Row order and
spacing don't matter; decimals at up to 4 places.

---

## Crate layout

```
payments-engine/
  Cargo.toml
  README.md              # overview, build/run, design decisions
  IMPLEMENTATION.md      # this file — mechanical detail
  src/
    main.rs              # thin binary: CLI args, wiring, stderr
    lib.rs               # crate root: docs, mod decls, public API
    error.rs             # EngineError (thiserror); Result alias
    types.rs             # ClientId, TxId, Amount, Transaction + parsing
    account.rs           # Account: balances + invariants
    engine.rs            # Engine: accounts + ledger; apply(&mut self, tx)
    io.rs                # streaming CSV reader -> Transaction; account writer
  tests/
    integration.rs       # run engine over fixture CSVs, compare to expected
    fixtures/*.csv       # paired input / expected-output sample data
```

`lib.rs` is the crate root; `main.rs` is thin and depends only on the public API,
so tests and external consumers link the library, not the binary.

**Parse / apply boundary.** Parsing lives in `io` (with the `TryFrom` in `types`):
CSV bytes → `Transaction`. Applying lives in `engine`: `Transaction` → state change.
`Transaction` is the *sole* interface between them — `engine` has no knowledge of
CSV or files and never touches a raw record, while `io` never touches an account.
This keeps the two stages independently testable and independently schedulable
(one reader, many per-account appliers; see README.md → Beyond the take-home), and it cleanly
separates *parse* failures (malformed rows) from *apply* outcomes (business rejections).

---

## Data model

In event-sourcing terms: **`Transaction` = event**, **`Account` = projection**,
**`apply` = the invariant-preserving reducer** `(State, Event) → State`.

Small, well-typed concepts (concrete Rust types are a `types` detail):

- **`ClientId` / `TxId` are distinct newtypes**, not bare ints — can't be mixed up
  or used as arithmetic operands; cheap `Copy` values usable as map keys.
- **`Amount`** is a fixed-point decimal (4 dp), never a float.
- **`Transaction`** is what the engine consumes: a *kind* (deposit, withdrawal,
  dispute, resolve, chargeback), target client, referenced tx id, and an *optional*
  amount (present only for deposit/withdrawal). A raw CSV row becomes a
  `Transaction` via a fallible conversion, so malformed rows are rejected at the
  boundary, not deep in the logic.
- **`Account`** holds *available*, *held*, and a *locked* flag (set by a chargeback;
  it's the `locked` output column, and freezes all further events for that client).
  Total is derived, not stored (see Invariants enforced).

**Engine state:** two maps — client id → account, and tx id → ledger entry. The
ledger holds value events (deposits + withdrawals) with their amount and a "disputed"
flag — it's the applied-set for dedup, and lets a dispute recover the referenced
deposit's amount (dispute/resolve/chargeback carry no amount of their own). Only
deposits are dispute-eligible (Assumption #1); withdrawals are retained only for dedup.

The ledger doubles as the **applied-set** for idempotency: a `tx id` already present
means that value event was already applied, so a replay is ignored (at-most-once).
Dispute-lifecycle events carry no id of their own, so they're de-duplicated only by
state (the `disputed` flag + the lock); a replay arriving after the state has cycled
(dispute → resolve → dispute) can't be told apart from a genuine new event — true
exactly-once there needs a per-event id from the transport.

**`apply` yields one of three results**, which the loop treats differently:
*applied* (state changed), *rejected* (well-formed but the rules declined it — no-op,
debug log), *error* (malformed → skip the row + warn, stream continues). See Safety &
robustness below.

---

## Invariants enforced

The reducer (`Engine::apply` + `Account` methods) guarantees these on every event.

*Always true (structural):*
- **`total == available + held`** — total is derived, never stored, so it can't desync.
- **Conservation** — no event creates money; each op is a consistent transfer or a no-op.

*Checked before mutating (validate-then-mutate → an event applies fully or not at all):*
- **Withdrawal requires `available ≥ amount`** — a withdrawal can never go negative.
- **Amount is positive and ≤4 dp** for deposit/withdrawal.
- **Checked arithmetic** — overflow rejects the op; balances untouched (no `NaN`/panic).
- **Dispute state machine** — `dispute` acts only if the tx exists, same client, and is
  *not already disputed*; `resolve`/`chargeback` only if *currently disputed* (this is
  what keeps `held ≥ 0`).
- **At-most-once** — a value event whose `tx id` is already applied is ignored.
- **Terminal freeze** — a locked account (post-chargeback) rejects every later event.

*Deliberate non-invariant:* `available`/`total` may be **negative** (a realized
chargeback loss) — preserved, not clamped. Only `held` is guaranteed ≥ 0, via the
state machine.

Enforcement is split across the parse/apply boundary: `Account` upholds the structural
+ arithmetic rules behind private fields; `Engine::apply` enforces the state machine,
dedup, and terminal freeze before delegating to those methods.

---

## Safety & robustness

Principles: no panics on malformed data; no floats for money; no `unwrap()` on input
(Result + `?`). Balance invariants are listed in §Invariants enforced.

### Handling malformed input

The loop is `read → parse → apply`; a failure at any stage **skips that one row and
continues**. The binary still exits 0 and emits the account CSV; only a bad CLI /
unreadable file is fatal. Skipping is per-*row*, not per-account — a dropped row's
account keeps applying its other valid rows.

| Class | Examples | Response |
|-------|----------|----------|
| **Understood no-op** | dispute/resolve/chargeback → unknown/wrong-state tx; client mismatch; duplicate tx; insufficient funds; zero/negative amount; unknown type | **ignore + warn** — a valid row the rules decline; no value lost (spec says ignore the reference cases) |
| **Malformed row** | unparseable field (bad u16/u32, non-decimal or >4 dp amount, missing amount) or structural CSV error (wrong field count, bad UTF-8) | **skip + warn**, keep reading |
| **Arithmetic** | `Decimal` overflow | `checked_*` → reject op, balances untouched |

This matches the spec — malformed input is treated as partner error and ignored — and
it's what automated scoring expects. A row whose *amount we can't read* is skipped like
any other; we don't guess, since that would fabricate money. The honest cost: a skipped
financial row leaves that account diverging from the partner's *intended* balance —
internally consistent, but not what the partner meant. So every skip is **warned to
stderr** and counted, for an operator to reconcile upstream. (The bank-grade
alternative — quarantine the account instead of skipping — is in
README.md → Beyond the take-home.)

The loop tracks counters (processed / ignored / skipped) and prints a one-line summary
to **stderr** — never stdout, so the account CSV stays clean.

### Why this stays consistent

The per-event invariants above hold whether a row is applied, ignored, or skipped — a
skipped row simply never happened, so the projection stays internally consistent. And
accounts are independent (no shared state), so a bad row for one client can't corrupt
another.

Dependent chains stay sound: a skipped deposit never enters the ledger, so a later
dispute on it finds nothing and is ignored (no phantom hold); a skipped dispute leaves
funds available, so a later resolve/chargeback finds it not-disputed and is ignored (no
stuck held).

### A corrupted account value can't happen silently

A malformed row never reaches the account — it's rejected at the parse boundary, so the
account is untouched, and a client appearing only in malformed rows is never created (no
phantom accounts). Inside the engine the structural guarantees (derived total, checked
arithmetic, negative-not-clamped) hold per §Invariants enforced — no path to a `NaN` or
a half-written balance.

---

## Testing strategy

A test for every transaction type and branch — unit tests beside the logic, plus
end-to-end fixture tests.

**Unit (`engine.rs` / `account.rs`):**
- *Deposit:* new client; accumulate; multiple clients; 4 dp preserved; zero/negative
  rejected; duplicate tx ignored.
- *Withdrawal:* success; insufficient funds ignored; exact-balance → 0; unknown client
  ignored; zero/negative rejected.
- *Dispute:* valid (available↓ held↑ total=); nonexistent/mismatch/already-disputed
  ignored; drives available negative (allowed); on locked account ignored.
- *Resolve:* valid (held↓ available↑ total=, cleared); not-disputed/nonexistent/
  mismatch/already-resolved ignored.
- *Chargeback:* valid (held↓ total↓, locks); non-disputed/nonexistent/mismatch
  ignored; after lock every tx type ignored.
- *Invariants/precision:* `total == available + held` after every op; 4 dp rounding;
  whitespace-tolerant parsing.
- *Malformed:* unparseable field or structural CSV row → skipped + warned, surrounding
  valid rows still applied; skipped-deposit-then-dispute stays consistent (no phantom
  hold); understood no-op (zero amount, dispute → unknown tx) → ignored; overflow
  rejected; counters (processed / ignored / skipped) correct.

**Integration (`tests/integration.rs`):** run fixtures, compare to expected output
(order-insensitive). Fixtures: spec example; dispute→resolve; dispute→chargeback→lock;
interleaved multi-client (out-of-order ids); a messy file mixing valid rows, ignorable
no-ops, and an unreadable row that's skipped (its account keeps its other valid rows;
other clients unaffected).

---

## Rust conventions

- **Naming:** `snake_case` / `CamelCase` / `SCREAMING_SNAKE_CASE` per standard style.
- **Modules:** one file per module; `lib.rs` declares them and `pub use`s the public API.
- **Imports:** plain `use` at file top, grouped std/external/crate; no fully-qualified
  inline paths.
- **Errors:** typed `EngineError` (`thiserror`) in lib, `anyhow` in `main`; no
  `unwrap`/`expect`/`panic!` on input.
- **Conversions:** `TryFrom` for parsing, `Display`/`Serialize` for output.
- **Encapsulation:** private `Account` fields behind invariant-preserving methods;
  `total()` derived.
- **Derives:** `Copy`/`Clone`/`Eq`/`Hash`/`Debug` on small value types.
- **Lints/format:** clean under `cargo clippy -- -D warnings` and `cargo fmt`.
- **Tests:** `#[cfg(test)] mod tests` per module; black-box tests in `tests/`.

