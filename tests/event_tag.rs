//! Phase 1 event-tag filtering: `Filter::event_tag` / `event_tag_any` drop
//! individual events by their per-event `EH_TAG`. Exercised on every read path
//! (`events`, `for_each`, `read_columns`) and every compression backend — the
//! stock (Bytes), by-bank, and per-column `check*` paths — so all three places
//! the filter is applied stay in sync. The tag is read from the event header
//! or the record directory without inflating any bank, so this is also the
//! pushdown smoke test.

use std::sync::atomic::{AtomicU64, Ordering};

use oxihipo::{Chain, Compression, DataType, Dict, Filter, Result, Schema, Writer};

const N: i64 = 60;

/// Per-event tag: a single bit chosen by `evno % 3` → {0b001, 0b010, 0b100}.
fn tag_of(evno: i64) -> u32 {
    1u32 << (evno % 3) as u32
}
/// REC::Particle is written only on even events — so a `require` clause and an
/// `event_tag` clause select different subsets, and their AND is non-trivial.
fn has_particle(evno: i64) -> bool {
    evno % 2 == 0
}

/// Every backend, so all three `check*` paths are covered (`None`/`Lz4` share
/// the Bytes path; `Lz4PerBank` and `Lz4PerColumn` have their own).
const FORMATS: &[(&str, Compression)] = &[
    ("None", Compression::None),
    ("Lz4", Compression::Lz4),
    ("Lz4PerBank", Compression::Lz4PerBank),
    ("Lz4PerColumn", Compression::Lz4PerColumn),
];

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
        31,
        [("pid".into(), DataType::Int, 1)],
    ));
    d
}

fn write_tagged(path: &std::path::Path, compression: Compression) -> Result<()> {
    let mut w = Writer::create(path)
        .schemas(&dict())
        .compression(compression)
        .build()?;
    for evno in 0..N {
        w.event(|ev| {
            ev.with_tag(tag_of(evno));
            ev.bank("REC::Event", |b| {
                b.row(|r| {
                    r.set("evno", evno)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            if has_particle(evno) {
                ev.bank("REC::Particle", |b| {
                    b.row(|r| {
                        r.set("pid", 11)?;
                        Ok(())
                    })?;
                    Ok(())
                })?;
            }
            Ok(())
        })?;
    }
    w.finish()?;
    Ok(())
}

/// The `evno`s that survive a filter, read back through `events()`.
fn surviving_evnos(chain: &Chain) -> Vec<i64> {
    chain
        .events()
        .map(Result::unwrap)
        .map(|ev| ev.bank("REC::Event").unwrap().get::<i64>("evno", 0))
        .collect()
}

#[test]
fn event_tag_set_filters_every_format_and_read_path() {
    let dir = tempfile::tempdir().unwrap();
    let want: Vec<i64> = (0..N).filter(|e| tag_of(*e) == 1).collect(); // evno % 3 == 0
    assert!(!want.is_empty());

    for (name, comp) in FORMATS {
        let path = dir.path().join(format!("{name}.hipo"));
        write_tagged(&path, *comp).unwrap();

        let g = Chain::open(&path)
            .unwrap()
            .with_filter(Filter::new().event_tag([1_u32]))
            .unwrap();

        // events(): only tag-1 events survive, and each really carries tag 1.
        assert_eq!(surviving_evnos(&g), want, "{name}: events() event_tag([1])");
        for ev in g.events().map(Result::unwrap) {
            assert_eq!(ev.tag(), 1, "{name}: survivor carries the tag");
        }

        // for_each (parallel) drops the same events.
        let seen = AtomicU64::new(0);
        let stats = g
            .for_each(0, |_| {
                seen.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap();
        assert_eq!(stats.events_yielded, want.len() as u64, "{name}: for_each");
        assert_eq!(seen.into_inner(), want.len() as u64);

        // read_columns (the columnar / Python path) drops them too: one
        // offsets entry per surviving event, plus the trailing bound.
        let bufs = g
            .read_columns(&[("REC::Event", &["evno"])], None, 1)
            .unwrap();
        assert_eq!(
            bufs[0].offsets.len(),
            want.len() + 1,
            "{name}: read_columns event_tag([1])"
        );
    }
}

#[test]
fn event_tag_any_is_a_bitmask_and_ands_with_require() {
    let dir = tempfile::tempdir().unwrap();
    for (name, comp) in FORMATS {
        let path = dir.path().join(format!("{name}.hipo"));
        write_tagged(&path, *comp).unwrap();

        // event_tag_any(0b101): keep events whose tag has bit 0 or bit 2 set,
        // i.e. tag ∈ {1, 4} → evno % 3 ∈ {0, 2}.
        let mask = 0b101_u32;
        let want_mask: Vec<i64> = (0..N).filter(|e| tag_of(*e) & mask != 0).collect();
        let g = Chain::open(&path)
            .unwrap()
            .with_filter(Filter::new().event_tag_any(mask))
            .unwrap();
        assert_eq!(
            surviving_evnos(&g),
            want_mask,
            "{name}: event_tag_any(0b101)"
        );

        // AND with a bank requirement: tag == 1 AND carries REC::Particle
        // (evno even) → evno % 6 == 0. Proves the clauses compose, not replace.
        let want_and: Vec<i64> = (0..N)
            .filter(|e| tag_of(*e) == 1 && has_particle(*e))
            .collect();
        assert!(!want_and.is_empty() && want_and.len() < want_mask.len());
        let g2 = Chain::open(&path)
            .unwrap()
            .with_filter(Filter::require(["REC::Particle"]).event_tag([1_u32]))
            .unwrap();
        assert_eq!(
            surviving_evnos(&g2),
            want_and,
            "{name}: require AND event_tag"
        );
    }
}
