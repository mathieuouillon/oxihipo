//! Composite banks must survive every codec.
//!
//! A composite bank carries its format string inside the bank, and the only
//! thing marking it as composite is the **top byte** of the structure length
//! word (`header_size` — the format string's length in bytes). The split
//! codecs, `Lz4PerBank` and `Lz4PerColumn`, take a record apart and store bank
//! payloads separately, discarding the structure headers; the descriptor in the
//! record directory is what rebuilds them. Until the directory grew a
//! `header_size` field, that byte was rebuilt as zero, so a composite bank came
//! back looking like an ordinary one and `composite()` returned `None`.
//!
//! Two composites are written, differing only in how much their format string
//! is padded. That matters for `Lz4PerColumn`, which stores a bank column-major
//! when a schema describes it and every event holds a whole number of rows —
//! `pad4` fails that row-count check by accident, `pad8` passes it, so only
//! `pad8` reaches the composite guard that keeps such a bank opaque.

use oxihipo::event::{BankBuilder, EventBuilder};
use oxihipo::{Chain, Compression, DataType, Dict, Schema, Writer};

/// Every codec, including the two that split records apart.
const CODECS: [(&str, Compression); 4] = [
    ("None", Compression::None),
    ("Lz4", Compression::Lz4),
    ("Lz4PerBank", Compression::Lz4PerBank),
    ("Lz4PerColumn", Compression::Lz4PerColumn),
];

/// Three rows of `(i32, f32)` — the values we expect to read back. The `"if"`
/// format gives an 8-byte row, so the data is `format.len() + 24` bytes.
const ROWS: [(i32, f32); 3] = [(10, 1.5), (20, 2.5), (30, 3.5)];

/// The two composites: name, group, item, format string. Both decode to the
/// same rows; only the padding differs, and with it whether the total data size
/// is a whole number of 8-byte rows (32 is, 28 is not).
const COMPOSITES: [(&str, u16, u8, &[u8]); 2] = [
    ("C::pad4", 700, 5, b"if\0\0"),
    ("C::pad8", 701, 6, b"if\0\0\0\0\0\0"),
];

/// Raw bytes of one composite bank: `group|item|type|length`, then the
/// NUL-padded format string, then the rows. The length word packs the data size
/// into its low 24 bits and the format string's length into its top byte.
fn composite_bank_bytes(group: u16, item: u8, format: &[u8]) -> Vec<u8> {
    let mut data = format.to_vec();
    for (i, f) in ROWS {
        data.extend_from_slice(&i.to_le_bytes());
        data.extend_from_slice(&f.to_le_bytes());
    }

    let mut v = Vec::new();
    v.extend_from_slice(&group.to_le_bytes());
    v.push(item);
    v.push(DataType::Byte as u8);
    let length = (data.len() as u32) | ((format.len() as u32) << 24);
    v.extend_from_slice(&length.to_le_bytes());
    v.extend_from_slice(&data);
    v
}

fn dict() -> Dict {
    let mut d = Dict::new();
    // An ordinary bank alongside the composites, so the record holds more than
    // one kind of bank and the split directories lay out a real column set too.
    d.add(Schema::from_columns(
        "A::b",
        300,
        1,
        [("x".into(), DataType::Int, 1)],
    ));
    // Each composite needs a name for `composite()` to look it up. Giving them
    // schemas is the harder case for `Lz4PerColumn`: it has to notice they are
    // composite and store them opaquely anyway.
    for (name, group, item, _) in COMPOSITES {
        d.add(Schema::from_columns(
            name,
            group,
            item,
            [
                ("f0".into(), DataType::Int, 1),
                ("f1".into(), DataType::Float, 1),
            ],
        ));
    }
    d
}

