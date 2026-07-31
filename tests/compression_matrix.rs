//! The (codec x layout) matrix: all 15 pairs write and read back.
//!
//! `Compression` used to be a flat list of six named combinations. It is now a
//! pair — 5 codecs x 3 layouts — and every cell has a wire tag, so every cell
//! must round-trip. This is the test that makes the matrix a matrix rather
//! than a naming change.
//!
//! Only the six that predate it (the whole-record codecs, plus LZ4-HC per bank
//! and per column) are readable by hipo-cpp and hipo-java; the rest are
//! oxihipo extensions those readers reject as an unknown tag. Their wire tags
//! are pinned in `composite_codecs.rs`, not here.

use oxihipo::{Chain, Codec, Compression, DataType, Dict, Layout, Schema, Writer};

const N: i64 = 240;

fn dict() -> Dict {
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
        [
            ("pid".into(), DataType::Int, 1),
            ("px".into(), DataType::Float, 1),
            ("charge".into(), DataType::Byte, 1),
            ("status".into(), DataType::Short, 1),
            ("vt".into(), DataType::Double, 1),
        ],
    ));
    d
}

fn write(path: &std::path::Path, c: Compression) {
    let d = dict();
    let mut w = Writer::create(path)
        .schemas(&d)
        .compression(c)
        .max_record_events(37)
        .build()
        .unwrap();
    for i in 0..N {
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| r.set("evno", i).map(|_| ()))?;
                Ok(())
            })?;
            ev.bank("REC::Particle", |b| {
                for k in 0..=(i % 5) {
                    b.row(|r| {
                        r.set("pid", (11 + k) as i32)?;
                        r.set("px", i as f32 * 0.25)?;
                        r.set("charge", (k % 3 - 1) as i8)?;
                        r.set("status", -(i as i16 % 9))?;
                        r.set("vt", i as f64 * 1.5)?;
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

/// (events, rows, checksum over every column) — the value fingerprint.
fn fingerprint(path: &std::path::Path) -> (u64, u64, i64) {
    let chain = Chain::open(path).unwrap();
    let (mut events, mut rows, mut sum) = (0u64, 0u64, 0i64);
    for ev in chain.events() {
        let ev = ev.unwrap();
        events += 1;
        let ctx = ev.ctx();
        if let Some(b) = ctx.bank("REC::Event") {
            for r in 0..b.rows() {
                sum = sum.wrapping_add(b.value_i64(0, r).unwrap());
            }
        }
        if let Some(b) = ctx.bank("REC::Particle") {
            for r in 0..b.rows() {
                rows += 1;
                for c in 0..b.schema().num_columns() {
                    // Mix every column in, so a codec that corrupts one and not
                    // the others cannot pass.
                    sum = sum.wrapping_add((b.value(c, r).unwrap() * 4.0) as i64);
                }
            }
        }
    }
    (events, rows, sum)
}

#[test]
fn every_codec_and_layout_pair_round_trips() {
    let dir = tempfile::tempdir().unwrap();

    // Ground truth: uncompressed, whole-record.
    let base_path = dir.path().join("base.hipo");
    write(&base_path, Compression::new(Codec::None, Layout::PerChunk));
    let expect = fingerprint(&base_path);
    assert_eq!(expect.0, N as u64);
    assert!(expect.1 > N as u64 && expect.2 != 0, "degenerate fixture");

    let mut sizes = Vec::new();
    for codec in [
        Codec::None,
        Codec::Lz4,
        Codec::Lz4Hc,
        Codec::Gzip,
        Codec::Zstd,
    ] {
        for layout in [Layout::PerChunk, Layout::PerBank, Layout::PerColumn] {
            let c = Compression::new(codec, layout);
            let p = dir.path().join(format!("{codec:?}_{layout:?}.hipo"));
            write(&p, c);
            assert_eq!(
                fingerprint(&p),
                expect,
                "{codec:?} x {layout:?} did not round-trip"
            );
            sizes.push((codec, layout, std::fs::metadata(&p).unwrap().len()));
        }
    }
    assert_eq!(sizes.len(), 15, "all 15 pairs must be exercised");

    // Every compressing pair must actually be smaller than uncompressed —
    // otherwise a codec silently doing nothing would still pass above.
    let uncompressed = sizes
        .iter()
        .find(|(c, l, _)| matches!((c, l), (Codec::None, Layout::PerChunk)))
        .unwrap()
        .2;
    for (codec, layout, size) in &sizes {
        if matches!(codec, Codec::None) {
            continue;
        }
        assert!(
            *size < uncompressed,
            "{codec:?} x {layout:?} produced {size} bytes, not smaller than \
             uncompressed {uncompressed} — is it compressing at all?"
        );
    }
}

#[test]
fn zstd_levels_all_read_back_through_one_tag() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("b.hipo");
    write(&base, Compression::None);
    let expect = fingerprint(&base);

    let mut last = u64::MAX;
    for level in 1u8..=6 {
        for layout in [Layout::PerChunk, Layout::PerBank, Layout::PerColumn] {
            let c = Compression::new(Codec::Zstd, layout).with_zstd_level(level);
            assert_eq!(c.zstd_level(), level);
            let p = dir.path().join(format!("z{level}_{layout:?}.hipo"));
            write(&p, c);
            // The level never reaches the wire: one tag decodes them all.
            assert_eq!(fingerprint(&p), expect, "zstd level {level} {layout:?}");
        }
        let p = dir.path().join(format!("z{level}_PerColumn.hipo"));
        last = std::fs::metadata(&p).unwrap().len();
    }
    assert!(last > 0);

    // Out-of-range levels clamp rather than panicking or reaching zstd.
    assert_eq!(Compression::None.with_zstd_level(0).zstd_level(), 1);
    assert_eq!(Compression::None.with_zstd_level(99).zstd_level(), 6);
}

#[test]
fn the_six_historical_names_still_mean_what_they_meant() {
    assert_eq!(
        Compression::None,
        Compression::new(Codec::None, Layout::PerChunk)
    );
    assert_eq!(
        Compression::Lz4,
        Compression::new(Codec::Lz4, Layout::PerChunk)
    );
    assert_eq!(
        Compression::Lz4Best,
        Compression::new(Codec::Lz4Hc, Layout::PerChunk)
    );
    assert_eq!(
        Compression::Gzip,
        Compression::new(Codec::Gzip, Layout::PerChunk)
    );
    // The two split codecs were always LZ4-**HC**, not plain LZ4.
    assert_eq!(
        Compression::Lz4PerBank,
        Compression::new(Codec::Lz4Hc, Layout::PerBank)
    );
    assert_eq!(
        Compression::Lz4PerColumn,
        Compression::new(Codec::Lz4Hc, Layout::PerColumn)
    );
}
