//! Read-throughput benchmark across every `Compression` variant.
//!
//! Re-encodes one identical set of events into each compression format, then
//! measures **single-thread read speed** at growing *scope* — each scope
//! reads *every value of every column* of that many banks, for every event:
//!
//! - **sel** — `REC::Event` only (1 bank).
//! - **full** — `REC::Particle` + `REC::Event` (2 banks, all their columns).
//! - **10 / 20 / 40 bk** — the first 10 / 20 / 40 populated banks.
//! - **all** — every populated bank (≈ 73 on real CLAS12 data).
//!
//! The per-bank / per-column formats decode only the banks — and, for
//! `Lz4PerColumn`, the columns — each scope names; the whole-record formats
//! (None/Lz4/Lz4Best/Gzip) inflate the whole record regardless.
//!
//! The `size MB` column is the on-disk file size and `ratio` is versus the
//! `None` (uncompressed) size. Read passes report best-of-`iters` (min = least
//! noise). Numbers are single-thread, warm-cache, on the running machine —
//! relative throughput between formats is the point, not absolute MB/s.
//!
//! ```sh
//! # synthetic data (default 100k events, 5 iters):
//! cargo run --release --example bench_read_compression
//! cargo run --release --example bench_read_compression -- 200000 7
//! # or a real file (first `cap` events, `iters` passes):
//! cargo run --release --example bench_read_compression -- /path/to/rec.hipo 5 100000
//!
//! # keep the per-format files (so another tool can read them) instead of a
//! # temp dir that's cleaned up on exit:
//! OXIHIPO_BENCH_KEEP=/tmp/fmt cargo run --release --example bench_read_compression -- rec.hipo 5 200000
//! ```

use std::collections::BTreeSet;
use std::env;
use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

use oxihipo::{Bank, Chain, Compression, DataType, Dict, Result, Schema, Writer};

const PIDS: [i32; 5] = [11, 211, -211, 2212, 22];

fn build_dict() -> Dict {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [
            ("category".into(), DataType::Long, 1),
            ("topology".into(), DataType::Long, 1),
            ("beamCharge".into(), DataType::Float, 1),
            ("liveTime".into(), DataType::Double, 1),
            ("startTime".into(), DataType::Float, 1),
            ("helicity".into(), DataType::Byte, 1),
        ],
    ));
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        31,
        [
            ("pid".into(), DataType::Int, 1),
            ("px".into(), DataType::Float, 1),
            ("py".into(), DataType::Float, 1),
            ("pz".into(), DataType::Float, 1),
            ("vx".into(), DataType::Float, 1),
            ("vy".into(), DataType::Float, 1),
            ("vz".into(), DataType::Float, 1),
            ("vt".into(), DataType::Float, 1),
            ("charge".into(), DataType::Byte, 1),
            ("beta".into(), DataType::Float, 1),
            ("chi2pid".into(), DataType::Float, 1),
            ("status".into(), DataType::Short, 1),
        ],
    ));
    d
}

/// Write `n_events` of semi-realistic, compressible CLAS12-shaped data.
/// Values are coarsely quantized so LZ4/gzip see real redundancy (random
/// floats would make every codec look identical and useless).
fn generate(path: &Path, n_events: u64) -> Result<()> {
    let dict = build_dict();
    let mut w = Writer::create(path)
        .schemas(&dict)
        .compression(Compression::Lz4)
        .build()?;
    for ev_i in 0..n_events {
        let n_parts = 4 + (ev_i % 9) as usize; // 4..=12 particles
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| {
                    r.set("category", (ev_i % 16) as i64)?;
                    r.set("topology", ((ev_i / 16) % 256) as i64)?;
                    r.set("beamCharge", (ev_i % 100) as f32 * 0.01)?;
                    r.set("liveTime", 0.95_f64)?;
                    r.set("startTime", (ev_i % 1024) as f32 * 0.1)?;
                    r.set("helicity", if ev_i % 2 == 0 { 1i8 } else { -1i8 })?;
                    Ok(())
                })?;
                Ok(())
            })?;
            ev.bank("REC::Particle", |b| {
                for k in 0..n_parts {
                    let idx = ev_i as usize + k;
                    let pid = PIDS[idx % PIDS.len()];
                    let q = (idx % 64) as f32 * 0.03125;
                    b.row(|r| {
                        r.set("pid", pid)?;
                        r.set("px", q - 1.0)?;
                        r.set("py", q * 0.5)?;
                        r.set("pz", 2.0 + q)?;
                        r.set("vx", 0.0_f32)?;
                        r.set("vy", 0.0_f32)?;
                        r.set("vz", q - 5.0)?;
                        r.set("vt", 0.0_f32)?;
                        r.set("charge", pid.signum() as i8)?;
                        r.set("beta", 0.9_f32 + q * 0.001)?;
                        r.set("chi2pid", q - 0.5)?;
                        r.set("status", ((ev_i + k as u64) % 4000) as i16)?;
                        Ok(())
                    })?;
                }
                Ok(())
            })?;
            Ok(())
        })?;
    }
    w.finish()?;
    Ok(())
}

