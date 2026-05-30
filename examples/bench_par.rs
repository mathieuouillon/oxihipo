//! Parallel-scan throughput benchmark.
//!
//! Times a sequential `Chain::events()` walk against `Chain::par_reduce`
//! over the same input and prints the speed-up. Intended for measuring on
//! shared filesystems — e.g. JLab ifarm `/cache` vs `/volatile`.
//!
//! Usage:
//!   cargo run --release -p hipo --example bench_par -- <file|dir|glob> [threads]
//!
//! `threads = 0` (the default) lets rayon pick one worker per logical CPU.
//! The path may be a single file, a directory, or a glob like `data/*.hipo`.

use std::env;
use std::hint::black_box;
use std::time::{Duration, Instant};

use oxihipo::{Chain, EventCtx, Result};

/// Sum REC::Particle row counts — a cheap, representative per-event probe.
fn particle_rows(ev: &EventCtx<'_>) -> u64 {
    ev.bank("REC::Particle").map_or(0, |b| b.rows() as u64)
}

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let path = args
        .next()
        .expect("usage: bench_par <file-or-dir> [threads]");
    let threads: usize = args
        .next()
        .map(|s| s.parse().expect("threads must be a number"))
        .unwrap_or(0);

    // `Chain::open` dispatches on file / directory / glob pattern.
    let chain = Chain::open(&path)?;
    // Async-prefetch the whole file's pages so the sequential pass below
    // gets the same I/O priming the parallel pass receives automatically.
    chain.prefetch();
    let events = chain.event_count();
    eprintln!(
        "bench_par: {} file(s), {events} events, {} records",
        chain.file_count(),
        chain.record_count(),
    );

    // ---- sequential baseline --------------------------------------------
    let start = Instant::now();
    let mut seq_sum: u64 = 0;
    for ev in chain.events() {
        seq_sum += particle_rows(&ev.ctx());
    }
    let seq = start.elapsed();
    let _ = black_box(seq_sum);

    // ---- parallel -------------------------------------------------------
    let start = Instant::now();
    let par_sum: u64 = chain.par_reduce(
        threads,
        || 0u64,
        |acc, ev| acc + particle_rows(ev),
        |a, b| a + b,
    )?;
    let par = start.elapsed();
    let _ = black_box(par_sum);

    assert_eq!(seq_sum, par_sum, "parallel sum must match sequential");

    report("sequential events()", seq, events);
    report(&format!("par_reduce(threads={threads})"), par, events);
    eprintln!(
        "  speed-up: {:.2}x",
        seq.as_secs_f64() / par.as_secs_f64().max(f64::MIN_POSITIVE),
    );
    Ok(())
}

fn report(label: &str, elapsed: Duration, events: u64) {
    let secs = elapsed.as_secs_f64();
    let kev_s = if secs > 0.0 {
        events as f64 / 1000.0 / secs
    } else {
        0.0
    };
    eprintln!("  {label:<28} {secs:>8.3}s  {kev_s:>9.0} kev/s");
}
