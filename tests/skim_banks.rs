//! Bank projection — `Chain::skim_banks`.
//!
//! The `hipoutils -filter` equivalent: rebuild each event from only the banks
//! that match a pattern. On a real CLAS12 DST, keeping `REC::Particle` and
//! `REC::Event` leaves 1.389% of the bytes (72x smaller) and the values
//! round-trip exactly.
//!
//! What these tests guard is that projection is *lossless for what it keeps*.
//! A projection that silently corrupted a kept bank would still produce a
//! small, readable file — which is why every test here compares values, not
//! just sizes.

use oxihipo::{BankPatterns, Chain, Compression, DataType, Dict, Schema, SkimOptions, Writer};

const N: i64 = 120;

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
        ],
    ));
    d.add(Schema::from_columns(
        "REC::Calorimeter",
        332,
        11,
        [
            ("pindex".into(), DataType::Short, 1),
            ("energy".into(), DataType::Float, 1),
        ],
    ));
    d.add(Schema::from_columns(
        "RECHB::Particle",
        310,
        1,
        [("pid".into(), DataType::Int, 1)],
    ));
    d
}

fn write_src(path: &std::path::Path, compression: Compression) {
    let d = dict();
    let mut w = Writer::create(path)
        .schemas(&d)
        .compression(compression)
        .max_record_events(13)
        .build()
        .unwrap();
    for evno in 0..N {
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| r.set("evno", evno).map(|_| ()))?;
                Ok(())
            })?;
            ev.bank("REC::Particle", |b| {
                for k in 0..=(evno % 4) {
                    b.row(|r| {
                        r.set("pid", (11 + k) as i32)?;
                        r.set("px", evno as f32 * 0.25)?;
                        Ok(())
                    })?;
                }
                Ok(())
            })?;
            ev.bank("REC::Calorimeter", |b| {
                b.row(|r| {
                    r.set("pindex", 0i16)?;
                    r.set("energy", 1.5f32)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            ev.bank("RECHB::Particle", |b| {
                b.row(|r| r.set("pid", 2212i32).map(|_| ()))?;
                Ok(())
            })?;
            Ok(())
        })
        .unwrap();
    }
    w.finish().unwrap();
}

/// Sum a column across the file, plus the row count — the value fingerprint.
fn fingerprint(path: &std::path::Path, bank: &str, col: &str) -> (u64, f64) {
    let chain = Chain::open(path).unwrap();
    let (mut rows, mut acc) = (0u64, 0f64);
    for ev in chain.events() {
        let ev = ev.unwrap();
        if let Some(b) = ev.ctx().bank(bank) {
            let ci = b.schema().column_index(col).unwrap();
            for r in 0..b.rows() {
                acc += b.value(ci, r).unwrap();
                rows += 1;
            }
        }
    }
    (rows, acc)
}

#[test]
fn projection_keeps_matching_banks_and_their_values_exactly() {
    let dir = tempfile::tempdir().unwrap();
    for codec in [
        Compression::None,
        Compression::Lz4,
        Compression::Lz4PerBank,
        Compression::Lz4PerColumn,
    ] {
        let src = dir.path().join("src.hipo");
        let out = dir.path().join("out.hipo");
        write_src(&src, codec);

        let before = fingerprint(&src, "REC::Particle", "px");
        assert!(before.0 > 0);

        let chain = Chain::open(&src).unwrap();
        let s = chain
            .skim_banks(&out, codec, &["REC::Particle", "REC::Event"])
            .unwrap();

        assert_eq!(s.write.events, N as u64, "{codec:?}");
        assert_eq!(s.kept, ["REC::Event", "REC::Particle"], "{codec:?}");
        // Two banks dropped per event.
        assert_eq!(s.dropped_structures, 2 * N as u64, "{codec:?}");

        // Kept values are byte-identical in effect.
        assert_eq!(
            fingerprint(&out, "REC::Particle", "px"),
            before,
            "{codec:?}"
        );
        let (rows, _) = fingerprint(&out, "REC::Event", "evno");
        assert_eq!(rows, N as u64, "{codec:?}");

        // The event header's own EH_SIZE word must be rewritten to the
        // projected length. Nothing in the *read* path slices events with it —
        // the record's offset table does that — so a stale value survives every
        // value check above. It surfaces here: `OwnedEvent::size()` measures
        // the record span while `EventCtx::size()` reads EH_SIZE, and a
        // projected file would be the first file in which those two disagree.
        // Downstream C++/Java readers and anything trusting the header would
        // see an event claiming to be several times its real length.
        let outc = Chain::open(&out).unwrap();
        for ev in outc.events() {
            let ev = ev.unwrap();
            assert_eq!(
                ev.size(),
                ev.ctx().size(),
                "{codec:?}: EH_SIZE disagrees with the record span"
            );
        }

        // Dropped banks really are gone.
        let outc = Chain::open(&out).unwrap();
        let any_cal = outc
            .events()
            .any(|ev| ev.unwrap().ctx().bank("REC::Calorimeter").is_some());
        assert!(!any_cal, "{codec:?}: dropped bank survived");

        // And the output is smaller than the source it came from.
        let (a, b) = (
            std::fs::metadata(&src).unwrap().len(),
            std::fs::metadata(&out).unwrap().len(),
        );
        assert!(b < a, "{codec:?}: {b} not smaller than {a}");
    }
}

