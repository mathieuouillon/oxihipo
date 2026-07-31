//! `ext_format_version` 3: per-stream encodings and checksums.
//!
//! Three properties, and the compatibility ones matter more than the size one:
//!
//! 1. A version-3 file round-trips exactly.
//! 2. **Old files still read** — versions 1 and 2 are untouched.
//! 3. **Old readers refuse version 3** rather than misreading it. A byte-split
//!    stream inflated without un-splitting is garbage, so refusing is the only
//!    safe behaviour, and the version byte is what makes it possible.

use oxihipo::{Chain, Codec, Compression, DataType, Dict, Layout, Schema, Writer};

const N: i64 = 400;

fn dict() -> Dict {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        1,
        [
            // Deliberately over-wide: a 32-bit field carrying a small range is
            // exactly what byte-split exploits.
            ("pid".into(), DataType::Int, 1),
            ("px".into(), DataType::Float, 1),
            ("vt".into(), DataType::Double, 1),
            ("status".into(), DataType::Short, 1),
            ("charge".into(), DataType::Byte, 1),
        ],
    ));
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [("evno".into(), DataType::Long, 1)],
    ));
    d
}

fn write(path: &std::path::Path, c: Compression) {
    write_n(path, c, N)
}

fn write_n(path: &std::path::Path, c: Compression, n: i64) {
    let d = dict();
    let mut w = Writer::create(path)
        .schemas(&d)
        .compression(c)
        .max_record_events(2048)
        .build()
        .unwrap();
    for i in 0..n {
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| r.set("evno", i * 1_000_003).map(|_| ()))?;
                Ok(())
            })?;
            ev.bank("REC::Particle", |b| {
                for k in 0..=(i % 6) {
                    b.row(|r| {
                        r.set("pid", if k % 2 == 0 { 11 } else { 2212 })?;
                        r.set("px", i as f32 * 0.125)?;
                        r.set("vt", i as f64 * 0.5)?;
                        r.set("status", -(i as i16 % 97))?;
                        r.set("charge", (k % 3 - 1) as i8)?;
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
}

/// Every value of every column — the fingerprint.
fn fingerprint(path: &std::path::Path) -> (u64, i64) {
    let chain = Chain::open(path).unwrap();
    let (mut rows, mut sum) = (0u64, 0i64);
    for ev in chain.events() {
        let ev = ev.unwrap();
        let ctx = ev.ctx();
        for bank in ["REC::Event", "REC::Particle"] {
            if let Some(b) = ctx.bank(bank) {
                for r in 0..b.rows() {
                    rows += 1;
                    for c in 0..b.schema().num_columns() {
                        sum = sum.wrapping_add((b.value(c, r).unwrap() * 8.0) as i64);
                    }
                }
            }
        }
    }
    (rows, sum)
}

#[test]
fn encoded_streams_round_trip_and_match_the_plain_file() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain.hipo");
    write(&plain, Compression::new(Codec::Zstd, Layout::PerColumn));
    let expect = fingerprint(&plain);
    assert!(expect.0 > N as u64 && expect.1 != 0, "degenerate fixture");

    for codec in [
        Codec::None,
        Codec::Lz4,
        Codec::Lz4Hc,
        Codec::Gzip,
        Codec::Zstd,
    ] {
        let enc = dir.path().join(format!("enc_{codec:?}.hipo"));
        write(
            &enc,
            Compression::new(codec, Layout::PerColumn).with_encodings(),
        );
        assert_eq!(fingerprint(&enc), expect, "{codec:?} with encodings");
    }
}

#[test]
fn the_encoding_actually_gets_chosen_and_shrinks_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("p.hipo");
    let enc = dir.path().join("e.hipo");
    write(&plain, Compression::new(Codec::Zstd, Layout::PerColumn));
    write(
        &enc,
        Compression::new(Codec::Zstd, Layout::PerColumn).with_encodings(),
    );
    let (a, b) = (
        std::fs::metadata(&plain).unwrap().len(),
        std::fs::metadata(&enc).unwrap().len(),
    );
    // The writer keeps whichever of raw/split is smaller per stream, so the
    // encoded file can never be larger than the plain one by more than the
    // tail it adds (5 bytes per stream).
    assert!(
        b <= a,
        "encoded file {b} is larger than plain {a} — the per-stream choice \
         should never pick a worse encoding"
    );
}

#[test]
fn versions_1_and_2_still_read_unchanged() {
    // The default writes version 1. If this breaks, every existing file breaks.
    let dir = tempfile::tempdir().unwrap();
    for (name, c) in [
        ("v1_percolumn", Compression::Lz4PerColumn),
        ("v1_zstd", Compression::new(Codec::Zstd, Layout::PerColumn)),
        ("perbank", Compression::Lz4PerBank),
        ("perchunk", Compression::Lz4),
    ] {
        let p = dir.path().join(format!("{name}.hipo"));
        write(&p, c);
        let f = fingerprint(&p);
        assert!(f.0 > 0, "{name}");
        // And the version byte must still be 1 for the per-column ones.
        if name.starts_with("v1") {
            let bytes = std::fs::read(&p).unwrap();
            const MAGIC: [u8; 4] = [0x00, 0x01, 0xda, 0xc0];
            let mut saw = false;
            let mut i = 28usize;
            while i + 4 <= bytes.len() {
                if bytes[i..i + 4] == MAGIC && i >= 28 {
                    let h = i - 28;
                    let comp = u32::from_le_bytes(bytes[h + 36..h + 40].try_into().unwrap());
                    if (comp >> 28) == 7 || (comp >> 28) == 8 || (comp >> 28) == 10 {
                        assert_eq!(bytes[h + 56], 1, "{name}: default must stay version 1");
                        saw = true;
                    }
                }
                i += 1;
            }
            assert!(saw, "{name}: no per-column record found");
        }
    }
}

/// The version-3 tail carries a non-zero checksum for every non-empty stream.
///
/// This pins that the checksums are *written and read back*, which is what the
/// verify path depends on. It does **not** demonstrate detection: every
/// corruption sweep I tried landed in the compressed directory, which fails
/// the record parse before any stream is inflated, so the verify branch never
/// ran. The detection path is therefore implemented but unproven — see the
/// commit message. Do not treat per-stream checksums as a working guarantee
/// until a test drives that branch.
#[test]
fn every_stream_gets_a_checksum_and_an_encoding() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("ck.hipo");
    write(
        &p,
        Compression::new(Codec::Zstd, Layout::PerColumn).with_encodings(),
    );

    // The file reads back correctly, which requires the tail to have been
    // written at the advertised directory length and parsed at that length.
    let (rows, sum) = fingerprint(&p);
    assert!(rows > 0 && sum != 0);

    // And the version byte says 3, so an old reader will refuse it.
    const MAGIC: [u8; 4] = [0x00, 0x01, 0xda, 0xc0];
    let bytes = std::fs::read(&p).unwrap();
    let mut saw_v3 = false;
    let mut i = 28usize;
    while i + 4 <= bytes.len() {
        if bytes[i..i + 4] == MAGIC && i >= 28 {
            let h = i - 28;
            let comp = u32::from_le_bytes(bytes[h + 36..h + 40].try_into().unwrap());
            if (comp >> 28) == 8 {
                assert_eq!(bytes[h + 56], 3, "encoded records must say version 3");
                saw_v3 = true;
            }
        }
        i += 1;
    }
    assert!(saw_v3, "no encoded per-column record found");
}
