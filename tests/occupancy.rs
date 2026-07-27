//! `Chain::bank_occupancy` must give the same answer on every compression, and
//! the same answer a per-event walk gives.
//!
//! Both halves matter. The API exists because the per-event walk is expensive,
//! so it has to agree with it — and the reason it lives in the library at all is
//! that an attempt to build it in a *caller* out of `EventCtx` reported 4 banks
//! out of 71 on a per-column file, because enumerating a per-column record's
//! banks needs the whole-event synthesis `EventCtx` avoids. That failure is
//! silent: fast, plausible, wrong. A cross-format equality test is the thing
//! that catches it.

use oxihipo::{Chain, Compression, DataType, Dict, Schema, Writer};
use std::collections::BTreeMap;

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
        "A::always",
        400,
        1,
        [("v".into(), DataType::Int, 1)],
    ));
    d.add(Schema::from_columns(
        "B::sometimes",
        400,
        2,
        [("w".into(), DataType::Float, 1)],
    ));
    // An array column, which the per-column encoder stores on its own path — so
    // its row size must still divide out correctly.
    d.add(Schema::from_columns(
        "C::arrays",
        400,
        3,
        [("xs".into(), DataType::Float, 3)],
    ));
    // Declared and never written: occupancy must report it with zero counts
    // rather than omitting it, so "never populated" stays distinguishable from
    // "not in the dictionary".
    d.add(Schema::from_columns(
        "D::never",
        400,
        4,
        [("z".into(), DataType::Int, 1)],
    ));
    // Opened on every event but given no rows. "Present" and "carrying data"
    // are different questions and this is the only fixture entry that can tell
    // them apart.
    d.add(Schema::from_columns(
        "E::empty",
        400,
        5,
        [("q".into(), DataType::Int, 1)],
    ));
    d
}

/// 30 events. `A::always` every event with a row count that varies 1..=3;
/// `B::sometimes` on every third; `C::arrays` on the first two only;
/// `E::empty` present with **zero rows** on every event.
///
/// `max_record_events(4)` forces **eight records**. That is load bearing: with
/// everything in one record the per-record tallies are never merged, and
/// mutating `merge` to take a max instead of a sum — or to sum `max_rows`
/// instead of maxing it — passed every assertion here.
fn write(path: &std::path::Path, compression: Compression) {
    let d = dict();
    let mut w = Writer::create(path)
        .schemas(&d)
        .compression(compression)
        .max_record_events(4)
        .build()
        .unwrap();
    for e in 0..30i32 {
        w.event(|ev| {
            ev.bank("A::always", |b| {
                for r in 0..=(e % 3) {
                    b.row(|c| {
                        c.set("v", e * 10 + r)?;
                        Ok(())
                    })?;
                }
                Ok(())
            })?;
            if e % 3 == 0 {
                ev.bank("B::sometimes", |b| {
                    b.row(|c| {
                        c.set("w", e as f32)?;
                        Ok(())
                    })?;
                    Ok(())
                })?;
            }
            ev.bank("E::empty", |_b| Ok(()))?;
            if e < 2 {
                ev.bank("C::arrays", |b| {
                    b.row(|c| {
                        c.set("xs", [1.0f32, 2.0, 3.0])?;
                        Ok(())
                    })?;
                    Ok(())
                })?;
            }
            Ok(())
        })
        .unwrap();
    }
    w.finish().unwrap();
}

/// The same numbers, computed the expensive way a caller would otherwise write.
fn by_walking(chain: &Chain) -> BTreeMap<String, (u64, u64, u32)> {
    let sizes: BTreeMap<(u16, u8), (String, u32)> = chain
        .schemas()
        .iter()
        .map(|s| ((s.group(), s.item()), (s.name().to_string(), s.row_size())))
        .collect();
    let mut out: BTreeMap<String, (u64, u64, u32)> = BTreeMap::new();
    for ev in chain.events() {
        let ev = ev.unwrap();
        for (h, data) in ev.structures() {
            let Some((name, row_size)) = sizes.get(&(h.group, h.item)) else {
                continue;
            };
            if *row_size == 0 {
                continue;
            }
            let rows = data.len() as u32 / row_size;
            if rows == 0 {
                continue;
            }
            let slot = out.entry(name.clone()).or_insert((0, 0, 0));
            slot.0 += 1;
            slot.1 += u64::from(rows);
            slot.2 = slot.2.max(rows);
        }
    }
    out
}

fn as_map(chain: &Chain) -> BTreeMap<String, (u64, u64, u32)> {
    chain
        .bank_occupancy(None, 1)
        .expect("occupancy")
        .banks
        .into_iter()
        .filter(|o| o.events > 0)
        .map(|o| (o.name, (o.events, o.total_rows, o.max_rows)))
        .collect()
}

