//! End-to-end tests for the `Lz4PerColumn` format: it must decode
//! **identically** to the row-contiguous (`Lz4`) and by-bank
//! (`Lz4PerBank`) formats — same typed values and same reassembled bank
//! bytes — while splitting every column into its own stream.

use oxihipo::{Chain, Compression, DataType, Dict, Filter, Result, Schema, Writer};

fn sample_dict() -> Dict {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        1,
        [
            ("pid".into(), DataType::Int, 1),
            ("px".into(), DataType::Float, 1),
            ("cov".into(), DataType::Float, 3), // array column (F#3)
            ("status".into(), DataType::Short, 1),
        ],
    ));
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [
            ("evno".into(), DataType::Long, 1),
            ("beamE".into(), DataType::Float, 1),
        ],
    ));
    d
}

/// Write `n` events with a mix of row counts (including empty
/// `REC::Particle`) using `compression`.
fn write_file(path: &std::path::Path, n: i64, compression: Compression) -> Result<()> {
    let mut w = Writer::create(path)
        .schemas(&sample_dict())
        .compression(compression)
        .max_record_events(17) // several records, exercises the directory
        .build()?;
    for evno in 0..n {
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| {
                    r.set("evno", evno)?;
                    r.set("beamE", 10.6_f32)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            let nrows = (evno % 4) as i32; // 0,1,2,3 rows — 0 exercises empty banks
            if nrows > 0 {
                ev.bank("REC::Particle", |b| {
                    for i in 0..nrows {
                        b.row(|r| {
                            r.set("pid", 11 + i + (evno as i32) * 100)?;
                            r.set("px", (evno as f32) + i as f32 * 0.25)?;
                            r.set("cov", [i as f32, i as f32 + 0.5, -(i as f32)])?;
                            r.set("status", (i as i16) - 1)?;
                            Ok(())
                        })?;
                    }
                    Ok(())
                })?;
            }
            Ok(())
        })?;
    }
    w.finish()?;
    Ok(())
}

type Row = (i32, f32, [f32; 3], i16);

/// Read back the typed values of every event (order-independent of the
/// on-disk bank layout).
fn collect_values(path: &std::path::Path) -> Vec<(i64, Vec<Row>)> {
    let chain = Chain::open(path).unwrap();
    let mut out = Vec::new();
    for ev in chain.events().map(Result::unwrap) {
        let evno = ev.bank("REC::Event").unwrap().col::<i64>("evno").unwrap()[0];
        let mut parts = Vec::new();
        if let Some(p) = ev.bank("REC::Particle") {
            let pid = p.col::<i32>("pid").unwrap();
            let px = p.col::<f32>("px").unwrap();
            let status = p.col::<i16>("status").unwrap();
            for r in 0..p.rows() {
                let cov: [f32; 3] = p.get("cov", r);
                parts.push((pid[r as usize], px[r as usize], cov, status[r as usize]));
            }
        }
        out.push((evno, parts));
    }
    out
}

/// Per event, the sorted `(group, item) -> raw bank bytes` list.
type StructMap = Vec<Vec<((u16, u8), Vec<u8>)>>;

/// Walk `ev.structures()` for every event — proves whole-event reassembly.
fn structures_map(path: &std::path::Path) -> StructMap {
    let chain = Chain::open(path).unwrap();
    let mut out = Vec::new();
    for ev in chain.events().map(Result::unwrap) {
        let mut m: Vec<((u16, u8), Vec<u8>)> = ev
            .structures()
            .map(|(h, d)| ((h.group, h.item), d.to_vec()))
            .collect();
        m.sort_by_key(|(k, _)| *k);
        out.push(m);
    }
    out
}

