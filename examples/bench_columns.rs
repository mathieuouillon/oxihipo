//! Benchmark `Chain::read_columns` on a real file — the Rust half of the
//! Rust-vs-Python read comparison (pairs with `py/examples/bench_columns.py`).
//!
//! Reads the requested columns of one bank from the whole (filtered) chain and
//! reports throughput. The cache is warmed by one untimed read, so the numbers
//! are decode-bound, not disk-bound.
//!
//! ```text
//! cargo run --release --example bench_columns -- <file> [bank] [c1,c2,..] [threads] [repeats]
//! ```

use std::hint::black_box;
use std::time::Instant;

use oxihipo::{Chain, Result};

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let Some(file) = a.next() else {
        eprintln!("usage: bench_columns <file> [bank] [cols] [threads] [repeats]");
        std::process::exit(2);
    };
    let bank = a.next().unwrap_or_else(|| "REC::Particle".into());
    let cols_arg = a.next().unwrap_or_else(|| "px,py,pz,pid".into());
    let threads: usize = a.next().map_or(0, |s| s.parse().unwrap());
    let repeats: usize = a.next().map_or(5, |s| s.parse().unwrap());
    let cols: Vec<&str> = cols_arg.split(',').collect();

    let file_gb = std::fs::metadata(&file)?.len() as f64 / 1e9;
    let chain = Chain::open(&file)?;
    let events = chain.event_count();
    let sel: &[(&str, &[&str])] = &[(bank.as_str(), &cols)];

    // Warm the OS cache + let the allocator settle (untimed).
    let rows = chain.read_columns(sel, None, threads)?[0].total_rows();

    let mut times: Vec<f64> = Vec::with_capacity(repeats);
    for _ in 0..repeats {
        let t = Instant::now();
        let bufs = chain.read_columns(sel, None, threads)?;
        black_box(&bufs);
        times.push(t.elapsed().as_secs_f64());
    }
    times.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let best = times[0];
    let median = times[times.len() / 2];

    println!("RUST  read_columns  bank={bank}  cols=[{cols_arg}]  threads={threads}");
    println!(
        "  {events} events, {rows} rows, {file_gb:.2} GB, {} cols",
        cols.len()
    );
    println!(
        "  best {best:.3}s  median {median:.3}s  |  {:.2} Mevt/s  {:.1} Mrow/s  {:.2} GB/s",
        events as f64 / 1e6 / best,
        rows as f64 / 1e6 / best,
        file_gb / best,
    );
    Ok(())
}
