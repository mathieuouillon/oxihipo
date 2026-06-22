//! Filter-and-rewrite: copy the events that carry every named bank into a
//! new HIPO file, re-encoded as `Lz4ByBank`.
//!
//! Usage:
//!   cargo run --release --example skim -- <in.hipo> <out.hipo> [BANK ...]
//!
//! With no bank names, every event is copied (a straight recook). With one
//! or more names, only events carrying all of them are written — the
//! filter's bank-presence pushdown runs on the read side, so unmatched
//! events are skipped cheaply. A misspelled bank name fails fast (the
//! `with_filter` call returns an error) rather than silently copying zero
//! events.
//!
//! The input may be a single file, a directory, or a glob; multiple files
//! merge into the one output. Note that per-*record* user tags are not
//! preserved (see `Chain::skim`).

use std::env;

use oxihipo::{Chain, Compression, Filter, Result};

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let input = args
        .next()
        .expect("usage: skim <in.hipo> <out.hipo> [BANK ...]");
    let output = args
        .next()
        .expect("usage: skim <in.hipo> <out.hipo> [BANK ...]");
    let banks: Vec<String> = args.collect();

    let mut chain = Chain::open(&input)?;
    if !banks.is_empty() {
        chain = chain.with_filter(Filter::require(banks.iter().map(|s| s.as_str())))?;
    }

    let summary = chain.skim(&output, Compression::Lz4ByBank)?;

    eprintln!("skim: {input} -> {output}");
    if banks.is_empty() {
        eprintln!("  (no filter — every event copied)");
    } else {
        eprintln!("  requiring banks: {}", banks.join(", "));
    }
    eprintln!(
        "  wrote {} events in {} records ({} bytes)",
        summary.events, summary.records, summary.bytes,
    );
    Ok(())
}
