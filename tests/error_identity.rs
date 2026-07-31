//! Decode errors must name the file and the record they came from.
//!
//! Decoder errors are constructed deep inside a record parse, where neither
//! the file offset nor the path is known — 58 of the 73 construction sites
//! pass `offset: 0`. An operator hitting a bad record in the middle of a
//! multi-gigabyte chain was told "corrupt record at offset 0x0", which points
//! at the file header, with no indication of *which* file.
//!
//! On a chain the path is the more valuable half: an offset you cannot map to
//! a file is not actionable.

use oxihipo::{Chain, Compression, DataType, Dict, Schema, Writer};

fn write(path: &std::path::Path, n: i64, tag: i64) {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [("evno".into(), DataType::Long, 1)],
    ));
    let mut w = Writer::create(path)
        .schemas(&d)
        .compression(Compression::Lz4)
        .max_record_events(20)
        .build()
        .unwrap();
    for i in 0..n {
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| r.set("evno", tag * 1000 + i).map(|_| ()))?;
                Ok(())
            })?;
            Ok(())
        })
        .unwrap();
    }
    w.finish().unwrap();
}

/// Corrupt the compressed payload of the last **data** record, leaving its
/// header intact so the index still points at it and the failure happens
/// during decode rather than during open.
///
/// Selecting the record matters and got this wrong first: the *last* record
/// header in the file belongs to the trailer index, and the one before it to
/// the dictionary — both stored uncompressed (codec 0). Corrupting those
/// produced no decode error at all and the test passed vacuously in the sense
/// that it failed for the wrong reason. Data records are the ones with a
/// non-zero compression codec.
fn corrupt_last_record(path: &std::path::Path) -> u64 {
    const MAGIC_LE: [u8; 4] = [0x00, 0x01, 0xda, 0xc0];
    const RH_COMP_WORD: usize = 36;
    const RECORD_HEADER_SIZE: usize = 56;

    let mut bytes = std::fs::read(path).unwrap();
    let mut data_records = Vec::new();
    let mut i = RECORD_HEADER_SIZE - 28;
    while i + 4 <= bytes.len() {
        if bytes[i..i + 4] == MAGIC_LE && i >= 28 {
            let h = i - 28;
            if h + RECORD_HEADER_SIZE <= bytes.len() {
                let comp = u32::from_le_bytes(
                    bytes[h + RH_COMP_WORD..h + RH_COMP_WORD + 4]
                        .try_into()
                        .unwrap(),
                );
                let rec_len = u32::from_le_bytes(bytes[h..h + 4].try_into().unwrap()) as usize * 4;
                if (comp >> 28) != 0 && rec_len > RECORD_HEADER_SIZE + 64 {
                    data_records.push((h, rec_len));
                }
            }
        }
        i += 1;
    }
    let (target, len) = *data_records
        .last()
        .expect("fixture found no compressed data record");

    // Scribble across the middle of the compressed payload: LZ4 back-references
    // then point outside the block and the decoder rejects it.
    let from = target + RECORD_HEADER_SIZE + 8;
    let to = std::cmp::min(target + len, bytes.len());
    for b in bytes[from..to].iter_mut() {
        *b ^= 0xFF;
    }
    std::fs::write(path, &bytes).unwrap();
    target as u64
}

#[test]
fn a_corrupt_record_names_its_file_and_its_offset() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("run_a.hipo");
    let b = dir.path().join("run_b.hipo");
    write(&a, 200, 1);
    write(&b, 200, 2);

    // Corrupt the *second* file, so a report naming only an offset would be
    // ambiguous between the two.
    let record_off = corrupt_last_record(&b);
    assert!(record_off > 0, "fixture did not find a data record");

    let chain = Chain::open([&a, &b]).unwrap();
    let err = chain
        .events()
        .find_map(Result::err)
        .expect("the corrupt record must surface as an error");
    let msg = err.to_string();

    // The file. This is the half that matters on a chain.
    assert!(
        msg.contains("run_b.hipo"),
        "error does not name the file it came from: {msg}"
    );
    assert!(
        !msg.contains("run_a.hipo"),
        "error blames the wrong file: {msg}"
    );

    // And it must carry the record's real position, not 0 — which points at
    // the file header and is what every unlocated decoder error used to say.
    assert!(
        msg.contains(&format!("offset {record_off:#x}")),
        "error does not locate the record (expected offset {record_off:#x}): {msg}"
    );
    assert!(
        !msg.contains("offset 0x0:"),
        "error still reports offset 0, which points at the file header: {msg}"
    );
}

