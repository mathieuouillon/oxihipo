//! Integration tests for `Chain` — eager multi-file open, dict
//! validation, and random access by global event index.

use hipo::{Chain, DataType, Dict, Schema, Writer};

fn dict() -> Dict {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [
            ("evno".into(), DataType::Long),
            ("beamE".into(), DataType::Float),
        ],
    ));
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        1,
        [("pid".into(), DataType::Int)],
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
fn chain_open_single_file_matches_open_all() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("one.hipo");
    write_file(&p, &dict(), 0, 50);
    let a = Chain::open(&p).unwrap();
    let b = Chain::open_all([&p]).unwrap();
    assert_eq!(a.event_count(), b.event_count());
    assert_eq!(a.file_count(), 1);
    assert_eq!(b.file_count(), 1);
}

#[test]
fn chain_open_validates_dict_match() {
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");
    // File 1: standard dict.
    write_file(&p1, &dict(), 0, 5);
    // File 2: completely different dict (different schemas).
    {
        let mut d2 = Dict::new();
        d2.add(Schema::from_columns(
            "OTHER::Thing",
            1,
            1,
            [("v".into(), DataType::Int)],
        ));
        let mut w = Writer::create(&p2).schemas(&d2).build().unwrap();
        for i in 0..5_i32 {
            w.event(|ev| {
                ev.bank("OTHER::Thing", |b| {
                    b.row(|r| r.set("v", i).map(|_| ()))?;
                    Ok(())
                })?;
                Ok(())
            })
            .unwrap();
        }
        w.finish().unwrap();
    }
    let err = Chain::open_all([&p1, &p2]).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("different dictionary"),
        "unexpected error: {msg}"
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
    let chain = Chain::open_all([&p1, &p2, &p3]).unwrap();
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
    let chain = Chain::open_all([&p1, &p2]).unwrap();

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
    let chain = Chain::open_all([&p1, &p2]).unwrap();
    let mut evnos: Vec<i64> = Vec::new();
    for ev in chain.events() {
        let v = ev.bank("REC::Event").unwrap().col::<i64>("evno").unwrap()[0];
        evnos.push(v);
    }
    let expected: Vec<i64> = (0..30).chain(1000..1020).collect();
    assert_eq!(evnos, expected);
}

#[test]
fn chain_open_dispatches_directory_to_open_dir() {
    let dir = tempfile::tempdir().unwrap();
    let d = dict();
    write_file(&dir.path().join("a.hipo"), &d, 0, 30);
    write_file(&dir.path().join("b.hipo"), &d, 1000, 20);

    // `Chain::open` on a directory opens every *.hipo inside it,
    // exactly like `Chain::open_dir`.
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
    assert!(matches!(err, hipo::HipoError::InvalidGlob { .. }));
}
