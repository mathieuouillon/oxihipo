//! Every damaged file must produce an error, never a panic.
//!
//! A HIPO file arrives from a detector, a network, or a half-finished write, so
//! the reader's input is not trustworthy. Returning `Err` lets the caller decide
//! what to do; panicking takes that choice away and, in a Python binding or a
//! long batch job, takes the process with it.
//!
//! `corruption.rs` covers specific, hand-chosen damage with an assertion about
//! each one. This file is the complementary shape: mutate a real file
//! mechanically — every byte, every truncation length, hostile values in every
//! header word — and drive the whole public read path over each result, caring
//! only that nothing panics. It is deliberately assertion-light, because the
//! property being tested is "no panic anywhere", not any particular output.
//!
//! It earns its place: it is what found the `if`-instead-of-`while` in
//! `iter.rs::next_result`, where landing on a zero-event record indexed past the
//! end of a one-element offsets table. Six single-byte mutations of a 2 KB file
//! reached it, each the low byte of a record's event count — and the
//! hand-written empty-record test in `corruption.rs` could not, because it also
//! blanks the trailer and the scan path drops empty records before iteration
//! ever sees one.

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
            ("v".into(), DataType::Short, 3),
        ],
    ));
    d.add(Schema::from_columns(
        "REC::Calo",
        332,
        11,
        [
            ("pindex".into(), DataType::Short, 1),
            ("e".into(), DataType::Double, 1),
        ],
    ));
    d
}

fn write_file(path: &std::path::Path, n: i32, per_record: u32, c: Compression) {
    let d = dict();
    let mut w = Writer::create(path)
        .schemas(&d)
        .compression(c)
        .max_record_events(per_record)
        .build()
        .unwrap();
    for i in 0..n {
        w.event(|ev| {
            ev.with_tag((i % 3) as u32);
            ev.bank("REC::Particle", |b| {
                b.row(|r| {
                    r.set("pid", 11i32)?;
                    r.set("px", i as f32)?;
                    // `BankColumnType` is implemented for `[T; N]` by value —
                    // a slice is not accepted.
                    r.set("v", [1i16, 2, 3])?;
                    Ok(())
                })?;
                Ok(())
            })?;
            if i % 2 == 0 {
                ev.bank("REC::Calo", |b| {
                    b.row(|r| {
                        r.set("pindex", 0i16)?;
                        r.set("e", 0.5f64)?;
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

/// Per-test directory: the tests run in parallel and each removes its own tree
/// at the end, so sharing one directory had them deleting each other's files.
fn dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("oxihipo_zz_mine_fuzz_{tag}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Drive every read path that a caller might reach, swallowing errors.
/// Any panic here fails the test.
fn exercise(path: &std::path::Path) {
    let Ok(chain) = Chain::open(path) else { return };
    let _ = chain.event_count();
    let _ = chain.record_count();
    let _ = chain.schemas().len();
    let _ = chain.record_spans();
    for ev in chain.events().flatten() {
        {
            let _ = ev.tag();
            for name in ["REC::Particle", "REC::Calo", "No::Such"] {
                if let Some(b) = ev.bank(name) {
                    for row in 0..b.rows() {
                        let _ = b.get::<i32>("pid", row);
                        let _ = b.get::<f32>("px", row);
                        let _ = b.get::<i16>("pindex", row);
                        let _ = b.get::<f64>("e", row);
                    }
                }
            }
        }
    }
    for t in [0usize, 1, 4] {
        let _ = chain.for_each(t, |_| {});
        let _ = chain.for_each_range(0..10, t, |_| {});
        let sel: [(&str, &[&str]); 1] = [("REC::Particle", &["pid", "px"][..])];
        let _ = chain.read_columns(&sel, None, t);
        let _ = chain.read_columns(&sel, Some(0..5), t);
        let _ = chain.bank_occupancy(None, t);
    }
    for i in 0..12u64 {
        let _ = chain.event(i);
    }
    let _ = Chain::open_salvage(path);
}

/// Flip bytes across the whole file, one at a time, and read each result.
///
/// Strided rather than exhaustive so the test stays quick while still covering
/// every structural region: header, dictionary record, data records, trailer.
#[test]
fn single_byte_corruption_never_panics() {
    let d = dir("bytes");
    for (name, codec) in [
        ("none", Compression::None),
        ("lz4", Compression::Lz4),
        ("gzip", Compression::Gzip),
        ("perbank", Compression::Lz4PerBank),
        ("percol", Compression::Lz4PerColumn),
    ] {
        let good = d.join(format!("{name}.hipo"));
        write_file(&good, 40, 7, codec);
        let bytes = std::fs::read(&good).unwrap();
        let broken = d.join(format!("{name}_broken.hipo"));

        let stride = (bytes.len() / 400).max(1);
        let mut checked = 0;
        for off in (0..bytes.len()).step_by(stride) {
            for patch in [0x00u8, 0xff, 0x7f] {
                let mut b = bytes.clone();
                if b[off] == patch {
                    continue;
                }
                b[off] = patch;
                std::fs::write(&broken, &b).unwrap();
                exercise(&broken);
                checked += 1;
            }
        }
        assert!(checked > 100, "{name}: only {checked} mutations exercised");
        eprintln!("{name}: {checked} single-byte mutations, no panic");
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// Truncation at every boundary that means something structurally.
#[test]
fn truncation_at_any_length_never_panics() {
    let d = dir("trunc");
    let good = d.join("trunc_src.hipo");
    write_file(&good, 40, 7, Compression::Lz4);
    let bytes = std::fs::read(&good).unwrap();
    let broken = d.join("trunc.hipo");

    // Every length from 0 to 200 (header + first record header region), then a
    // stride over the rest so the payload and trailer are covered too.
    let mut lens: Vec<usize> = (0..200.min(bytes.len())).collect();
    lens.extend((200..bytes.len()).step_by((bytes.len() / 200).max(1)));
    lens.push(bytes.len());
    for n in lens {
        std::fs::write(&broken, &bytes[..n]).unwrap();
        exercise(&broken);
    }
    eprintln!("truncation: every length checked, no panic");
    let _ = std::fs::remove_dir_all(&d);
}

/// Values that a length or count field can hold which the buffer cannot back.
#[test]
fn absurd_wire_lengths_never_panic() {
    let d = dir("len");
    let good = d.join("len_src.hipo");
    write_file(&good, 20, 5, Compression::Lz4);
    let bytes = std::fs::read(&good).unwrap();
    let broken = d.join("len.hipo");

    // Overwrite each 4-byte-aligned little-endian word in the first 512 bytes
    // (file header + first record header) with hostile values.
    for word in (0..512.min(bytes.len() / 4 * 4)).step_by(4) {
        for v in [
            0u32,
            1,
            u32::MAX,
            u32::MAX - 1,
            0x8000_0000,
            0x7fff_ffff,
            bytes.len() as u32 + 1,
        ] {
            let mut b = bytes.clone();
            b[word..word + 4].copy_from_slice(&v.to_le_bytes());
            std::fs::write(&broken, &b).unwrap();
            exercise(&broken);
        }
    }
    eprintln!("absurd lengths: no panic");
    let _ = std::fs::remove_dir_all(&d);
}