#[test]
fn the_dictionary_is_pruned_by_default_and_kept_on_request() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s.hipo");
    write_src(&src, Compression::Lz4);
    let chain = Chain::open(&src).unwrap();
    let pats = BankPatterns::from_slice(&["REC::Particle"]).unwrap();

    let pruned = dir.path().join("pruned.hipo");
    chain
        .skim_banks_with(
            &pruned,
            Compression::Lz4,
            &pats,
            SkimOptions::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(Chain::open(&pruned).unwrap().schemas().len(), 1);

    let whole = dir.path().join("whole.hipo");
    chain
        .skim_banks_with(
            &whole,
            Compression::Lz4,
            &pats,
            SkimOptions { prune_dict: false },
            |_| {},
        )
        .unwrap();
    assert_eq!(Chain::open(&whole).unwrap().schemas().len(), 4);
}

/// Keeping a bank whose `pindex` points at a dropped bank must be *reported*,
/// because the resulting join is silently empty rather than an error.
#[test]
fn a_pindex_into_a_dropped_bank_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s.hipo");
    write_src(&src, Compression::Lz4);
    let chain = Chain::open(&src).unwrap();

    // Keep the referrer, drop the referent.
    let out = dir.path().join("dangling.hipo");
    let s = chain
        .skim_banks(&out, Compression::Lz4, &["REC::Calorimeter"])
        .unwrap();
    assert!(
        s.dangling_refs
            .iter()
            .any(|(from, to)| from == "REC::Calorimeter" && to.ends_with("::Particle")),
        "expected a dangling-ref report, got {:?}",
        s.dangling_refs
    );

    // Keeping both leaves nothing dangling.
    let ok = dir.path().join("ok.hipo");
    let s = chain
        .skim_banks(
            &ok,
            Compression::Lz4,
            &["REC::Calorimeter", "REC::Particle"],
        )
        .unwrap();
    assert!(s.dangling_refs.is_empty(), "{:?}", s.dangling_refs);
}

#[test]
fn patterns_span_families_and_typos_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s.hipo");
    write_src(&src, Compression::Lz4);
    let chain = Chain::open(&src).unwrap();

    // `*::Particle` spans REC:: and RECHB::.
    let out = dir.path().join("fam.hipo");
    let s = chain
        .skim_banks(&out, Compression::Lz4, &["*::Particle"])
        .unwrap();
    assert_eq!(s.kept, ["REC::Particle", "RECHB::Particle"]);

    // `REC::*` does not leak into RECHB::.
    let out2 = dir.path().join("pre.hipo");
    let s = chain
        .skim_banks(&out2, Compression::Lz4, &["REC::*"])
        .unwrap();
    assert_eq!(s.kept, ["REC::Event", "REC::Particle", "REC::Calorimeter"]);

    // A typo is an error, not a silently empty file.
    let err = chain
        .skim_banks(
            dir.path().join("no.hipo"),
            Compression::Lz4,
            &["REC::Partical"],
        )
        .unwrap_err();
    assert!(err.to_string().contains("REC::Partical"), "{err}");
}

/// Per-event tags must survive projection — the event header is copied
/// verbatim and only its size word is patched.
#[test]
fn event_tags_survive_projection() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("t.hipo");
    let d = dict();
    let mut w = Writer::create(&src)
        .schemas(&d)
        .compression(Compression::Lz4)
        .max_record_events(9)
        .build()
        .unwrap();
    for evno in 0..40i64 {
        w.event(|ev| {
            ev.with_tag((evno as u32 % 7) + 1);
            ev.bank("REC::Particle", |b| {
                b.row(|r| r.set("pid", 11i32).map(|_| ()))?;
                Ok(())
            })?;
            ev.bank("REC::Calorimeter", |b| {
                b.row(|r| {
                    r.set("pindex", 0i16)?;
                    r.set("energy", 1.0f32)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            Ok(())
        })
        .unwrap();
    }
    w.finish().unwrap();

    let out = dir.path().join("t_out.hipo");
    Chain::open(&src)
        .unwrap()
        .skim_banks(&out, Compression::Lz4, &["REC::Particle"])
        .unwrap();

    let tags: Vec<u32> = Chain::open(&out)
        .unwrap()
        .events()
        .map(|ev| ev.unwrap().tag())
        .collect();
    let expect: Vec<u32> = (0..40u32).map(|i| (i % 7) + 1).collect();
    assert_eq!(tags, expect);
}