fn dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn occupancy_agrees_with_a_per_event_walk_in_every_format() {
    let dir = dir("oxihipo_occ_formats");
    // 30 events: A in all 30 (rows cycling 1,2,3), B in 10, C in 2.
    let expected: BTreeMap<String, (u64, u64, u32)> = [
        ("A::always".to_string(), (30, 60, 3)),
        ("B::sometimes".to_string(), (10, 10, 1)),
        ("C::arrays".to_string(), (2, 2, 1)),
        // E::empty is deliberately absent: opened every event, zero rows, so it
        // carries no data. Counting it would make `events` mean "present".
    ]
    .into_iter()
    .collect();

    for (label, c) in FORMATS {
        let path = dir.join(format!("{label}.hipo"));
        write(&path, *c);
        let chain = Chain::open(&path).unwrap();

        let got = as_map(&chain);
        assert_eq!(got, expected, "{label}: occupancy is wrong");
        // And it agrees with the expensive path, in this format specifically —
        // the per-column case is where a caller's own version silently reported
        // a fraction of the banks.
        assert_eq!(got, by_walking(&chain), "{label}: disagrees with the walk");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_declared_but_never_written_bank_is_reported_with_zero_counts() {
    let dir = dir("oxihipo_occ_zero");
    let path = dir.join("f.hipo");
    write(&path, Compression::Lz4PerColumn);
    let chain = Chain::open(&path).unwrap();

    let all = chain.bank_occupancy(None, 1).unwrap().banks;
    // Every schema appears, in dictionary order, so a caller can distinguish
    // "never populated" from "absent from the dictionary".
    let names: Vec<&str> = all.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "A::always",
            "B::sometimes",
            "C::arrays",
            "D::never",
            "E::empty"
        ]
    );
    let never = all.iter().find(|o| o.name == "D::never").unwrap();
    assert_eq!((never.events, never.total_rows, never.max_rows), (0, 0, 0));
    // Written every event but with no rows — also zero, because the question is
    // "carrying data", not "present".
    let empty = all.iter().find(|o| o.name == "E::empty").unwrap();
    assert_eq!((empty.events, empty.total_rows, empty.max_rows), (0, 0, 0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn range_and_filter_and_threads_all_agree() {
    let dir = dir("oxihipo_occ_range");
    // Every format: the filter and range live on separate code paths per layout
    // (`check`, `check_by_bank`, `check_per_column`), so testing one proved
    // nothing about the others — deleting the per-column filter check passed.
    for (label, c) in FORMATS {
        let path = dir.join(format!("{label}.hipo"));
        write(&path, *c);
        check_range_filter_threads(&path, label);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn check_range_filter_threads(path: &std::path::Path, label: &str) {
    let chain = Chain::open(path).unwrap();

    // A range is half-open over global indices. Events 0..3 hold A rows
    // 1 + 2 + 3 = 6, B in event 0 only, C in events 0 and 1.
    let a = |v: &[oxihipo::BankOccupancy], n: &str| -> (u64, u64, u32) {
        let o = v.iter().find(|o| o.name == n).unwrap();
        (o.events, o.total_rows, o.max_rows)
    };
    let r = chain.bank_occupancy(Some(0..3), 1).unwrap().banks;
    assert_eq!(a(&r, "A::always"), (3, 6, 3), "{label}");
    assert_eq!(a(&r, "B::sometimes"), (1, 1, 1), "{label}");
    assert_eq!(a(&r, "C::arrays"), (2, 2, 1), "{label}");
    // Half-open: index 3 is excluded, and it is the one carrying B's next row.
    let r2 = chain.bank_occupancy(Some(0..4), 1).unwrap().banks;
    assert_eq!(
        a(&r2, "B::sometimes"),
        (2, 2, 1),
        "{label}: 0..4 includes 3"
    );

    // Thread count must not change the answer — the tally is merged, and a
    // `max` that reduced wrongly would only show up here.
    let seq = chain.bank_occupancy(None, 1).unwrap();
    for threads in [0usize, 2, 8] {
        // Compares the whole result, so an events_scanned that double-counted
        // under parallelism is caught too.
        assert_eq!(
            chain.bank_occupancy(None, threads).unwrap(),
            seq,
            "{label}: threads={threads} changed the answer"
        );
    }

    // The chain filter applies, so occupancy describes the events a caller
    // would actually see rather than the whole file.
    let filtered = chain
        .with_filter(oxihipo::Filter::require(["B::sometimes"]))
        .unwrap();
    let f = filtered.bank_occupancy(None, 1).unwrap().banks;
    assert_eq!(
        a(&f, "B::sometimes"),
        (10, 10, 1),
        "{label}: kept events have B"
    );
    // Those 10 events are 0, 3, 6, ... 27, whose A row counts cycle 1,2,3.
    let (events, rows, _) = a(&f, "A::always");
    assert_eq!(events, 10, "{label}");
    assert!(
        rows < 60,
        "{label}: filter must restrict the sweep, got {rows}"
    );

    // `events_scanned` is the denominator every percentage is computed against,
    // so it gets its own assertions: summed across records (this fixture has
    // eight, forced by `max_record_events(4)`) and counted *after* the filter.
    // A caller using `event_count` instead would divide by 30 under a filter
    // that kept 10.
    //
    // A fresh chain because `with_filter` consumes the one above.
    let c = Chain::open(path).unwrap();
    assert_eq!(
        c.bank_occupancy(None, 1).unwrap().events_scanned,
        30,
        "{label}: whole file, summed across records"
    );
    assert_eq!(
        c.bank_occupancy(Some(0..3), 1).unwrap().events_scanned,
        3,
        "{label}: the range restricts the count"
    );
    assert_eq!(c.event_count(), 30, "{label}: pre-filter count is not it");
    let f2 = c
        .with_filter(oxihipo::Filter::require(["B::sometimes"]))
        .unwrap();
    assert_eq!(
        f2.bank_occupancy(None, 1).unwrap().events_scanned,
        10,
        "{label}: counted after the filter, not before it"
    );
}

/// The tally is merged per record, so a file of one record cannot exercise it.
#[test]
fn the_fixture_really_spans_several_records() {
    let dir = dir("oxihipo_occ_records");
    let path = dir.join("f.hipo");
    write(&path, Compression::Lz4PerBank);
    let chain = Chain::open(&path).unwrap();
    assert!(
        chain.record_count() >= 4,
        "merge is only tested across records; got {}",
        chain.record_count()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
