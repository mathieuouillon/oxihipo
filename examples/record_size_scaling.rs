//! Does record size, or stream size, limit parallel read scaling?
//!
//! `for_each` parallelises over **records** — the work unit is one record, and
//! within it a worker inflates streams lazily and serially. So the ceiling on a
//! parallel scan is the record *count*, not anything about an individual
//! stream. A file with 4 records cannot use more than 4 cores no matter how
//! large its banks are.
//!
//! `max_record_bytes` is already a public knob, so more work units cost only
//! compression ratio (each record's streams restart LZ4's match window). This
//! sweeps it and reports both sides of that trade.
//!
//! ```sh
//! cargo run --release --example record_size_scaling -- src.hipo [cap_events]
//! ```

use std::time::Instant;

use oxihipo::{Chain, Compression, Writer};

fn main() -> oxihipo::Result<()> {
    let mut args = std::env::args().skip(1);
    let src = args
        .next()
        .expect("usage: record_size_scaling <src.hipo> [cap_events]");
    let cap: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);

    let source = Chain::open(&src)?;
    let dict = source.schemas().clone();
    let bank_names: Vec<String> = dict.iter().map(|s| s.name().to_string()).collect();

    let dir = tempfile::tempdir().expect("temp dir");
    println!(
        "source {src}\ncores  {}\n",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );
    println!(
        "{:>10} {:>8} {:>9} {:>8} {:>11} {:>11} {:>9}",
        "rec MB", "records", "size MB", "vs 32MB", "1 thread ms", "all cores ms", "speed-up"
    );

    let mut baseline_size = 0f64;
    for mb in [32usize, 16, 8, 4, 2, 1] {
        let path = dir.path().join(format!("r{mb}.hipo"));
        let mut w = Writer::create(&path)
            .schemas(&dict)
            .compression(Compression::Lz4PerBank)
            .max_record_bytes(mb * 1024 * 1024)
            .build()?;
        let mut n = 0u64;
        for ev in source.events() {
            let ev = ev?;
            w.append_raw(ev.bytes())?;
            n += 1;
            if n >= cap {
                break;
            }
        }
        w.finish()?;

        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as f64 / 1e6;
        if mb == 32 {
            baseline_size = size;
        }

        let chain = Chain::open(&path)?;
        let records = chain.record_count();

        // Read every value of every bank, so the inflate work is real.
        let touch = |threads: usize| -> oxihipo::Result<f64> {
            let mut best = f64::MAX;
            for _ in 0..3 {
                let t = Instant::now();
                chain.for_each(threads, |ev| {
                    for name in &bank_names {
                        // For Lz4PerBank, resolving the bank is what
                        // inflates its stream — that is the work being timed.
                        if let Some(b) = ev.bank(name) {
                            std::hint::black_box(b.rows());
                        }
                    }
                })?;
                best = best.min(t.elapsed().as_secs_f64() * 1e3);
            }
            Ok(best)
        };
        let serial = touch(1)?;
        let par = touch(0)?;

        println!(
            "{:>10} {:>8} {:>9.1} {:>+7.2}% {:>11.1} {:>11.1} {:>8.2}x",
            mb,
            records,
            size,
            (size / baseline_size - 1.0) * 100.0,
            serial,
            par,
            serial / par
        );
    }
    Ok(())
}
