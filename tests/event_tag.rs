//! Phase 1 event-tag filtering: `Filter::event_tag` / `event_tag_any` drop
//! individual events by their per-event `EH_TAG`. Exercised on every read path
//! (`events`, `for_each`, `read_columns`) and every compression backend — the
//! stock (Bytes), by-bank, and per-column `check*` paths — so all three places
//! the filter is applied stay in sync. The tag is read from the event header
//! or the record directory without inflating any bank, so this is also the
//! pushdown smoke test.

use std::sync::atomic::{AtomicU64, Ordering};

use oxihipo::{
    Chain, Compression, DataType, Dict, EventCtx, Filter, HipoError, Result, Schema, TagRegistry,
    TagSet, Writer,
};

// Named flags via the `tag_flags!` macro — the Phase 2 ergonomics.
oxihipo::tag_flags! {
    /// Physics categories for the named-tag test.
    pub Cat {
        Dvcs = 0,
        Sidis = 1,
        Elastic = 2,
    }
}

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

#[test]
fn event_tags_column_aligns_with_read_columns() {
    let dir = tempfile::tempdir().unwrap();
    let all: Vec<u32> = (0..N).map(tag_of).collect();
    for (name, comp) in FORMATS {
        let path = dir.path().join(format!("{name}.hipo"));
        write_tagged(&path, *comp).unwrap();
        let chain = Chain::open(&path).unwrap();

        // Unfiltered: every event's tag, in order; sequential and parallel agree.
        assert_eq!(
            chain.event_tags(None, 1).unwrap(),
            all,
            "{name}: event_tags"
        );
        assert_eq!(
            chain.event_tags(None, 0).unwrap(),
            all,
            "{name}: event_tags (parallel)"
        );

        // Range [10, 20): exactly those events' tags.
        let want_range: Vec<u32> = (10..20).map(tag_of).collect();
        assert_eq!(
            chain.event_tags(Some(10..20), 1).unwrap(),
            want_range,
            "{name}: event_tags range"
        );

        // Under a filter, the tag column is 1:1 with read_columns' event axis.
        let g = chain.with_filter(Filter::new().event_tag([1_u32])).unwrap();
        let tags = g.event_tags(None, 0).unwrap();
        assert!(tags.iter().all(|&t| t == 1), "{name}: only tag-1 survive");
        let bufs = g
            .read_columns(&[("REC::Event", &["evno"])], None, 1)
            .unwrap();
        assert_eq!(
            tags.len(),
            bufs[0].offsets.len() - 1,
            "{name}: event_tags aligns with read_columns"
        );
    }
}

