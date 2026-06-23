//! Hot-loop benchmark binary for profiling.
//!
//! Runs `file.events()` over the same file N times, accumulating a
//! side-effect to defeat dead-code elimination. Designed to give a
//! profiler a long, steady-state signal.
//!
//! Usage:
//!   cargo run --release -p hipo --example bench_scan -- <file.hipo> [iters]

use std::env;
use std::hint::black_box;

use oxihipo::{Chain, Result};

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let path = args.next().expect("usage: bench_scan <file.hipo> [iters]");
    let iters: usize = args
        .next()
        .map(|s| s.parse().expect("iters must be a number"))
        .unwrap_or(10);

    let file = Chain::open(&path)?;
    let particle = file.schemas().require("REC::Particle")?;
    let h_pid = particle.handle::<i32>("pid")?;
    let h_px = particle.handle::<f32>("px")?;

    let start = std::time::Instant::now();
    let mut events_total: u64 = 0;
    let mut rows_total: u64 = 0;
    let mut sum_pid: i64 = 0;
    let mut sum_px: f64 = 0.0;

    for _ in 0..iters {
        for ev in file.events() {
            let ev = ev?;
            events_total += 1;
            if let Some(p) = ev.bank("REC::Particle") {
                let pid = p.read(h_pid);
                let px = p.read(h_px);
                rows_total += pid.len() as u64;
                for &v in pid.iter() {
                    sum_pid = sum_pid.wrapping_add(v as i64);
                }
                for &v in px.iter() {
                    sum_px += v as f64;
                }
            }
        }
    }

    // black_box to prevent the optimizer from eliding the loops above.
    let _ = black_box(sum_pid);
    let _ = black_box(sum_px);
    let _ = black_box(rows_total);

    let elapsed = start.elapsed();
    let per_iter = elapsed / iters as u32;
    let per_event_ns = elapsed.as_nanos() as f64 / events_total as f64;
    eprintln!(
        "bench_scan: {iters} iters, {events_total} events, {rows_total} rows, {sum_pid} pid sum, {sum_px:.3} px sum",
    );
    eprintln!(
        "  elapsed: {:.3}s  per-iter: {:.3}s  per-event: {:.1} ns ({:.0} kev/s)",
        elapsed.as_secs_f64(),
        per_iter.as_secs_f64(),
        per_event_ns,
        events_total as f64 / 1000.0 / elapsed.as_secs_f64(),
    );
    Ok(())
}
