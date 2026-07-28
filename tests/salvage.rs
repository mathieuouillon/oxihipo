//! `Chain::open_salvage` — opening a file whose 56-byte header is unusable.
//!
//! The header is bookkeeping: magic, version, counts, where the dictionary
//! starts, where the trailer is. Everything it says can be re-derived, because
//! each record carries its own header and magic. These tests damage a real file
//! in the ways it actually gets damaged and check what comes back.
//!
//! The cases that matter are the ones where salvage could be *wrong* rather
//! than merely fail: recovering fewer events than survive, inventing a
//! dictionary, or mistaking payload bytes for the start of a record.

use oxihipo::{Chain, Compression, DataType, Dict, Schema, Writer};

fn dict() -> Dict {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        1,
        [
            ("pid".into(), DataType::Int, 1),
            ("px".into(), DataType::Float, 1),
        ],
    ));
    d
}

/// A file of `n` events, `pid` marking each event's index.
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
                    r.set("px", i as f32)?;
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

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("oxihipo_salvage");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    let _ = std::fs::remove_file(&p);
    p
}

/// Copy `src`, then zero `count` bytes at `at`.
fn damaged(src: &std::path::Path, dst: &std::path::Path, at: usize, count: usize) {
    let mut b = std::fs::read(src).unwrap();
    for i in at..(at + count).min(b.len()) {
        b[i] = 0;
    }
    std::fs::write(dst, b).unwrap();
}

/// Every `pid` the chain yields, in order.
fn pids(chain: &Chain) -> Vec<i32> {
    let mut out = Vec::new();
    for ev in chain.events() {
        let ev = ev.unwrap();
        if let Some(b) = ev.bank("REC::Particle") {
            out.push(b.get::<i32>("pid", 0));
        }
    }
    out
}

#[test]
fn a_zeroed_file_header_is_recovered_whole() {
    let good = tmp("good.hipo");
    write(&good, 120, 10, Compression::Lz4);
    let broken = tmp("zeroed.hipo");
    damaged(&good, &broken, 0, 56);

    // The normal path cannot open it at all — that is the premise.
    assert!(
        Chain::open(&broken).is_err(),
        "a zeroed header must still defeat the normal open"
    );

    let chain = Chain::open_salvage(&broken).unwrap();
    assert_eq!(chain.event_count(), 120, "every event should come back");
    assert_eq!(pids(&chain), (0..120).collect::<Vec<i32>>());
    // The dictionary lives *after* the header, so it survives this damage.
    assert!(
        chain.schemas().get("REC::Particle").is_some(),
        "the dictionary is intact and must be recovered, not guessed"
    );
}

#[test]
fn salvage_matches_the_undamaged_file_event_for_event() {
    // The strongest statement available: what comes out of the broken file is
    // what comes out of the good one, not merely a plausible number of events.
    let good = tmp("cmp_good.hipo");
    write(&good, 97, 7, Compression::Lz4);
    let broken = tmp("cmp_broken.hipo");
    damaged(&good, &broken, 0, 56);

    let want = pids(&Chain::open(&good).unwrap());
    let got = pids(&Chain::open_salvage(&broken).unwrap());
    assert_eq!(got, want);
}

#[test]
fn every_record_layout_survives() {
    // Each codec lays a record out differently, and salvage has to find the
    // record boundaries in all of them.
    for (name, c) in [
        ("none", Compression::None),
        ("lz4", Compression::Lz4),
        ("perbank", Compression::Lz4PerBank),
        ("percolumn", Compression::Lz4PerColumn),
    ] {
        let good = tmp(&format!("layout_{name}.hipo"));
        write(&good, 60, 8, c);
        let broken = tmp(&format!("layout_{name}_broken.hipo"));
        damaged(&good, &broken, 0, 56);

        let chain = Chain::open_salvage(&broken).unwrap();
        assert_eq!(chain.event_count(), 60, "layout {name}");
        assert_eq!(pids(&chain), (0..60).collect::<Vec<i32>>(), "layout {name}");
    }
}

#[test]
fn losing_the_dictionary_too_yields_events_but_no_schemas() {
    // Damage that reaches past byte 56 takes the dictionary record with it.
    // Bank names and column types exist nowhere else in the file, so the honest
    // outcome is events without schemas — not an invented dictionary.
    let good = tmp("nodict_good.hipo");
    write(&good, 40, 10, Compression::Lz4);
    let broken = tmp("nodict.hipo");
    // 56-byte header plus the dictionary record that follows it.
    damaged(&good, &broken, 0, 400);

    let chain = Chain::open_salvage(&broken).unwrap();
    assert!(
        chain.schemas().get("REC::Particle").is_none(),
        "a destroyed dictionary must not be invented"
    );
    // The events are still there and still copyable, which is what makes a
    // verbatim repair possible; they just cannot be decoded into named banks.
    assert!(
        chain.event_count() > 0,
        "events after the damage should still be found"
    );
}

#[test]
fn a_file_with_no_records_at_all_fails_rather_than_returning_nothing() {
    // Random bytes are not a HIPO file. Salvage must say so instead of handing
    // back an empty chain, which a caller would read as "the file is fine and
    // holds no events".
    let junk = tmp("junk.hipo");
    std::fs::write(&junk, vec![0x5a; 4096]).unwrap();
    assert!(Chain::open_salvage(&junk).is_err());
}

#[test]
fn a_truncated_tail_still_recovers_the_prefix() {
    // The realistic double failure: a killed writer left no header *and* no
    // trailer, and the last record is half-written.
    let good = tmp("trunc_good.hipo");
    write(&good, 100, 10, Compression::Lz4);
    let mut bytes = std::fs::read(&good).unwrap();
    bytes.truncate(bytes.len() * 2 / 3);
    for b in bytes.iter_mut().take(56) {
        *b = 0;
    }
    let broken = tmp("trunc.hipo");
    std::fs::write(&broken, &bytes).unwrap();

    let chain = Chain::open_salvage(&broken).unwrap();
    let got = pids(&chain);
    assert!(!got.is_empty(), "the surviving prefix should be recovered");
    assert!(
        got.len() < 100,
        "a truncated file cannot yield every event ({} came back)",
        got.len()
    );
    // Whatever came back is a contiguous prefix, in order — recovery must not
    // reorder or interleave events.
    assert_eq!(got, (0..got.len() as i32).collect::<Vec<i32>>());
}

#[test]
fn salvage_agrees_with_a_normal_open_on_an_undamaged_file() {
    // Salvage should not *need* damage. Running it on a healthy file is the
    // cheapest check that its scanning finds the same records the header
    // pointed at, rather than something merely self-consistent.
    let good = tmp("healthy.hipo");
    write(&good, 83, 9, Compression::Lz4);
    let normal = Chain::open(&good).unwrap();
    let scanned = Chain::open_salvage(&good).unwrap();
    assert_eq!(scanned.event_count(), normal.event_count());
    assert_eq!(pids(&scanned), pids(&normal));
    assert_eq!(
        scanned.schemas().len(),
        normal.schemas().len(),
        "the same dictionary should be found by scanning"
    );
}