/// The `BadMagic` arm rebases rather than overwrites.
///
/// Its offset is a field position *within* the record header
/// (`RH_MAGIC_NUMBER` = 28), not a file position, so a corrupt record anywhere
/// in a file reported `invalid HIPO magic at offset 0x1c`. Rebasing turns that
/// into the record's real position plus 28 — still pointing at the magic word,
/// but findable with a hex editor.
#[test]
fn bad_magic_is_rebased_onto_the_record_not_overwritten() {
    const MAGIC_LE: [u8; 4] = [0x00, 0x01, 0xda, 0xc0];
    const RH_MAGIC_NUMBER: u64 = 28;

    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("magic.hipo");
    write(&f, 200, 3);

    // Find the last compressed data record, then break *its* magic word. The
    // index still points at it, so the failure happens on read.
    let mut bytes = std::fs::read(&f).unwrap();
    let mut last = None;
    let mut i = 28usize;
    while i + 4 <= bytes.len() {
        if bytes[i..i + 4] == MAGIC_LE {
            let h = i - 28;
            let comp = u32::from_le_bytes(bytes[h + 36..h + 40].try_into().unwrap());
            if (comp >> 28) != 0 {
                last = Some(h);
            }
        }
        i += 1;
    }
    let h = last.expect("no compressed data record");
    bytes[h + RH_MAGIC_NUMBER as usize] ^= 0xFF;
    std::fs::write(&f, &bytes).unwrap();

    let chain = Chain::open(&f).unwrap();
    let err = chain
        .events()
        .find_map(Result::err)
        .expect("a broken record magic must surface as an error");
    let msg = err.to_string();

    let expected = h as u64 + RH_MAGIC_NUMBER;
    assert!(
        msg.contains(&format!("offset {expected:#x}")),
        "expected the magic's rebased position {expected:#x}, got: {msg}"
    );
    // The old symptom: the bare in-header field offset.
    assert!(
        !msg.contains("offset 0x1c:") && !msg.ends_with("offset 0x1c"),
        "error still reports the in-header field offset: {msg}"
    );
    assert!(msg.contains("magic.hipo"), "{msg}");
}

/// Every read entry point must carry the context, not just event iteration —
/// `bank_occupancy` in particular was missing from the review's list.
#[test]
fn every_read_entry_point_names_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("solo.hipo");
    write(&f, 200, 7);
    corrupt_last_record(&f);

    let named = |e: oxihipo::HipoError, what: &str| {
        let msg = e.to_string();
        assert!(msg.contains("solo.hipo"), "{what}: unnamed file in {msg}");
    };

    let chain = Chain::open(&f).unwrap();
    named(chain.events().find_map(Result::err).unwrap(), "events()");
    named(
        chain.for_each(1, |_| {}).unwrap_err(),
        "for_each (process_record)",
    );
    named(
        chain
            .read_columns(&[("REC::Event", &["evno"][..])], None, 1)
            .unwrap_err(),
        "read_columns (process_record_columns)",
    );
    named(
        chain.bank_occupancy(None, 1).unwrap_err(),
        "bank_occupancy (occupancy::process_record)",
    );
}

/// Writer-API misuse is not record corruption and must not claim to be.
#[test]
fn writer_misuse_is_invalid_usage_not_a_corrupt_record() {
    use oxihipo::HipoError;
    use oxihipo::event::BankBuilder;

    let s = Schema::from_columns("A::b", 300, 1, [("v".into(), DataType::Int, 1)]);
    let mut bb = BankBuilder::new(&s);

    // set_* before push_row.
    let err = bb.set_i32("v", 1).unwrap_err();
    assert!(
        matches!(err, HipoError::InvalidUsage { .. }),
        "expected InvalidUsage, got {err:?}"
    );
    assert!(!err.to_string().contains("corrupt"), "{err}");

    // Row index out of range.
    bb.push_rows(2);
    let err = bb.set_i32_at("v", 99, 1).unwrap_err();
    assert!(
        matches!(err, HipoError::InvalidUsage { .. }),
        "expected InvalidUsage, got {err:?}"
    );
}

