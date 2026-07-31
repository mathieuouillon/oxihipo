//! High-level, end-to-end coverage across **every** compression format.
//!
//! The other round-trip tests each exercise one or two formats; this one asserts
//! that all six write→read paths — and a cross-format `skim` between them —
//! decode to byte-identical data, including a fixed-length array column (`cov`,
//! which the per-column encoder stores specially). If any format's writer or
//! reader corrupts a value, or a `skim` re-encode changes the data, one of these
//! fails.

use oxihipo::{Chain, Compression, DataType, Dict, Result, Schema, Writer};

/// Every writable compression format, by name.
const FORMATS: &[(&str, Compression)] = &[
    ("None", Compression::None),
    ("Lz4", Compression::Lz4),
    ("Lz4Best", Compression::Lz4Best),
    ("Gzip", Compression::Gzip),
    ("Lz4PerBank", Compression::Lz4PerBank),
    ("Lz4PerColumn", Compression::Lz4PerColumn),
];

fn dict() -> Dict {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [
            ("evno".into(), DataType::Long, 1),
            ("beamE".into(), DataType::Float, 1),
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
            ("charge".into(), DataType::Byte, 1),
            // A fixed-length array column — the per-column encoder handles these
            // on a separate path, so it's worth carrying through every format.
            ("cov".into(), DataType::Float, 3),
        ],
    ));
    d
}

/// One event's data, in a form that compares across formats regardless of
/// codec. Floats are compared by bit pattern (exact round-trip is required).
#[derive(Debug, PartialEq, Eq, Clone)]
struct EventSnap {
    evno: i64,
    beam_bits: u32,
    // (pid, px_bits, py_bits, charge, [cov0,cov1,cov2] bits)
    parts: Vec<(i32, u32, u32, i8, [u32; 3])>,
}

const N_EVENTS: i64 = 120;

fn n_parts(evno: i64) -> i32 {
    (evno % 5) as i32 // 0..=4 — exercises empty and non-empty particle banks
}

