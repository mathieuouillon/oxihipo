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