/// A `CorruptRecord` built with `offset: 0` must have the record's offset
/// **filled in**, not wrapped around it.
///
/// Without the dedicated arm, such an error falls through to the generic
/// wrapper and the operator sees *both* numbers — "record at offset 0x768:
/// corrupt record at offset 0x0: …" — where the inner 0x0 is meaningless and
/// contradicts the outer one. Filling replaces it.
///
/// The split codecs are the reachable source: `ByBankRecord::parse` rejects an
/// unsupported `ext_format_version` with exactly this shape.
#[test]
fn a_zero_offset_corrupt_record_is_filled_not_nested() {
    const MAGIC_LE: [u8; 4] = [0x00, 0x01, 0xda, 0xc0];
    const RECORD_HEADER_SIZE: usize = 56;

    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("split.hipo");

    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [("evno".into(), DataType::Long, 1)],
    ));
    let mut w = Writer::create(&f)
        .schemas(&d)
        .compression(Compression::Lz4PerBank)
        .max_record_events(20)
        .build()
        .unwrap();
    for i in 0..100i64 {
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| r.set("evno", i).map(|_| ()))?;
                Ok(())
            })?;
            Ok(())
        })
        .unwrap();
    }
    w.finish().unwrap();

    // Poison the ext_format_version byte of the last by-bank record.
    let mut bytes = std::fs::read(&f).unwrap();
    let mut last = None;
    let mut i = 28usize;
    while i + 4 <= bytes.len() {
        if bytes[i..i + 4] == MAGIC_LE {
            let h = i - 28;
            let comp = u32::from_le_bytes(bytes[h + 36..h + 40].try_into().unwrap());
            if (comp >> 28) == 6 {
                last = Some(h);
            }
        }
        i += 1;
    }
    let h = last.expect("no Lz4PerBank record found");
    bytes[h + RECORD_HEADER_SIZE] = 9; // neither 2 nor 3
    std::fs::write(&f, &bytes).unwrap();

    let chain = Chain::open(&f).unwrap();
    let err = chain
        .events()
        .find_map(Result::err)
        .expect("an unsupported extension version must surface as an error");
    let msg = err.to_string();

    assert!(
        msg.contains("unsupported extension-format version"),
        "wrong error: {msg}"
    );
    assert!(
        msg.contains(&format!("offset {:#x}", h)),
        "expected the record's offset {:#x}: {msg}",
        h
    );
    // The point of the dedicated arm: no leftover 0x0 alongside the real one.
    assert!(
        !msg.contains("offset 0x0"),
        "the meaningless zero offset is still present: {msg}"
    );
    assert!(msg.contains("split.hipo"), "{msg}");
}

