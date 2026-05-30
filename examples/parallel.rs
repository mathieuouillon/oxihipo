//! Parallel event processing — `Chain::par_for_each` and `Chain::par_reduce`.
//!
//! Both fan work across every record of every file in the chain on a
//! rayon thread pool. `threads = 0` lets rayon pick one worker per
//! logical CPU. The path may be a file, a directory, or a glob.
//!
//! Usage:
//!   cargo run --release -p hipo --example parallel -- <file|dir|glob> [threads]

use std::env;
use std::sync::atomic::{AtomicU64, Ordering};

use oxihipo::{Chain, EventCtx, Result};

/// Per-thread accumulator for `par_reduce`. Each worker folds events into
/// its own `Stats`; `combine` then merges the workers' partial results.
#[derive(Clone, Copy, Default)]
struct Stats {
    events: u64,
    particles: u64,
    electrons: u64,
}

impl Stats {
    /// The `par_reduce` `fold` step: add one event to the accumulator.
    fn add_event(mut self, ev: &EventCtx<'_>) -> Self {
        self.events += 1;
        if let Some(parts) = ev.bank("REC::Particle") {
            let rows = parts.rows();
            self.particles += rows as u64;
            for r in 0..rows {
                let pid: i32 = parts.get("pid", r);
                if pid == 11 {
                    self.electrons += 1;
                }
            }
        }
        self
    }

    /// The `par_reduce` `combine` step: merge two workers' accumulators.
    /// Must be associative — records finish in no particular order.
    fn merge(self, other: Self) -> Self {
        Self {
            events: self.events + other.events,
            particles: self.particles + other.particles,
            electrons: self.electrons + other.electrons,
        }
    }
}

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

    // par_reduce — stateful aggregation without a shared-state footgun.
    // `init` builds a fresh accumulator per worker, `fold` adds one event
    // to it, `combine` merges two workers' accumulators. Preferred when
    // computing a value (number or struct) over the whole chain.
    let Stats {
        events,
        particles,
        electrons,
    } = chain.par_reduce(
        threads,
        Stats::default,
        |acc, ev| acc.add_event(ev),
        Stats::merge,
    )?;
    println!(
        "par_reduce:   {events} events, {particles} REC::Particle rows, {electrons} electrons"
    );

    // par_for_each — one closure call per event, for side effects only.
    // Order is not preserved and the closure runs on every worker, so
    // shared state must be atomic (or behind a lock). Returns `ChainStats`.
    let multi_particle = AtomicU64::new(0);
    let run = chain.par_for_each(threads, |ev| {
        if ev.bank("REC::Particle").is_some_and(|p| p.rows() > 1) {
            multi_particle.fetch_add(1, Ordering::Relaxed);
        }
    })?;
    let multi = multi_particle.load(Ordering::Relaxed);
    println!(
        "par_for_each: {} events, {} records, {:.3}s, {:.0} kev/s",
        run.events_in,
        run.records,
        run.elapsed.as_secs_f64(),
        run.throughput_kev_s(),
    );
    println!("  {multi} of those events have more than one particle");

    Ok(())
}