struct Stats {
    file_bytes: u64,
    sel: Duration,
    full: Duration,
    b10: Duration,
    b20: Duration,
    b40: Duration,
    all: Duration,
}

/// Warm up once, then time `iters` passes; return the fastest (min noise).
fn best_of(iters: usize, mut pass: impl FnMut() -> Result<u64>) -> Result<Duration> {
    black_box(pass()?); // warm-up
    let mut best = Duration::MAX;
    for _ in 0..iters {
        let t = Instant::now();
        let sink = pass()?;
        let d = t.elapsed();
        black_box(sink);
        best = best.min(d);
    }
    Ok(best)
}

/// Read (and sum) *every value of every column* of `b` — forces the bank's
/// columns to decompress. Scalar columns are read whole; array columns
/// (`length > 1`) are read row-by-row.
fn read_bank_values(b: &Bank, sink: &mut u64) {
    for e in b.schema().entries() {
        let name = &e.name;
        if e.length == 1 {
            match e.ty {
                DataType::Byte => sum(b.col::<i8>(name).ok(), sink, |v| v as u64),
                DataType::Short => sum(b.col::<i16>(name).ok(), sink, |v| v as u64),
                DataType::Int => sum(b.col::<i32>(name).ok(), sink, |v| v as u64),
                DataType::Long => sum(b.col::<i64>(name).ok(), sink, |v| v as u64),
                DataType::Float => sum(b.col::<f32>(name).ok(), sink, |v| v.to_bits() as u64),
                DataType::Double => sum(b.col::<f64>(name).ok(), sink, |v| v.to_bits()),
            }
        } else {
            for r in 0..b.rows() {
                match e.ty {
                    DataType::Byte => sum(b.array_at::<i8>(name, r).ok(), sink, |v| v as u64),
                    DataType::Short => sum(b.array_at::<i16>(name, r).ok(), sink, |v| v as u64),
                    DataType::Int => sum(b.array_at::<i32>(name, r).ok(), sink, |v| v as u64),
                    DataType::Long => sum(b.array_at::<i64>(name, r).ok(), sink, |v| v as u64),
                    DataType::Float => sum(b.array_at::<f32>(name, r).ok(), sink, |v| {
                        v.to_bits() as u64
                    }),
                    DataType::Double => sum(b.array_at::<f64>(name, r).ok(), sink, |v| v.to_bits()),
                }
            }
        }
    }
}

/// Fold a column/array slice into `sink` with a per-element projection.
fn sum<T: Copy>(vals: Option<std::borrow::Cow<'_, [T]>>, sink: &mut u64, f: impl Fn(T) -> u64) {
    if let Some(c) = vals {
        for &v in &*c {
            *sink = sink.wrapping_add(f(v));
        }
    }
}

/// Best-of `iters` timing of reading every value of every bank in `names`,
/// for every event in the chain.
fn bench_banks(chain: &Chain, iters: usize, names: &[String]) -> Result<Duration> {
    best_of(iters, || {
        let mut sink = 0u64;
        for ev in chain.events() {
            let ev = ev?;
            for name in names {
                if let Some(b) = ev.bank(name) {
                    read_bank_values(&b, &mut sink);
                }
            }
        }
        Ok(sink)
    })
}

fn bench_one(path: &Path, iters: usize) -> Result<Stats> {
    let chain = Chain::open(path)?;
    let file_bytes = std::fs::metadata(path)?.len();

    // Populated bank names, sorted (deterministic), sampled from the first
    // events (untimed). The 10/20/40-bank scopes are the first N of this list.
    let bank_names: Vec<String> = {
        let mut set = BTreeSet::new();
        for ev in chain.events().take(8000) {
            let ev = ev?;
            for s in chain.schemas().iter() {
                if ev.has(s.name()) {
                    set.insert(s.name().to_string());
                }
            }
        }
        set.into_iter().collect()
    };
    let take = |k: usize| bank_names[..k.min(bank_names.len())].to_vec();

    // Read scope grows: 1 bank → 2 banks → 10 → 20 → 40 → all. Each reads
    // *every value of every column* of the named banks, for every event.
    let sel = bench_banks(&chain, iters, &["REC::Event".to_string()])?;
    let full = bench_banks(
        &chain,
        iters,
        &["REC::Particle".to_string(), "REC::Event".to_string()],
    )?;
    let b10 = bench_banks(&chain, iters, &take(10))?;
    let b20 = bench_banks(&chain, iters, &take(20))?;
    let b40 = bench_banks(&chain, iters, &take(40))?;
    let all = bench_banks(&chain, iters, &bank_names)?;

    Ok(Stats {
        file_bytes,
        sel,
        full,
        b10,
        b20,
        b40,
        all,
    })
}