fn write_file(path: &std::path::Path, codec: Compression) {
    let d = dict();
    let mut w = Writer::create(path)
        .schemas(&d)
        .compression(codec)
        // Two events per record, so the four events span several records and
        // the directory is built more than once.
        .max_record_events(2)
        .build()
        .unwrap();
    let banks: Vec<Vec<u8>> = COMPOSITES
        .iter()
        .map(|&(_, g, i, f)| composite_bank_bytes(g, i, f))
        .collect();
    let ordinary = d.get("A::b").unwrap();
    for i in 0..4i32 {
        let mut bb = BankBuilder::new(ordinary);
        bb.push_row().set_i32("x", i).unwrap();
        let mut eb = EventBuilder::new();
        eb.add(bb).unwrap();
        for bank in &banks {
            eb.add_bank_bytes(bank);
        }
        w.append_raw(&eb.finish()).unwrap();
    }
    w.finish().unwrap();
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn composite_header_size_survives_every_codec() {
    let dir = scratch("oxihipo_composite_codecs_hdr");

    for (codec_name, codec) in CODECS {
        let path = dir.join(format!("{codec_name}.hipo"));
        write_file(&path, codec);

        let chain = Chain::open(&path).unwrap();
        let mut events = 0;
        for ev in chain.events() {
            let ev = ev.unwrap();
            for (name, group, item, format) in COMPOSITES {
                let (hdr, data) = ev
                    .structures()
                    .find(|(h, _)| h.group == group && h.item == item)
                    .unwrap_or_else(|| panic!("{codec_name}/{name}: bank missing from the event"));
                assert_eq!(
                    hdr.header_size as usize,
                    format.len(),
                    "{codec_name}/{name}: header_size lost — the bank no longer reads as composite"
                );
                assert_eq!(
                    data.len(),
                    format.len() + ROWS.len() * 8,
                    "{codec_name}/{name}: data size wrong"
                );
            }
            events += 1;
        }
        assert_eq!(events, 4, "{codec_name}: wrong event count");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn composite_values_decode_under_every_codec() {
    let dir = scratch("oxihipo_composite_codecs_val");

    for (codec_name, codec) in CODECS {
        let path = dir.join(format!("{codec_name}.hipo"));
        write_file(&path, codec);

        let chain = Chain::open(&path).unwrap();
        let mut events = 0;
        for ev in chain.events() {
            let ev = ev.unwrap();
            for (name, ..) in COMPOSITES {
                let c = ev
                    .composite(name)
                    .unwrap_or_else(|| panic!("{codec_name}/{name}: composite() returned None"));
                assert_eq!(
                    c.rows(),
                    ROWS.len() as u32,
                    "{codec_name}/{name}: row count"
                );
                assert_eq!(c.format().row_size(), 8, "{codec_name}/{name}: row size");
                for (row, (i, f)) in ROWS.iter().enumerate() {
                    let row = row as u32;
                    assert_eq!(c.i32(0, row), *i, "{codec_name}/{name}: field 0 row {row}");
                    assert_eq!(c.f32(1, row), *f, "{codec_name}/{name}: field 1 row {row}");
                }
            }
            events += 1;
        }
        assert_eq!(events, 4, "{codec_name}: wrong event count");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// The split codecs' on-disk `ext_format_version` must stay **2** (by-bank) and
/// **1** (by-column).
///
/// The composite `header_size` table is appended after every other directory
/// table, so a reader that predates it never looks that far and is unaffected.
/// 0.7.0 bumped the versions anyway, and that broke the C++ and Java
/// implementations of these codecs — `hipo-cpp` segfaulted and `hipo-java`
/// threw `failed to decode ByBank record section`, both on files whose *data*
/// they parse perfectly once the version byte is put back. Measured against
/// `hipo-cpp`/`hipo-java` `feature/bybank-bycolumn-compression`, which document
/// exactly these two version numbers.
///
/// So the version byte is a compatibility contract with those readers, not a
/// private detail. The library detects the tail by directory length instead.
#[test]
fn split_codec_format_versions_are_unchanged() {
    let dir = scratch("oxihipo_composite_codecs_ver");

    // (codec, wire compression tag, required ext_format_version)
    for (codec, tag, want) in [(Compression::Lz4PerBank, 6u8, 2u8), (Compression::Lz4PerColumn, 7, 1)] {
        let path = dir.join(format!("{codec:?}.hipo"));
        write_file(&path, codec);
        let bytes = std::fs::read(&path).unwrap();

        // Walk records from the file header to the first split-codec record.
        let mut off = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize * 4;
        let mut found = None;
        while off + 56 <= bytes.len() {
            if bytes[off + 28..off + 32] != [0x00, 0x01, 0xda, 0xc0] {
                break;
            }
            let comp = u32::from_le_bytes(bytes[off + 36..off + 40].try_into().unwrap());
            if ((comp >> 28) & 0xF) as u8 == tag {
                found = Some(bytes[off + 56]); // first payload byte = ext_format_version
                break;
            }
            let rl = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize * 4;
            if rl == 0 {
                break;
            }
            off += rl;
        }

        assert_eq!(
            found,
            Some(want),
            "{codec:?}: ext_format_version must stay {want} — the C++ and Java \
             readers implement that version and break on anything else"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
