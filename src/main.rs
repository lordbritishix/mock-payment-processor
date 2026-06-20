//! Binary entry point: parse the CSV path, fold events through the engine,
//! write accounts to stdout, and report a one-line summary to stderr.

use std::env;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;

use payments_engine::{run, write_accounts, Engine, Stats, TransactionReader};

fn main() -> ExitCode {
    match execute() {
        Ok(stats) => {
            eprintln!(
                "done: {} applied, {} ignored, {} skipped",
                stats.processed, stats.ignored, stats.skipped
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn execute() -> anyhow::Result<Stats> {
    let path = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .context("usage: payments-engine <transactions.csv>")?;

    let file = File::open(&path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut engine = Engine::new();
    let stats = run(TransactionReader::new(BufReader::new(file)), &mut engine);

    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    write_accounts(&engine, &mut out)?;
    out.flush()?;
    Ok(stats)
}
