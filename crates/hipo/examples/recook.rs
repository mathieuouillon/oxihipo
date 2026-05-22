//! Re-cook a HIPO file into the `Lz4Chunked` format for benchmarking.
//!
//! Reads every event from `<in>` and writes them to `<out>` with the new
//! `Compression::Lz4Chunked { events_per_chunk }` codec. The dictionary
//! is carried over unchanged.
//!
//! Files written by this example are **not** readable by the C++
//! `hipo4` reader (new compression tag = 4) — they're intended for
//! Rust-side performance experiments.
//!
//! ```sh
//! cargo run -p hipo --release --example recook -- \
//!     /volatile/.../in.hipo \
//!     /scratch/$USER/out_chunked.hipo \
//!     32
//! ```
//!
//! Then time both files with `bench_par`:
//!
//! ```sh
//! cargo run -p hipo --release --example bench_par -- /scratch/.../in.hipo 0
//! cargo run -p hipo --release --example bench_par -- /scratch/.../out_chunked.hipo 0
//! ```

use std::env;
use std::time::Instant;

use hipo::{Chain, Compression, Result, Writer};

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let input = args
        .next()
        .expect("usage: recook <in> <out> [events_per_chunk]");
    let output = args
        .next()
        .expect("usage: recook <in> <out> [events_per_chunk]");
    let events_per_chunk: u32 = args
        .next()
        .map(|s| s.parse().expect("events_per_chunk must be a number"))
        .unwrap_or(32);

    let chain = Chain::open(&input)?;
    let dict = chain.schemas().clone();
    let total_events = chain.event_count();
    eprintln!(
        "recook: {input} -> {output} ({total_events} events, events_per_chunk = {events_per_chunk})"
    );

    let start = Instant::now();
    {
        let mut w = Writer::create(&output)
            .schemas(&dict)
            .compression(Compression::Lz4Chunked { events_per_chunk })
            .build()?;
        let mut written: u64 = 0;
        let mut last_pct = -1i64;
        for ev in chain.events() {
            // The OwnedEvent exposes its raw event bytes; we write them
            // through directly so we don't re-parse / re-serialise banks.
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

    // File size deltas — useful to track the chunked compression ratio.
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
