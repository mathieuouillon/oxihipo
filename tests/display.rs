//! `Display` for `Schema`, `Dict` and `Bank`, plus the type-erased
//! `Bank::value` family.
//!
//! These exist because every consumer was hand-rolling both. The contract that
//! matters most is `value` returning `Option`: a type-erased accessor that
//! reports 0.0 for "wrong type" is indistinguishable from a stored zero, which
//! is exactly the bug `Composite::f64` had.

use oxihipo::{Bank, Chain, DataType, Dict, Schema, Writer};

fn particle_schema() -> Schema {
    Schema::from_columns(
        "REC::Particle",
        300,
        31,
        [
            ("pid".into(), DataType::Int, 1),
            ("px".into(), DataType::Float, 1),
            ("charge".into(), DataType::Byte, 1),
            ("evno".into(), DataType::Long, 1),
            ("cov".into(), DataType::Float, 6),
        ],
    )
}

#[test]
fn schema_display_lists_columns_and_alternate_adds_offsets() {
    let s = particle_schema();
    let brief = s.to_string();
    assert!(brief.contains("REC::Particle"), "{brief}");
    assert!(brief.contains("group 300"), "{brief}");
    assert!(brief.contains("item 31"), "{brief}");
    assert!(brief.contains("5 columns"), "{brief}");
    for name in ["pid", "px", "charge", "evno", "cov"] {
        assert!(brief.contains(name), "missing {name} in:\n{brief}");
    }
    assert!(brief.contains("float"), "{brief}");
    // Brief form carries no offsets and no round-trip text.
    assert!(!brief.contains("off"), "{brief}");
    assert!(!brief.contains("text:"), "{brief}");

    let full = format!("{s:#}");
    assert!(full.contains("off"), "{full}");
    // The round-trip text must be the real thing, not a rendering of it.
    assert!(full.contains(&s.to_text()), "{full}");
    assert!(full.len() > brief.len());
}

#[test]
fn dict_display_sorts_by_id_and_caps_without_alternate() {
    let mut d = Dict::new();
    // Inserted out of order on purpose — the rendering sorts by (group, item).
    for (g, i) in [(300u16, 31u8), (40, 4), (40, 0), (332, 11)] {
        d.add(Schema::from_columns(
            format!("B{g}::{i}").as_str(),
            g,
            i,
            [("v".into(), DataType::Int, 1)],
        ));
    }
    let out = d.to_string();
    let order: Vec<&str> = out
        .lines()
        .filter_map(|l| l.split_whitespace().nth(2))
        .filter(|t| t.starts_with('B'))
        .collect();
    assert_eq!(order, ["B40::0", "B40::4", "B300::31", "B332::11"], "{out}");
    assert!(out.starts_with("Dict: 4 schemas"), "{out}");

    // Past the cap, the brief form withholds and says so; `{:#}` does not.
    let mut big = Dict::new();
    for i in 0..20u16 {
        big.add(Schema::from_columns(
            format!("S{i}").as_str(),
            i,
            0,
            [("v".into(), DataType::Int, 1)],
        ));
    }
    let brief = big.to_string();
    assert!(brief.contains("... 12 more"), "{brief}");
    let full = format!("{big:#}");
    assert!(!full.contains("more ("), "{full}");
    assert_eq!(full.lines().count(), 22, "header + column head + 20 rows");
}

#[test]
fn bank_display_renders_rows_and_caps_without_alternate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.hipo");
    let s = particle_schema();
    let mut d = Dict::new();
    d.add(s.clone());

    let mut w = Writer::create(&path).schemas(&d).build().unwrap();
    w.event(|ev| {
        ev.bank("REC::Particle", |b| {
            for i in 0..25i32 {
                b.row(|r| {
                    r.set("pid", if i == 0 { -211 } else { 11 + i })?;
                    r.set("px", -0.5249287_f32)?;
                    r.set("charge", -1i8)?;
                    r.set("evno", 9_007_199_254_740_993i64)?;
                    r.set("cov", [1.5f32; 6])?;
                    Ok(())
                })?;
            }
            Ok(())
        })?;
        Ok(())
    })
    .unwrap();
    w.finish().unwrap();

    let chain = Chain::open(&path).unwrap();
    let ev = chain.event(0).unwrap();
    let bank: Bank<'_> = ev.ctx().bank("REC::Particle").unwrap();

    let brief = bank.to_string();
    assert!(
        brief.starts_with("REC::Particle  25 rows x 5 cols"),
        "{brief}"
    );
    assert!(brief.contains("... 15 more rows"), "{brief}");
    // Floats keep full precision — a fixed {:.3} would render -0.525.
    assert!(brief.contains("-0.5249287"), "{brief}");
    assert!(brief.contains("-211"), "{brief}");
    // Array columns are summarised, not expanded, in the brief form.
    assert!(brief.contains("[F x6]"), "{brief}");

    let full = format!("{bank:#}");
    assert!(!full.contains("more rows"), "{full}");
    assert!(full.contains("[1.5 1.5 1.5 1.5 1.5 1.5]"), "{full}");
}

#[test]
fn value_widens_every_type_and_refuses_what_it_cannot_answer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.hipo");
    let s = particle_schema();
    let mut d = Dict::new();
    d.add(s.clone());

    let mut w = Writer::create(&path).schemas(&d).build().unwrap();
    w.event(|ev| {
        ev.bank("REC::Particle", |b| {
            b.row(|r| {
                r.set("pid", -211i32)?;
                r.set("px", 0.5f32)?;
                r.set("charge", -1i8)?;
                r.set("evno", 9_007_199_254_740_993i64)?; // 2^53 + 1
                r.set("cov", [2.0f32; 6])?;
                Ok(())
            })?;
            Ok(())
        })?;
        Ok(())
    })
    .unwrap();
    w.finish().unwrap();

    let chain = Chain::open(&path).unwrap();
    let ev = chain.event(0).unwrap();
    let bank = ev.ctx().bank("REC::Particle").unwrap();

    // Every scalar type widens, including the integer ones.
    assert_eq!(bank.value(0, 0), Some(-211.0)); // Int
    assert_eq!(bank.value(1, 0), Some(0.5)); // Float
    assert_eq!(bank.value(2, 0), Some(-1.0)); // Byte
    assert_eq!(bank.value_by_name("pid", 0), Some(-211.0));

    // Long is exact through value_i64 and lossy through value — the documented
    // caveat, asserted rather than assumed.
    assert_eq!(bank.value_i64(3, 0), Some(9_007_199_254_740_993));
    assert_eq!(bank.value(3, 0), Some(9_007_199_254_740_992.0));
    assert_ne!(bank.value(3, 0).unwrap() as i64, 9_007_199_254_740_993);

    // Float/Double decline value_i64 rather than truncating.
    assert_eq!(bank.value_i64(1, 0), None);

    // Array columns are refused by the scalar accessor, not silently reduced
    // to element 0 — that is the whole reason `array_values` exists.
    assert_eq!(bank.value(4, 0), None);
    let mut out = Vec::new();
    assert!(bank.array_values(4, 0, &mut out));
    assert_eq!(out, vec![2.0; 6]);

    // Out of range is None on every accessor, and leaves `out` cleared.
    assert_eq!(bank.value(99, 0), None);
    assert_eq!(bank.value(0, 99), None);
    assert_eq!(bank.value_i64(99, 0), None);
    assert!(!bank.array_values(99, 0, &mut out));
    assert!(out.is_empty());
}
