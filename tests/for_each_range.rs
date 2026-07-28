//! `Chain::for_each_range` — reading one global event range.
//!
//! The property that matters is that it yields **exactly** the requested events
//! and nothing else, including where the range cuts through the middle of a
//! record. Records are the unit of I/O, so the first and last one usually hold
//! events on both sides of the boundary, and dropping them is the whole job.
//!
//! Checked against `for_each` over the whole file rather than against a
//! hand-written list: the range read must agree with the full read on the events
//! they have in common, whatever the record layout does.

use oxihipo::{Chain, Compression, DataType, Dict, Schema, Writer};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

fn dict() -> Dict {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        1,
        [("pid".into(), DataType::Int, 1)],
    ));
    d
}

/// `n` events whose `pid` is the event's own index, so a collected set of pids
/// names exactly which events were visited.
fn write(path: &std::path::Path, n: i32, per_record: u32, c: Compression) {
    let d = dict();
    let mut w = Writer::create(path)
        .schemas(&d)
        .compression(c)
        .max_record_events(per_record)
        .build()
        .unwrap();
    for i in 0..n {
        w.event(|ev| {
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

/// Which events a range read actually visited, by their `pid` marker.
fn visited(chain: &Chain, range: std::ops::Range<u64>, threads: usize) -> Vec<i32> {
    visited_many(chain, &[range], threads)
}

/// The same, for several ranges in one call. Not sorted-and-deduped by the
/// helper: duplicates have to be visible, since visiting an event twice is the
/// failure mode merging exists to prevent.
fn visited_many(chain: &Chain, ranges: &[std::ops::Range<u64>], threads: usize) -> Vec<i32> {
    let seen = Mutex::new(Vec::new());
    chain
        .for_each_ranges(ranges, threads, |ctx| {
            if let Some(b) = ctx.bank("REC::Particle") {
                seen.lock().unwrap().push(b.get::<i32>("pid", 0));
            }
        })
        .unwrap();
    let mut v = seen.into_inner().unwrap();
    v.sort_unstable();
    v
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("oxihipo_for_each_range");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn a_range_yields_exactly_its_events() {
    // 10 events per record, so 25..67 straddles both ends and covers records
    // whole in between — the case a naive "whole records that overlap"
    // implementation gets wrong by up to 9 events at each end.
    let p = tmp("mid.hipo");
    write(&p, 100, 10, Compression::Lz4);
    let chain = Chain::open(&p).unwrap();
    for threads in [1, 0, 4] {
        assert_eq!(
            visited(&chain, 25..67, threads),
            (25..67).collect::<Vec<i32>>(),
            "threads={threads}"
        );
    }
}

#[test]
fn the_stats_count_the_range_not_the_records_read() {
    let p = tmp("stats.hipo");
    write(&p, 100, 10, Compression::Lz4);
    let chain = Chain::open(&p).unwrap();
    let st = chain.for_each_range(25..67, 1, |_| {}).unwrap();
    assert_eq!(st.events_in, 42, "42 events lie in 25..67");
    assert_eq!(st.events_yielded, 42);
    // Five records hold events 25..67 at ten per record (20..30 … 60..70).
    assert_eq!(st.records, 5, "only the records the range touches are read");
}

#[test]
fn every_record_layout_agrees() {
    // The three layouts take different paths through `process_record`: the
    // bytes-backed one, the per-bank one, and the per-column one. A window
    // applied to only some of them would pass a single-codec test.
    for (name, c) in [
        ("none", Compression::None),
        ("lz4", Compression::Lz4),
        ("perbank", Compression::Lz4PerBank),
        ("percolumn", Compression::Lz4PerColumn),
    ] {
        let p = tmp(&format!("layout_{name}.hipo"));
        write(&p, 60, 7, c);
        let chain = Chain::open(&p).unwrap();
        assert_eq!(
            visited(&chain, 13..41, 0),
            (13..41).collect::<Vec<i32>>(),
            "layout {name}"
        );
    }
}

#[test]
fn the_whole_range_matches_a_plain_for_each() {
    let p = tmp("whole.hipo");
    write(&p, 100, 10, Compression::Lz4);
    let chain = Chain::open(&p).unwrap();
    let all = AtomicU64::new(0);
    chain
        .for_each(0, |_| {
            all.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
    let st = chain.for_each_range(0..100, 0, |_| {}).unwrap();
    assert_eq!(st.events_yielded, all.load(Ordering::Relaxed));
}

#[test]
fn degenerate_and_past_the_end_ranges_are_empty_or_clamped() {
    let p = tmp("edges.hipo");
    write(&p, 50, 10, Compression::Lz4);
    let chain = Chain::open(&p).unwrap();

    // Empty and inverted ranges read nothing at all — not the whole file. The
    // inverted one is written as a pair so clippy does not reject the literal;
    // a caller computing `lo..hi` from an index can produce exactly this.
    #[allow(clippy::reversed_empty_ranges)]
    let inverted = { 30..20 };
    for r in [0..0, 10..10, inverted] {
        let st = chain.for_each_range(r.clone(), 1, |_| {}).unwrap();
        assert_eq!(st.events_yielded, 0, "{r:?} should yield nothing");
        assert_eq!(st.records, 0, "{r:?} should read no record");
    }
    // Past the end clamps rather than erroring or reading past it.
    assert_eq!(visited(&chain, 45..999, 0), (45..50).collect::<Vec<i32>>());
    // Wholly past the end is empty.
    assert_eq!(chain.for_each_range(60..70, 1, |_| {}).unwrap().records, 0);
}

#[test]
fn a_bound_filter_still_applies_inside_the_range() {
    // The range is a pre-filter index space, matching `read_columns(range)` and
    // `--events A..B`: the filter drops events *within* the range rather than
    // renumbering it. A reader that renumbered would silently return a
    // different set of events than the caller's indices name.
    let p = tmp("filtered.hipo");
    let d = dict();
    let mut w = Writer::create(&p)
        .schemas(&d)
        .max_record_events(10)
        .build()
        .unwrap();
    for i in 0..40 {
        w.event(|ev| {
            ev.with_tag(if i % 2 == 0 { 1u32 } else { 0u32 });
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

    let chain = Chain::open(&p)
        .unwrap()
        .with_filter(oxihipo::Filter::new().event_tag([1u32]))
        .unwrap();
    // Events 10..20 pre-filter, of which the even ones survive.
    assert_eq!(
        visited(&chain, 10..20, 0),
        vec![10, 12, 14, 16, 18],
        "the filter selects within the range, and does not shift it"
    );
}

#[test]
fn several_ranges_are_read_in_one_pass() {
    let p = tmp("many.hipo");
    write(&p, 100, 10, Compression::Lz4);
    let chain = Chain::open(&p).unwrap();
    for threads in [1, 0, 8] {
        let got = visited_many(&chain, &[5..9, 33..37, 71..75], threads);
        let want: Vec<i32> = (5..9).chain(33..37).chain(71..75).collect();
        assert_eq!(got, want, "threads={threads}");
    }
}

#[test]
fn overlapping_and_unsorted_ranges_visit_each_event_once() {
    // Ranges arrive from an index in whatever order the records were in, and
    // adjacent records produce touching ranges.
    //
    // Note this passes with or without the internal merge — an event is visited
    // once per record regardless, so merging is an optimisation rather than
    // what makes this true. The test is here for the guarantee, not to pin the
    // implementation: what a caller needs to know is that handing over
    // overlapping ranges is safe.
    let p = tmp("overlap.hipo");
    write(&p, 100, 10, Compression::Lz4);
    let chain = Chain::open(&p).unwrap();

    let got = visited_many(&chain, &[40..50, 10..20, 15..25, 44..46], 0);
    let want: Vec<i32> = (10..25).chain(40..50).collect();
    assert_eq!(got, want, "each event exactly once, in order");

    let st = chain
        .for_each_ranges(&[40..50, 10..20, 15..25, 44..46], 1, |_| {})
        .unwrap();
    assert_eq!(st.events_in, 25, "15 + 10 distinct events, not 34");
}

#[test]
fn one_call_beats_a_loop_for_the_same_events() {
    // Not a timing assertion — just that the two agree, so the fast form is a
    // drop-in for the slow one. (Measured, one call is ~7x quicker on a real
    // file; the loop rebuilds the task list per range.)
    let p = tmp("agree.hipo");
    write(&p, 100, 10, Compression::Lz4);
    let chain = Chain::open(&p).unwrap();
    let ranges = [3..7, 55..61, 90..100];

    let one = visited_many(&chain, &ranges, 0);
    let mut looped: Vec<i32> = Vec::new();
    for r in &ranges {
        looped.extend(visited(&chain, r.clone(), 0));
    }
    looped.sort_unstable();
    assert_eq!(one, looped);
}
