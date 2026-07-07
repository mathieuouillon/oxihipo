//! Integration tests for `Chain::read_columns` — the bulk columnar
//! materializer behind the planned Python binding.
//!
//! The central test writes one *logical* dataset under every storage format
//! (`None`, `Lz4`, `Lz4ByBank`, `Lz4PerColumn`) and asserts that
//! `read_columns` produces byte-identical offsets + content for all of them,
//! matching an independently computed expectation. The rest cover the
//! alignment/absent-bank/filter/range/error contracts.

use oxihipo::{Chain, ColumnData, Compression, DataType, Dict, Schema, Writer};

// --- ground-truth data model (used by BOTH the writer and the expectations) --

/// Particle rows in event `i`: 0,1,2,3,0,1,2,3,… so events 0,4,8,… are empty.
fn particle_rows(i: usize) -> usize {
    i % 4
}
fn pid(i: usize, r: usize) -> i32 {
    (i * 100 + r) as i32
}
fn px(i: usize, r: usize) -> f32 {
    i as f32 + r as f32 * 0.1
}
fn cov(i: usize, r: usize) -> [f32; 3] {
    let b = pid(i, r) as f32;
    [b, b + 0.5, b + 0.25]
}
fn evno(i: usize) -> i64 {
    1000 + i as i64
}

fn dict() -> Dict {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        1,
        [
            ("pid".into(), DataType::Int, 1),
            ("px".into(), DataType::Float, 1),
            ("cov".into(), DataType::Float, 3), // array column, inner_len 3
        ],
    ));
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [("evno".into(), DataType::Long, 1)],
    ));
    // Present in the dictionary but never written — exercises the
    // "bank absent from every record" path.
    d.add(Schema::from_columns(
        "REC::Calorimeter",
        300,
        40,
        [("energy".into(), DataType::Float, 1)],
    ));
    d
}

/// Write `n` events of the model under `compression`. `REC::Event` is present
/// in every event; `REC::Particle` is written only when it has ≥1 row.
fn write_file(path: &std::path::Path, compression: Compression, n: usize) {
    let d = dict();
    let mut w = Writer::create(path)
        .schemas(&d)
        .compression(compression)
        .max_record_events(3) // force several records → cross-record + parallel
        .build()
        .unwrap();
    for i in 0..n {
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| {
                    r.set("evno", evno(i))?;
                    Ok(())
                })?;
                Ok(())
            })?;
            let rows = particle_rows(i);
            if rows > 0 {
                ev.bank("REC::Particle", |b| {
                    for r in 0..rows {
                        b.row(|rw| {
                            rw.set("pid", pid(i, r))?;
                            rw.set("px", px(i, r))?;
                            rw.set("cov", cov(i, r))?;
                            Ok(())
                        })?;
                    }
                    Ok(())
                })?;
            }
            Ok(())
        })
        .unwrap();
    }
    w.finish().unwrap();
}

const FORMATS: [Compression; 4] = [
    Compression::None,
    Compression::Lz4,
    Compression::Lz4ByBank,
    Compression::Lz4PerColumn,
];

// --- expectation builders over the SURVIVING event set -----------------------

struct ParticleExpect {
    offsets: Vec<i64>,
    pid: Vec<i32>,
    px: Vec<f32>,
    cov: Vec<f32>, // flat, 3 per row
}

fn particle_expect(events: &[usize]) -> ParticleExpect {
    let mut e = ParticleExpect {
        offsets: vec![0],
        pid: vec![],
        px: vec![],
        cov: vec![],
    };
    let mut run = 0i64;
    for &i in events {
        let rows = particle_rows(i);
        run += rows as i64;
        e.offsets.push(run);
        for r in 0..rows {
            e.pid.push(pid(i, r));
            e.px.push(px(i, r));
            e.cov.extend_from_slice(&cov(i, r));
        }
    }
    e
}

fn event_expect(events: &[usize]) -> (Vec<i64>, Vec<i64>) {
    let offsets: Vec<i64> = (0..=events.len() as i64).collect(); // one row each
    let evnos: Vec<i64> = events.iter().map(|&i| evno(i)).collect();
    (offsets, evnos)
}

fn tmp(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join(name);
    (dir, p)
}

