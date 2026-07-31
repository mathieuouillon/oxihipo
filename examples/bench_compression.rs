//! The compression matrix: every `Codec` x `Layout` pair, measured.
//!
//! Usage: `bench_compression <src.hipo> [reps]`
//!
//! Re-encodes `src` into all 15 pairs and reports, for each: output size,
//! write time, a full scan that touches every bank, and a **selective** scan
//! that reads one column of one bank. The selective number is the one that
//! separates the layouts — it is what `PerColumn` exists for, and a
//! whole-record codec cannot win it because it must inflate everything to
//! reach anything.
//!
//! Method, because these numbers are easy to get wrong:
//!
//! - Every timing is **best-of-`reps`**, not a mean. A mean on a shared
//!   machine measures the machine's other work as much as the code's.
//! - The reps are **interleaved across cells** rather than run cell-by-cell,
//!   so drift over the run cannot land on whichever cell went last.
//! - The source file is read once up front to warm the page cache, so no cell
//!   pays the cold read.
//! - Every cell's read is checksummed and the checksums must agree; a codec
//!   that reads faster by reading less would otherwise look like a win.

use std::time::{Duration, Instant};

use oxihipo::{BankPatterns, Chain, Codec, Compression, Layout, Result, SkimOptions};

const CODECS: [Codec; 5] = [
    Codec::None,
    Codec::Lz4,
    Codec::Lz4Hc,
    Codec::Gzip,
    Codec::Zstd,
];
const LAYOUTS: [Layout; 3] = [Layout::PerChunk, Layout::PerBank, Layout::PerColumn];

struct Cell {
    codec: Codec,
    layout: Layout,
    bytes: u64,
    write: Duration,
    scan_all: Duration,
    scan_one: Duration,
    checksum: i64,
}

/// Read a value out of every bank of every event — the "I need the whole
/// event" workload.
///
/// Reading a *value* is the point: `rows()` alone comes from the record
/// directory and inflates nothing, so a version of this that only counted rows
/// reported the split layouts as faster than their own selective read, which
/// is impossible. Every bank must actually be decompressed here.
fn scan_all(path: &std::path::Path) -> Result<(Duration, i64)> {
    let chain = Chain::open(path)?;
    let names: Vec<String> = chain
        .schemas()
        .iter()
        .map(|s| s.name().to_string())
        .collect();
    let t = Instant::now();
    let mut sum = 0i64;
    for ev in chain.events() {
        let ev = ev?;
        let ctx = ev.ctx();
        for n in &names {
            if let Some(b) = ctx.bank(n) {
                for r in 0..b.rows() {
                    if let Some(v) = b.value(0, r) {
                        sum = sum.wrapping_add(v as i64);
                    }
                }
            }
        }
    }
    Ok((t.elapsed(), sum))
}

/// Read one column of one bank across the file, through the columnar API —
/// the workload the split layouts exist for, and the way you would actually
/// write it.
fn scan_one(path: &std::path::Path, bank: &str, col: &str) -> Result<(Duration, i64)> {
    let chain = Chain::open(path)?;
    let t = Instant::now();
    let bufs = chain.read_columns(&[(bank, &[col][..])], None, 1)?;
    let mut sum = 0i64;
    for b in &bufs {
        for c in &b.columns {
            sum = sum.wrapping_add(c.data.len() as i64);
        }
    }
    Ok((t.elapsed(), sum))
}

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let src = a
        .next()
        .expect("usage: bench_compression <src.hipo> [reps]");
    let reps: usize = a.next().map(|s| s.parse().unwrap()).unwrap_or(3);

    // Pick the widest bank in the file to read a column from.
    let probe = Chain::open(&src)?;
    let (bank, col) = probe
        .schemas()
        .iter()
        .max_by_key(|s| s.num_columns())
        .map(|s| (s.name().to_string(), s.entries()[0].name.clone()))
        .expect("source has no schemas");
    drop(probe);

    let dir = std::env::temp_dir().join("oxihipo-bench-compression");
    std::fs::create_dir_all(&dir)?;
    let all = BankPatterns::from_slice(&["*"])?;

    // Warm the page cache once so no cell pays the cold read.
    let _ = std::fs::read(&src)?;

    let mut cells: Vec<Cell> = Vec::new();
    for codec in CODECS {
        for layout in LAYOUTS {
            cells.push(Cell {
                codec,
                layout,
                bytes: 0,
                write: Duration::MAX,
                scan_all: Duration::MAX,
                scan_one: Duration::MAX,
                checksum: 0,
            });
        }
    }

    // Interleaved: every cell gets rep 1 before any cell gets rep 2.
    for rep in 1..=reps {
        eprintln!("rep {rep}/{reps}");
        for cell in cells.iter_mut() {
            let c = Compression::new(cell.codec, cell.layout);
            let out = dir.join(format!("{:?}_{:?}.hipo", cell.codec, cell.layout));

            let chain = Chain::open(&src)?;
            let t = Instant::now();
            let s = chain.skim_banks_with(&out, c, &all, SkimOptions::default(), |_| {})?;
            cell.write = cell.write.min(t.elapsed());
            cell.bytes = s.write.bytes;

            let (d, sum) = scan_all(&out)?;
            cell.scan_all = cell.scan_all.min(d);
            let (d, one) = scan_one(&out, &bank, &col)?;
            cell.scan_one = cell.scan_one.min(d);
            cell.checksum = sum.wrapping_add(one);
        }
    }

    // Every cell must have read the same values.
    let expect = cells[0].checksum;
    for c in &cells {
        assert_eq!(
            c.checksum, expect,
            "{:?} x {:?} read different values — the comparison is invalid",
            c.codec, c.layout
        );
    }

    let raw = cells
        .iter()
        .find(|c| matches!((c.codec, c.layout), (Codec::None, Layout::PerChunk)))
        .map(|c| c.bytes)
        .unwrap();

    println!("\nsource   {src}");
    println!("column   {bank}.{col}   (the selective read)");
    println!("reps     {reps}, best-of, interleaved\n");
    println!(
        "{:<7} {:<10} {:>12} {:>7} {:>9} {:>10} {:>11}",
        "codec", "layout", "bytes", "ratio", "write s", "scan all", "scan 1 col"
    );
    println!("{}", "-".repeat(72));
    for c in &cells {
        println!(
            "{:<7} {:<10} {:>12} {:>6.2}x {:>9.2} {:>9.0}ms {:>10.0}ms",
            format!("{:?}", c.codec),
            format!("{:?}", c.layout),
            c.bytes,
            raw as f64 / c.bytes as f64,
            c.write.as_secs_f64(),
            c.scan_all.as_secs_f64() * 1000.0,
            c.scan_one.as_secs_f64() * 1000.0,
        );
    }
    println!("\nchecksum {expect} — identical across all 15 cells");
    Ok(())
}
