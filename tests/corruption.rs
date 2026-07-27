//! Corruption handling: `events()` must surface a record-level corruption
//! as a recoverable `Err` (the iterator yields `Result<OwnedEvent>`);
//! calling `.unwrap()` on that `Err` is what panics. Neither may abort/UB.

use oxihipo::{Chain, Compression, Dict, Schema, Writer};

/// Byte offsets of every HIPO record header in `bytes`, found by scanning
/// for the little-endian header magic `0xc0da_0100` at the in-header magic
/// offset (`RH_MAGIC_NUMBER = 28`) and confirming the header-length word
/// (`= 14`, i.e. a 56-byte header). Returns header *start* offsets in file
/// order: `[0]` is the file header, `[1]` the dictionary/user-header
/// record, `[2..]` the data records, and the last is the trailer.
fn record_header_offsets(bytes: &[u8]) -> Vec<usize> {
    const MAGIC_LE: [u8; 4] = [0x00, 0x01, 0xda, 0xc0]; // 0xc0da_0100
    const RH_MAGIC_NUMBER: usize = 28;
    const RH_HEADER_LENGTH: usize = 8;
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 4 <= bytes.len() {
        if bytes[i..i + 4] == MAGIC_LE && i >= RH_MAGIC_NUMBER {
            let hdr = i - RH_MAGIC_NUMBER;
            if hdr + RH_HEADER_LENGTH + 4 <= bytes.len() {
                let hlw = u32::from_le_bytes(
                    bytes[hdr + RH_HEADER_LENGTH..hdr + RH_HEADER_LENGTH + 4]
                        .try_into()
                        .unwrap(),
                );
                if hlw == 14 {
                    out.push(hdr);
                }
            }
        }
        i += 1;
    }
    out
}

fn write_small_lz4(path: &std::path::Path, n_events: i32) {
    let mut dict = Dict::new();
    dict.add(Schema::parse_text("{T/300/1}{x/I}").unwrap());
    let mut w = Writer::create(path)
        .schemas(&dict)
        .compression(Compression::Lz4)
        .max_record_events(1) // one event per record → several data records
        .build()
        .unwrap();
    for i in 0..n_events {
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
}

#[test]
fn events_surfaces_corruption_as_err() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt.hipo");
    write_small_lz4(&path, 6);

    // Clean read works through both paths.
    {
        let chain = Chain::open(&path).unwrap();
        let all: oxihipo::Result<Vec<_>> = chain.events().collect();
        assert_eq!(all.unwrap().len() as u64, chain.event_count());
        let chain = Chain::open(&path).unwrap();
        assert_eq!(chain.events().count() as u64, chain.event_count());
    }

    // Corrupt the LZ4 payload of the first data record (header offsets:
    // [0]=file header, [1]=dict record, [2]=first data record) by filling
    // it with 0xFF. Every record *header* stays intact, so the file index
    // is unaffected and `open()` still succeeds — only that record's
    // decompression fails.
    let mut bytes = std::fs::read(&path).unwrap();
    let heads = record_header_offsets(&bytes);
    assert!(
        heads.len() >= 3,
        "expected file + dict + >=1 data record headers, got {}",
        heads.len()
    );
    let hdr = heads[2];
    let record_len = u32::from_le_bytes(bytes[hdr..hdr + 4].try_into().unwrap()) as usize * 4;
    let header_len = 56;
    assert!(record_len > header_len, "data record should have a payload");
    for b in &mut bytes[hdr + header_len..hdr + record_len] {
        *b = 0xFF;
    }
    std::fs::write(&path, &bytes).unwrap();

    // open() still succeeds (data payloads aren't decoded at open).
    let chain = Chain::open(&path).unwrap();

    // events() surfaces the corruption as an Err — no panic, no UB.
    let mut saw_err = false;
    for r in chain.events() {
        if r.is_err() {
            saw_err = true;
            break;
        }
    }
    assert!(saw_err, "events() must yield an Err on the corrupt record");

    // Unwrapping the yielded Result aborts iteration on the same input —
    // caught here so the corruption can't take down the test binary.
    let chain2 = Chain::open(&path).unwrap();
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        chain2.events().for_each(|r| {
            r.unwrap();
        });
    }))
    .is_err();
    assert!(
        panicked,
        "events() + unwrap must panic on the corrupt record"
    );
}

fn write_small_none(path: &std::path::Path, n_events: i32) {
    let mut dict = Dict::new();
    dict.add(Schema::parse_text("{T/300/1}{x/I}").unwrap());
    let mut w = Writer::create(path)
        .schemas(&dict)
        .compression(Compression::None)
        .max_record_events(1)
        .build()
        .unwrap();
    for i in 0..n_events {
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
}

/// A wildcard-free path that does not exist must error, not silently open an
/// empty (0-event) chain. Regression for the "typo'd filename = 0 events"
/// footgun.
#[test]
fn open_missing_file_errors() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("definitely-not-here.hipo");
    assert!(Chain::open(&missing).is_err(), "missing file must Err");
}

