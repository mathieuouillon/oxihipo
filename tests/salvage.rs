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

// ---------------------------------------------------------------------------
// The sequential scan
//
// `build_index_by_scanning` runs on two paths: salvage always, and the normal
// path whenever the trailer is missing *or does not parse*. The tests below
// damage a file in ways that force each path through it.
// ---------------------------------------------------------------------------

const RECORD_HEADER_SIZE: usize = 56;
const FILE_HEADER_SIZE: usize = 56;
const ENDIAN_MAGIC: [u8; 4] = [0x00, 0x01, 0xda, 0xc0];

/// Offsets of every record header, walking record lengths from the file header.
/// Index 0 is the dictionary; data records follow; the trailer is last.
fn record_offsets(bytes: &[u8]) -> Vec<usize> {
    let mut v = Vec::new();
    let mut i = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize * 4;
    let _ = FILE_HEADER_SIZE;
    while i + RECORD_HEADER_SIZE <= bytes.len() {
        if bytes[i + 28..i + 32] != ENDIAN_MAGIC {
            break;
        }
        let total = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap()) as usize * 4;
        if total < RECORD_HEADER_SIZE || i + total > bytes.len() {
            break;
        }
        v.push(i);
        i += total;
    }
    v
}

/// Corrupt the trailer's payload, leaving its record header intact. The reader
/// then fails to build an index from it and falls back to the scan — which is
/// the only way the *normal* path reaches that scan on a file that has a
/// trailer at all.
fn break_trailer(bytes: &mut [u8]) {
    let tp = u64::from_le_bytes(bytes[40..48].try_into().unwrap()) as usize;
    for x in bytes[tp + RECORD_HEADER_SIZE..tp + RECORD_HEADER_SIZE + 16].iter_mut() {
        *x = 0xFF;
    }
}

/// A trailer is an ordinary one-event record holding the `file::index` bank —
/// no header bit sets it apart. A scan that walks into one indexes it as data
/// and invents an event, which is what happened on a file whose trailer was
/// present but unparseable: 12 events came back as 13.
#[test]
fn a_scan_forced_by_an_unparseable_trailer_does_not_index_the_trailer() {
    let good = tmp("scan_trailer_good.hipo");
    write(&good, 12, 4, Compression::None);
    assert_eq!(Chain::open(&good).unwrap().event_count(), 12);

    let broken = tmp("scan_trailer_broken.hipo");
    let mut bytes = std::fs::read(&good).unwrap();
    break_trailer(&mut bytes);
    std::fs::write(&broken, &bytes).unwrap();

    let chain = Chain::open(&broken).unwrap();
    assert_eq!(chain.event_count(), 12, "the trailer was indexed as data");
    assert_eq!(pids(&chain), (0..12).collect::<Vec<_>>());
}

/// One damaged record header used to cost the whole file, salvage included —
/// which defeats the point of having a salvage path. It now resynchronises and
/// recovers the records on either side of the damage.
#[test]
fn salvage_resynchronises_past_a_damaged_record_header() {
    let good = tmp("scan_resync_good.hipo");
    write(&good, 12, 4, Compression::None);

    let broken = tmp("scan_resync_broken.hipo");
    let mut bytes = std::fs::read(&good).unwrap();
    let offsets = record_offsets(&bytes);
    // offsets[0] is the dictionary, so this is the second of three data records.
    bytes[offsets[2] + 28] ^= 0xFF;
    std::fs::write(&broken, &bytes).unwrap();

    // The normal path is unaffected: it indexes from the intact trailer and
    // never runs the scan, so the file opens and only the damaged record fails
    // to read. Reporting that corruption rather than quietly returning fewer
    // events is this library's contract.
    let normal = Chain::open(&broken).unwrap();
    assert_eq!(normal.event_count(), 12);
    assert!(
        normal.events().any(|e| e.is_err()),
        "the damaged record should fail on read"
    );

    let chain = Chain::open_salvage(&broken).unwrap();
    assert_eq!(
        pids(&chain),
        vec![0, 1, 2, 3, 8, 9, 10, 11],
        "salvage should recover every record but the damaged one"
    );
}

/// `event_count` is a header field and was taken on trust, so a corrupt one
/// propagated straight into `Chain::event_count()`. The index array bounds it
/// at four bytes per event — a header field too, so no decompression is needed.
#[test]
fn a_record_claiming_more_events_than_its_index_array_holds_is_rejected() {
    let good = tmp("scan_count_good.hipo");
    write(&good, 12, 4, Compression::None);

    // Corrupt the first data record's count. Two copies: the normal path only
    // reaches the scan when the trailer is unusable, while salvage always
    // scans — and salvage identifies the trailer by its contents, so breaking
    // those would leave it unable to tell the trailer from a data record.
    let mut bytes = std::fs::read(&good).unwrap();
    let offsets = record_offsets(&bytes);
    let victim = offsets[1];
    bytes[victim + 12..victim + 16].copy_from_slice(&1_000_000u32.to_le_bytes());

    let for_salvage = tmp("scan_count_salvage.hipo");
    std::fs::write(&for_salvage, &bytes).unwrap();

    let for_normal = tmp("scan_count_broken.hipo");
    break_trailer(&mut bytes);
    std::fs::write(&for_normal, &bytes).unwrap();

    let err = Chain::open(&for_normal).unwrap_err().to_string();
    assert!(
        err.contains("event_count exceeds"),
        "expected the count to be rejected, got: {err}"
    );

    // Salvage drops the lying record and keeps the rest.
    let chain = Chain::open_salvage(&for_salvage).unwrap();
    assert_eq!(
        pids(&chain),
        vec![4, 5, 6, 7, 8, 9, 10, 11],
        "salvage should skip only the record with the impossible count"
    );
}

/// An intact file must be unaffected by all of the above: the scan is also the
/// normal path for a file written without a trailer.
#[test]
fn the_scan_still_indexes_an_intact_file_exactly() {
    for codec in [
        Compression::None,
        Compression::Lz4,
        Compression::Lz4PerBank,
        Compression::Lz4PerColumn,
    ] {
        let path = tmp(&format!("scan_intact_{codec:?}.hipo"));
        write(&path, 12, 4, codec);
        let mut bytes = std::fs::read(&path).unwrap();
        // Zero `trailer_position` so the reader scans instead of reading the
        // trailer index, while the trailer record itself stays in the file.
        bytes[40..48].copy_from_slice(&0u64.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let chain = Chain::open(&path).unwrap();
        assert_eq!(
            pids(&chain),
            (0..12).collect::<Vec<_>>(),
            "{codec:?}: scan of an intact file"
        );
    }
}