fn write_file(path: &std::path::Path, compression: Compression) -> Result<()> {
    let mut w = Writer::create(path)
        .schemas(&dict())
        .compression(compression)
        .build()?;
    for evno in 0..N_EVENTS {
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| {
                    r.set("evno", evno)?;
                    r.set("beamE", 10.6_f32 + evno as f32 * 0.01)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            ev.bank("REC::Particle", |b| {
                for i in 0..n_parts(evno) {
                    b.row(|r| {
                        r.set("pid", 11 + i)?;
                        r.set("px", i as f32 * 0.1 - 1.0)?;
                        r.set("py", -(i as f32) * 0.25)?;
                        r.set("charge", (i as i8) - 1)?;
                        r.set("cov", [i as f32, i as f32 + 0.5, -(i as f32)])?;
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

/// Read a whole file into a comparable snapshot.
fn read_snapshot(path: &std::path::Path) -> Result<Vec<EventSnap>> {
    let chain = Chain::open(path)?;
    let mut out = Vec::new();
    for ev in chain.events() {
        let ev = ev?;
        let evb = ev.bank("REC::Event").expect("REC::Event present");
        let evno = evb.get::<i64>("evno", 0);
        let beam_bits = evb.get::<f32>("beamE", 0).to_bits();

        let mut parts = Vec::new();
        if let Some(pb) = ev.bank("REC::Particle") {
            for r in 0..pb.rows() {
                let cov = pb.array_at::<f32>("cov", r).unwrap();
                parts.push((
                    pb.get::<i32>("pid", r),
                    pb.get::<f32>("px", r).to_bits(),
                    pb.get::<f32>("py", r).to_bits(),
                    pb.get::<i8>("charge", r),
                    [cov[0].to_bits(), cov[1].to_bits(), cov[2].to_bits()],
                ));
            }
        }
        out.push(EventSnap {
            evno,
            beam_bits,
            parts,
        });
    }
    Ok(out)
}

/// The reference data every format must reproduce, computed independently of
/// the reader so a shared decode bug can't hide it.
fn expected() -> Vec<EventSnap> {
    (0..N_EVENTS)
        .map(|evno| EventSnap {
            evno,
            beam_bits: (10.6_f32 + evno as f32 * 0.01).to_bits(),
            parts: (0..n_parts(evno))
                .map(|i| {
                    (
                        11 + i,
                        (i as f32 * 0.1 - 1.0).to_bits(),
                        (-(i as f32) * 0.25).to_bits(),
                        (i as i8) - 1,
                        [
                            (i as f32).to_bits(),
                            (i as f32 + 0.5).to_bits(),
                            (-(i as f32)).to_bits(),
                        ],
                    )
                })
                .collect(),
        })
        .collect()
}

#[test]
fn every_format_preserves_data() {
    let dir = tempfile::tempdir().unwrap();
    let want = expected();
    for (name, comp) in FORMATS {
        let path = dir.path().join(format!("{name}.hipo"));
        write_file(&path, *comp).unwrap();
        let got = read_snapshot(&path).unwrap();
        assert_eq!(got.len(), N_EVENTS as usize, "{name}: event count");
        assert_eq!(got, want, "{name}: decoded data differs from the source");
    }
}

#[test]
fn cross_format_skim_preserves_data() {
    // Write once (Lz4), then re-encode into every format via `skim` and check
    // the data survives the decode → re-encode → decode round-trip.
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.hipo");
    write_file(&src, Compression::Lz4).unwrap();
    let want = expected();

    for (name, comp) in FORMATS {
        let out = dir.path().join(format!("skim-{name}.hipo"));
        let summary = Chain::open(&src).unwrap().skim(&out, *comp).unwrap();
        assert_eq!(summary.events, N_EVENTS as u64, "{name}: skim event count");
        assert_eq!(
            read_snapshot(&out).unwrap(),
            want,
            "{name}: skim changed data"
        );
    }
}

#[test]
fn every_format_supports_partial_read() {
    // Reading a single bank must work — and return the right values — on every
    // format, including the by-bank / per-column ones that inflate lazily.
    let dir = tempfile::tempdir().unwrap();
    let want: i64 = (0..N_EVENTS).sum(); // Σ evno
    for (name, comp) in FORMATS {
        let path = dir.path().join(format!("{name}.hipo"));
        write_file(&path, *comp).unwrap();

        let chain = Chain::open(&path).unwrap();
        let mut sum = 0i64;
        for ev in chain.events() {
            // Touch only REC::Event; REC::Particle stays untouched (and, for the
            // lazy formats, uninflated) without breaking the read.
            sum += ev
                .unwrap()
                .bank("REC::Event")
                .unwrap()
                .get::<i64>("evno", 0);
        }
        assert_eq!(sum, want, "{name}: partial single-bank read");
    }
}

#[test]
fn for_each_column_works_on_every_format() {
    // The column-major sweep had a format-shaped hole: its fallback comment
    // claimed to cover "Bytes / ByBank / chunked", but the fallback calls
    // `decode_record_into`, which expects a whole-record payload. An
    // `Lz4PerBank` record is per-bank streams plus a directory, so the LZ4 call
    // failed outright — `lz4 decompress failed`. Every other format worked, and
    // the existing test in tests/per_column.rs only ever tried `Lz4PerColumn`
    // and `Lz4`, so nothing noticed.
    //
    // Sum the same column through `for_each_column` and through ordinary
    // per-event reads, on all six formats, and require they agree.
    let dir = tempfile::tempdir().unwrap();
    let want: i64 = (0..N_EVENTS).sum(); // Σ evno

    for (name, comp) in FORMATS {
        let path = dir.path().join(format!("fec-{name}.hipo"));
        write_file(&path, *comp).unwrap();
        let chain = Chain::open(&path).unwrap();

        let mut sum = 0i64;
        chain
            .for_each_column::<i64, _>("REC::Event", "evno", |v| sum += v.iter().sum::<i64>())
            .unwrap_or_else(|e| panic!("{name}: for_each_column failed: {e}"));
        assert_eq!(sum, want, "{name}: evno sum");

        // A jagged bank too: the by-bank path reads per event out of one
        // stream, so a bank with a varying row count is the case that would
        // mis-slice if the byte ranges were wrong rather than merely absent.
        let mut rows = 0usize;
        let mut px = 0f64;
        chain
            .for_each_column::<f32, _>("REC::Particle", "px", |v| {
                rows += v.len();
                px += v.iter().map(|&x| x as f64).sum::<f64>();
            })
            .unwrap_or_else(|e| panic!("{name}: for_each_column(px) failed: {e}"));

        // Reference: the same reduction through per-event reads.
        let mut ref_rows = 0usize;
        let mut ref_px = 0f64;
        for ev in chain.events() {
            let ev = ev.unwrap();
            if let Some(b) = ev.bank("REC::Particle") {
                let h = chain
                    .schemas()
                    .require("REC::Particle")
                    .unwrap()
                    .handle::<f32>("px")
                    .unwrap();
                ref_rows += b.rows() as usize;
                ref_px += b.read(h).iter().map(|&x| x as f64).sum::<f64>();
            }
        }
        assert_eq!(rows, ref_rows, "{name}: px row count");
        assert!((px - ref_px).abs() < 1e-3, "{name}: px sum");
    }
}

/// `size()` must be codec-independent, and must not decompress to find out.
///
/// It used to route the split codecs through `bytes()`, which reassembles the
/// whole event — inflating every bank stream in the record to answer a
/// question the directory already holds. Measured over 20,000 events that was
/// 5.7 Mev/s against 19.0 for the directory route on `Lz4PerBank` (3.3x) and
/// 5.6 against 19.6 on `Lz4PerColumn` (3.5x).
///
/// The ground truth here is `Compression::None`, where an event is a
/// contiguous span and `size()` is `end - start` — a number no bank-summing
/// loop is involved in producing. Every other codec must agree with it
/// event-for-event, which catches a summing bug that a self-consistent
/// comparison (`size()` against `ctx().size()`) would not: those two share the
/// implementation now.
#[test]
fn size_agrees_across_codecs_and_matches_the_uncompressed_span() {
    use oxihipo::{Chain, Compression, DataType, Dict, Schema, Writer};

    let dir = tempfile::tempdir().unwrap();

    // Several banks, varying row counts, and one bank that is absent from
    // some events — so the "sum the *present* banks" loop is exercised
    // rather than a fixed total.
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
        1,
        [
            ("pid".into(), DataType::Int, 1),
            ("px".into(), DataType::Float, 1),
            ("charge".into(), DataType::Byte, 1),
            ("status".into(), DataType::Short, 1),
        ],
    ));
    d.add(Schema::from_columns(
        "REC::Sparse",
        400,
        1,
        [("v".into(), DataType::Double, 1)],
    ));

    let write = |path: &std::path::Path, c: Compression| {
        let mut w = Writer::create(path)
            .schemas(&d)
            .compression(c)
            .max_record_events(23)
            .build()
            .unwrap();
        for i in 0..200i64 {
            w.event(|ev| {
                ev.bank("REC::Event", |b| {
                    b.row(|r| r.set("evno", i).map(|_| ()))?;
                    Ok(())
                })?;
                ev.bank("REC::Particle", |b| {
                    for r_i in 0..=(i % 5) {
                        b.row(|r| {
                            r.set("pid", (11 + r_i) as i32)?;
                            r.set("px", i as f32)?;
                            r.set("charge", (r_i % 3 - 1) as i8)?;
                            r.set("status", -(i as i16 % 7))?;
                            Ok(())
                        })?;
                    }
                    Ok(())
                })?;
                // Present on only a third of events.
                if i % 3 == 0 {
                    ev.bank("REC::Sparse", |b| {
                        b.row(|r| r.set("v", i as f64).map(|_| ()))?;
                        Ok(())
                    })?;
                }
                Ok(())
            })
            .unwrap();
        }
        w.finish().unwrap();
    };

    let sizes = |path: &std::path::Path| -> Vec<u32> {
        Chain::open(path)
            .unwrap()
            .events()
            .map(|ev| ev.unwrap().size())
            .collect()
    };

    let base_path = dir.path().join("none.hipo");
    write(&base_path, Compression::None);
    let baseline = sizes(&base_path);
    assert_eq!(baseline.len(), 200);
    // Sanity: the sparse bank really does move the size around, so an
    // implementation returning a constant would not pass by accident.
    assert!(
        baseline
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            > 5,
        "fixture is degenerate: sizes are {:?}",
        &baseline[..8]
    );

    for (name, c) in [
        ("lz4", Compression::Lz4),
        ("perbank", Compression::Lz4PerBank),
        ("percolumn", Compression::Lz4PerColumn),
    ] {
        let p = dir.path().join(format!("{name}.hipo"));
        write(&p, c);
        assert_eq!(sizes(&p), baseline, "size() disagreed for {name}");
    }

    // And the split codecs must reach that answer without decompressing:
    // `ctx()` is O(1) and `ctx().size()` sums the directory, so the two
    // routes must agree too.
    let p = dir.path().join("perbank.hipo");
    let via_ctx: Vec<u32> = Chain::open(&p)
        .unwrap()
        .events()
        .map(|ev| ev.unwrap().ctx().size())
        .collect();
    assert_eq!(via_ctx, baseline);
}

/// `for_each_column` must refuse a filtered chain rather than sweep past it.
///
/// It walks the record index and casts whole column streams, so a filter would
/// simply not be consulted — the caller would get every value in the file and
/// no indication that the filter had been dropped. The old behaviour was
/// documented ("it is a trap"), which is not the same as being safe.
///
/// The test pins all three halves of the contract: unfiltered still sweeps,
/// filtered errors, and the recommended alternative returns the *filtered*
/// answer — so the error message is pointing somewhere that actually works.
#[test]
fn for_each_column_refuses_a_filtered_chain() {
    use oxihipo::{Chain, Compression, DataType, Dict, Filter, HipoError, Schema, Writer};

    let dir = tempfile::tempdir().unwrap();

    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        1,
        [("pid".into(), DataType::Int, 1)],
    ));
    d.add(Schema::from_columns(
        "RAW::tag",
        500,
        1,
        [("v".into(), DataType::Int, 1)],
    ));

    for (name, compression) in [
        ("none", Compression::None),
        ("lz4", Compression::Lz4),
        ("perbank", Compression::Lz4PerBank),
        ("percolumn", Compression::Lz4PerColumn),
    ] {
        let path = dir.path().join(format!("f_{name}.hipo"));
        let mut w = Writer::create(&path)
            .schemas(&d)
            .compression(compression)
            .max_record_events(17)
            .build()
            .unwrap();
        for i in 0..200i32 {
            w.event(|ev| {
                ev.bank("REC::Particle", |b| {
                    b.row(|r| r.set("pid", i).map(|_| ()))?;
                    Ok(())
                })?;
                // Only three quarters of events carry the tag bank.
                if i % 4 != 0 {
                    ev.bank("RAW::tag", |b| {
                        b.row(|r| r.set("v", i).map(|_| ()))?;
                        Ok(())
                    })?;
                }
                Ok(())
            })
            .unwrap();
        }
        w.finish().unwrap();

        // Unfiltered: still sweeps everything, unchanged.
        let plain = Chain::open(&path).unwrap();
        let mut n = 0usize;
        let mut sum = 0i64;
        plain
            .for_each_column::<i32, _>("REC::Particle", "pid", |vals| {
                n += vals.len();
                sum += vals.iter().map(|&v| i64::from(v)).sum::<i64>();
            })
            .unwrap();
        assert_eq!(n, 200, "{name}: unfiltered sweep");
        assert_eq!(sum, (0..200i64).sum::<i64>(), "{name}: unfiltered sum");

        // Filtered: refuses, and names both the bank and the column so the
        // message is actionable.
        let filtered = Chain::open(&path)
            .unwrap()
            .with_filter(Filter::require(["RAW::tag"]))
            .unwrap();
        let err = filtered
            .for_each_column::<i32, _>("REC::Particle", "pid", |_| {
                panic!("{name}: must not visit anything on a filtered chain")
            })
            .expect_err("a filtered chain must be refused, not swept");
        assert!(
            matches!(err, HipoError::FilterIgnoredByColumnSweep { .. }),
            "{name}: wrong error variant: {err}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("REC::Particle") && msg.contains("pid"),
            "{name}: {msg}"
        );
        assert!(
            msg.contains("read_columns"),
            "{name}: no alternative offered: {msg}"
        );

        // And the alternative the message points at does honour the filter —
        // 150 of 200 events, so the two answers genuinely differ.
        let cols = filtered
            .read_columns(&[("REC::Particle", &["pid"][..])], None, 1)
            .unwrap();
        assert_eq!(cols.len(), 1);
        assert_eq!(
            cols[0].total_rows(),
            150,
            "{name}: read_columns should see only the events the filter kept"
        );
    }
}
