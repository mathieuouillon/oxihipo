//! Property tests for the two areas that had none: the hand-written schema
//! parser and the write→read record round-trip.
//!
//! The existing suite pins fixed values, which covers the shapes someone
//! thought of. These generate the shapes nobody thought of — random column
//! type/length combinations, random per-event row counts (including empty
//! banks), and every compression format — and assert the invariants that must
//! hold for all of them.

use oxihipo::{Chain, Compression, DataType, Dict, Schema, Writer};
use proptest::prelude::*;

// ---- generators ------------------------------------------------------------

fn any_data_type() -> impl Strategy<Value = DataType> {
    prop_oneof![
        Just(DataType::Byte),
        Just(DataType::Short),
        Just(DataType::Int),
        Just(DataType::Long),
        Just(DataType::Float),
        Just(DataType::Double),
    ]
}

/// A column: a lowercase-ascii name, a type, and a length (1 = scalar).
fn any_column() -> impl Strategy<Value = (String, DataType, u32)> {
    ("[a-z][a-z0-9_]{0,7}", any_data_type(), 1u32..6)
}

/// A schema with 1..6 uniquely-named columns.
fn any_schema() -> impl Strategy<Value = Schema> {
    (
        "[A-Z][A-Za-z]{0,5}::[A-Z][A-Za-z]{0,7}",
        1u16..40000,
        1u8..250,
        prop::collection::vec(any_column(), 1..6),
    )
        .prop_map(|(name, group, item, cols)| {
            // Dedupe names — a schema with a repeated column name is not a
            // shape the writer can produce.
            let mut seen = std::collections::HashSet::new();
            let cols: Vec<_> = cols
                .into_iter()
                .filter(|(n, _, _)| seen.insert(n.clone()))
                .collect();
            Schema::from_columns(name, group, item, cols)
        })
        .prop_filter("needs at least one column", |s| !s.entries().is_empty())
}

fn any_compression() -> impl Strategy<Value = Compression> {
    prop_oneof![
        Just(Compression::None),
        Just(Compression::Lz4),
        Just(Compression::Lz4Best),
        Just(Compression::Gzip),
        Just(Compression::Lz4PerBank),
        Just(Compression::Lz4PerColumn),
    ]
}

// ---- properties ------------------------------------------------------------