#[test]
fn read_columns_matches_across_all_formats() {
    let n = 12;
    let all: Vec<usize> = (0..n).collect();
    let pexp = particle_expect(&all);
    let (eoff, evnos) = event_expect(&all);

    for fmt in FORMATS {
        let (_dir, p) = tmp("f.hipo");
        write_file(&p, fmt, n);
        let chain = Chain::open(&p).unwrap();

        let bufs = chain
            .read_columns(
                &[
                    ("REC::Particle", &["pid", "px", "cov"]),
                    ("REC::Event", &["evno"]),
                ],
                None,
                1,
            )
            .unwrap();
        assert_eq!(bufs.len(), 2, "{fmt:?}");

        // ---- REC::Particle ----
        let part = &bufs[0];
        assert_eq!(part.bank, "REC::Particle");
        assert_eq!(part.offsets, pexp.offsets, "particle offsets ({fmt:?})");
        assert_eq!(part.columns[0].name, "pid");
        assert_eq!(part.columns[0].inner_len, 1);
        assert_eq!(
            part.columns[0].data,
            ColumnData::I32(pexp.pid.clone()),
            "pid ({fmt:?})"
        );
        assert_eq!(
            part.columns[1].data,
            ColumnData::F32(pexp.px.clone()),
            "px ({fmt:?})"
        );
        // Array column: inner_len 3, flat content = total_rows * 3.
        assert_eq!(part.columns[2].name, "cov");
        assert_eq!(part.columns[2].inner_len, 3);
        assert_eq!(
            part.columns[2].data,
            ColumnData::F32(pexp.cov.clone()),
            "cov ({fmt:?})"
        );
        assert_eq!(
            part.columns[2].data.len() as i64,
            part.total_rows() * 3,
            "cov flat length == rows*inner_len ({fmt:?})"
        );

        // ---- REC::Event ----
        let evb = &bufs[1];
        assert_eq!(evb.bank, "REC::Event");
        assert_eq!(evb.offsets, eoff, "event offsets ({fmt:?})");
        assert_eq!(
            evb.columns[0].data,
            ColumnData::I64(evnos.clone()),
            "evno ({fmt:?})"
        );
    }
}

#[test]
fn absent_events_give_empty_sublists() {
    // Events 0, 4, 8 have zero particles → offset deltas of 0 there.
    let (_dir, p) = tmp("f.hipo");
    write_file(&p, Compression::Lz4PerColumn, 12);
    let chain = Chain::open(&p).unwrap();
    let bufs = chain
        .read_columns(&[("REC::Particle", &["pid"])], None, 1)
        .unwrap();
    let off = &bufs[0].offsets;
    assert_eq!(off.len(), 13); // 12 events + 1
    // A zero-particle event ⇒ no advance in offsets.
    assert_eq!(off[0], off[1]); // event 0 empty
    assert_eq!(off[4], off[5]); // event 4 empty
    assert_eq!(off[8], off[9]); // event 8 empty
    assert!(off.windows(2).all(|w| w[0] <= w[1]), "monotonic");
}

#[test]
fn never_written_bank_is_all_zero_offsets() {
    let (_dir, p) = tmp("f.hipo");
    write_file(&p, Compression::Lz4ByBank, 6);
    let chain = Chain::open(&p).unwrap();
    // REC::Calorimeter is in the dict but never written to any record.
    let bufs = chain
        .read_columns(&[("REC::Calorimeter", &["energy"])], None, 0)
        .unwrap();
    assert_eq!(bufs[0].offsets, vec![0; 7]); // 6 events, all empty
    assert!(bufs[0].columns[0].data.is_empty());
    assert_eq!(bufs[0].total_rows(), 0);
}

#[test]
fn range_selects_a_subset() {
    let n = 12;
    for fmt in FORMATS {
        let (_dir, p) = tmp("f.hipo");
        write_file(&p, fmt, n);
        let chain = Chain::open(&p).unwrap();
        // Global events [4, 9).
        let sel: Vec<usize> = (4..9).collect();
        let pexp = particle_expect(&sel);
        let (eoff, evnos) = event_expect(&sel);

        let bufs = chain
            .read_columns(
                &[("REC::Particle", &["pid"]), ("REC::Event", &["evno"])],
                Some(4..9),
                0,
            )
            .unwrap();
        assert_eq!(
            bufs[0].offsets, pexp.offsets,
            "range particle offsets ({fmt:?})"
        );
        assert_eq!(
            bufs[0].columns[0].data,
            ColumnData::I32(pexp.pid),
            "range pid ({fmt:?})"
        );
        assert_eq!(bufs[1].offsets, eoff, "range event offsets ({fmt:?})");
        assert_eq!(
            bufs[1].columns[0].data,
            ColumnData::I64(evnos),
            "range evno ({fmt:?})"
        );
    }
}

#[test]
fn filter_keeps_only_surviving_events_aligned() {
    use oxihipo::Filter;
    let n = 12;
    // Surviving = events that carry REC::Particle (rows > 0): 1,2,3,5,6,7,9,10,11.
    let surv: Vec<usize> = (0..n).filter(|&i| particle_rows(i) > 0).collect();
    for fmt in FORMATS {
        let (_dir, p) = tmp("f.hipo");
        write_file(&p, fmt, n);
        let chain = Chain::open(&p)
            .unwrap()
            .with_filter(Filter::require(["REC::Particle"]))
            .unwrap();

        let pexp = particle_expect(&surv);
        let (eoff, evnos) = event_expect(&surv);
        let bufs = chain
            .read_columns(
                &[("REC::Particle", &["pid"]), ("REC::Event", &["evno"])],
                None,
                1,
            )
            .unwrap();
        // Both banks aligned to the SAME surviving-event set.
        assert_eq!(bufs[0].offsets.len(), surv.len() + 1, "{fmt:?}");
        assert_eq!(bufs[1].offsets, eoff, "filtered event offsets ({fmt:?})");
        assert_eq!(
            bufs[0].columns[0].data,
            ColumnData::I32(pexp.pid),
            "filtered pid ({fmt:?})"
        );
        assert_eq!(
            bufs[1].columns[0].data,
            ColumnData::I64(evnos),
            "filtered evno ({fmt:?})"
        );
        // No empty sublists survive (every surviving event has ≥1 particle).
        assert!(bufs[0].offsets.windows(2).all(|w| w[1] > w[0]), "{fmt:?}");
    }
}

