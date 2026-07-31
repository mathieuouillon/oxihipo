//! Big-endian read support.
//!
//! No current writer produces big-endian HIPO, so these tests build one: an
//! ordinary little-endian file is written with `Compression::None` (so the
//! payload sits verbatim in the file) and then **transcoded** to big-endian
//! byte-for-byte by the helper below — an independent walk of the format that
//! swaps every multi-byte field and flips the header magic markers.
//!
//! The reader must then return exactly the values that were written. Asserting
//! concrete expected values (not merely "LE and BE agree") is what makes this a
//! real test: a shared misconception between the transcoder and the reader's
//! normalizer would have to reproduce the exact original numbers to pass.

use oxihipo::{Chain, Compression, DataType, Dict, Result, Schema, Writer};

const N_EVENTS: i64 = 12;

fn dict() -> Dict {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [("evno".into(), DataType::Long, 1)],
    ));
    // One column of every scalar width, plus a fixed-length array column.
    d.add(Schema::from_columns(
        "Test::Types",
        400,
        1,
        [
            ("b".into(), DataType::Byte, 1),
            ("s".into(), DataType::Short, 1),
            ("i".into(), DataType::Int, 1),
            ("l".into(), DataType::Long, 1),
            ("f".into(), DataType::Float, 1),
            ("d".into(), DataType::Double, 1),
            ("arr".into(), DataType::Int, 3),
        ],
    ));
    d
}

fn n_rows(evno: i64) -> i32 {
    (evno % 4) as i32 // some events have an empty Test::Types bank
}

fn write_le(path: &std::path::Path) -> Result<()> {
    let mut w = Writer::create(path)
        .schemas(&dict())
        .compression(Compression::None)
        .max_record_events(5) // several records
        .build()?;
    for evno in 0..N_EVENTS {
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| {
                    r.set("evno", 1000 + evno)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            ev.bank("Test::Types", |b| {
                for k in 0..n_rows(evno) {
                    b.row(|r| {
                        r.set("b", (evno as i8).wrapping_add(k as i8))?;
                        r.set("s", (evno * 7 + k as i64) as i16)?;
                        r.set("i", (evno * 1000 + k as i64) as i32)?;
                        r.set("l", evno * 1_000_000 + k as i64)?;
                        r.set("f", evno as f32 * 0.5 + k as f32)?;
                        r.set("d", evno as f64 * 0.25 + k as f64)?;
                        r.set("arr", [k, k + 1, k + 2])?;
                        Ok(())
                    })?;
                }
                Ok(())
            })?;
            Ok(())
        })?;
    }
    w.finish()?;
    Ok(())
}

// ---- the transcoder: little-endian file bytes -> big-endian ---------------

const FILE_HEADER: usize = 56;
const REC_HEADER: usize = 56;

fn rd32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn swap(b: &mut [u8], off: usize, width: usize) {
    b[off..off + width].reverse();
}
fn swap_each(b: &mut [u8], elem: usize) {
    for c in b.chunks_exact_mut(elem) {
        c.reverse();
    }
}

/// Byte-swap one structure's payload given the schemas we wrote (column-major)
/// or the trailer's fixed file-index layout.
fn swap_structure(data: &mut [u8], group: u16, item: u8) {
    // (group, item) -> column widths, in schema order, with array length.
    let cols: &[(usize, usize)] = match (group, item) {
        (300, 30) => &[(8, 1)], // evno/L
        (400, 1) => &[(1, 1), (2, 1), (4, 1), (8, 1), (4, 1), (8, 1), (4, 3)],
        (32111, 1) => {
            // trailer file::index — position/L length/I entries/I uw1/L uw2/L
            let rows = data.len() / 32;
            let mut off = 0;
            for w in [8usize, 4, 4, 8, 8] {
                let span = rows * w;
                swap_each(&mut data[off..off + span], w);
                off += span;
            }
            return;
        }
        _ => return, // dictionary strings and anything else: char data
    };
    let row_size: usize = cols.iter().map(|(w, n)| w * n).sum();
    if row_size == 0 {
        return;
    }
    let rows = data.len() / row_size;
    let mut off = 0;
    for (w, n) in cols {
        let span = rows * w * n;
        swap_each(&mut data[off..off + span], *w);
        off += span;
    }
}

