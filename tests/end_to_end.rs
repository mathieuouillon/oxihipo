//! Broad end-to-end coverage of the public API — one realistic workflow that
//! writes a multi-bank, multi-record file and then exercises the whole read
//! surface: metadata, sequential + random-access + parallel reads, every scalar
//! and array data type, filters, the columnar materializer, `skim`, multi-file
//! chains, and error handling. Complements the per-feature tests by driving the
//! library the way a real analysis would, front to back.

use std::sync::atomic::{AtomicU64, Ordering};

use oxihipo::{Chain, Compression, DataType, Dict, Filter, Result, Schema, Writer};

const N: i64 = 60;

fn n_parts(evno: i64) -> i32 {
    (evno % 5) as i32 // 0..=4, so some events have an empty particle bank
}
fn has_calo(evno: i64) -> bool {
    evno % 3 == 0
}

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
            ("charge".into(), DataType::Byte, 1),
            ("cov".into(), DataType::Float, 3), // fixed-length array column
        ],
    ));
    d.add(Schema::from_columns(
        "REC::Calorimeter",
        300,
        32,
        [("energy".into(), DataType::Float, 1)],
    ));
    // One column of every scalar type + an array, to round-trip them all.
    d.add(Schema::from_columns(
        "Test::Types",
        400,
        1,
        [
            ("b".into(), DataType::Byte, 1),
            ("s".into(), DataType::Short, 1),
            ("i".into(), DataType::Int, 1),
            ("l".into(), DataType::Long, 1),
            ("f".into(), DataType::Float, 1),
            ("d".into(), DataType::Double, 1),
            ("arr".into(), DataType::Int, 4),
        ],
    ));
    d
}

/// Write `N` events into `path`, small `max_record_events` so the file spans
/// several records (exercising record flush + multi-record reads).
fn write_workflow_file(path: &std::path::Path) -> Result<()> {
    let mut w = Writer::create(path)
        .schemas(&dict())
        .compression(Compression::Lz4PerColumn)
        .max_record_events(16)
        .build()?;
    for evno in 0..N {
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| {
                    r.set("evno", evno)?;
                    r.set("beamE", 10.6_f32)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            ev.bank("Test::Types", |b| {
                b.row(|r| {
                    r.set("b", evno as i8)?;
                    r.set("s", (evno * 7) as i16)?;
                    r.set("i", (evno * 1000) as i32)?;
                    r.set("l", evno * 1_000_000)?;
                    r.set("f", evno as f32 * 0.5)?;
                    r.set("d", evno as f64 * 0.25)?;
                    r.set(
                        "arr",
                        [
                            evno as i32,
                            evno as i32 + 1,
                            evno as i32 + 2,
                            evno as i32 + 3,
                        ],
                    )?;
                    Ok(())
                })?;
                Ok(())
            })?;
            ev.bank("REC::Particle", |b| {
                for k in 0..n_parts(evno) {
                    b.row(|r| {
                        r.set("pid", 11 + k)?;
                        r.set("px", k as f32 * 0.1)?;
                        r.set("charge", (k as i8) - 1)?;
                        r.set("cov", [k as f32, k as f32 + 0.5, -(k as f32)])?;
                        Ok(())
                    })?;
                }
                Ok(())
            })?;
            if has_calo(evno) {
                ev.bank("REC::Calorimeter", |b| {
                    b.row(|r| {
                        r.set("energy", evno as f32 * 2.0)?;
                        Ok(())
                    })?;
                    Ok(())
                })?;
            }
            Ok(())
        })?;
    }
    w.finish()?;
    Ok(())
}

fn total_particles() -> u64 {
    (0..N).map(|e| n_parts(e) as u64).sum()
}

