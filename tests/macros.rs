//! Smoke tests for the `or_continue!` / `or_break!` / `bank_row!` macros
//! and the direct `ev.get` / `ev.col` accessors.

use oxihipo::{Chain, DataType, Dict, Schema, Writer};

// A typed row over REC::Particle. `missing` / `missing_arr` map columns the
// schema does NOT carry, exercising the placeholder → `T::default()` path
// for both a scalar and a fixed-length array field.
oxihipo::bank_row! {
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct RecParticle for "REC::Particle" @ (300, 1) {
        pid: i32 => "pid",
        px:  f32 => "px",
        py:  f32 => "py",
        pz:  f32 => "pz",
        missing:     i32      => "not_in_schema",
        missing_arr: [f32; 3] => "also_missing",
    }
}

fn particle_dict() -> Dict {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        1,
        [
            ("pid".into(), DataType::Int, 1),
            ("px".into(), DataType::Float, 1),
            ("py".into(), DataType::Float, 1),
            ("pz".into(), DataType::Float, 1),
        ],
    ));
    d
}

#[test]
fn bank_row_macro_and_direct_accessors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.hipo");
    let d = particle_dict();
    {
        let mut w = Writer::create(&path).schemas(&d).build().unwrap();
        for i in 0..5 {
            w.event(|ev| {
                ev.bank("REC::Particle", |b| {
                    b.row(|r| {
                        r.set("pid", 11 + i)?;
                        r.set("px", i as f32)?;
                        r.set("py", i as f32 + 0.5)?;
                        r.set("pz", i as f32 + 0.25)?;
                        Ok(())
                    })?;
                    b.row(|r| {
                        r.set("pid", 211 + i)?;
                        r.set("px", -(i as f32) - 1.0)?;
                        r.set("py", 1.0_f32)?;
                        r.set("pz", 2.0_f32)?;
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

    let chain = Chain::open(&path).unwrap();
    let mut total_rows = 0;
    for (ei, ev) in chain.events().map(Result::unwrap).enumerate() {
        let i = ei as i32;

        // `bank_row!` + ev.rows::<T>() — typed decode.
        let rows: Vec<RecParticle> = ev.rows::<RecParticle>().collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].pid, 11 + i);
        assert_eq!(rows[0].px, i as f32);
        assert_eq!(rows[1].pid, 211 + i);
        assert_eq!(rows[1].px, -(i as f32) - 1.0);
        // Columns absent from the schema decode as `T::default()`.
        assert_eq!(rows[0].missing, 0);
        assert_eq!(rows[0].missing_arr, [0.0, 0.0, 0.0]);

        // ev.rows::<T>() yields the same typed rows on a second pass.
        assert_eq!(ev.rows::<RecParticle>().collect::<Vec<_>>(), rows);

        // Item 1: direct ev.get / ev.col agree with the typed fields.
        let pid0: i32 = ev.get("REC::Particle", "pid", 0);
        assert_eq!(pid0, rows[0].pid);
        let px_col = ev.col::<f32>("REC::Particle", "px").unwrap();
        assert_eq!(px_col.len(), 2);
        assert_eq!(px_col[0], rows[0].px);
        assert_eq!(px_col[1], rows[1].px);

        // Absent bank: get → default, col → Ok(empty).
        assert_eq!(ev.get::<i32>("NOPE::missing", "x", 0), 0);
        assert!(ev.col::<f32>("NOPE::missing", "x").unwrap().is_empty());

        total_rows += rows.len();
    }
    assert_eq!(total_rows, 10);
}

/// The per-event bank cache must not cross-contaminate when reads alternate
/// between two banks: each `get` must re-validate and rebuild the right one.
#[test]
fn bank_cache_correct_across_banks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("two.hipo");
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [("evno".into(), DataType::Int, 1)],
    ));
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        31,
        [("pid".into(), DataType::Int, 1)],
    ));
    {
        let mut w = Writer::create(&path).schemas(&d).build().unwrap();
        for i in 0..10 {
            w.event(|ev| {
                ev.bank("REC::Event", |b| {
                    b.row(|r| r.set("evno", 1000 + i).map(|_| ()))?;
                    Ok(())
                })?;
                ev.bank("REC::Particle", |b| {
                    b.row(|r| r.set("pid", 11 + i).map(|_| ()))?;
                    b.row(|r| r.set("pid", 211 + i).map(|_| ()))?;
                    Ok(())
                })?;
                Ok(())
            })
            .unwrap();
        }
        w.finish().unwrap();
    }

    for (i, ev) in Chain::open(&path)
        .unwrap()
        .events()
        .map(Result::unwrap)
        .enumerate()
    {
        let i = i as i32;
        // Interleave the two banks repeatedly: hit, switch (miss), rebuild.
        for _ in 0..3 {
            assert_eq!(ev.get::<i32>("REC::Particle", "pid", 0), 11 + i);
            assert_eq!(ev.get::<i32>("REC::Event", "evno", 0), 1000 + i);
            assert_eq!(ev.get::<i32>("REC::Particle", "pid", 1), 211 + i);
        }
    }
}

/// The per-event *column* cache must switch columns within a bank and keep
/// validating the requested type — a hit on the cached column index still
/// returns `T::default()` on a type mismatch.
#[test]
fn get_column_cache_switches_columns_and_types() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cols.hipo");
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        31,
        [
            ("pid".into(), DataType::Int, 1),
            ("px".into(), DataType::Float, 1),
        ],
    ));
    {
        let mut w = Writer::create(&path).schemas(&d).build().unwrap();
        for i in 0..8 {
            w.event(|ev| {
                ev.bank("REC::Particle", |b| {
                    b.row(|r| {
                        r.set("pid", 11 + i)?;
                        r.set("px", i as f32 * 0.5)?;
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

    for (i, ev) in Chain::open(&path)
        .unwrap()
        .events()
        .map(Result::unwrap)
        .enumerate()
    {
        let i = i as i32;
        for _ in 0..3 {
            assert_eq!(ev.get::<i32>("REC::Particle", "pid", 0), 11 + i);
            assert_eq!(ev.get::<f32>("REC::Particle", "px", 0), i as f32 * 0.5);
            assert_eq!(ev.get::<i32>("REC::Particle", "pid", 0), 11 + i);
            // Wrong type on a cached column → default (type still validated).
            assert_eq!(ev.get::<f32>("REC::Particle", "pid", 0), 0.0);
            // Absent column → default.
            assert_eq!(ev.get::<i32>("REC::Particle", "nope", 0), 0);
        }
    }
}

#[test]
fn or_continue_skips_none() {
    let inputs = [Some(1_i32), None, Some(2), None, Some(3)];
    let mut kept = Vec::new();
    for opt in inputs {
        let v = oxihipo::or_continue!(opt);
        kept.push(v);
    }
    assert_eq!(kept, vec![1, 2, 3]);
}

#[test]
fn or_break_exits_on_none() {
    let inputs = [Some(1_i32), Some(2), None, Some(3)];
    let mut kept = Vec::new();
    for opt in inputs {
        let v = oxihipo::or_break!(opt);
        kept.push(v);
    }
    assert_eq!(kept, vec![1, 2]);
}

#[test]
fn or_continue_in_nested_loops_targets_inner() {
    let mut visited = Vec::new();
    for outer in 0..3 {
        for inner in [Some(outer), None, Some(outer + 100)] {
            let v = oxihipo::or_continue!(inner);
            visited.push(v);
        }
    }
    // outer=0: skip None → 0, 100; outer=1: 1, 101; outer=2: 2, 102.
    assert_eq!(visited, vec![0, 100, 1, 101, 2, 102]);
}
