//! Deterministic replay of malformed inputs through the fuzz entry points.
//!
//! The `fuzz/` targets need nightly, so they don't run in ordinary CI. This
//! harness drives the *same* entry points with a fixed corpus of hostile inputs
//! on stable, as a normal `cargo test`. Two jobs:
//!
//! 1. Cover the malformed-input paths on every CI platform, not just wherever a
//!    fuzzer happens to be run.
//! 2. Be the home for **minimized fuzz findings**: when a target crashes, add
//!    the reduced input here so the fix is locked in permanently.
//!
//! The contract under test is simply: *no input may panic or abort.* Every case
//! must come back as `Err` or an empty/None result.

use oxihipo::{Chain, Schema};

/// Write `bytes` to a temp file and run the full open + read path over it.
/// Returns `true` if the chain opened (most inputs won't).
fn try_open(bytes: &[u8]) -> bool {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("input.hipo");
    std::fs::write(&path, bytes).unwrap();
    let Ok(chain) = Chain::open(&path) else {
        return false;
    };
    // Touch everything a reader would: events, banks, columns, random access.
    for (n, ev) in chain.events().enumerate() {
        if n >= 32 {
            break;
        }
        let Ok(ev) = ev else { break };
        let _ = ev.tag();
        for schema in chain.schemas().iter() {
            if let Some(bank) = ev.bank(schema.name()) {
                let rows = bank.rows().min(32);
                for e in schema.entries() {
                    for r in 0..rows {
                        let _ = bank.get::<i64>(&e.name, r);
                        let _ = bank.get::<f64>(&e.name, r);
                    }
                }
            }
        }
    }
    let _ = chain.event(0);
    let _ = chain.event(u64::MAX);
    true
}

/// A minimal, valid-looking file header: the "HIPO" unique word, a 14-word
/// header length, version 6 in bit_info, and the little-endian magic. Enough to
/// get past `FileHeader::parse` so the mutations below reach deeper code.
fn plausible_file_header() -> Vec<u8> {
    let mut b = vec![0u8; 56];
    b[0..4].copy_from_slice(&0x4F50_4948u32.to_le_bytes()); // "HIPO"
    b[8..12].copy_from_slice(&14u32.to_le_bytes()); // header length (words)
    b[20..24].copy_from_slice(&6u32.to_le_bytes()); // bit_info: version 6
    b[28..32].copy_from_slice(&0xc0da_0100u32.to_le_bytes()); // LE magic
    b
}

#[test]
fn empty_and_tiny_inputs_do_not_panic() {
    for n in 0..64usize {
        try_open(&vec![0u8; n]);
        try_open(&vec![0xFFu8; n]);
    }
}

#[test]
fn random_looking_bytes_do_not_panic() {
    // Deterministic pseudo-random (no rand dependency): a simple LCG.
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    for len in [16usize, 56, 57, 128, 512, 4096] {
        let mut buf = Vec::with_capacity(len);
        for _ in 0..len {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            buf.push((state >> 33) as u8);
        }
        try_open(&buf);
    }
}

#[test]
fn plausible_header_with_hostile_lengths_do_not_panic() {
    // Walk each u32 field of an otherwise-plausible header through extreme
    // values. These are exactly the fields that drive allocation sizes, slice
    // bounds, and loop counts in the record decoder.
    let hostile = [
        0u32,
        1,
        2,
        u32::MAX,
        u32::MAX - 1,
        0x7FFF_FFFF,
        0x00FF_FFFF,
        1 << 24,
        1 << 30,
    ];
    for field in [4usize, 8, 12, 16, 20, 24, 32, 36, 40, 44, 48, 52] {
        for v in hostile {
            let mut b = plausible_file_header();
            b[field..field + 4].copy_from_slice(&v.to_le_bytes());
            try_open(&b);
            // …and again with a record-sized tail, so a header claiming a huge
            // payload has some bytes to (mis)read.
            let mut with_tail = b.clone();
            with_tail.extend(std::iter::repeat_n(0xABu8, 256));
            try_open(&with_tail);
        }
    }
}