#[test]
fn metadata_is_correct() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wf.hipo");
    write_workflow_file(&path).unwrap();

    let chain = Chain::open(&path).unwrap();
    assert_eq!(chain.event_count(), N as u64);
    assert_eq!(chain.file_count(), 1);
    assert_eq!(chain.files().count(), 1);
    assert_eq!(chain.schemas().len(), 4);
    let names: Vec<&str> = chain.schemas().iter().map(|s| s.name()).collect();
    for b in [
        "REC::Event",
        "REC::Particle",
        "REC::Calorimeter",
        "Test::Types",
    ] {
        assert!(names.contains(&b), "dict missing {b}");
    }
}

#[test]
fn sequential_read_returns_written_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wf.hipo");
    write_workflow_file(&path).unwrap();

    let chain = Chain::open(&path).unwrap();
    let mut seen = 0u64;
    let mut particles = 0u64;
    for ev in chain.events() {
        let ev = ev.unwrap();
        let evno = ev.bank("REC::Event").unwrap().get::<i64>("evno", 0);
        assert_eq!(evno, seen as i64, "events arrive in order");

        let p = ev.bank("REC::Particle").unwrap();
        assert_eq!(p.rows() as i32, n_parts(evno));
        particles += p.rows() as u64;
        // Spot-check a value via each accessor style.
        if p.rows() > 0 {
            assert_eq!(p.get::<i32>("pid", 0), 11);
            assert_eq!(p.col::<f32>("px").unwrap()[0], 0.0);
            let cov = p.array_at::<f32>("cov", 0).unwrap();
            assert_eq!(&cov[..], &[0.0, 0.5, 0.0]);
        }
        // Calorimeter present only on some events.
        assert_eq!(ev.bank("REC::Calorimeter").is_some(), has_calo(evno));
        seen += 1;
    }
    assert_eq!(seen, N as u64);
    assert_eq!(particles, total_particles());
}

#[test]
fn every_data_type_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wf.hipo");
    write_workflow_file(&path).unwrap();

    let chain = Chain::open(&path).unwrap();
    for (evno, ev) in chain.events().map(Result::unwrap).enumerate() {
        let e = evno as i64;
        let t = ev.bank("Test::Types").unwrap();
        assert_eq!(t.get::<i8>("b", 0), e as i8, "Byte");
        assert_eq!(t.get::<i16>("s", 0), (e * 7) as i16, "Short");
        assert_eq!(t.get::<i32>("i", 0), (e * 1000) as i32, "Int");
        assert_eq!(t.get::<i64>("l", 0), e * 1_000_000, "Long");
        assert_eq!(t.get::<f32>("f", 0), e as f32 * 0.5, "Float");
        assert_eq!(t.get::<f64>("d", 0), e as f64 * 0.25, "Double");
        let arr = t.array_at::<i32>("arr", 0).unwrap();
        assert_eq!(
            &arr[..],
            &[e as i32, e as i32 + 1, e as i32 + 2, e as i32 + 3],
            "array"
        );
    }
}

#[test]
fn random_access_by_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wf.hipo");
    write_workflow_file(&path).unwrap();

    let chain = Chain::open(&path).unwrap();
    for idx in [0u64, 15, 16, 42, (N - 1) as u64] {
        let ev = chain.event(idx).expect("event in range");
        assert_eq!(
            ev.bank("REC::Event").unwrap().get::<i64>("evno", 0),
            idx as i64
        );
    }
    assert!(chain.event(N as u64).is_none(), "past-the-end is None");
}

#[test]
fn parallel_scan_agrees_with_sequential() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wf.hipo");
    write_workflow_file(&path).unwrap();
    let chain = Chain::open(&path).unwrap();

    // Single-threaded (1), all-cores (0), and a fixed count must all agree on
    // the total particle count, though parallel modes visit out of order.
    for threads in [1usize, 0, 3] {
        let particles = AtomicU64::new(0);
        let stats = chain
            .for_each(threads, |ev| {
                if let Some(p) = ev.bank("REC::Particle") {
                    particles.fetch_add(p.rows() as u64, Ordering::Relaxed);
                }
            })
            .unwrap();
        assert_eq!(
            stats.events_yielded, N as u64,
            "threads={threads}: event count"
        );
        assert_eq!(
            particles.into_inner(),
            total_particles(),
            "threads={threads}: particle total"
        );
    }
}