/// Random access (`Chain::event`) on a corrupt record must return `None`,
/// never panic/abort — matching the `events()` iterator. Regression for the
/// `.expect("decompress well-formed record")` that previously aborted.
#[test]
fn random_access_on_corrupt_record_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt_ra.hipo");
    write_small_lz4(&path, 6);

    let mut bytes = std::fs::read(&path).unwrap();
    let heads = record_header_offsets(&bytes);
    let hdr = heads[2];
    let record_len = u32::from_le_bytes(bytes[hdr..hdr + 4].try_into().unwrap()) as usize * 4;
    for b in &mut bytes[hdr + 56..hdr + record_len] {
        *b = 0xFF;
    }
    std::fs::write(&path, &bytes).unwrap();

    let chain = Chain::open(&path).unwrap();
    // Touch every event by index; the corrupt record's event must be None,
    // and no call may panic/abort.
    let n = chain.event_count();
    let got_none = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (0..n).any(|i| chain.event(i).is_none())
    }));
    assert!(
        matches!(got_none, Ok(true)),
        "Chain::event on a corrupt record must return None without panicking"
    );
}

/// An index array claiming an event larger than the record payload must be
/// rejected as `Err`, not slice out of bounds (panic/abort). Regression for
/// the unchecked cumulative-offset -> `payload[lo..hi]` slice.
#[test]
fn oversized_event_offset_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad_index.hipo");
    write_small_none(&path, 4);

    // For a `None` record the decompressed payload is stored verbatim, so the
    // first data record's payload begins with its index array (one u32 per
    // event; here 1 event/record). Overwrite that event-size word with a huge
    // value so the cumulative offset runs past the payload.
    let mut bytes = std::fs::read(&path).unwrap();
    let heads = record_header_offsets(&bytes);
    let hdr = heads[2];
    let payload_start = hdr + 56; // 56-byte record header, then the index array
    bytes[payload_start..payload_start + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();

    let chain = Chain::open(&path).unwrap();
    let saw_err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        chain.events().any(|r| r.is_err())
    }));
    assert!(
        matches!(saw_err, Ok(true)),
        "a record with oversized event offsets must surface Err, not abort"
    );
}

/// An empty record mid-file must not truncate a trailer-less scan. Regression
/// for `build_index_by_scanning` breaking on the first `event_count == 0`.
#[test]
fn empty_record_does_not_truncate_a_scan() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty_mid.hipo");
    write_small_none(&path, 6); // 6 records, 1 event each

    // Zero the event count + index length of the 2nd data record (header
    // offsets: [0]=file, [1]=dict, [2..]=data), making it a legitimately empty
    // record, then blank the trailer position so the reader must scan.
    let mut bytes = std::fs::read(&path).unwrap();
    let heads = record_header_offsets(&bytes);
    assert!(heads.len() >= 5);
    let hdr = heads[3];
    bytes[hdr + 12..hdr + 16].copy_from_slice(&0u32.to_le_bytes()); // event_count
    // File header trailer_position (offset 40, u64) -> 0 forces the scan path.
    bytes[40..48].copy_from_slice(&0u64.to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();

    let chain = Chain::open(&path).unwrap();
    // The records after the empty one must still be indexed: before the fix the
    // scan stopped at the empty record and reported only what preceded it.
    assert!(
        chain.event_count() > 1,
        "scan truncated at the empty record: {} events",
        chain.event_count()
    );
}

/// A corrupted by-bank offset table must not panic.
///
/// `read_columns` (and `for_each_column`) sliced the decompressed bank stream with
/// a byte range taken from the record's own offset table. A corrupted table points
/// past the end, and indexing a slice raw panicked — "range end index 3400 out of
/// range for slice of length 3379" — where every other kind of damage in this
/// reader surfaces as an error.
///
/// Found by property-testing the downstream CLI against byte-flipped files, not by
/// reasoning about the code: the offsets survive enough of the header to be used,
/// which is a narrow enough window that no hand-written case had hit it.
#[test]
fn a_corrupt_by_bank_offset_table_errors_rather_than_panicking() {
    use oxihipo::{Chain, Compression, DataType, Dict, Schema, Writer};

    let dir = std::env::temp_dir().join("oxihipo_corrupt_offsets");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.hipo");

    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "A::b",
        900,
        1,
        [("v".into(), DataType::Float, 1)],
    ));
    let mut w = Writer::create(&path)
        .schemas(&d)
        .compression(Compression::Lz4PerBank)
        .build()
        .unwrap();
    for e in 0..64i32 {
        w.event(|ev| {
            ev.bank("A::b", |b| {
                for r in 0..=(e % 4) {
                    b.row(|c| {
                        c.set("v", (e * 10 + r) as f32)?;
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

    // Flip bytes across the whole file. Most mutations are rejected earlier; the
    // point is that none of them reaches a panic, whichever stage catches it.
    let good = std::fs::read(&path).unwrap();
    let mut reached = 0usize;
    for i in 0..good.len() {
        for mask in [0xff_u8, 0x01, 0x80] {
            let mut bytes = good.clone();
            bytes[i] ^= mask;
            std::fs::write(&path, &bytes).unwrap();

            // Every columnar entry point that slices a bank stream.
            if let Ok(chain) = Chain::open(&path) {
                reached += 1;
                let _ = chain.read_columns(&[("A::b", &["v"][..])], None, 1);
                let _ = chain.bank_occupancy(None, 1);
                let _ = chain.for_each_column::<f32, _>("A::b", "v", |_| {});
                let _ = chain.event_count();
            }
        }
    }
    assert!(
        reached > 0,
        "no mutation produced an openable file, so nothing was exercised"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
