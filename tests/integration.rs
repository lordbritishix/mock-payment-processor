//! End-to-end fixture tests: run the engine over an input CSV and compare to an
//! expected-output CSV. Row order is unspecified, so data rows are compared
//! order-insensitively (sorted, header kept first).

use std::fs;

use payments_engine::{run, write_accounts, Engine, Stats, TransactionReader};

/// Run the engine over a fixture (via the shared `run` loop) and return its
/// output as sorted rows.
fn run_fixture(name: &str) -> Vec<String> {
    let (rows, _) = run_fixture_with_stats(name);
    rows
}

fn run_fixture_with_stats(name: &str) -> (Vec<String>, Stats) {
    let input = fs::read(format!("tests/fixtures/{name}.csv")).unwrap();
    let mut engine = Engine::new();
    let stats = run(TransactionReader::new(input.as_slice()), &mut engine);
    let mut out = Vec::new();
    write_accounts(&engine, &mut out).unwrap();
    (sorted_rows(&String::from_utf8(out).unwrap()), stats)
}

fn expected(name: &str) -> Vec<String> {
    let text = fs::read_to_string(format!("tests/fixtures/{name}.expected.csv")).unwrap();
    sorted_rows(&text)
}

/// Header first, remaining (data) rows sorted — makes comparison order-insensitive.
fn sorted_rows(csv: &str) -> Vec<String> {
    let mut rows: Vec<String> = csv.lines().map(str::to_string).collect();
    if rows.len() > 1 {
        rows[1..].sort();
    }
    rows
}

fn check(name: &str) {
    assert_eq!(run_fixture(name), expected(name), "fixture {name} mismatch");
}

#[test]
fn spec_example() {
    check("spec_example");
}

#[test]
fn dispute_then_resolve() {
    check("dispute_resolve");
}

#[test]
fn dispute_then_chargeback_locks() {
    check("dispute_chargeback");
}

#[test]
fn messy_input_skips_bad_rows() {
    check("messy");
}

#[test]
fn extra_whitespace_is_accepted() {
    check("whitespace");
}

#[test]
fn messy_stats_are_counted() {
    let (_, stats) = run_fixture_with_stats("messy");
    // 3 applied (deposits tx1/tx3, withdrawal tx4), 2 ignored (dispute of
    // unknown tx, insufficient withdrawal), 2 skipped (broken row, "abc").
    assert_eq!(
        stats,
        Stats {
            processed: 3,
            ignored: 2,
            skipped: 2
        }
    );
}

/// Run an in-memory CSV string through the engine and return its sorted output.
fn run_csv(input: &str) -> Vec<String> {
    let mut engine = Engine::new();
    run(TransactionReader::new(input.as_bytes()), &mut engine);
    let mut out = Vec::new();
    write_accounts(&engine, &mut out).unwrap();
    sorted_rows(&String::from_utf8(out).unwrap())
}

#[test]
fn crlf_line_endings_are_accepted() {
    // Windows-style \r\n must parse identically to \n.
    let lf = "type, client, tx, amount\ndeposit, 1, 1, 1.0\nwithdrawal, 1, 2, 0.5\n";
    let crlf = "type, client, tx, amount\r\ndeposit, 1, 1, 1.0\r\nwithdrawal, 1, 2, 0.5\r\n";
    assert_eq!(run_csv(crlf), run_csv(lf));
}

#[test]
fn empty_input_yields_only_the_header() {
    assert_eq!(
        run_csv("type, client, tx, amount\n"),
        vec!["client,available,held,total,locked"]
    );
}

#[test]
fn output_is_deterministic_across_runs() {
    let input = fs::read_to_string("tests/fixtures/spec_example.csv").unwrap();
    assert_eq!(run_csv(&input), run_csv(&input));
}