/// Copy the first `cap` events of `src` into `dst` as `Lz4` — a bounded,
/// representative subset. (Re-encoding an 8 GB file seven ways would not fit
/// on disk; capping keeps each variant a few hundred MB.) Reads only as many
/// records as needed to reach `cap`, so it does not scan the whole file.
fn cap_copy(src: &Path, dst: &Path, cap: u64) -> Result<()> {
    let chain = Chain::open(src)?;
    let dict = chain.schemas().clone();
    let mut w = Writer::create(dst)
        .schemas(&dict)
        .compression(Compression::Lz4)
        .build()?;
    for (n, ev) in chain.events().enumerate() {
        if n as u64 >= cap {
            break;
        }
        w.append_raw(ev?.bytes())?;
    }
    w.finish()?;
    Ok(())
}

/// Print a one-line `name/letter` schema summary for `bank` if present.
fn print_schema(chain: &Chain, bank: &str) {
    if let Some(s) = chain.schemas().get(bank) {
        let cols: Vec<String> = s
            .entries()
            .iter()
            .map(|e| format!("{}/{}", e.name, e.ty.letter()))
            .collect();
        eprintln!("  {bank}: {} cols  [{}]", cols.len(), cols.join(" "));
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    // Per-format files land in a temp dir (cleaned up) unless
    // `OXIHIPO_BENCH_KEEP=<dir>` asks to persist them for another tool to read.
    let keep = env::var_os("OXIHIPO_BENCH_KEEP").map(std::path::PathBuf::from);
    let tmp = if keep.is_none() {
        Some(tempfile::tempdir()?)
    } else {
        None
    };
    let dpath: &Path = match (&keep, &tmp) {
        (Some(d), _) => {
            std::fs::create_dir_all(d)?;
            d.as_path()
        }
        (None, Some(t)) => t.path(),
        _ => unreachable!(),
    };
    let base_path = dpath.join("base.hipo");
    let iters: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5);

    // `<file.hipo> [iters] [cap]`  → benchmark the first `cap` events of a real file.
    // `[events] [iters]`           → benchmark synthetic data.
    match args.first() {
        Some(a) if Path::new(a).is_file() => {
            let cap: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(500_000);
            let total = Chain::open(a)?.event_count();
            eprintln!(
                "input: {a}\n  {total} events in file; re-encoding first {} into each format…",
                cap.min(total)
            );
            cap_copy(Path::new(a), &base_path, cap)?;
        }
        other => {
            let n: u64 = other.and_then(|s| s.parse().ok()).unwrap_or(100_000);
            eprintln!("generating {n} synthetic events…");
            generate(&base_path, n)?;
        }
    }

    let base = Chain::open(&base_path)?;
    eprintln!(
        "base: {} events, {} schemas; {iters} timed passes/format (best-of)",
        base.event_count(),
        base.schemas().len()
    );
    print_schema(&base, "REC::Particle");
    print_schema(&base, "REC::Event");
    eprintln!();

    let variants: [(&str, Compression); 6] = [
        ("None", Compression::None),
        ("Lz4", Compression::Lz4),
        ("Lz4Best", Compression::Lz4Best),
        ("Gzip", Compression::Gzip),
        ("Lz4PerBank", Compression::Lz4PerBank),
        ("Lz4PerColumn", Compression::Lz4PerColumn),
    ];

    // Re-encode the base into every format.
    for (name, comp) in &variants {
        base.skim(dpath.join(format!("{name}.hipo")), *comp)?;
    }

    // Time every format first, then print with a size ratio against `None`.
    let stats: Vec<(&str, Stats)> = variants
        .iter()
        .map(|(name, _)| {
            Ok((
                *name,
                bench_one(&dpath.join(format!("{name}.hipo")), iters)?,
            ))
        })
        .collect::<Result<_>>()?;
    let base_bytes = stats
        .iter()
        .find(|(n, _)| *n == "None")
        .map(|(_, s)| s.file_bytes)
        .unwrap_or(0)
        .max(1);

    println!(
        "{:<12} {:>8} {:>6} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "format", "size MB", "ratio", "sel", "full", "10 bk", "20 bk", "40 bk", "all"
    );
    println!("{}", "-".repeat(84));
    for (name, st) in &stats {
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        println!(
            "{:<12} {:>8.1} {:>5.2}x {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8.1}",
            name,
            st.file_bytes as f64 / 1e6,
            st.file_bytes as f64 / base_bytes as f64,
            ms(st.sel),
            ms(st.full),
            ms(st.b10),
            ms(st.b20),
            ms(st.b40),
            ms(st.all),
        );
    }
    println!(
        "\nnote: single-thread, best-of-{iters}, warm cache. `ratio` = file size vs `None`\n\
         (smaller is better). Read columns give ms to read *every value of every column* of\n\
         that many banks, for every event: sel = REC::Event (1 bank); full = REC::Particle +\n\
         REC::Event (2, all their columns); then the first 10 / 20 / 40 populated banks; then\n\
         all of them. Per-bank/column formats pay only for banks/columns touched; whole-record\n\
         formats inflate the whole record regardless of scope."
    );

    if let Some(d) = &keep {
        println!("\nkept per-format files in {}", d.display());
    }
    Ok(())
}