#[test]
fn columnar_read_and_entry_range() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wf.hipo");
    write_workflow_file(&path).unwrap();
    let chain = Chain::open(&path).unwrap();

    // Whole file: one offsets entry per event + 1, flat rows == all particles.
    let bufs = chain
        .read_columns(&[("REC::Particle", &["px"])], None, 1)
        .unwrap();
    assert_eq!(bufs[0].offsets.len(), N as usize + 1);
    assert_eq!(bufs[0].total_rows() as u64, total_particles());

    // Sub-range [10, 20): exactly 10 events' worth of offsets.
    let sub = chain
        .read_columns(&[("REC::Particle", &["px"])], Some(10..20), 1)
        .unwrap();
    assert_eq!(sub[0].offsets.len(), 11);
    let want: u64 = (10..20).map(|e| n_parts(e) as u64).sum();
    assert_eq!(sub[0].total_rows() as u64, want);
}

#[test]
fn filter_then_skim_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wf.hipo");
    write_workflow_file(&path).unwrap();
    let calo_events = (0..N).filter(|e| has_calo(*e)).count() as u64;

    // Filter to Calorimeter-carrying events, skim into a new file, re-read.
    let out = dir.path().join("skim.hipo");
    let summary = Chain::open(&path)
        .unwrap()
        .with_filter(Filter::require(["REC::Calorimeter"]))
        .unwrap()
        .skim(&out, Compression::Lz4)
        .unwrap();
    assert_eq!(summary.events, calo_events);

    let re = Chain::open(&out).unwrap();
    assert_eq!(re.event_count(), calo_events);
    // Every surviving event must carry the bank we filtered on, with its value.
    let mut seen = 0u64;
    for ev in re.events().map(Result::unwrap) {
        let evno = ev.bank("REC::Event").unwrap().get::<i64>("evno", 0);
        assert!(has_calo(evno));
        assert_eq!(
            ev.bank("REC::Calorimeter").unwrap().get::<f32>("energy", 0),
            evno as f32 * 2.0
        );
        seen += 1;
    }
    assert_eq!(seen, calo_events);
}

#[test]
fn multi_file_chain_reads_as_one() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.hipo");
    let b = dir.path().join("b.hipo");
    write_workflow_file(&a).unwrap();
    write_workflow_file(&b).unwrap();

    // Open an explicit list; it reads as one chain, in order.
    let chain = Chain::open([&a, &b]).unwrap();
    assert_eq!(chain.file_count(), 2);
    assert_eq!(chain.event_count(), 2 * N as u64);

    let evnos: Vec<i64> = chain
        .events()
        .map(Result::unwrap)
        .map(|ev| ev.bank("REC::Event").unwrap().get::<i64>("evno", 0))
        .collect();
    assert_eq!(evnos.len(), 2 * N as usize);
    // Each file restarts at evno 0, so the sequence is 0..N twice.
    assert_eq!(evnos[N as usize - 1], N - 1);
    assert_eq!(evnos[N as usize], 0);
}

#[test]
fn missing_path_yields_empty_chain() {
    // A path containing glob metacharacters that matches nothing gives an empty
    // chain, not an error (the documented `IntoSources` behavior). A
    // wildcard-free non-existent path errors instead — see
    // `corruption::open_missing_file_errors`.
    let chain = Chain::open("/definitely/not/a/real/dir/*.hipo").unwrap();
    assert_eq!(chain.event_count(), 0);
    assert_eq!(chain.file_count(), 0);
    assert_eq!(chain.events().count(), 0);
}

#[test]
fn garbage_file_errors_on_open() {
    // A file that exists but isn't a HIPO file must fail at open (bad header),
    // as an `Err` — never a panic.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("garbage.hipo");
    std::fs::write(&path, b"this is not a hipo file, not even close").unwrap();
    assert!(
        Chain::open(&path).is_err(),
        "opening a malformed file must error, not panic"
    );
}
