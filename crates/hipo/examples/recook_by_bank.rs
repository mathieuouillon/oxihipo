//! Re-cook a HIPO file into the `Lz4ByBank` format for benchmarking
//! true partial decompression.
//!
//! Reads every event from `<in>` and writes them to `<out>` using
//! `Compression::Lz4ByBank`. The dictionary is carried over unchanged.
//! Files produced are not readable by the C++ `hipo4` reader (new
//! compression tag = 5).
//!
//! ```sh
//! cargo run -p hipo --release --example recook_by_bank -- \
//!     /Users/.../rec0.hipo /tmp/rec0_by_bank.hipo
//! ```
//!
//! Then bench:
//!
//! ```sh
//! cargo run -p hipo --release --example bench_par -- /tmp/rec0_by_bank.hipo 0
//! ```

use std::env;
use std::time::Instant;

use hipo::{Chain, Compression, Result, Writer};

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let input = args.next().expect("usage: recook_by_bank <in> <out>");
    let output = args.next().expect("usage: recook_by_bank <in> <out>");

    let chain = Chain::open(&input)?;
    let dict = chain.schemas().clone();
    let total_events = chain.event_count();
    eprintln!("recook_by_bank: {input} -> {output} ({total_events} events)");

    let start = Instant::now();
    {
        let mut w = Writer::create(&output)
            .schemas(&dict)
            .compression(Compression::Lz4ByBank)
            .build()?;
        let mut written: u64 = 0;
        let mut last_pct = -1i64;
        for ev in chain.events() {
            // For Lz4ByBank source records, `ev.bytes()` triggers
            // synthetic-bytes synthesis (decompressing every bank).
            // For Bytes-backed source records (the typical case
            // when recook-ing a vanilla Lz4 file), it's zero-copy.
            w.append_raw(ev.bytes())?;
            written += 1;
            if let Some(pct) = (written * 100).checked_div(total_events) {
                let pct = pct as i64;
                if pct != last_pct && pct % 10 == 0 {
                    eprintln!("  {pct:3}%  ({written}/{total_events})");
                    last_pct = pct;
                }
            }
        }
        w.finish()?;
    }
    let elapsed = start.elapsed();

    let in_bytes = std::fs::metadata(&input).map(|m| m.len()).unwrap_or(0);
    let out_bytes = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "done in {:.2}s — {} bytes → {} bytes ({:+.1}%)",
        elapsed.as_secs_f64(),
        in_bytes,
        out_bytes,
        100.0 * (out_bytes as f64 - in_bytes as f64) / (in_bytes as f64).max(1.0),
    );
    Ok(())
}