fn swap_event(ev: &mut [u8]) {
    // header: magic(4 chars, left as-is) size(u32) tag(u32) reserved(u32)
    let size = rd32(ev, 4) as usize;
    swap(ev, 4, 4);
    swap(ev, 8, 4);
    swap(ev, 12, 4);
    let end = size.min(ev.len());
    let mut pos = 16;
    while pos + 8 <= end {
        let group = u16::from_le_bytes([ev[pos], ev[pos + 1]]);
        let item = ev[pos + 2];
        let len_word = rd32(ev, pos + 4);
        let data_size = (len_word & 0x00FF_FFFF) as usize;
        swap(ev, pos, 2); // group
        swap(ev, pos + 4, 4); // length word
        let ds = pos + 8;
        let de = ds + data_size;
        if de > end {
            break;
        }
        swap_structure(&mut ev[ds..de], group, item);
        pos = de;
    }
}

/// Transcode a whole uncompressed HIPO file to big-endian.
fn transcode_to_be(bytes: &mut [u8]) {
    // ---- file header ----
    for off in [4usize, 8, 12, 16, 20, 24, 48, 52] {
        swap(bytes, off, 4); // u32 fields ("HIPO" unique word at 0 stays)
    }
    swap(bytes, 32, 8); // user_register
    let trailer_pos = u64::from_le_bytes(bytes[40..48].try_into().unwrap()) as usize;
    swap(bytes, 40, 8); // trailer_position
    bytes[28..32].reverse(); // magic -> big-endian marker

    // ---- every record (dictionary, data records, trailer) ----
    let mut off = FILE_HEADER;
    while off + REC_HEADER <= bytes.len() {
        let rec_len = rd32(bytes, off) as usize * 4;
        let event_count = rd32(bytes, off + 12) as usize;
        let index_len = rd32(bytes, off + 16) as usize;
        let bit_info = rd32(bytes, off + 20);
        let user_hdr_len = rd32(bytes, off + 24) as usize;
        let hdr_len = rd32(bytes, off + 8) as usize * 4;
        let user_pad = ((bit_info >> 20) & 0x3) as usize;
        if rec_len == 0 || off + rec_len > bytes.len() {
            break;
        }
        let payload = off + hdr_len;

        // Walk the payload BEFORE swapping the header words we still need.
        let data_start = payload + index_len + user_hdr_len + user_pad;
        let mut ev_off = data_start;
        for i in 0..event_count {
            let ev_size = rd32(bytes, payload + i * 4) as usize;
            if ev_off + ev_size > bytes.len() {
                break;
            }
            swap_event(&mut bytes[ev_off..ev_off + ev_size]);
            ev_off += ev_size;
        }
        // index array
        swap_each(&mut bytes[payload..payload + index_len], 4);

        // record header words
        for f in [0usize, 4, 8, 12, 16, 20, 24, 32, 36] {
            swap(bytes, off + f, 4);
        }
        swap(bytes, off + 40, 8);
        swap(bytes, off + 48, 8);
        bytes[off + 28..off + 32].reverse(); // magic

        if off == trailer_pos {
            break;
        }
        off += rec_len;
    }
}

fn make_be_file(dir: &std::path::Path) -> std::path::PathBuf {
    let le = dir.join("le.hipo");
    write_le(&le).unwrap();
    let mut bytes = std::fs::read(&le).unwrap();
    transcode_to_be(&mut bytes);
    let be = dir.join("be.hipo");
    std::fs::write(&be, &bytes).unwrap();
    be
}

// ---- the tests -------------------------------------------------------------

