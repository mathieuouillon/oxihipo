//! Integration tests for `Chain`'s parallel iteration —
//! `par_for_each` and `par_reduce`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use oxihipo::{Chain, DataType, Dict, Filter, Schema, Writer};

fn dict() -> Dict {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [
            ("evno".into(), DataType::Long),
            ("beamE".into(), DataType::Float),
        ],
    ));
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        1,
        [("pid".into(), DataType::Int)],
    ));
    d
}

fn write_file(path: &std::path::Path, dict: &Dict, evno_start: i64, count: i32) {
    let mut w = Writer::create(path)
        .schemas(dict)
        .max_record_events(50)
        .build()
        .unwrap();
    for i in 0..count {
        let evno = evno_start + i as i64;
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| {
                    r.set("evno", evno)?;
                    r.set("beamE", 10.6_f32)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            ev.bank("REC::Particle", |b| {
                b.row(|r| {
                    r.set("pid", i)?;
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

#[test]
fn par_for_each_event_count_matches_sequential() {
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");
    let p3 = dir.path().join("c.hipo");
    let d = dict();
    write_file(&p1, &d, 0, 100);
    write_file(&p2, &d, 1000, 200);
    write_file(&p3, &d, 5000, 500);
    let chain = Chain::open_all([&p1, &p2, &p3]).unwrap();

    let counter = AtomicU64::new(0);
    let stats = chain
        .par_for_each(0, |_ev| {
            counter.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
    assert_eq!(counter.load(Ordering::Relaxed), 800);
    assert_eq!(stats.events_in, 800);
    assert_eq!(stats.events_yielded, 800);
    assert_eq!(stats.files, 3);
}

#[test]
fn par_reduce_matches_sequential_reduce() {
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");
    let d = dict();
    write_file(&p1, &d, 0, 100);
    write_file(&p2, &d, 1000, 200);
    let chain = Chain::open_all([&p1, &p2]).unwrap();

    // sequential
    let mut seq_total: u64 = 0;
    for ev in chain.events() {
        seq_total += ev.bank("REC::Particle").map_or(0, |b| b.rows() as u64);
    }
    // parallel
    let par_total: u64 = chain
        .par_reduce(
            0,
            || 0u64,
            |acc, ev| acc + ev.bank("REC::Particle").map_or(0, |b| b.rows() as u64),
            |a, b| a + b,
        )
        .unwrap();
    assert_eq!(seq_total, par_total);
    assert_eq!(seq_total, 300);
}

#[test]
fn par_for_each_respects_filter() {
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");

    // Build a dict with an optional bank.
    let mut d = dict();
    d.add(Schema::from_columns(
        "RAW::tag",
        500,
        1,
        [("v".into(), DataType::Int)],
    ));
    // Write files where only every 5th event has RAW::tag.
    let mk = |path: &std::path::Path, evno_start: i64, count: i32| {
        let mut w = Writer::create(path)
            .schemas(&d)
            .max_record_events(50)
            .build()
            .unwrap();
        for i in 0..count {
            let evno = evno_start + i as i64;
            w.event(|ev| {
                ev.bank("REC::Event", |b| {
                    b.row(|r| {
                        r.set("evno", evno)?;
                        r.set("beamE", 1.0_f32)?;
                        Ok(())
                    })?;
                    Ok(())
                })?;
                if i % 5 == 0 {
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
    };
    mk(&p1, 0, 100); // 20 tagged
    mk(&p2, 1000, 50); // 10 tagged

    let chain = Chain::open_all([&p1, &p2])
        .unwrap()
        .with_filter(Filter::require(["RAW::tag"]));

    let counter = Arc::new(AtomicU64::new(0));
    let counter_ref = Arc::clone(&counter);
    let stats = chain
        .par_for_each(0, move |ev| {
            assert!(ev.has("RAW::tag"));
            counter_ref.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
    assert_eq!(counter.load(Ordering::Relaxed), 30);
    assert_eq!(stats.events_in, 150); // all events visited (filter is event-level, not pre-skip)
    assert_eq!(stats.events_yielded, 30);
}

#[test]
fn par_for_each_empty_chain() {
    let chain = Chain::default();
    let stats = chain.par_for_each(0, |_ev| panic!("no events")).unwrap();
    assert_eq!(stats.events_in, 0);
    assert_eq!(stats.events_yielded, 0);
    assert_eq!(stats.files, 0);
}

#[test]
fn par_reduce_threads_zero_uses_rayon_default() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.hipo");
    write_file(&p, &dict(), 0, 200);
    let chain = Chain::open(&p).unwrap();
    let total: u64 = chain
        .par_reduce(0, || 0u64, |a, _| a + 1, |a, b| a + b)
        .unwrap();
    assert_eq!(total, 200);
}

#[test]
fn par_for_each_total_matches_event_count() {
    // 3 files: 100, 200, 500 events. par_for_each.events_in == 800.
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");
    let p3 = dir.path().join("c.hipo");
    let d = dict();
    write_file(&p1, &d, 0, 100);
    write_file(&p2, &d, 1000, 200);
    write_file(&p3, &d, 5000, 500);
    let chain = Chain::open_all([&p1, &p2, &p3]).unwrap();
    let stats = chain.par_for_each(2, |_ev| {}).unwrap();
    assert_eq!(stats.events_in, chain.event_count());
}