/// `bank()` cannot tell "this event has no such bank" from "this bank is
/// corrupt". `try_bank()` can.
///
/// On the split codecs a bank's LZ4 stream inflates lazily inside the
/// accessor, so a corrupt record makes `ev.bank("REC::Particle")` return
/// `None` — exactly what an event with no particles returns. An analysis
/// counting hits silently under-counts instead of failing, and the answer
/// looks plausible.
///
/// The fixture **searches** for a byte whose corruption produces that
/// specific failure rather than assuming where one is. A blanket XOR over
/// half the record does not work: swept one byte at a time over a 192-byte
/// by-bank payload, 59 flips break the directory (so the record does not
/// parse at all), 128 have no observable effect, and only **5** leave the
/// directory intact while breaking a bank stream. Guessing lands in the 128.
#[test]
fn try_bank_separates_a_corrupt_bank_from_an_absent_one() {
    use oxihipo::HipoError;

    const MAGIC_LE: [u8; 4] = [0x00, 0x01, 0xda, 0xc0];
    const RECORD_HEADER_SIZE: usize = 56;

    // `Lz4PerBank` only. On `Lz4PerColumn`, a non-opaque bank is constructed
    // without inflating anything — `try_bank` returns `Ok(Some(_))` and the
    // streams inflate later, per column, inside `Bank::col_bytes`, which is
    // infallible by construction and degrades to an empty column. That path
    // is *not* fixed here and is documented as such on `try_bank`; a swept
    // single-byte corruption finds no byte that `try_bank` can report on a
    // per-column file, which is exactly the point.
    for (label, codec, tag) in [("Lz4PerBank", Compression::Lz4PerBank, 6u32)] {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("c.hipo");

        let mut d = Dict::new();
        d.add(Schema::from_columns(
            "REC::Event",
            300,
            30,
            [("evno".into(), DataType::Long, 1)],
        ));
        d.add(Schema::from_columns(
            "REC::Particle",
            300,
            1,
            [("pid".into(), DataType::Int, 1)],
        ));
        // Never written to any event: the genuine-absence control.
        d.add(Schema::from_columns(
            "REC::Absent",
            400,
            1,
            [("v".into(), DataType::Int, 1)],
        ));

        let mut w = Writer::create(&f)
            .schemas(&d)
            .compression(codec)
            .max_record_events(25)
            .build()
            .unwrap();
        for i in 0..100i64 {
            w.event(|ev| {
                ev.bank("REC::Event", |b| {
                    b.row(|r| r.set("evno", i).map(|_| ()))?;
                    Ok(())
                })?;
                ev.bank("REC::Particle", |b| {
                    b.row(|r| r.set("pid", 11i32).map(|_| ()))?;
                    Ok(())
                })?;
                Ok(())
            })
            .unwrap();
        }
        w.finish().unwrap();

        let pristine = std::fs::read(&f).unwrap();

        // Baseline on the intact file: the three outcomes are distinct.
        {
            let chain = Chain::open(&f).unwrap();
            let ev = chain.event(0).unwrap();
            let ctx = ev.ctx();
            assert!(ctx.try_bank("REC::Particle").unwrap().is_some(), "{label}");
            assert!(
                ctx.try_bank("REC::Absent").unwrap().is_none(),
                "{label}: a bank no event carries must be Ok(None), not an error"
            );
            assert!(
                matches!(
                    ctx.try_bank("REC::Nonexistent"),
                    Err(HipoError::UnknownSchema { .. })
                ),
                "{label}: a bank the dictionary does not define should say so"
            );
        }

        // Locate the last split-codec data record.
        let (mut h, mut len) = (0usize, 0usize);
        let mut i = 28usize;
        while i + 4 <= pristine.len() {
            if pristine[i..i + 4] == MAGIC_LE {
                let hh = i - 28;
                let comp = u32::from_le_bytes(pristine[hh + 36..hh + 40].try_into().unwrap());
                if (comp >> 28) == tag {
                    h = hh;
                    len = u32::from_le_bytes(pristine[hh..hh + 4].try_into().unwrap()) as usize * 4;
                }
            }
            i += 1;
        }
        assert!(len > RECORD_HEADER_SIZE, "{label}: no split record found");

        // Search the payload for a flip that leaves the record parseable but
        // breaks a bank stream — the case `bank()` cannot report.
        let mut hit = None;
        for off in (h + RECORD_HEADER_SIZE)..(h + len) {
            let mut bytes = pristine.clone();
            bytes[off] ^= 0xFF;
            std::fs::write(&f, &bytes).unwrap();
            let Ok(chain) = Chain::open(&f) else { continue };
            let mut saw_err = false;
            let mut saw_missing_event = false;
            for i in 0..chain.event_count() {
                match chain.event(i) {
                    None => saw_missing_event = true,
                    Some(ev) => {
                        if ev.ctx().try_bank("REC::Particle").is_err() {
                            saw_err = true;
                        }
                    }
                }
            }
            if saw_err && !saw_missing_event {
                hit = Some(off);
                break;
            }
        }
        let off = hit.unwrap_or_else(|| {
            panic!("{label}: no corruption reachable through the lazy bank inflate")
        });

        // On that file, compare the two accessors event by event.
        let mut bytes = pristine.clone();
        bytes[off] ^= 0xFF;
        std::fs::write(&f, &bytes).unwrap();

        let chain = Chain::open(&f).unwrap();
        let (mut opt_none, mut errs, mut ok_none) = (0u32, 0u32, 0u32);
        for i in 0..chain.event_count() {
            let ev = chain.event(i).expect("record still parses");
            let ctx = ev.ctx();
            if ctx.bank("REC::Particle").is_none() {
                opt_none += 1;
            }
            match ctx.try_bank("REC::Particle") {
                Ok(Some(_)) => {}
                Ok(None) => ok_none += 1,
                Err(_) => errs += 1,
            }
        }

        assert!(
            opt_none > 0,
            "{label}: nothing failed, so nothing was tested"
        );
        // Every event `bank()` silently dropped is one `try_bank()` reports as
        // an error rather than as an absent bank. That is the whole point.
        assert_eq!(
            errs, opt_none,
            "{label}: try_bank reported {errs} errors against {opt_none} silent Nones"
        );
        assert_eq!(
            ok_none, 0,
            "{label}: corruption must never be reported as genuine absence"
        );
    }
}
