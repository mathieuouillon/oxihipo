//! Integration tests for `Chain` — eager multi-file open, dict
//! validation, and random access by global event index.

use oxihipo::{Chain, DataType, Dict, Schema, Writer};

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
        1,
        [("pid".into(), DataType::Int, 1)],
    ));
    d
}

fn write_file(path: &std::path::Path, dict: &Dict, evno_start: i64, count: i32) {
    let mut w = Writer::create(path)
        .schemas(dict)
        .max_record_events(50)
        .build()
        .unwrap();
    for i in 0..count {
        let evno = evno_start + i as i64;
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| {
                    r.set("evno", evno)?;
                    r.set("beamE", 10.6_f32)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            ev.bank("REC::Particle", |b| {
                b.row(|r| {
                    r.set("pid", i)?;
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

#[test]
fn chain_open_single_file_matches_open_list() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("one.hipo");
    write_file(&p, &dict(), 0, 50);
    let a = Chain::open(&p).unwrap();
    let b = Chain::open([&p]).unwrap();
    assert_eq!(a.event_count(), b.event_count());
    assert_eq!(a.file_count(), 1);
    assert_eq!(b.file_count(), 1);
}

/// Write a file carrying one single-column bank, so a test can build a
/// dictionary that disagrees with `dict()` in a chosen way.
fn write_other(path: &std::path::Path, name: &str, group: u16, item: u8, col: &str) {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        name,
        group,
        item,
        [(col.into(), DataType::Int, 1)],
    ));
    let mut w = Writer::create(path).schemas(&d).build().unwrap();
    for i in 0..5_i32 {
        w.event(|ev| {
            ev.bank(name, |b| {
                b.row(|r| r.set(col, i).map(|_| ()))?;
                Ok(())
            })?;
            Ok(())
        })
        .unwrap();
    }
    w.finish().unwrap();
}

#[test]
fn chain_open_unions_dictionaries_that_do_not_conflict() {
    // A real run period is not dictionary-uniform: a pass-2 cook adds a bank,
    // an MC file carries MC::Lund. A bank simply being absent from a file is
    // something the read path already handles, so opening these together is
    // allowed and `schemas()` reports the union.
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");
    write_file(&p1, &dict(), 0, 5);
    write_other(&p2, "OTHER::Thing", 400, 1, "v");

    let chain = Chain::open([&p1, &p2]).unwrap();
    assert_eq!(chain.event_count(), 10);
    let names: Vec<&str> = chain.schemas().iter().map(|s| s.name()).collect();
    for want in ["REC::Event", "REC::Particle", "OTHER::Thing"] {
        assert!(names.contains(&want), "union missing {want}: {names:?}");
    }

    // A bank absent from a file yields empty entries for that file's events,
    // not an error and not a short result.
    let cols = chain
        .read_columns(&[("OTHER::Thing", &["v"][..])], None, 1)
        .unwrap();
    assert_eq!(
        cols[0].offsets.len(),
        11,
        "one offset per event across both files"
    );
    assert_eq!(
        cols[0].total_rows(),
        5,
        "only the file declaring the bank contributes rows"
    );
}

#[test]
fn chain_open_accepts_dictionaries_that_differ_only_in_order() {
    // Nothing makes a dictionary's write order meaningful, but the old
    // equality compared a positional Vec plus index tables keyed on insertion
    // position, so this pair used to be rejected.
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");
    let forward = dict();
    let mut reversed = Dict::new();
    for s in forward.iter().collect::<Vec<_>>().into_iter().rev() {
        reversed.add(s.clone());
    }
    assert_ne!(forward, reversed, "the two orders really do differ");
    write_file(&p1, &forward, 0, 5);
    write_file(&p2, &reversed, 5, 5);

    let chain = Chain::open([&p1, &p2]).unwrap();
    assert_eq!(chain.event_count(), 10);
    assert_eq!(chain.schemas().len(), 2);
}

#[test]
fn chain_open_rejects_a_bank_redefined_with_another_layout() {
    // Same name, different columns: reading them together would decode one
    // file's bytes against the other's column offsets.
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");
    write_file(&p1, &dict(), 0, 5);
    write_other(&p2, "REC::Particle", 300, 1, "totally_different");

    let msg = Chain::open([&p1, &p2]).unwrap_err().to_string();
    assert!(
        msg.contains("REC::Particle") && msg.contains("different layout"),
        "unexpected error: {msg}"
    );
}

#[test]
fn chain_open_rejects_a_reused_bank_id() {
    // Different names sharing one (group, item). Banks are located by id on the
    // columnar path, so this would silently decode one bank as the other —
    // wrong numbers rather than an error, which is the worst outcome available.
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");
    write_file(&p1, &dict(), 0, 5);
    write_other(&p2, "IMPOSTOR::Bank", 300, 1, "v");

    let msg = Chain::open([&p1, &p2]).unwrap_err().to_string();
    assert!(
        msg.contains("IMPOSTOR::Bank") && msg.contains("REC::Particle"),
        "error should name both claimants: {msg}"
    );
}

#[test]
fn chain_event_count_sums_across_files() {
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");
    let p3 = dir.path().join("c.hipo");
    let d = dict();
    write_file(&p1, &d, 0, 100);
    write_file(&p2, &d, 1000, 200);
    write_file(&p3, &d, 5000, 500);
    let chain = Chain::open([&p1, &p2, &p3]).unwrap();
    assert_eq!(chain.file_count(), 3);
    assert_eq!(chain.event_count(), 800);
}

#[test]
fn chain_event_random_access_crosses_files() {
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");
    let d = dict();
    write_file(&p1, &d, 0, 100); // global 0..99   → evno 0..99
    write_file(&p2, &d, 1000, 200); // global 100..299 → evno 1000..1199
    let chain = Chain::open([&p1, &p2]).unwrap();

    let ev_5 = chain.event(5).unwrap();
    assert_eq!(
        ev_5.bank("REC::Event").unwrap().col::<i64>("evno").unwrap()[0],
        5
    );
    let ev_150 = chain.event(150).unwrap();
    // global 150 = file 1, local 50 → evno = 1000 + 50 = 1050
    assert_eq!(
        ev_150
            .bank("REC::Event")
            .unwrap()
            .col::<i64>("evno")
            .unwrap()[0],
        1050
    );
    let ev_299 = chain.event(299).unwrap();
    assert_eq!(
        ev_299
            .bank("REC::Event")
            .unwrap()
            .col::<i64>("evno")
            .unwrap()[0],
        1199
    );
    assert!(chain.event(300).is_none());
    assert!(chain.event(u64::MAX).is_none());
}

#[test]
fn chain_events_iter_concat_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");
    let d = dict();
    write_file(&p1, &d, 0, 30);
    write_file(&p2, &d, 1000, 20);
    let chain = Chain::open([&p1, &p2]).unwrap();
    let mut evnos: Vec<i64> = Vec::new();
    for ev in chain.events().map(Result::unwrap) {
        let v = ev.bank("REC::Event").unwrap().col::<i64>("evno").unwrap()[0];
        evnos.push(v);
    }
    let expected: Vec<i64> = (0..30).chain(1000..1020).collect();
    assert_eq!(evnos, expected);
}

#[test]
fn chain_open_many_files_preserves_order() {
    // `Chain::open` resolves the files in parallel; a larger chain stresses
    // that the parallel collect keeps input order — events must concatenate
    // file-by-file exactly as a serial open would produce them.
    let dir = tempfile::tempdir().unwrap();
    let d = dict();
    let n = 8usize;
    let per = 40i32;
    let paths: Vec<std::path::PathBuf> = (0..n)
        .map(|k| {
            let p = dir.path().join(format!("f{k}.hipo"));
            write_file(&p, &d, k as i64 * 1000, per);
            p
        })
        .collect();

    let chain = Chain::open(paths.as_slice()).unwrap();
    assert_eq!(chain.file_count(), n);
    assert_eq!(chain.event_count(), n as u64 * per as u64);

    let expected: Vec<i64> = (0..n)
        .flat_map(|k| (0..per).map(move |i| k as i64 * 1000 + i as i64))
        .collect();
    let got: Vec<i64> = chain
        .events()
        .map(Result::unwrap)
        .map(|ev| ev.bank("REC::Event").unwrap().col::<i64>("evno").unwrap()[0])
        .collect();
    assert_eq!(got, expected, "parallel open must preserve file order");
}

#[test]
fn chain_open_dispatches_directory() {
    let dir = tempfile::tempdir().unwrap();
    let d = dict();
    write_file(&dir.path().join("a.hipo"), &d, 0, 30);
    write_file(&dir.path().join("b.hipo"), &d, 1000, 20);

    // `Chain::open` on a directory opens every *.hipo inside it.
    let chain = Chain::open(dir.path()).unwrap();
    assert_eq!(chain.file_count(), 2);
    assert_eq!(chain.event_count(), 50);
}

#[test]
fn chain_open_expands_glob_pattern() {
    let dir = tempfile::tempdir().unwrap();
    let d = dict();
    write_file(&dir.path().join("run_a.hipo"), &d, 0, 30);
    write_file(&dir.path().join("run_b.hipo"), &d, 1000, 20);
    // Same HIPO content, non-matching name — the `*.hipo` glob skips it.
    write_file(&dir.path().join("skip_me.dat"), &d, 5000, 99);

    let pattern = dir.path().join("*.hipo");
    let chain = Chain::open(pattern.to_str().unwrap()).unwrap();
    assert_eq!(chain.file_count(), 2);
    assert_eq!(chain.event_count(), 50);
}

#[test]
fn chain_open_rejects_malformed_glob() {
    // An unclosed `[` character class is an invalid glob pattern.
    let err = Chain::open("some/dir/[bad.hipo").unwrap_err();
    assert!(matches!(err, oxihipo::HipoError::InvalidGlob { .. }));
}

/// A required bank that one chain file does not declare must reject that file's
/// events, not pass them.
///
/// `Filter::bind` resolves required names against **each file's own**
/// dictionary. An unresolvable name contributed no id, and a filter with no ids
/// is exactly what "require nothing" looks like — so the clause was silently
/// dropped for that file.
///
/// `Chain::open` accepts the chain, because a bank one file lacks is a subset
/// rather than a layout conflict, and `with_filter` accepts the name, because
/// the chain's dictionary does declare it. So nothing upstream catches it, and
/// the two read paths disagreed: `events()` returned **15** where `for_each`
/// returned **6**, on the same chain with the same filter.
#[test]
fn a_required_bank_missing_from_one_file_rejects_that_file() {
    use oxihipo::{Compression, Filter};

    fn write(p: &std::path::Path, with_extra: bool, n: i32) {
        let mut d = Dict::new();
        d.add(Schema::from_columns(
            "A::b",
            300,
            1,
            [("x".into(), DataType::Int, 1)],
        ));
        if with_extra {
            d.add(Schema::from_columns(
                "Extra::c",
                301,
                1,
                [("y".into(), DataType::Int, 1)],
            ));
        }
        let mut w = Writer::create(p)
            .schemas(&d)
            .compression(Compression::Lz4)
            .max_record_events(3)
            .build()
            .unwrap();
        for i in 0..n {
            w.event(|ev| {
                ev.bank("A::b", |b| {
                    b.row(|r| {
                        r.set("x", i)?;
                        Ok(())
                    })?;
                    Ok(())
                })?;
                if with_extra {
                    ev.bank("Extra::c", |b| {
                        b.row(|r| {
                            r.set("y", i)?;
                            Ok(())
                        })?;
                        Ok(())
                    })?;
                }
                Ok(())
            })
            .unwrap();
        }
        w.finish().unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a_has.hipo"), true, 6);
    write(&dir.path().join("b_lacks.hipo"), false, 9);

    let glob = dir.path().join("*.hipo");
    let chain = Chain::open(glob.to_str().unwrap()).expect("a missing bank is not a conflict");
    assert_eq!(chain.event_count(), 15, "both files should be in the chain");

    let filtered = chain
        .with_filter(Filter::require(["Extra::c"]))
        .expect("the chain's dictionary declares Extra::c");

    let via_iter = filtered.events().filter(|e| e.is_ok()).count() as u64;
    let via_for_each = filtered.for_each(1, |_| {}).unwrap().events_yielded;
    let via_parallel = filtered.for_each(4, |_| {}).unwrap().events_yielded;

    assert_eq!(via_iter, 6, "only a_has.hipo's events carry Extra::c");
    assert_eq!(
        via_iter, via_for_each,
        "the sequential and parallel readers must agree under a filter"
    );
    assert_eq!(via_for_each, via_parallel, "thread count must not matter");
}
