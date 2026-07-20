//! High-level, end-to-end coverage across **every** compression format.
//!
//! The other round-trip tests each exercise one or two formats; this one asserts
//! that all six write→read paths — and a cross-format `skim` between them —
//! decode to byte-identical data, including a fixed-length array column (`cov`,
//! which the per-column encoder stores specially). If any format's writer or
//! reader corrupts a value, or a `skim` re-encode changes the data, one of these
//! fails.

use oxihipo::{Chain, Compression, DataType, Dict, Result, Schema, Writer};

/// Every writable compression format, by name.
const FORMATS: &[(&str, Compression)] = &[
    ("None", Compression::None),
    ("Lz4", Compression::Lz4),
    ("Lz4Best", Compression::Lz4Best),
    ("Gzip", Compression::Gzip),
    ("Lz4PerBank", Compression::Lz4PerBank),
    ("Lz4PerColumn", Compression::Lz4PerColumn),
];

fn dict() -> Dict {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [
            ("evno".into(), DataType::Long, 1),
            ("beamE".into(), DataType::Float, 1),
        ],
    ));
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        31,
        [
            ("pid".into(), DataType::Int, 1),
            ("px".into(), DataType::Float, 1),
            ("py".into(), DataType::Float, 1),
            ("charge".into(), DataType::Byte, 1),
            // A fixed-length array column — the per-column encoder handles these
            // on a separate path, so it's worth carrying through every format.
            ("cov".into(), DataType::Float, 3),
        ],
    ));
    d
}

/// One event's data, in a form that compares across formats regardless of
/// codec. Floats are compared by bit pattern (exact round-trip is required).
#[derive(Debug, PartialEq, Eq, Clone)]
struct EventSnap {
    evno: i64,
    beam_bits: u32,
    // (pid, px_bits, py_bits, charge, [cov0,cov1,cov2] bits)
    parts: Vec<(i32, u32, u32, i8, [u32; 3])>,
}

const N_EVENTS: i64 = 120;

fn n_parts(evno: i64) -> i32 {
    (evno % 5) as i32 // 0..=4 — exercises empty and non-empty particle banks
}

fn write_file(path: &std::path::Path, compression: Compression) -> Result<()> {
    let mut w = Writer::create(path)
        .schemas(&dict())
        .compression(compression)
        .build()?;
    for evno in 0..N_EVENTS {
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| {
                    r.set("evno", evno)?;
                    r.set("beamE", 10.6_f32 + evno as f32 * 0.01)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            ev.bank("REC::Particle", |b| {
                for i in 0..n_parts(evno) {
                    b.row(|r| {
                        r.set("pid", 11 + i)?;
                        r.set("px", i as f32 * 0.1 - 1.0)?;
                        r.set("py", -(i as f32) * 0.25)?;
                        r.set("charge", (i as i8) - 1)?;
                        r.set("cov", [i as f32, i as f32 + 0.5, -(i as f32)])?;
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

/// Read a whole file into a comparable snapshot.
fn read_snapshot(path: &std::path::Path) -> Result<Vec<EventSnap>> {
    let chain = Chain::open(path)?;
    let mut out = Vec::new();
    for ev in chain.events() {
        let ev = ev?;
        let evb = ev.bank("REC::Event").expect("REC::Event present");
        let evno = evb.get::<i64>("evno", 0);
        let beam_bits = evb.get::<f32>("beamE", 0).to_bits();

        let mut parts = Vec::new();
        if let Some(pb) = ev.bank("REC::Particle") {
            for r in 0..pb.rows() {
                let cov = pb.array_at::<f32>("cov", r).unwrap();
                parts.push((
                    pb.get::<i32>("pid", r),
                    pb.get::<f32>("px", r).to_bits(),
                    pb.get::<f32>("py", r).to_bits(),
                    pb.get::<i8>("charge", r),
                    [cov[0].to_bits(), cov[1].to_bits(), cov[2].to_bits()],
                ));
            }
        }
        out.push(EventSnap {
            evno,
            beam_bits,
            parts,
        });
    }
    Ok(out)
}

/// The reference data every format must reproduce, computed independently of
/// the reader so a shared decode bug can't hide it.
fn expected() -> Vec<EventSnap> {
    (0..N_EVENTS)
        .map(|evno| EventSnap {
            evno,
            beam_bits: (10.6_f32 + evno as f32 * 0.01).to_bits(),
            parts: (0..n_parts(evno))
                .map(|i| {
                    (
                        11 + i,
                        (i as f32 * 0.1 - 1.0).to_bits(),
                        (-(i as f32) * 0.25).to_bits(),
                        (i as i8) - 1,
                        [
                            (i as f32).to_bits(),
                            (i as f32 + 0.5).to_bits(),
                            (-(i as f32)).to_bits(),
                        ],
                    )
                })
                .collect(),
        })
        .collect()
}

#[test]
fn every_format_preserves_data() {
    let dir = tempfile::tempdir().unwrap();
    let want = expected();
    for (name, comp) in FORMATS {
        let path = dir.path().join(format!("{name}.hipo"));
        write_file(&path, *comp).unwrap();
        let got = read_snapshot(&path).unwrap();
        assert_eq!(got.len(), N_EVENTS as usize, "{name}: event count");
        assert_eq!(got, want, "{name}: decoded data differs from the source");
    }
}

#[test]
fn cross_format_skim_preserves_data() {
    // Write once (Lz4), then re-encode into every format via `skim` and check
    // the data survives the decode → re-encode → decode round-trip.
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.hipo");
    write_file(&src, Compression::Lz4).unwrap();
    let want = expected();

    for (name, comp) in FORMATS {
        let out = dir.path().join(format!("skim-{name}.hipo"));
        let summary = Chain::open(&src).unwrap().skim(&out, *comp).unwrap();
        assert_eq!(summary.events, N_EVENTS as u64, "{name}: skim event count");
        assert_eq!(
            read_snapshot(&out).unwrap(),
            want,
            "{name}: skim changed data"
        );
    }
}

#[test]
fn every_format_supports_partial_read() {
    // Reading a single bank must work — and return the right values — on every
    // format, including the by-bank / per-column ones that inflate lazily.
    let dir = tempfile::tempdir().unwrap();
    let want: i64 = (0..N_EVENTS).sum(); // Σ evno
    for (name, comp) in FORMATS {
        let path = dir.path().join(format!("{name}.hipo"));
        write_file(&path, *comp).unwrap();

        let chain = Chain::open(&path).unwrap();
        let mut sum = 0i64;
        for ev in chain.events() {
            // Touch only REC::Event; REC::Particle stays untouched (and, for the
            // lazy formats, uninflated) without breaking the read.
            sum += ev
                .unwrap()
                .bank("REC::Event")
                .unwrap()
                .get::<i64>("evno", 0);
        }
        assert_eq!(sum, want, "{name}: partial single-bank read");
    }
}
