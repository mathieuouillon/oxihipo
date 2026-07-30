//! Scan-throughput benchmark: shared-atomic `for_each` vs `par_fold`.
//!
//! Times the same per-event sum four ways over the same input — sequential
//! and parallel, through `for_each` (accumulating into a shared atomic) and
//! through `par_fold` (a per-worker accumulator joined by `reduce`). The
//! only thing that differs between the sequential and parallel runs of a
//! given API is the `threads` argument.
//!
//! The point of the pairing: `for_each`'s signature returns only
//! `ChainStats`, so anything you want to *collect* has to cross a shared
//! cache line. `par_fold` removes that. How much it is worth depends on
//! which regime the input is in, and this benchmark exists to tell you
//! which one you are in rather than to assume:
//!
//!   - cheap events (`gen_synthetic`, 4M one-row events) — the atomic is
//!     the whole cost. Measured on 12 threads, `for_each` goes *0.32x*
//!     going from one thread to twelve, i.e. three times slower than
//!     serial, while `par_fold` goes 7.98x.
//!   - a real 599k-event CLAS12 DST — LZ4 decode of 47 banks dominates,
//!     `for_each` already scales 3.27x, and the two APIs land within
//!     run-to-run noise (0.92-1.06x across repeats).
//!
//! Also intended for measuring on shared filesystems — e.g. JLab ifarm
//! `/cache` vs `/volatile`.
//!
//! Usage:
//!   cargo run --release --example bench_par -- <file|dir|glob> [threads] [reps]
//!
//! `threads = 0` (the default) lets rayon pick one worker per logical CPU.
//! The path may be a single file, a directory, or a glob like `data/*.hipo`.
//! Each variant runs `reps` times (default 3) and the best is reported, so
//! the first run's cold-cache penalty doesn't dominate.

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
        .expect("usage: bench_par <file-or-dir> [threads] [reps]");
    let threads: usize = args
        .next()
        .map(|s| s.parse().expect("threads must be a number"))
        .unwrap_or(0);
    let reps: usize = args
        .next()
        .map(|s| s.parse().expect("reps must be a number"))
        .unwrap_or(3);

    // `Chain::open` dispatches on file / directory / glob pattern.
    let chain = Chain::open(&path)?;
    // The reader streams each record on demand (one `pread` per record into a
    // recycled buffer), so there's no whole-file priming step — both passes
    // rely on the kernel's per-descriptor readahead.
    let events = chain.event_count();
    eprintln!(
        "bench_par: {} file(s), {events} events, {} records, {reps} reps (best reported)",
        chain.file_count(),
        chain.record_count(),
    );

    // The four variants. Boxed so they can be driven from one loop —
    // **interleaved**, one rep of each per round rather than all reps of one
    // before the next. Running them in blocks lets any drift over the run
    // (thermal, page-cache warming) land entirely on whichever variant went
    // last, which is exactly the kind of bias that invents a 5% difference.
    type Variant<'a> = (&'static str, Box<dyn Fn() -> Result<u64> + 'a>);
    let variants: Vec<Variant<'_>> = vec![
        (
            "for_each(1)   shared atomic",
            Box::new(|| {
                let sum = AtomicU64::new(0);
                chain.for_each(1, |ev| {
                    sum.fetch_add(particle_rows(ev), Ordering::Relaxed);
                })?;
                Ok(sum.into_inner())
            }),
        ),
        (
            "for_each(n)   shared atomic",
            Box::new(|| {
                let sum = AtomicU64::new(0);
                chain.for_each(threads, |ev| {
                    sum.fetch_add(particle_rows(ev), Ordering::Relaxed);
                })?;
                Ok(sum.into_inner())
            }),
        ),
        (
            "par_fold(1)   per-worker acc",
            Box::new(|| {
                let (sum, _) = chain.par_fold(
                    1,
                    || 0u64,
                    |acc, ev| *acc += particle_rows(ev),
                    |a, b| a + b,
                )?;
                Ok(sum)
            }),
        ),
        (
            "par_fold(n)   per-worker acc",
            Box::new(|| {
                let (sum, _) = chain.par_fold(
                    threads,
                    || 0u64,
                    |acc, ev| *acc += particle_rows(ev),
                    |a, b| a + b,
                )?;
                Ok(sum)
            }),
        ),
    ];

    let mut bests = vec![Duration::MAX; variants.len()];
    // Every variant must produce the identical checksum; a faster wrong
    // answer is not a speed-up.
    let mut checksum: Option<u64> = None;
    let mut runs = 0usize;
    for _ in 0..reps {
        for (i, (label, run)) in variants.iter().enumerate() {
            let start = Instant::now();
            let sum = run()?;
            bests[i] = bests[i].min(start.elapsed());
            let _ = black_box(sum);
            runs += 1;
            match checksum {
                None => checksum = Some(sum),
                Some(want) => assert_eq!(sum, want, "{label} disagreed: {sum} != {want}"),
            }
        }
    }

    let want = checksum.unwrap_or(0);
    eprintln!("\n  checksum {want} (identical across all {runs} runs)\n");
    for (i, (label, _)) in variants.iter().enumerate() {
        report(
            &label.replace("(n)", &format!("({threads})")),
            bests[i],
            events,
        );
    }
    eprintln!();
    speedup("for_each scaling", bests[0], bests[1]);
    speedup("par_fold scaling", bests[2], bests[3]);
    speedup("par_fold(n) vs for_each(n)", bests[1], bests[3]);
    Ok(())
}

fn report(label: &str, elapsed: Duration, events: u64) {
    let secs = elapsed.as_secs_f64();
    let kev_s = if secs > 0.0 {
        events as f64 / 1000.0 / secs
    } else {
        0.0
    };
    eprintln!("  {label:<30} {secs:>8.3}s  {kev_s:>9.0} kev/s");
}

fn speedup(label: &str, base: Duration, faster: Duration) {
    eprintln!(
        "  {label:<30} {:>7.2}x",
        base.as_secs_f64() / faster.as_secs_f64().max(f64::MIN_POSITIVE),
    );
}