#[test]
fn truncated_at_every_length_does_not_panic() {
    // Build a real file, then truncate it at every byte boundary. Catches
    // "reads past the end of a legitimately-shaped file" bugs.
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("real.hipo");
    {
        use oxihipo::{Compression, DataType, Dict, Writer};
        let mut d = Dict::new();
        d.add(Schema::from_columns(
            "T",
            300,
            1,
            [
                ("x".into(), DataType::Int, 1),
                ("cov".into(), DataType::Float, 3),
            ],
        ));
        let mut w = Writer::create(&src)
            .schemas(&d)
            .compression(Compression::None)
            .max_record_events(2)
            .build()
            .unwrap();
        for i in 0..6 {
            w.event(|ev| {
                ev.bank("T", |b| {
                    b.row(|r| {
                        r.set("x", i)?;
                        r.set("cov", [i as f32, 0.5, -1.0])?;
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
    let full = std::fs::read(&src).unwrap();
    // Every length, plus every single-byte corruption at a stride.
    for n in 0..full.len() {
        try_open(&full[..n]);
    }
    for i in (0..full.len()).step_by(7) {
        let mut b = full.clone();
        b[i] ^= 0xFF;
        try_open(&b);
    }
}

#[test]
fn hostile_schema_text_does_not_panic() {
    let cases = [
        "",
        "{",
        "}",
        "{}{}",
        "{X/1/1}",
        "{X/1/1}{}",
        "{X/1/1}{a}",
        "{X/1/1}{a/}",
        "{X/1/1}{/I}",
        "{X/1/1}{a/Z}",
        "{X/1/1}{a/I#}",
        "{X/1/1}{a/I#0}",
        "{X/1/1}{a/I#-1}",
        "{X/1/1}{a/I#4294967295}",
        "{X/1/1}{a/D#999999999}",
        "{X/99999/1}{a/I}",
        "{X/1/99999}{a/I}",
        "{X/-1/-1}{a/I}",
        "{/1/1}{a/I}",
        "{X}{a/I}",
        "{X/1}{a/I}",
        "{X/1/1/1}{a/I}",
        "\0\0\0",
        "{\u{1F600}/1/1}{a/I}",
        // JSON form
        "{}",
        r#"{"name":"X"}"#,
        r#"{"name":"X","group":1,"item":1,"entries":[]}"#,
        r#"{"name":"X","group":1,"item":1,"entries":[{"name":"a","type":"I#0"}]}"#,
        r#"{"name":"X","group":99999,"item":1,"entries":[{"name":"a","type":"I"}]}"#,
        r#"{"name":"X","group":1,"item":1,"entries":[{"name":"a","type":"D#99999999"}]}"#,
    ];
    for c in cases {
        // Both parsers must return without panicking; a success must be
        // self-consistent.
        for parsed in [Schema::parse_text(c), Schema::parse_json(c)]
            .into_iter()
            .flatten()
        {
            let sum: u64 = parsed
                .entries()
                .iter()
                .map(|e| e.ty.size() as u64 * e.length as u64)
                .sum();
            assert_eq!(sum, parsed.row_size() as u64, "row_size mismatch for {c:?}");
        }
    }
}

#[test]
fn composite_accessors_are_bounds_safe() {
    // Composite getters used to index `fields` / `data` directly, so an
    // out-of-range field or row aborted the process. They must return the
    // type's default instead, matching the lenient `Bank::get`.
    use oxihipo::event::{Composite, CompositeFormat};
    let format = CompositeFormat::parse("ilf").unwrap();
    // Two rows' worth of zeroed data.
    let data = vec![0u8; format.row_size() as usize * 2];
    let c = Composite::from_parts(format, &data).unwrap();
    for field in [0usize, 1, 2, 3, 99, usize::MAX] {
        for row in [0u32, 1, 2, 1000, u32::MAX] {
            let _ = c.i8(field, row);
            let _ = c.i16(field, row);
            let _ = c.i32(field, row);
            let _ = c.i64(field, row);
            let _ = c.f32(field, row);
            let _ = c.f64(field, row);
        }
    }
}

#[test]
fn bank_builder_rejects_out_of_range_rows() {
    // `set_*_at` indexed the column buffer directly; a row past the pushed
    // count sliced out of bounds. It must be an error, not a panic.
    use oxihipo::DataType;
    use oxihipo::event::BankBuilder;

    let schema = Schema::from_columns(
        "T",
        1,
        1,
        [
            ("x".into(), DataType::Int, 1),
            ("arr".into(), DataType::Float, 2),
        ],
    );
    let mut b = BankBuilder::with_row_capacity(&schema, 2);
    b.push_rows(2);
    assert!(b.set_i32_at("x", 0, 7).is_ok());
    assert!(b.set_i32_at("x", 1, 8).is_ok());
    for bad in [2u32, 3, 1000, u32::MAX] {
        assert!(b.set_i32_at("x", bad, 9).is_err(), "row {bad} must Err");
        assert!(
            b.set_array_at("arr", bad, &[1.0f32, 2.0]).is_err(),
            "array row {bad} must Err"
        );
    }
}
