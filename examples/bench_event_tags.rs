//! Micro-benchmark: does the event-tag machinery slow the read path?
//!
//! Writes a synthetic tagged file, then times the per-event read paths. The
//! headline question — "no slowdown" — is answered by the **delta** between an
//! unfiltered `for_each` (which never calls the filter) and an all-pass
//! `event_tag_any` filter (which runs the per-event tag check on *every* event):
//! that delta is the entire cost the tagging feature adds to a scan. An
//! unfiltered read never touches the filter at all, so it is unchanged by
//! construction; this quantifies the only per-event cost that exists.
//!
//! Usage:
//!   cargo run --release --example bench_event_tags -- [events] [iters]

use std::env;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use oxihipo::{Chain, Compression, DataType, Dict, EventCtx, Filter, Result, Schema, Writer};

fn dict() -> Dict {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [("evno".into(), DataType::Long, 1)],
    ));
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        31,
        [
            ("pid".into(), DataType::Int, 1),
            ("px".into(), DataType::Float, 1),
        ],
    ));
    d
}

fn write_file(path: &str, comp: Compression, n: u64) -> Result<()> {
    let mut w = Writer::create(path)
        .schemas(&dict())
        .compression(comp)
        .build()?;
    for i in 0..n {
        w.event(|ev| {
            ev.with_tag(1u32 << (i % 3) as u32); // tags cycle 1, 2, 4
            ev.bank("REC::Event", |b| {
                b.row(|r| {
                    r.set("evno", i as i64)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            // A few particles per event so records are realistically sized.
            ev.bank("REC::Particle", |b| {
                for r in 0..3 {
                    b.row(|rw| {
                        rw.set("pid", 11)?;
                        rw.set("px", r as f32)?;
                        Ok(())
                    })?;
                }
                Ok(())
            })?;
            Ok(())
        })?;
    }
    w.finish()?;
    Ok(())
}

/// Time `f` over `iters` runs (plus one warm-up); return the total elapsed.
/// `f` returns a checksum that is `black_box`ed to defeat dead-code elimination.
fn time(iters: usize, f: impl Fn() -> u64) -> Duration {
    black_box(f()); // warm the page cache / branch predictors
    let start = Instant::now();
    let mut sum = 0u64;
    for _ in 0..iters {
        sum = sum.wrapping_add(f());
    }
    black_box(sum);
    start.elapsed()
}

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let n: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let iters: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(15);

    let dir = std::env::temp_dir();
    // Empty closure: isolates record-stream + event-iterate + (filter check),
    // with no bank inflation — exactly the surface the tag pushdown touches.
    let counter = AtomicU64::new(0);
    let work = |_: &EventCtx<'_>| {
        counter.fetch_add(1, Ordering::Relaxed);
    };

    for (label, comp) in [
        ("Lz4  (tag read from the event header)", Compression::Lz4),
        (
            "Lz4PerColumn  (tag read from the record directory)",
            Compression::Lz4PerColumn,
        ),
    ] {
        let path = dir
            .join(format!("oxihipo_tagbench_{comp:?}.hipo"))
            .to_string_lossy()
            .into_owned();
        write_file(&path, comp, n)?;
        let chain = Chain::open(&path)?;

        // Build the filtered chains once, so timing covers only the scan.
        let allpass = chain
            .clone()
            .with_filter(Filter::new().event_tag_any(0b111u32))?; // keeps every event
        let selective = chain
            .clone()
            .with_filter(Filter::new().event_tag_any(0b001u32))?; // keeps 1 of 3
        let require = chain
            .clone()
            .with_filter(Filter::require(["REC::Particle"]))?;

        let base = time(iters, || chain.for_each(1, work).unwrap().events_in);
        let pass = time(iters, || allpass.for_each(1, work).unwrap().events_in);
        let sel = time(iters, || {
            selective.for_each(1, work).unwrap().events_yielded
        });
        let req = time(iters, || require.for_each(1, work).unwrap().events_in);
        let tags = time(iters, || chain.event_tags(None, 1).unwrap().len() as u64);

        let visited = (n * iters as u64) as f64;
        let ns = |d: Duration| d.as_nanos() as f64 / visited;
        let kev_s = |d: Duration| n as f64 / 1000.0 / (d.as_secs_f64() / iters as f64);

        println!("\n{label}  —  {n} events × {iters} iters");
        println!(
            "  {:<34} {:>7.2} ns/ev   {:>8.0} kev/s",
            "unfiltered for_each (baseline)",
            ns(base),
            kev_s(base)
        );
        println!(
            "  {:<34} {:>7.2} ns/ev   Δ {:+.2} ns/ev  ← tag-check cost",
            "event_tag_any, all-pass",
            ns(pass),
            ns(pass) - ns(base)
        );
        println!(
            "  {:<34} {:>7.2} ns/ev   (keeps 1 of 3)",
            "event_tag_any, selective",
            ns(sel)
        );
        println!(
            "  {:<34} {:>7.2} ns/ev   (comparison pushdown)",
            "require REC::Particle",
            ns(req)
        );
        println!(
            "  {:<34} {:>7.2} ns/ev   {:>8.0} kev/s",
            "event_tags() column",
            ns(tags),
            kev_s(tags)
        );
        std::fs::remove_file(&path).ok();
    }

    Ok(())
}