#[test]
fn named_tags_via_macro_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("named.hipo");

    // Each event's category rotates by i % 3, written with the named flags.
    let cat = |i: i64| -> TagSet {
        match i % 3 {
            0 => Cat::Dvcs,
            1 => Cat::Sidis | Cat::Elastic,
            _ => Cat::Elastic,
        }
    };

    let mut w = Writer::create(&path)
        .schemas(&dict())
        .compression(Compression::Lz4PerColumn)
        .build()
        .unwrap();
    for i in 0..30i64 {
        w.event(|ev| {
            ev.with_tag(cat(i)); // a TagSet flows straight into with_tag
            ev.bank("REC::Event", |b| {
                b.row(|r| {
                    r.set("evno", i)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            Ok(())
        })
        .unwrap();
    }
    w.finish().unwrap();

    // Filter by a named flag: only Dvcs events (i % 3 == 0) survive, and each
    // reads back as exactly Cat::Dvcs.
    let g = Chain::open(&path)
        .unwrap()
        .with_filter(Filter::new().event_tag_any(Cat::Dvcs))
        .unwrap();
    let evnos: Vec<i64> = g
        .events()
        .map(Result::unwrap)
        .map(|e| e.bank("REC::Event").unwrap().get::<i64>("evno", 0))
        .collect();
    assert_eq!(evnos, (0..30).filter(|i| i % 3 == 0).collect::<Vec<_>>());
    for e in g.events().map(Result::unwrap) {
        assert_eq!(TagSet::from(e.tag()), Cat::Dvcs);
    }

    // Elastic appears alone (i % 3 == 2) and combined with Sidis (i % 3 == 1),
    // so event_tag_any(Elastic) keeps both → i % 3 != 0.
    let elastic = Chain::open(&path)
        .unwrap()
        .with_filter(Filter::new().event_tag_any(Cat::Elastic))
        .unwrap();
    assert_eq!(
        elastic.event_tags(None, 1).unwrap().len(),
        (0..30).filter(|i| i % 3 != 0).count(),
    );
}

/// Phase 2b: `tag_names` writes the name↔bit registry into the file, so a
/// reader resolves names *without* the `tag_flags!` decl; `skim` carries it
/// through; a file written without it exposes an empty registry.
#[test]
fn tag_registry_persists_and_survives_skim() {
    let dir = tempfile::tempdir().unwrap();
    let want = TagRegistry::from_names(Cat::NAMES.iter().copied());

    for (name, comp) in FORMATS {
        let path = dir.path().join(format!("reg_{name}.hipo"));
        let mut w = Writer::create(&path)
            .schemas(&dict())
            .tag_names(Cat::NAMES) // record the registry
            .compression(*comp)
            .build()
            .unwrap();
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
                Ok(())
            })
            .unwrap();
        }
        w.finish().unwrap();

        // The reader recovers the full registry from the file.
        let chain = Chain::open(&path).unwrap();
        assert_eq!(chain.tag_registry(), &want, "{name}: registry round-trip");
        assert_eq!(chain.tag_registry().name(0), Some("Dvcs"));
        assert_eq!(chain.tag_registry().mask("Elastic"), Some(0b100));

        // A name resolved through the persisted registry filters correctly —
        // `tag_of(evno) == 1` (bit 0 = Dvcs) ⇔ evno % 3 == 0.
        let mask = chain.tag_registry().mask("Dvcs").unwrap();
        let dvcs = chain
            .clone()
            .with_filter(Filter::new().event_tag_any(mask))
            .unwrap();
        assert_eq!(
            surviving_evnos(&dvcs),
            (0..N).filter(|e| e % 3 == 0).collect::<Vec<_>>(),
            "{name}: filter by name-resolved mask"
        );

        // skim copies the registry into the output file.
        let skimmed = dir.path().join(format!("skim_{name}.hipo"));
        chain.skim(&skimmed, Compression::Lz4PerColumn).unwrap();
        assert_eq!(
            Chain::open(&skimmed).unwrap().tag_registry(),
            &want,
            "{name}: registry survives skim"
        );
    }

    // A file written *without* `tag_names` has an empty registry (and reading
    // the extra bank never breaks the untagged path).
    let plain = dir.path().join("plain.hipo");
    write_tagged(&plain, Compression::Lz4PerColumn).unwrap();
    assert!(Chain::open(&plain).unwrap().tag_registry().is_empty());
}

/// Phase 3: `skim_tagged` retags each surviving event via a classifier and
/// records a fresh output registry — the select→label→write→reread loop.
#[test]
fn skim_tagged_retags_records_registry_and_rereads_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let want_reg = TagRegistry::from_names(Cat::NAMES.iter().copied());

    // A fresh scheme, unrelated to the source tags: carries REC::Particle
    // (evno even) → Dvcs, else Sidis. Non-capturing ⇒ `Copy`, reusable.
    let classify = |ev: &EventCtx<'_>| -> TagSet {
        if ev.bank("REC::Particle").is_some() {
            Cat::Dvcs
        } else {
            Cat::Sidis
        }
    };
    let new_tag = |evno: i64| if has_particle(evno) { 1 } else { 2 }; // Dvcs=1, Sidis=2

    for (name, comp) in FORMATS {
        let src_path = dir.path().join(format!("src_{name}.hipo"));
        write_tagged(&src_path, *comp).unwrap();
        assert!(Chain::open(&src_path).unwrap().tag_registry().is_empty());

        let dst = dir.path().join(format!("tagged_{name}.hipo"));
        let summary = Chain::open(&src_path)
            .unwrap()
            .skim_tagged(&dst, Compression::Lz4PerColumn, Cat::NAMES, classify)
            .unwrap();
        assert_eq!(summary.events, N as u64, "{name}: every event copied");

        let out = Chain::open(&dst).unwrap();
        // Self-describing: the fresh registry is recorded (source had none)…
        assert_eq!(out.tag_registry(), &want_reg, "{name}: registry recorded");
        assert_eq!(out.tag_registry().mask("Dvcs"), Some(1));
        // …the new per-event tags are written…
        let want_tags: Vec<u32> = (0..N).map(new_tag).collect();
        assert_eq!(
            out.event_tags(None, 1).unwrap(),
            want_tags,
            "{name}: events retagged"
        );
        // …banks are copied through intact…
        assert_eq!(
            surviving_evnos(&out),
            (0..N).collect::<Vec<_>>(),
            "{name}: banks intact"
        );
        // …and the DST rereads by name.
        let mask = out.tag_registry().mask("Dvcs").unwrap();
        let dvcs = out
            .clone()
            .with_filter(Filter::new().event_tag_any(mask))
            .unwrap();
        assert_eq!(
            surviving_evnos(&dvcs),
            (0..N).filter(|e| has_particle(*e)).collect::<Vec<_>>(),
            "{name}: filter DST by name-resolved mask"
        );
    }

    // A source filter applies *before* retagging: only tag-1 survivors
    // (evno % 3 == 0) reach the DST, and `&[]` records no registry.
    let src_path = dir.path().join("src_for_filter.hipo");
    write_tagged(&src_path, Compression::None).unwrap();
    let dst = dir.path().join("filtered_tagged.hipo");
    let summary = Chain::open(&src_path)
        .unwrap()
        .with_filter(Filter::new().event_tag([1_u32]))
        .unwrap()
        .skim_tagged(&dst, Compression::Lz4PerColumn, &[], classify)
        .unwrap();
    let survivors: Vec<i64> = (0..N).filter(|e| tag_of(*e) == 1).collect();
    assert_eq!(summary.events, survivors.len() as u64);
    let out = Chain::open(&dst).unwrap();
    assert!(out.tag_registry().is_empty(), "no names → empty registry");
    assert_eq!(surviving_evnos(&out), survivors);
}