#[test]
fn parallel_matches_sequential() {
    let n = 40; // many records
    for fmt in FORMATS {
        let (_dir, p) = tmp("f.hipo");
        write_file(&p, fmt, n);
        let chain = Chain::open(&p).unwrap();
        let sel: &[(&str, &[&str])] = &[
            ("REC::Particle", &["pid", "cov"]),
            ("REC::Event", &["evno"]),
        ];
        let seq = chain.read_columns(sel, None, 1).unwrap();
        let par0 = chain.read_columns(sel, None, 0).unwrap();
        let par4 = chain.read_columns(sel, None, 4).unwrap();
        assert_eq!(seq, par0, "threads=0 must match sequential ({fmt:?})");
        assert_eq!(seq, par4, "threads=4 must match sequential ({fmt:?})");
    }
}

#[test]
fn read_column_typed_and_column_values() {
    let n = 12;
    let all: Vec<usize> = (0..n).collect();
    let pexp = particle_expect(&all);
    let (_dir, p) = tmp("f.hipo");
    write_file(&p, Compression::Lz4PerColumn, n);
    let chain = Chain::open(&p).unwrap();

    let (offsets, pids) = chain
        .read_column_typed::<i32>("REC::Particle", "pid", None)
        .unwrap();
    assert_eq!(offsets, pexp.offsets);
    assert_eq!(pids, pexp.pid);

    // Array cell type round-trips as [f32; 3].
    let (coff, covs) = chain
        .read_column_typed::<[f32; 3]>("REC::Particle", "cov", None)
        .unwrap();
    assert_eq!(coff, pexp.offsets);
    let expect_cells: Vec<[f32; 3]> = pexp
        .cov
        .chunks_exact(3)
        .map(|c| [c[0], c[1], c[2]])
        .collect();
    assert_eq!(covs, expect_cells);

    // column_values is the values half.
    let vals = chain
        .column_values::<i32>("REC::Particle", "pid", None)
        .unwrap();
    assert_eq!(vals, pexp.pid);
}

#[test]
fn typed_read_rejects_wrong_type_and_length() {
    let (_dir, p) = tmp("f.hipo");
    write_file(&p, Compression::Lz4, 4);
    let chain = Chain::open(&p).unwrap();
    // pid is Int; asking for f32 is a type mismatch.
    assert!(
        chain
            .read_column_typed::<f32>("REC::Particle", "pid", None)
            .is_err()
    );
    // cov is F#3; a scalar f32 handle is a length mismatch.
    assert!(
        chain
            .read_column_typed::<f32>("REC::Particle", "cov", None)
            .is_err()
    );
}

#[test]
fn unknown_bank_and_column_error() {
    let (_dir, p) = tmp("f.hipo");
    write_file(&p, Compression::Lz4, 4);
    let chain = Chain::open(&p).unwrap();
    let err = chain
        .read_columns(&[("NOPE::Bank", &["x"])], None, 1)
        .unwrap_err();
    assert!(matches!(err, oxihipo::HipoError::UnknownSchema { .. }));
    let err = chain
        .read_columns(&[("REC::Particle", &["nope"])], None, 1)
        .unwrap_err();
    assert!(matches!(err, oxihipo::HipoError::UnknownColumn { .. }));
}

#[test]
fn all_columns_when_selection_empty() {
    let (_dir, p) = tmp("f.hipo");
    write_file(&p, Compression::Lz4PerColumn, 8);
    let chain = Chain::open(&p).unwrap();
    let bufs = chain
        .read_columns(&[("REC::Particle", &[])], None, 1)
        .unwrap();
    let cols: Vec<&str> = bufs[0].columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(cols, ["pid", "px", "cov"]); // every column, schema order
}

#[test]
fn record_spans_cover_every_event_in_order() {
    let (_dir, p) = tmp("f.hipo");
    write_file(&p, Compression::Lz4, 10); // max_record_events=3 → 4 records
    let chain = Chain::open(&p).unwrap();
    let spans = chain.record_spans();
    assert_eq!(spans.len(), 4);
    let mut expected_start = 0u64;
    for s in &spans {
        assert_eq!(s.global_event_start, expected_start);
        expected_start += u64::from(s.event_count);
    }
    assert_eq!(expected_start, chain.event_count());
}

#[test]
fn record_decompressed_sizes_are_per_record_and_positive() {
    let (_dir, p) = tmp("f.hipo");
    write_file(&p, Compression::Lz4PerColumn, 10); // 4 records
    let chain = Chain::open(&p).unwrap();
    let sizes = chain.record_decompressed_sizes().unwrap();
    assert_eq!(sizes.len(), chain.record_spans().len());
    assert!(
        sizes.iter().all(|&s| s > 0),
        "every record has payload bytes"
    );
}
