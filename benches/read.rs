//! Read-path benchmarks.
//!
//! The project's headline claim is read throughput, but the `examples/bench_*`
//! binaries print best-of-N timings to stdout: not statistically rigorous, and
//! nothing compares against a baseline, so a regression can land unnoticed.
//! These are `criterion` benches — run `cargo bench` to record a baseline, then
//! `cargo bench` again on a change to get a per-benchmark verdict.
//!
//! Every bench builds its own fixture in a temp dir, so there is no committed
//! data and no environment assumption. Fixtures are written once per process
//! and shared (via `OnceLock`) across benchmarks.
//!
//! Grouped by what a change is likely to affect:
//!
//! - `scan/<format>` — full sequential read: decode + per-event bank access +
//!   typed column reads. The number a user feels when looping over a file.
//! - `columns/<format>` — the columnar materializer (`read_columns`), the path
//!   behind the Python binding.
//! - `random_access/<format>` — `Chain::event(i)` over scattered indices.
//! - `read_at/<format>` — `read_columns_at`, the path behind Python `entries=`.
//! - `open/<format>` — `Chain::open`: header + dictionary + trailer index.
//! - `schema_parse` — the hand-written dictionary parser.

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use criterion::{Criterion, criterion_group, criterion_main};
use oxihipo::{Chain, Compression, DataType, Dict, Schema, Writer};

const N_EVENTS: usize = 20_000;

fn dict() -> Dict {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [("evno".into(), DataType::Long, 1)],
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
            ("cov".into(), DataType::Float, 3),
        ],
    ));
    d
}

/// Write a fixture with `compression`; realistic multiplicities (1..5 particles
/// per event) and partially-redundant values, so compression ratios are not
/// degenerate.
fn write_fixture(path: &Path, compression: Compression) {
    let mut w = Writer::create(path)
        .schemas(&dict())
        .compression(compression)
        // Several records, so `random_access/scattered` genuinely crosses
        // record boundaries. With one giant record every index would land in
        // the same record and the numbers would flatter the record cache.
        .max_record_events(2_000)
        .build()
        .unwrap();
    for e in 0..N_EVENTS {
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| {
                    r.set("evno", e as i64)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            ev.bank("REC::Particle", |b| {
                for k in 0..(1 + e % 5) {
                    b.row(|r| {
                        r.set("pid", (11 + (e + k) % 7) as i32)?;
                        r.set("px", e as f32 * 0.001 + k as f32)?;
                        r.set("py", e as f32 * -0.002 + k as f32)?;
                        r.set("pz", e as f32 * 0.003 + k as f32)?;
                        r.set("cov", [k as f32, k as f32 + 0.5, -(k as f32)])?;
                        Ok(())
                    })?;
                }
                Ok(())
            })?;
            Ok(())
        })
        .unwrap();
    }
    w.finish().unwrap();
}

/// Formats worth tracking: no compression (pure decode cost), the default LZ4,
/// and the per-column extension (the partial-decode path).
const FORMATS: [(&str, Compression); 3] = [
    ("none", Compression::None),
    ("lz4", Compression::Lz4),
    ("percolumn", Compression::Lz4PerColumn),
];