#[test]
fn big_endian_file_opens_with_correct_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let be = make_be_file(dir.path());
    let chain = Chain::open(&be).unwrap();
    assert_eq!(chain.event_count(), N_EVENTS as u64);
    assert_eq!(chain.schemas().len(), 2);
    assert!(chain.schemas().get("Test::Types").is_some());
}

#[test]
fn big_endian_values_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let be = make_be_file(dir.path());
    let chain = Chain::open(&be).unwrap();

    let mut seen = 0i64;
    for ev in chain.events() {
        let ev = ev.unwrap();
        let evno = seen;
        assert_eq!(
            ev.bank("REC::Event").unwrap().get::<i64>("evno", 0),
            1000 + evno,
            "evno (i64) at event {evno}"
        );
        let t = ev.bank("Test::Types").unwrap();
        assert_eq!(t.rows() as i32, n_rows(evno), "row count at event {evno}");
        for k in 0..n_rows(evno) {
            let r = k as u32;
            assert_eq!(t.get::<i8>("b", r), (evno as i8).wrapping_add(k as i8));
            assert_eq!(t.get::<i16>("s", r), (evno * 7 + k as i64) as i16);
            assert_eq!(t.get::<i32>("i", r), (evno * 1000 + k as i64) as i32);
            assert_eq!(t.get::<i64>("l", r), evno * 1_000_000 + k as i64);
            assert_eq!(t.get::<f32>("f", r), evno as f32 * 0.5 + k as f32);
            assert_eq!(t.get::<f64>("d", r), evno as f64 * 0.25 + k as f64);
            let arr = t.array_at::<i32>("arr", r).unwrap();
            assert_eq!(&arr[..], &[k, k + 1, k + 2], "array column at {evno}/{k}");
        }
        seen += 1;
    }
    assert_eq!(seen, N_EVENTS);
}

#[test]
fn big_endian_random_access_and_columns() {
    let dir = tempfile::tempdir().unwrap();
    let be = make_be_file(dir.path());
    let chain = Chain::open(&be).unwrap();

    // Random access (decodes one record on its own path).
    for idx in [0u64, 5, 7, (N_EVENTS - 1) as u64] {
        let ev = chain.event(idx).expect("event in range");
        assert_eq!(
            ev.bank("REC::Event").unwrap().get::<i64>("evno", 0),
            1000 + idx as i64
        );
    }

    // Zero-copy column slice — the path that reinterprets bytes wholesale.
    let mut total = 0usize;
    for (evno, ev) in chain.events().map(|e| e.unwrap()).enumerate() {
        let t = ev.bank("Test::Types").unwrap();
        let col = t.col::<i32>("i").unwrap();
        for (k, v) in col.iter().enumerate() {
            assert_eq!(
                *v,
                (evno as i64 * 1000 + k as i64) as i32,
                "zero-copy column at {evno}/{k}"
            );
        }
        total += t.rows() as usize;
    }
    assert!(total > 0);

    // Columnar materializer across the whole chain.
    let cols = chain
        .read_columns(&[("REC::Event", &["evno"][..])], None, 1)
        .unwrap();
    let cb = &cols[0];
    assert_eq!(cb.bank, "REC::Event");
    assert_eq!(cb.offsets.len() as i64, N_EVENTS + 1);
    assert_eq!(cb.columns.len(), 1);
}

