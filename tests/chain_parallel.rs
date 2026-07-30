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
fn par_fold_matches_for_each_across_thread_counts() {
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");
    let p3 = dir.path().join("c.hipo");
    let d = dict();
    write_file(&p1, &d, 0, 100);
    write_file(&p2, &d, 1000, 200);
    write_file(&p3, &d, 5000, 500);
    let chain = Chain::open([&p1, &p2, &p3]).unwrap();

    // `pid` is the per-file event index, so the expected sum is three
    // triangular numbers — a value that changes if any event is dropped,
    // double-counted, or if a worker's partial is lost in the reduce.
    let tri = |n: i64| n * (n - 1) / 2;
    let want = tri(100) + tri(200) + tri(500);

    // Baseline: the shared-atomic shape `par_fold` is meant to replace.
    let via_for_each = {
        let acc = AtomicU64::new(0);
        chain
            .for_each(1, |ev| {
                let pid = ev
                    .bank("REC::Particle")
                    .map(|b| b.get::<i32>("pid", 0))
                    .unwrap_or(0);
                acc.fetch_add(pid as u64, Ordering::Relaxed);
            })
            .unwrap();
        acc.into_inner()
    };
    assert_eq!(via_for_each, want as u64);

    for threads in [1usize, 0, 2, 4] {
        let (sum, stats) = chain
            .par_fold(
                threads,
                || 0i64,
                |acc, ev| {
                    *acc += ev
                        .bank("REC::Particle")
                        .map(|b| b.get::<i32>("pid", 0))
                        .unwrap_or(0) as i64;
                },
                |a, b| a + b,
            )
            .unwrap();
        assert_eq!(sum, want, "threads={threads}");
        assert_eq!(stats.events_in, 800, "threads={threads}");
        assert_eq!(stats.events_yielded, 800, "threads={threads}");
        assert_eq!(stats.files, 3, "threads={threads}");
    }

    // The sequential `fold` must land on the same number.
    let (sum, stats) = chain
        .fold(0i64, |acc, ev| {
            *acc += ev
                .bank("REC::Particle")
                .map(|b| b.get::<i32>("pid", 0))
                .unwrap_or(0) as i64;
        })
        .unwrap();
    assert_eq!(sum, want);
    assert_eq!(stats.events_yielded, 800);
}

#[test]
fn par_fold_respects_filter() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.hipo");

    let mut d = dict();
    d.add(Schema::from_columns(
        "RAW::tag",
        500,
        1,
        [("v".into(), DataType::Int, 1)],
    ));
    let mut w = Writer::create(&p)
        .schemas(&d)
        .max_record_events(50)
        .build()
        .unwrap();
    for i in 0..100 {
        w.event(|ev| {
            ev.bank("REC::Particle", |b| {
                b.row(|r| r.set("pid", i).map(|_| ()))?;
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

    let chain = Chain::open(&p)
        .unwrap()
        .with_filter(Filter::require(["RAW::tag"]))
        .unwrap();

    // Only every 5th event survives, so the fold sees 20 of 100.
    for threads in [1usize, 0, 3] {
        let (n, stats) = chain
            .par_fold(
                threads,
                || 0u64,
                |acc, ev| {
                    assert!(ev.has("RAW::tag"));
                    *acc += 1;
                },
                |a, b| a + b,
            )
            .unwrap();
        assert_eq!(n, 20, "threads={threads}");
        assert_eq!(stats.events_in, 100, "threads={threads}");
        assert_eq!(stats.events_yielded, 20, "threads={threads}");
    }
}

#[test]
fn fold_accepts_a_non_send_accumulator() {
    // The point of `fold` existing next to `par_fold`. `Rc<Cell<u64>>` is
    // neither `Send` nor `Sync`, and the closure mutates captured state, so
    // this stops compiling the moment a thread bound comes back — which is
    // exactly what `for_each`/`par_fold` demand even at `threads == 1`.
    use std::cell::Cell;
    use std::rc::Rc;

    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.hipo");
    write_file(&p, &dict(), 0, 200);
    let chain = Chain::open(&p).unwrap();

    let seen = Rc::new(Cell::new(0u64));
    let mirror = Rc::clone(&seen);
    let (last, stats) = chain
        .fold(None::<i64>, move |acc, ev| {
            mirror.set(mirror.get() + 1);
            *acc = ev.bank("REC::Event").map(|b| b.get::<i64>("evno", 0));
        })
        .unwrap();

    assert_eq!(seen.get(), 200);
    assert_eq!(stats.events_yielded, 200);
    // `fold` is documented as input order, so the last value is the last event.
    assert_eq!(last, Some(199));
}

#[test]
fn par_fold_empty_chain_returns_the_identity() {
    let chain = Chain::default();
    for threads in [1usize, 0, 2] {
        let (v, stats) = chain
            .par_fold(threads, || 7u64, |acc, _| *acc += 1, |a, b| a + b)
            .unwrap();
        // No events, so nothing folds; the value is whatever `id` produced,
        // possibly reduced with itself. With `7` and `+` that is a multiple of
        // 7 — the assertion that matters is that no event was counted.
        assert_eq!(v % 7, 0, "threads={threads}");
        assert_eq!(stats.events_in, 0, "threads={threads}");
        assert_eq!(stats.events_yielded, 0, "threads={threads}");
    }
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