proptest! {
    /// The compact text form is the inverse of the parser, for any schema.
    #[test]
    fn schema_text_round_trips(schema in any_schema()) {
        let text = schema.to_text();
        let parsed = Schema::parse_text(&text)
            .unwrap_or_else(|e| panic!("failed to re-parse {text:?}: {e}"));
        prop_assert_eq!(parsed.name(), schema.name());
        prop_assert_eq!(parsed.group(), schema.group());
        prop_assert_eq!(parsed.item(), schema.item());
        prop_assert_eq!(parsed.entries().len(), schema.entries().len());
        prop_assert_eq!(parsed.row_size(), schema.row_size());
        for (a, b) in parsed.entries().iter().zip(schema.entries()) {
            prop_assert_eq!(&a.name, &b.name);
            prop_assert_eq!(a.ty, b.ty);
            prop_assert_eq!(a.length, b.length);
        }
        // …and re-emitting is byte-identical (the form is canonical).
        prop_assert_eq!(parsed.to_text(), text);
    }

    /// A parsed schema is always self-consistent: `row_size` equals the sum of
    /// its columns' widths, and every column resolves by name.
    #[test]
    fn parsed_schema_is_self_consistent(schema in any_schema()) {
        let parsed = Schema::parse_text(&schema.to_text()).unwrap();
        let sum: u64 = parsed
            .entries()
            .iter()
            .map(|e| e.ty.size() as u64 * e.length as u64)
            .sum();
        prop_assert_eq!(sum, parsed.row_size() as u64);
        for e in parsed.entries() {
            prop_assert!(parsed.column_index(&e.name).is_some());
        }
    }

    /// Never panic on arbitrary text, whichever parser is used.
    #[test]
    fn schema_parsers_never_panic(text in ".{0,120}") {
        let _ = Schema::parse_text(&text);
        let _ = Schema::parse_json(&text);
    }

    /// Write random per-event row counts through every compression format and
    /// read them back: event count, per-event row counts, and every value must
    /// match. Row counts include zeros, so empty banks are covered.
    #[test]
    fn record_round_trips(
        rows_per_event in prop::collection::vec(0u32..5, 1..12),
        compression in any_compression(),
        max_record_events in 1u32..5,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prop.hipo");

        let mut dict = Dict::new();
        dict.add(Schema::from_columns(
            "P::bank",
            300,
            1,
            [
                ("i".into(), DataType::Int, 1),
                ("l".into(), DataType::Long, 1),
                ("f".into(), DataType::Float, 1),
                ("d".into(), DataType::Double, 1),
                ("s".into(), DataType::Short, 1),
                ("b".into(), DataType::Byte, 1),
                ("arr".into(), DataType::Int, 3),
            ],
        ));

        // Deterministic value functions so the reader can assert exactly.
        let vi = |e: usize, r: u32| (e as i32) * 100 + r as i32;
        let vl = |e: usize, r: u32| (e as i64) * 1_000_000 + r as i64;
        let vf = |e: usize, r: u32| e as f32 * 0.5 + r as f32;
        let vd = |e: usize, r: u32| e as f64 * 0.25 + r as f64;
        let vs = |e: usize, r: u32| ((e as i32) * 7 + r as i32) as i16;
        let vb = |e: usize, r: u32| ((e as i32) + r as i32) as i8;

        {
            let mut w = Writer::create(&path)
                .schemas(&dict)
                .compression(compression)
                .max_record_events(max_record_events)
                .build()
                .unwrap();
            for (e, &n) in rows_per_event.iter().enumerate() {
                w.event(|ev| {
                    ev.bank("P::bank", |b| {
                        for r in 0..n {
                            b.row(|row| {
                                row.set("i", vi(e, r))?;
                                row.set("l", vl(e, r))?;
                                row.set("f", vf(e, r))?;
                                row.set("d", vd(e, r))?;
                                row.set("s", vs(e, r))?;
                                row.set("b", vb(e, r))?;
                                row.set("arr", [r as i32, r as i32 + 1, r as i32 + 2])?;
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

        let chain = Chain::open(&path).unwrap();
        prop_assert_eq!(chain.event_count(), rows_per_event.len() as u64);
        let mut e = 0usize;
        for ev in chain.events() {
            let ev = ev.unwrap();
            let bank = ev.bank("P::bank").expect("bank present");
            prop_assert_eq!(bank.rows(), rows_per_event[e]);
            for r in 0..rows_per_event[e] {
                prop_assert_eq!(bank.get::<i32>("i", r), vi(e, r));
                prop_assert_eq!(bank.get::<i64>("l", r), vl(e, r));
                prop_assert_eq!(bank.get::<f32>("f", r), vf(e, r));
                prop_assert_eq!(bank.get::<f64>("d", r), vd(e, r));
                prop_assert_eq!(bank.get::<i16>("s", r), vs(e, r));
                prop_assert_eq!(bank.get::<i8>("b", r), vb(e, r));
                let arr = bank.array_at::<i32>("arr", r).unwrap();
                prop_assert_eq!(&arr[..], &[r as i32, r as i32 + 1, r as i32 + 2]);
            }
            e += 1;
        }
        prop_assert_eq!(e, rows_per_event.len());
    }

    /// Random access must agree with sequential iteration for any file shape.
    #[test]
    fn random_access_matches_sequential(
        rows_per_event in prop::collection::vec(0u32..4, 1..10),
        compression in any_compression(),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ra.hipo");
        let mut dict = Dict::new();
        dict.add(Schema::from_columns(
            "R::b",
            300,
            1,
            [("x".into(), DataType::Int, 1)],
        ));
        {
            let mut w = Writer::create(&path)
                .schemas(&dict)
                .compression(compression)
                .max_record_events(2)
                .build()
                .unwrap();
            for (e, &n) in rows_per_event.iter().enumerate() {
                w.event(|ev| {
                    ev.bank("R::b", |b| {
                        for r in 0..n {
                            b.row(|row| {
                                row.set("x", (e as i32) * 10 + r as i32)?;
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

        let chain = Chain::open(&path).unwrap();
        let sequential: Vec<Vec<i32>> = chain
            .events()
            .map(|ev| {
                let ev = ev.unwrap();
                let b = ev.bank("R::b").unwrap();
                (0..b.rows()).map(|r| b.get::<i32>("x", r)).collect()
            })
            .collect();
        for (i, expect) in sequential.iter().enumerate() {
            let ev = chain.event(i as u64).expect("index in range");
            let b = ev.bank("R::b").unwrap();
            let got: Vec<i32> = (0..b.rows()).map(|r| b.get::<i32>("x", r)).collect();
            prop_assert_eq!(&got, expect);
        }
        // Out of range must be None, not a panic.
        prop_assert!(chain.event(rows_per_event.len() as u64).is_none());
    }
}
