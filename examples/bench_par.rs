//! Scan-throughput benchmark.
//!
//! Times a single-threaded scan against a parallel one over the same
//! input and prints the speed-up — both through the *same* `for_each`
//! call, differing only in the `threads` argument. Intended for measuring
//! on shared filesystems — e.g. JLab ifarm `/cache` vs `/volatile`.
//!
//! Usage:
//!   cargo run --release --example bench_par -- <file|dir|glob> [threads]
//!
//! `threads = 0` (the default) lets rayon pick one worker per logical CPU.
//! The path may be a single file, a directory, or a glob like `data/*.hipo`.

use std::env;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
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
    // The reader streams each record on demand (one `pread` per record into a
    // recycled buffer), so there's no whole-file priming step — both passes
    // rely on the kernel's per-descriptor readahead.
    let events = chain.event_count();
    eprintln!(
        "bench_par: {} file(s), {events} events, {} records",
        chain.file_count(),
        chain.record_count(),
    );

    // ---- single-threaded: for_each(1, ...) ------------------------------
    let seq_sum = AtomicU64::new(0);
    let start = Instant::now();
    chain.for_each(1, |ev| {
        seq_sum.fetch_add(particle_rows(ev), Ordering::Relaxed);
    })?;
    let seq = start.elapsed();
    let seq_sum = seq_sum.into_inner();
    let _ = black_box(seq_sum);

    // ---- parallel: the *same* closure, just `threads` instead of 1 ------
    let par_sum = AtomicU64::new(0);
    let start = Instant::now();
    chain.for_each(threads, |ev| {
        par_sum.fetch_add(particle_rows(ev), Ordering::Relaxed);
    })?;
    let par = start.elapsed();
    let par_sum = par_sum.into_inner();
    let _ = black_box(par_sum);

    assert_eq!(seq_sum, par_sum, "parallel sum must match single-threaded");

    report("for_each(1)  single-thread", seq, events);
    report(&format!("for_each({threads}) parallel"), par, events);
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
