//! Concurrent random-access throughput: `Chain::event` from N threads.
//!
//! Usage: `bench_random <file> [n] [threads]`
//!
//! This is the benchmark for the record cache. `event()` decodes a whole
//! record on a miss (~8 MB on a CLAS12 DST), so what matters is whether
//! concurrent misses serialise. They used to: the cache guard was held across
//! the decode, and throughput was flat at ~380 ev/s from 1 to 12 threads.
//!
//! Compare thread counts on the same file, and interleave builds when
//! comparing two — measuring one to completion then the other lets page-cache
//! warming masquerade as a 1.5x speedup on a single thread, which is
//! impossible for a lock-only change and is how that error announces itself.
//!
//! The checksum must match across thread counts and builds: it is the proof
//! that a faster run did the same work.
use oxihipo::{Chain, Result};
use std::env;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

fn main() -> Result<()> {
    let mut a = env::args().skip(1);
    let path = a.next().expect("usage: probe_rand <file> <n> <threads>");
    let n: u64 = a.next().map(|s| s.parse().unwrap()).unwrap_or(2000);
    let threads: usize = a.next().map(|s| s.parse().unwrap()).unwrap_or(8);

    let base = Chain::open(&path)?;
    let total = base.event_count();
    // Deterministic xorshift indices, identical across builds.
    let mut x = 12345u64;
    let idx: Vec<u64> = (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % total
        })
        .collect();

    let sum = AtomicU64::new(0);
    let got = AtomicU64::new(0);
    let chunk = idx.len().div_ceil(threads);
    let t = Instant::now();
    std::thread::scope(|s| {
        for part in idx.chunks(chunk) {
            let c = base.clone();
            let (sum, got) = (&sum, &got);
            s.spawn(move || {
                for &i in part {
                    if let Some(ev) = c.event(i) {
                        sum.fetch_add(
                            u64::from(ev.size()) ^ u64::from(ev.tag()),
                            Ordering::Relaxed,
                        );
                        got.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    let el = t.elapsed();
    println!(
        "{threads:>2}t  {:>8.1} ev/s  got={}  checksum={}",
        n as f64 / el.as_secs_f64(),
        got.load(Ordering::Relaxed),
        sum.load(Ordering::Relaxed)
    );
    Ok(())
}
