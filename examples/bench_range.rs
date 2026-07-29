//! `for_each_range` against `event(idx)` per index — the comparison that
//! decides whether an index can be exploited at all.
use oxihipo::Chain;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

fn main() -> oxihipo::Result<()> {
    let path = std::env::args()
        .nth(1)
        .expect("usage: bench_range FILE [FRACTION]");
    let frac: f64 = std::env::args().nth(2).map_or(0.15, |s| s.parse().unwrap());
    let chain = Chain::open(&path)?;
    let total = chain.event_count();
    let spans = chain.record_spans();

    // Take every Nth record, as a rare-value index would leave behind.
    let step = (1.0 / frac).round().max(1.0) as usize;
    let ranges: Vec<(u64, u64)> = spans
        .iter()
        .step_by(step)
        .map(|s| {
            (
                s.global_event_start,
                s.global_event_start + s.event_count as u64,
            )
        })
        .collect();
    let kept: u64 = ranges.iter().map(|(a, b)| b - a).sum();
    println!(
        "{total} events, {} records; keeping {kept} in {} range(s)",
        spans.len(),
        ranges.len()
    );

    // One contiguous range covering the same share, to separate the per-call
    // cost (task list rebuilt per call, one rayon dispatch each) from the
    // per-event cost. 27 calls against 1 is otherwise the thing being measured.
    let one = (0u64, kept.min(total));
    for threads in [1usize, 16] {
        let n = AtomicU64::new(0);
        let t0 = Instant::now();
        chain.for_each_range(one.0..one.1, threads, |_| {
            n.fetch_add(1, Ordering::Relaxed);
        })?;
        let dt = t0.elapsed().as_secs_f64();
        println!(
            "  ONE range  -j{threads:<3}      {dt:7.3}s  {:.0} kev/s",
            n.load(Ordering::Relaxed) as f64 / dt / 1e3
        );
    }
    {
        let t0 = Instant::now();
        let mut n = 0u64;
        for idx in one.0..one.1 {
            if chain.event(idx).is_some() {
                n += 1;
            }
        }
        let dt = t0.elapsed().as_secs_f64();
        println!(
            "  ONE range  event(idx)   {dt:7.3}s  {:.0} kev/s",
            n as f64 / dt / 1e3
        );
    }
    println!();

    for threads in [1usize, 16] {
        let n = AtomicU64::new(0);
        let t0 = Instant::now();
        for &(a, b) in &ranges {
            chain.for_each_range(a..b, threads, |_| {
                n.fetch_add(1, Ordering::Relaxed);
            })?;
        }
        let dt = t0.elapsed().as_secs_f64();
        println!(
            "  for_each_range -j{threads:<3} {dt:7.3}s  {:.0} kev/s  ({} events)",
            n.load(Ordering::Relaxed) as f64 / dt / 1e3,
            n.load(Ordering::Relaxed)
        );
    }

    // All the ranges in ONE call — what an index would actually hand over.
    for threads in [1usize, 16] {
        let rs: Vec<std::ops::Range<u64>> = ranges.iter().map(|&(a, b)| a..b).collect();
        let n = AtomicU64::new(0);
        let t0 = Instant::now();
        chain.for_each_ranges(&rs, threads, |_| {
            n.fetch_add(1, Ordering::Relaxed);
        })?;
        let dt = t0.elapsed().as_secs_f64();
        println!(
            "  ALL ranges, one call -j{threads:<3} {dt:7.3}s  {:.0} kev/s  ({} events)",
            n.load(Ordering::Relaxed) as f64 / dt / 1e3,
            n.load(Ordering::Relaxed)
        );
    }

    let t0 = Instant::now();
    let mut n = 0u64;
    for &(a, b) in &ranges {
        for idx in a..b {
            if chain.event(idx).is_some() {
                n += 1;
            }
        }
    }
    let dt = t0.elapsed().as_secs_f64();
    println!(
        "  event(idx) per event  {dt:7.3}s  {:.0} kev/s  ({n} events)",
        n as f64 / dt / 1e3
    );
    Ok(())
}