/// In-place tag update: rewrite an event's `EH_TAG` on disk without a full
/// rewrite (uncompressed records only). The file size is unchanged, and the new
/// tag is visible to the same chain and to a fresh open.
#[test]
fn set_event_tag_patches_uncompressed_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("none.hipo");
    write_tagged(&path, Compression::None).unwrap();

    let size_before = std::fs::metadata(&path).unwrap().len();
    let chain = Chain::open(&path).unwrap();

    // A single patch, then a batch of three — assorted new tag values.
    chain.set_event_tag(5, 0xABCD_u32).unwrap();
    assert_eq!(
        chain
            .set_event_tags([(10, 7_u32), (20, 8), (30, 9)])
            .unwrap(),
        3
    );

    // No rewrite: the file is exactly the same size.
    assert_eq!(std::fs::metadata(&path).unwrap().len(), size_before);

    // Visible immediately through the same chain (records stream fresh)…
    assert_eq!(chain.event(5).unwrap().tag(), 0xABCD);
    // …and through a fresh open, via the tag column.
    let tags = Chain::open(&path).unwrap().event_tags(None, 1).unwrap();
    assert_eq!(tags[5], 0xABCD);
    assert_eq!(tags[10], 7);
    assert_eq!(tags[20], 8);
    assert_eq!(tags[30], 9);
    // Untouched events keep their original tag.
    for e in [0_i64, 6, 11, 59] {
        assert_eq!(tags[e as usize], tag_of(e), "event {e} must be unchanged");
    }

    // A `TagSet` flows straight in, like `with_tag` / `event_tag_any`.
    chain.set_event_tag(0, Cat::Dvcs | Cat::Elastic).unwrap();
    assert_eq!(Chain::open(&path).unwrap().event(0).unwrap().tag(), 0b101);

    // An out-of-range index errors cleanly, without writing.
    assert!(matches!(
        chain.set_event_tag(N as u64, 1_u32),
        Err(HipoError::EventIndexOutOfRange { .. })
    ));
}

/// Compressed records can't be patched in place — the tag lives inside a
/// compressed block — so the call errors and the file is left byte-for-byte
/// intact (use `skim_tagged` to rewrite those).
#[test]
fn set_event_tag_rejects_compressed_records() {
    let dir = tempfile::tempdir().unwrap();
    for comp in [
        Compression::Lz4,
        Compression::Gzip,
        Compression::Lz4PerBank,
        Compression::Lz4PerColumn,
    ] {
        let path = dir.path().join(format!("{comp:?}.hipo"));
        write_tagged(&path, comp).unwrap();
        let before = std::fs::read(&path).unwrap();

        let err = Chain::open(&path)
            .unwrap()
            .set_event_tag(3, 1_u32)
            .unwrap_err();
        assert!(
            matches!(err, HipoError::InPlaceTagUnsupported { .. }),
            "{comp:?}: expected InPlaceTagUnsupported, got {err:?}"
        );
        // The file is untouched.
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "{comp:?}: file changed"
        );
    }
}