/// Fixtures live for the whole process; `TempDir` is leaked deliberately so the
/// paths stay valid for every benchmark in the run.
fn fixtures() -> &'static Vec<(&'static str, PathBuf)> {
    static FIXTURES: OnceLock<Vec<(&'static str, PathBuf)>> = OnceLock::new();
    FIXTURES.get_or_init(|| {
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        FORMATS
            .iter()
            .map(|(name, comp)| {
                let p = dir.path().join(format!("bench_{name}.hipo"));
                write_fixture(&p, *comp);
                (*name, p)
            })
            .collect()
    })
}

/// Full sequential read: what a user's analysis loop actually costs.
fn bench_scan(c: &mut Criterion) {
    let mut g = c.benchmark_group("scan");
    for (name, path) in fixtures() {
        let chain = Chain::open(path).unwrap();
        let schema = chain.schemas().require("REC::Particle").unwrap();
        let h_pid = schema.handle::<i32>("pid").unwrap();
        let h_px = schema.handle::<f32>("px").unwrap();
        g.throughput(criterion::Throughput::Elements(chain.event_count()));
        g.bench_function(*name, |b| {
            b.iter(|| {
                let mut pid_sum = 0i64;
                let mut px_sum = 0f64;
                for ev in chain.events() {
                    let ev = ev.unwrap();
                    if let Some(bank) = ev.bank("REC::Particle") {
                        for &v in bank.read(h_pid).iter() {
                            pid_sum += v as i64;
                        }
                        for &v in bank.read(h_px).iter() {
                            px_sum += v as f64;
                        }
                    }
                }
                black_box((pid_sum, px_sum))
            })
        });
    }
    g.finish();
}

/// The columnar materializer — the path the Python binding runs on.
fn bench_columns(c: &mut Criterion) {
    let mut g = c.benchmark_group("columns");
    for (name, path) in fixtures() {
        let chain = Chain::open(path).unwrap();
        g.throughput(criterion::Throughput::Elements(chain.event_count()));
        g.bench_function(*name, |b| {
            b.iter(|| {
                let cols = chain
                    .read_columns(&[("REC::Particle", &["pid", "px"][..])], None, 1)
                    .unwrap();
                black_box(cols.len())
            })
        });
    }
    g.finish();
}

/// Random access over scattered indices — a different decode route (one record
/// per call for the classic formats, lazy per-bank for the extensions).
fn bench_random_access(c: &mut Criterion) {
    let mut g = c.benchmark_group("random_access");
    for (name, path) in fixtures() {
        let chain = Chain::open(path).unwrap();
        let n = chain.event_count();
        // Two access patterns, because they stress different things:
        //   scattered — a prime stride, so almost every call lands in a new
        //               record: the worst case, and what a record cache cannot
        //               help with.
        //   sorted    — an ascending slice of indices (what reading a list of
        //               interesting events actually looks like), where
        //               consecutive hits share a record.
        let scattered: Vec<u64> = (0..256u64).map(|i| (i * 7919) % n).collect();
        let sorted: Vec<u64> = (0..256u64).map(|i| i * 3).filter(|&i| i < n).collect();
        for (pattern, idx) in [("scattered", &scattered), ("sorted", &sorted)] {
            g.throughput(criterion::Throughput::Elements(idx.len() as u64));
            g.bench_function(format!("{name}/{pattern}"), |b| {
                b.iter(|| {
                    let mut acc = 0i64;
                    for &i in idx {
                        if let Some(ev) = chain.event(i)
                            && let Some(bank) = ev.bank("REC::Event")
                        {
                            acc += bank.get::<i64>("evno", 0);
                        }
                    }
                    black_box(acc)
                })
            });
        }
    }
    g.finish();
}

/// `read_columns_at` over the same two patterns as `random_access`.
///
/// Separate from `random_access` because it is a different code path — the one
/// the Python `entries=` argument runs on — and because it is the path that
/// replays entries through `Chain::event`'s single-slot record cache. The
/// scattered case is the one to watch: every index landing in a new record
/// costs a full record decode.
fn bench_read_columns_at(c: &mut Criterion) {
    let mut g = c.benchmark_group("read_at");
    for (name, path) in fixtures() {
        let chain = Chain::open(path).unwrap();
        let n = chain.event_count();
        let scattered: Vec<u64> = (0..256u64).map(|i| (i * 7919) % n).collect();
        let sorted: Vec<u64> = (0..256u64).map(|i| i * 3).filter(|&i| i < n).collect();
        for (pattern, idx) in [("scattered", &scattered), ("sorted", &sorted)] {
            g.throughput(criterion::Throughput::Elements(idx.len() as u64));
            g.bench_function(format!("{name}/{pattern}"), |b| {
                b.iter(|| {
                    let cols = chain
                        .read_columns_at(&[("REC::Particle", &["pid", "px"][..])], idx)
                        .unwrap();
                    black_box(cols.len())
                })
            });
        }
    }
    g.finish();
}

/// `Chain::open` — the per-file open cost: file header, dictionary record, and
/// the trailer index, each a separate positioned read. Nothing else measures
/// it, so a change to the open sequence has no baseline to be judged against.
fn bench_open(c: &mut Criterion) {
    let mut g = c.benchmark_group("open");
    for (name, path) in fixtures() {
        g.bench_function(*name, |b| {
            b.iter(|| black_box(Chain::open(black_box(path)).unwrap().event_count()))
        });
    }
    g.finish();
}

/// Schema-text parsing: cheap per call, but it runs once per dictionary entry at
/// every file open, and it is hand-written.
fn bench_schema_parse(c: &mut Criterion) {
    let text = "{REC::Particle/300/31}{pid/I,px/F,py/F,pz/F,vx/F,vy/F,vz/F,\
                chi2pid/F,status/S,charge/B,beta/F,cov/F#15}";
    c.bench_function("schema_parse", |b| {
        b.iter(|| black_box(Schema::parse_text(black_box(text)).unwrap()))
    });
}

criterion_group!(
    benches,
    bench_scan,
    bench_columns,
    bench_random_access,
    bench_read_columns_at,
    bench_open,
    bench_schema_parse
);
criterion_main!(benches);