/// The split codecs must **reject** a big-endian record, not decode it as
/// little-endian.
///
/// The transcoder above is only wired to `Compression::None` — line 51 is this
/// file's sole `.compression(...)` call — so until now no big-endian test
/// touched `Lz4`, `Gzip`, or either split codec. That matters because
/// `wire/endian.rs` is wired into the whole-record paths only
/// (`decode_record_into`, `Record::load_with_header`); neither
/// `ByBankRecord::parse` nor `PerColumnRecord::parse` goes through them, and
/// every field of a split section is read with `read_u32_le` unconditionally.
///
/// `RecordHeader::parse` accepts the big-endian magic, so such a record used
/// to reach the split parsers with `endianness == Big`. It did not actually
/// mis-parse — an unrelated `event_count` cross-check happened to stop it —
/// but it stopped for the wrong reason and reported the wrong cause. Now the
/// refusal is explicit.
///
/// Nothing can write such a file: only oxihipo emits tags 6 and 7, always
/// little-endian. The record here is built by byte-swapping a real record
/// header word for word, which is precisely what a big-endian writer would
/// have produced for the header.
#[test]
fn split_codecs_reject_a_big_endian_record() {
    const RH_MAGIC_OFFSET: usize = 28;
    const RH_COMP_WORD: usize = 36;
    const RH_USER_WORD1: usize = 40; // u64
    const RH_USER_WORD2: usize = 48; // u64
    const RECORD_HEADER_SIZE: usize = 56;
    const MAGIC_LE: [u8; 4] = [0x00, 0x01, 0xda, 0xc0]; // 0xc0da_0100

    for (label, compression) in [
        ("Lz4PerBank", Compression::Lz4PerBank),
        ("Lz4PerColumn", Compression::Lz4PerColumn),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("be.hipo");
        let d = dict();
        let mut w = Writer::create(&path)
            .schemas(&d)
            .compression(compression)
            .max_record_events(5)
            .build()
            .unwrap();
        for evno in 0..N_EVENTS {
            w.event(|ev| {
                ev.bank("REC::Event", |b| {
                    b.row(|r| r.set("evno", evno).map(|_| ()))?;
                    Ok(())
                })?;
                Ok(())
            })
            .unwrap();
        }
        w.finish().unwrap();

        // Rewrite each *data* record header the way a big-endian writer
        // would: the ten u32 fields byte-swapped in place, and the two
        // trailing u64 user words swapped as 8-byte units — a uniform u32
        // swap would corrupt those and the header would not read back. The
        // magic sits in the u32 range, so swapping it is what flips the
        // marker to big-endian and gets the record past the compression-tag
        // check and on to the new guard.
        let mut bytes = std::fs::read(&path).unwrap();
        let mut patched = 0usize;
        let mut i = 0usize;
        while i + 4 <= bytes.len() {
            if bytes[i..i + 4] == MAGIC_LE && i >= RH_MAGIC_OFFSET {
                let hdr = i - RH_MAGIC_OFFSET;
                if hdr + RECORD_HEADER_SIZE <= bytes.len() {
                    // Only the data records carry the split tag; this skips
                    // the file header and the dictionary record.
                    let comp = u32::from_le_bytes(
                        bytes[hdr + RH_COMP_WORD..hdr + RH_COMP_WORD + 4]
                            .try_into()
                            .unwrap(),
                    );
                    let codec = (comp >> 28) & 0xF;
                    if codec == 6 || codec == 7 {
                        for off in (0..RH_USER_WORD1).step_by(4) {
                            let o = hdr + off;
                            bytes[o..o + 4].reverse();
                        }
                        for off in [RH_USER_WORD1, RH_USER_WORD2] {
                            let o = hdr + off;
                            bytes[o..o + 8].reverse();
                        }
                        patched += 1;
                    }
                }
            }
            i += 1;
        }
        assert!(
            patched > 0,
            "{label}: no split-codec record header was found, so nothing was exercised"
        );
        std::fs::write(&path, &bytes).unwrap();

        // Opening may or may not survive the mangled index; what must never
        // happen is a successful decode of a big-endian split record.
        let msg = match Chain::open(&path) {
            Err(e) => e.to_string(),
            Ok(chain) => match chain.events().find_map(Result::err) {
                Some(e) => e.to_string(),
                None => panic!("{label}: a big-endian split record decoded successfully"),
            },
        };
        assert!(
            msg.contains("big-endian records are not supported"),
            "{label}: expected the explicit endianness refusal, got: {msg}"
        );
        assert!(
            msg.contains(label),
            "{label}: error should name the codec: {msg}"
        );
    }
}
