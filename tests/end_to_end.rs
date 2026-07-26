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

#[test]
fn user_config_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cfg.hipo");
    let mut dict = Dict::new();
    dict.add(Schema::parse_text("{T/300/1}{x/I}").unwrap());
    let mut w = Writer::create(&path)
        .schemas(&dict)
        .config("run", "42")
        .config("beam_energy", "10.6")
        .config("target", "LD2 with spaces & symbols")
        .build()
        .unwrap();
    for i in 0..3 {
        w.event(|ev| {
            ev.bank("T", |b| {
                b.row(|r| {
                    r.set("x", i)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            Ok(())
        })
        .unwrap();
    }
    w.finish().unwrap();

    let chain = Chain::open(&path).unwrap();
    assert_eq!(chain.config("run"), Some("42"));
    assert_eq!(chain.config("beam_energy"), Some("10.6"));
    assert_eq!(chain.config("target"), Some("LD2 with spaces & symbols"));
    assert_eq!(chain.config("missing"), None);
    assert_eq!(chain.user_config().len(), 3);
    // data still reads correctly alongside the config
    assert_eq!(chain.event_count(), 3);
}

#[test]
fn no_user_config_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nocfg.hipo");
    let mut dict = Dict::new();
    dict.add(Schema::parse_text("{T/300/1}{x/I}").unwrap());
    Writer::create(&path)
        .schemas(&dict)
        .build()
        .unwrap()
        .finish()
        .unwrap();
    let chain = Chain::open(&path).unwrap();
    assert!(chain.user_config().is_empty());
    assert_eq!(chain.config("anything"), None);
}

#[test]
fn random_access_is_correct_across_records() {
    // `Chain::event` caches the last decoded record. Exercise the patterns that
    // could return stale data: repeats, forward runs within one record,
    // alternating between records, and backwards traversal.
    let dir = tempfile::tempdir().unwrap();
    for compression in [
        Compression::None,
        Compression::Lz4,
        Compression::Lz4PerBank,
        Compression::Lz4PerColumn,
    ] {
        let path = dir.path().join(format!("ra_{compression:?}.hipo"));
        let mut d = Dict::new();
        d.add(Schema::from_columns(
            "T::b",
            300,
            1,
            [("x".into(), DataType::Long, 1)],
        ));
        const N: i64 = 40;
        {
            let mut w = Writer::create(&path)
                .schemas(&d)
                .compression(compression)
                .max_record_events(3) // ~14 records, so access crosses them
                .build()
                .unwrap();
            for e in 0..N {
                w.event(|ev| {
                    ev.bank("T::b", |b| {
                        b.row(|r| {
                            r.set("x", 1000 + e)?;
                            Ok(())
                        })?;
                        Ok(())
                    })?;
                    Ok(())
                })
                .unwrap();
            }
            w.finish().unwrap();
        }

        let chain = Chain::open(&path).unwrap();
        let get = |i: u64| {
            chain
                .event(i)
                .unwrap_or_else(|| panic!("{compression:?}: event {i} missing"))
                .bank("T::b")
                .unwrap()
                .get::<i64>("x", 0)
        };

        // forward, backward, repeated, and alternating-across-records
        for i in 0..N as u64 {
            assert_eq!(get(i), 1000 + i as i64, "{compression:?} forward {i}");
        }
        for i in (0..N as u64).rev() {
            assert_eq!(get(i), 1000 + i as i64, "{compression:?} backward {i}");
        }
        for _ in 0..3 {
            assert_eq!(get(7), 1007, "{compression:?} repeat");
        }
        for i in 0..N as u64 {
            let j = (N as u64 - 1) - i; // alternate far apart
            assert_eq!(get(i), 1000 + i as i64, "{compression:?} alt-a {i}");
            assert_eq!(get(j), 1000 + j as i64, "{compression:?} alt-b {j}");
        }
        // A cloned chain shares the cache; it must still be correct.
        let c2 = chain.clone();
        for i in 0..N as u64 {
            assert_eq!(
                c2.event(i)
                    .unwrap()
                    .bank("T::b")
                    .unwrap()
                    .get::<i64>("x", 0),
                1000 + i as i64,
                "{compression:?} clone {i}"
            );
        }
        assert!(chain.event(N as u64).is_none(), "{compression:?} past end");
    }
}

/// `read_columns_at` must agree with `read_columns` element for element, keep
/// the caller's order, and treat an out-of-range index as an empty entry —
/// the three properties the Python `entries=` keyword rests on.
#[test]
fn read_columns_at_matches_the_range_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("at.hipo");

    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        31,
        [
            ("pid".into(), DataType::Int, 1),
            ("px".into(), DataType::Float, 1),
        ],
    ));
    // Several records, so an index list can cross record boundaries.
    let mut w = Writer::create(&path)
        .schemas(&d)
        .max_record_events(7)
        .build()
        .unwrap();
    for e in 0..40i32 {
        w.event(|ev| {
            ev.bank("REC::Particle", |b| {
                for r in 0..(e % 3) {
                    b.row(|row| {
                        row.set("pid", e * 100 + r)?;
                        row.set("px", e as f32 + r as f32 * 0.5)?;
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

    /// The `pid` rows of entry-slot `k` of a `ColumnBuffers`.
    fn pid_rows(b: &oxihipo::ColumnBuffers, k: usize) -> Vec<i32> {
        let (lo, hi) = (b.offsets[k] as usize, b.offsets[k + 1] as usize);
        match &b.columns[0].data {
            oxihipo::ColumnData::I32(v) => v[lo..hi].to_vec(),
            other => panic!("pid should be I32, got {other:?}"),
        }
    }

    let chain = Chain::open(&path).unwrap();
    let sel = [("REC::Particle", &["pid", "px"][..])];

    // A contiguous ascending list must reproduce the equivalent range exactly.
    let want = chain.read_columns(&sel, Some(10..20), 1).unwrap();
    let idx: Vec<u64> = (10..20).collect();
    let got = chain.read_columns_at(&sel, &idx, 1).unwrap();
    assert_eq!(got, want, "contiguous entries must equal the same range");

    // Caller order is preserved, including duplicates.
    //
    // Entries are grouped by record before reading, so slot k must still carry
    // entry `shuffled[k]`'s rows. Checking row *counts* alone would not show a
    // permutation that happens to move equal-length entries around, so assert
    // on the values: the fixture writes `pid = e * 100 + r`, which makes every
    // row name the entry it came from.
    let shuffled = [5u64, 5, 31, 2, 17];
    let got = chain.read_columns_at(&sel, &shuffled, 1).unwrap();
    assert_eq!(got[0].offsets.len(), shuffled.len() + 1);
    for (k, &e) in shuffled.iter().enumerate() {
        let one = chain.read_columns_at(&sel, &[e], 1).unwrap();
        let rows = (got[0].offsets[k + 1] - got[0].offsets[k]) as usize;
        assert_eq!(rows, one[0].total_rows() as usize, "row count at slot {k}");
        assert_eq!(
            pid_rows(&got[0], k),
            (0..e as i32 % 3)
                .map(|r| e as i32 * 100 + r)
                .collect::<Vec<_>>(),
            "slot {k} must carry entry {e}'s rows"
        );
    }

    // Grouping changes the order records are read in; it must not change the
    // answer, at any thread count.
    for threads in [0usize, 2, 4] {
        assert_eq!(
            chain.read_columns_at(&sel, &shuffled, threads).unwrap(),
            got,
            "threads={threads} disagrees with the sequential result"
        );
    }

    // A long descending list spans every record backwards — the case that used
    // to re-decode a record per index, and the one most likely to mis-order.
    let descending: Vec<u64> = (0..40u64).rev().collect();
    let desc = chain.read_columns_at(&sel, &descending, 0).unwrap();
    for (k, &e) in descending.iter().enumerate() {
        assert_eq!(
            pid_rows(&desc[0], k),
            (0..e as i32 % 3)
                .map(|r| e as i32 * 100 + r)
                .collect::<Vec<_>>(),
            "descending slot {k} must carry entry {e}"
        );
    }

    // A non-decreasing list takes a different internal path from a shuffled one
    // (one chunk per record instead of one per entry), so it needs its own
    // coverage — duplicates included, since those repeat an event within a
    // single record's chunk.
    let ascending_dups = [2u64, 2, 5, 5, 5, 31, 31];
    let got = chain.read_columns_at(&sel, &ascending_dups, 1).unwrap();
    assert_eq!(got[0].offsets.len(), ascending_dups.len() + 1);
    for (k, &e) in ascending_dups.iter().enumerate() {
        assert_eq!(
            pid_rows(&got[0], k),
            (0..e as i32 % 3)
                .map(|r| e as i32 * 100 + r)
                .collect::<Vec<_>>(),
            "ascending slot {k} must carry entry {e}"
        );
    }
    // ...and it must agree with the shuffled path over the same multiset.
    let mut shuffled_same = ascending_dups;
    shuffled_same.reverse();
    let rev = chain.read_columns_at(&sel, &shuffled_same, 1).unwrap();
    for (k, &e) in shuffled_same.iter().enumerate() {
        assert_eq!(
            pid_rows(&rev[0], k),
            pid_rows(&got[0], ascending_dups.len() - 1 - k),
            "reversed slot {k} (entry {e}) must mirror the ascending read"
        );
    }

    // No entries at all: one offset, no rows — not an empty result.
    let none = chain.read_columns_at(&sel, &[], 1).unwrap();
    assert_eq!(
        none[0].offsets,
        vec![0],
        "empty entries still describes a bank"
    );

    // An *ascending* list that still contains an out-of-range index, and that
    // spans more than one record so it actually reaches the grouped path (a
    // single-record list is replayed through `Chain::event` instead). This is
    // the one shape where the cheap concatenating path would be taken but must
    // not be: a skipped entry breaks the "records concatenate into caller
    // order" invariant it relies on, and the result would come back short.
    let ascending_oob = [3u64, 20, 9_999];
    let got = chain.read_columns_at(&sel, &ascending_oob, 1).unwrap();
    assert_eq!(
        got[0].offsets.len(),
        ascending_oob.len() + 1,
        "an ascending list with an out-of-range index keeps one slot per entry"
    );
    for (k, &e) in ascending_oob.iter().enumerate() {
        // 9_999 is past the end, so it contributes nothing — same as an entry
        // whose row count happens to be zero.
        let want: Vec<i32> = if e >= 40 {
            Vec::new()
        } else {
            (0..e as i32 % 3).map(|r| e as i32 * 100 + r).collect()
        };
        assert_eq!(pid_rows(&got[0], k), want, "slot {k} is entry {e}");
    }

    // Out of range is an empty entry, not an error, and does not shorten the
    // result — otherwise offsets would stop lining up with `entries`.
    let with_oob = [3u64, 9_999, 4];
    let got = chain.read_columns_at(&sel, &with_oob, 1).unwrap();
    assert_eq!(
        got[0].offsets.len(),
        4,
        "one offset per requested entry + 1"
    );
    assert_eq!(
        got[0].offsets[2] - got[0].offsets[1],
        0,
        "out-of-range index must contribute zero rows"
    );
}