#[test]
fn per_column_matches_other_formats() {
    let dir = tempfile::tempdir().unwrap();
    let n = 200;
    let lz4 = dir.path().join("lz4.hipo");
    let bybank = dir.path().join("bybank.hipo");
    let percol = dir.path().join("percol.hipo");
    write_file(&lz4, n, Compression::Lz4).unwrap();
    write_file(&bybank, n, Compression::Lz4PerBank).unwrap();
    write_file(&percol, n, Compression::Lz4PerColumn).unwrap();

    let ref_vals = collect_values(&lz4);
    assert_eq!(ref_vals.len(), n as usize);
    assert_eq!(collect_values(&bybank), ref_vals, "ByBank ≠ Lz4");
    assert_eq!(collect_values(&percol), ref_vals, "PerColumn ≠ Lz4");

    // Whole-event reassembly: PerColumn's structures() must rebuild the
    // exact same per-bank column-major bytes as the contiguous format.
    assert_eq!(
        structures_map(&percol),
        structures_map(&lz4),
        "PerColumn reassembled bank bytes ≠ Lz4"
    );
}

#[test]
fn for_each_column_matches_per_event() {
    let dir = tempfile::tempdir().unwrap();
    let n = 200i64;
    let percol = dir.path().join("pc.hipo");
    let lz4 = dir.path().join("lz4.hipo");
    write_file(&percol, n, Compression::Lz4PerColumn).unwrap();
    write_file(&lz4, n, Compression::Lz4).unwrap();

    // Reference reductions via ordinary per-event reads.
    let ref_vals = collect_values(&lz4);
    let ref_px: f64 = ref_vals
        .iter()
        .flat_map(|(_, p)| p.iter().map(|(_, px, _, _)| *px as f64))
        .sum();
    let ref_pid: i64 = ref_vals
        .iter()
        .flat_map(|(_, p)| p.iter().map(|(pid, _, _, _)| *pid as i64))
        .sum();
    let ref_rows: usize = ref_vals.iter().map(|(_, p)| p.len()).sum();
    let ref_evno: i64 = (0..n).sum();

    // The column-major scan must reduce to the same totals on the
    // per-column format (fast path) *and* a whole-record format (fallback).
    for path in [&percol, &lz4] {
        let chain = Chain::open(path).unwrap();

        let mut px_sum = 0f64;
        chain
            .for_each_column::<f32, _>("REC::Particle", "px", |v| {
                px_sum += v.iter().map(|&x| x as f64).sum::<f64>();
            })
            .unwrap();
        assert!((px_sum - ref_px).abs() < 1e-2, "px sum @ {path:?}");

        let mut pid_sum = 0i64;
        chain
            .for_each_column::<i32, _>("REC::Particle", "pid", |v| {
                pid_sum += v.iter().map(|&x| x as i64).sum::<i64>();
            })
            .unwrap();
        assert_eq!(pid_sum, ref_pid, "pid sum @ {path:?}");

        // Array column: one [f32; 3] per row.
        let mut cov_rows = 0usize;
        chain
            .for_each_column::<[f32; 3], _>("REC::Particle", "cov", |v| cov_rows += v.len())
            .unwrap();
        assert_eq!(cov_rows, ref_rows, "cov row count @ {path:?}");

        let mut evno_sum = 0i64;
        chain
            .for_each_column::<i64, _>("REC::Event", "evno", |v| evno_sum += v.iter().sum::<i64>())
            .unwrap();
        assert_eq!(evno_sum, ref_evno, "evno sum @ {path:?}");
    }
}

#[test]
fn per_column_filter_and_random_access() {
    let dir = tempfile::tempdir().unwrap();
    let n = 120;
    let path = dir.path().join("pc.hipo");
    write_file(&path, n, Compression::Lz4PerColumn).unwrap();

    // Random access (event(i)) returns the right event.
    let chain = Chain::open(&path).unwrap();
    assert_eq!(chain.event_count(), n as u64);
    for i in [0u64, 1, 5, 63, (n as u64) - 1] {
        let ev = chain.event(i).unwrap();
        let evno = ev.bank("REC::Event").unwrap().col::<i64>("evno").unwrap()[0];
        assert_eq!(evno, i as i64);
    }

    // Filtering on a required bank keeps only events that carry it.
    let filtered = Chain::open(&path)
        .unwrap()
        .with_filter(Filter::require(["REC::Particle"]))
        .unwrap();
    for ev in filtered.events().map(Result::unwrap) {
        assert!(ev.has("REC::Particle"));
        // evno % 4 == 0 events have no particles and must be filtered out.
        let evno = ev.bank("REC::Event").unwrap().col::<i64>("evno").unwrap()[0];
        assert_ne!(evno % 4, 0);
    }
}
