//! Integration tests for `Chain::for_each` — single-threaded and
//! parallel, selected by the `threads` argument.

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
            ("evno".into(), DataType::Long, 1),
            ("beamE".into(), DataType::Float, 1),
        ],
    ));
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        1,
        [("pid".into(), DataType::Int, 1)],
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
fn for_each_single_and_parallel_agree() {
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");
    let p3 = dir.path().join("c.hipo");
    let d = dict();
    write_file(&p1, &d, 0, 100);
    write_file(&p2, &d, 1000, 200);
    write_file(&p3, &d, 5000, 500);
    let chain = Chain::open([&p1, &p2, &p3]).unwrap();

    // The only difference between the runs is the `threads` argument.
    for threads in [1usize, 0, 2] {
        let counter = AtomicU64::new(0);
        let stats = chain
            .for_each(threads, |_ev| {
                counter.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 800, "threads={threads}");
        assert_eq!(stats.events_in, 800);
        assert_eq!(stats.events_yielded, 800);
        assert_eq!(stats.files, 3);
    }
}

#[test]
fn for_each_single_matches_iterator() {
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");
    let d = dict();
    write_file(&p1, &d, 0, 100);
    write_file(&p2, &d, 1000, 200);
    let chain = Chain::open([&p1, &p2]).unwrap();

    // The `events()` iterator and a single-threaded `for_each(1)` must
    // visit the exact same data; a parallel `for_each(0)` the same total.
    let mut iter_total: u64 = 0;
    for ev in chain.events().map(Result::unwrap) {
        iter_total += ev.bank("REC::Particle").map_or(0, |b| b.rows() as u64);
    }

    let sum_via = |threads: usize| -> u64 {
        let acc = AtomicU64::new(0);
        chain
            .for_each(threads, |ev| {
                acc.fetch_add(
                    ev.bank("REC::Particle").map_or(0, |b| b.rows() as u64),
                    Ordering::Relaxed,
                );
            })
            .unwrap();
        acc.into_inner()
    };

    assert_eq!(iter_total, 300);
    assert_eq!(sum_via(1), 300);
    assert_eq!(sum_via(0), 300);
}

#[test]
fn for_each_respects_filter() {
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");

    // Build a dict with an optional bank.
    let mut d = dict();
    d.add(Schema::from_columns(
        "RAW::tag",
        500,
        1,
        [("v".into(), DataType::Int, 1)],
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

    let chain = Chain::open([&p1, &p2])
        .unwrap()
        .with_filter(Filter::require(["RAW::tag"]))
        .unwrap();

    let counter = Arc::new(AtomicU64::new(0));
    let counter_ref = Arc::clone(&counter);
    let stats = chain
        .for_each(0, move |ev| {
            assert!(ev.has("RAW::tag"));
            counter_ref.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
    assert_eq!(counter.load(Ordering::Relaxed), 30);
    assert_eq!(stats.events_in, 150); // all events visited (filter is event-level, not pre-skip)
    assert_eq!(stats.events_yielded, 30);
}

#[test]
fn for_each_empty_chain() {
    let chain = Chain::default();
    for threads in [1usize, 0] {
        let stats = chain.for_each(threads, |_ev| panic!("no events")).unwrap();
        assert_eq!(stats.events_in, 0);
        assert_eq!(stats.events_yielded, 0);
        assert_eq!(stats.files, 0);
    }
}

#[test]
fn for_each_threads_zero_uses_rayon_default() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.hipo");
    write_file(&p, &dict(), 0, 200);
    let chain = Chain::open(&p).unwrap();
    let total = AtomicU64::new(0);
    chain
        .for_each(0, |_ev| {
            total.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
    assert_eq!(total.into_inner(), 200);
}

#[test]
fn for_each_total_matches_event_count() {
    // 3 files: 100, 200, 500 events. for_each.events_in == 800.
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");
    let p3 = dir.path().join("c.hipo");
    let d = dict();
    write_file(&p1, &d, 0, 100);
    write_file(&p2, &d, 1000, 200);
    write_file(&p3, &d, 5000, 500);
    let chain = Chain::open([&p1, &p2, &p3]).unwrap();
    let stats = chain.for_each(2, |_ev| {}).unwrap();
    assert_eq!(stats.events_in, chain.event_count());
}
