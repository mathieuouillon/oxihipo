//! Write a synthetic file of very cheap events, to isolate per-event API
//! overhead from decode cost.
//!
//! Real CLAS12 DST events cost microseconds each to decode (47 banks, real
//! row counts), which swamps anything the calling convention does. A file
//! of one-row, one-column events costs tens of nanoseconds each, so the
//! callback shape is the whole measurement. Both regimes are worth having.
//!
//! Usage: cargo run --release --example gen_synthetic -- <out.hipo> [events] [codec]
//!   codec: none | lz4 | lz4-per-bank | lz4-per-column   (default lz4)

use std::env;

use oxihipo::{Chain, Compression, DataType, Dict, Result, Schema, Writer};

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let out = args
        .next()
        .expect("usage: gen_synthetic <out.hipo> [events] [codec]");
    let events: i64 = args.next().map(|s| s.parse().unwrap()).unwrap_or(4_000_000);
    let codec = args.next().unwrap_or_else(|| "lz4".into());

    let compression = match codec.as_str() {
        "none" => Compression::None,
        "lz4" => Compression::Lz4,
        "lz4-per-bank" => Compression::Lz4PerBank,
        "lz4-per-column" => Compression::Lz4PerColumn,
        other => panic!("unknown codec {other}"),
    };

    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        1,
        [("pid".into(), DataType::Int, 1)],
    ));

    let mut w = Writer::create(&out)
        .schemas(&d)
        .compression(compression)
        .build()?;
    for i in 0..events {
        w.event(|ev| {
            ev.bank("REC::Particle", |b| {
                b.row(|r| r.set("pid", (i % 2212) as i32).map(|_| ()))?;
                Ok(())
            })?;
            Ok(())
        })?;
    }
    let summary = w.finish()?;

    let chain = Chain::open(&out)?;
    eprintln!(
        "wrote {out}: {} events, {} records, {codec}, {} bytes",
        chain.event_count(),
        chain.record_count(),
        summary.bytes,
    );
    Ok(())
}
