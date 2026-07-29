//! Event processing with `Chain::for_each`.
//!
//! `for_each(threads, f)` runs the closure `f` on every event of every
//! file in the chain. The `threads` argument is the only knob — `1` is
//! single-threaded (in input order), `0` is one worker per logical CPU,
//! and `n` is exactly `n` workers. Shared state in `f` must be atomic (or
//! behind a lock) because the parallel modes visit events out of order.
//!
//! Usage: `cargo run --release --example parallel -- <file|dir|glob> [threads]`

use std::env;
use std::sync::atomic::{AtomicU64, Ordering};

use oxihipo::{Chain, Result};

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let path = args
        .next()
        .expect("usage: parallel <file-or-dir> [threads]");
    let threads: usize = args
        .next()
        .map(|s| s.parse().expect("threads must be a number"))
        .unwrap_or(0);

    let chain = Chain::open(&path)?;
    println!(
        "opened {} file(s), {} events",
        chain.file_count(),
        chain.event_count(),
    );

    // Functional aggregation: one closure, shared counters as atomics.
    // Pass `threads = 1` for the identical single-threaded scan — the only
    // difference between the two is this argument.
    let particles = AtomicU64::new(0);
    let electrons = AtomicU64::new(0);
    let multi_particle = AtomicU64::new(0);

    let stats = chain.for_each(threads, |ev| {
        if let Some(p) = ev.bank("REC::Particle") {
            let rows = p.rows();
            particles.fetch_add(rows as u64, Ordering::Relaxed);
            if rows > 1 {
                multi_particle.fetch_add(1, Ordering::Relaxed);
            }
            for r in 0..rows {
                let pid: i32 = p.get("pid", r);
                if pid == 11 {
                    electrons.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    })?;

    println!(
        "for_each(threads={threads}): {} events, {} records, {:.3}s, {:.0} kev/s",
        stats.events_in,
        stats.records,
        stats.elapsed.as_secs_f64(),
        stats.throughput_kev_s(),
    );
    println!(
        "  {} REC::Particle rows, {} electrons, {} events with >1 particle",
        particles.load(Ordering::Relaxed),
        electrons.load(Ordering::Relaxed),
        multi_particle.load(Ordering::Relaxed),
    );
    Ok(())
}
