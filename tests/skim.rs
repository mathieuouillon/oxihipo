//! Integration tests for `Chain::skim` — filter-and-rewrite.

use oxihipo::{Chain, Compression, DataType, Dict, Filter, Schema, Writer};

fn dict() -> Dict {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [("evno".into(), DataType::Long, 1)],
    ));
    d.add(Schema::from_columns(
        "RAW::tag",
        500,
        1,
        [("v".into(), DataType::Int, 1)],
    ));
    d
}

/// Write `count` events; event `i` carries `RAW::tag` (with `v = i`) iff
/// `tag_every` is `Some(n)` and `i % n == 0`.
fn write_file(path: &std::path::Path, d: &Dict, count: i32, tag_every: Option<i32>) {
    let mut w = Writer::create(path)
        .schemas(d)
        .max_record_events(20)
        .build()
        .unwrap();
    for i in 0..count {
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| r.set("evno", i as i64).map(|_| ()))?;
                Ok(())
            })?;
            if tag_every.is_some_and(|n| i % n == 0) {
                ev.bank("RAW::tag", |b| {
                    b.row(|r| r.set("v", i).map(|_| ()))?;
                    Ok(())
                })?;
            }
            Ok(())
        })
        .unwrap();
    }
    w.finish().unwrap();
}

#[test]
fn skim_filters_and_rewrites() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.hipo");
    let out = dir.path().join("skim.hipo");
    let d = dict();
    write_file(&src, &d, 100, Some(5)); // 20 tagged: 0, 5, …, 95

    let summary = Chain::open(&src)
        .unwrap()
        .with_filter(Filter::require(["RAW::tag"]))
        .unwrap()
        .skim(&out, Compression::Lz4PerBank)
        .unwrap();
    assert_eq!(summary.events, 20);
    assert!(summary.records >= 1);

    // Reopen: every survivor carries RAW::tag, the count is 20, and the
    // values match the filtered source exactly.
    let skimmed = Chain::open(&out).unwrap();
    assert_eq!(skimmed.event_count(), 20);
    let mut seen = Vec::new();
    for ev in skimmed.events().map(Result::unwrap) {
        let tag = ev.bank("RAW::tag").expect("survivor carries RAW::tag");
        seen.push(tag.get::<i32>("v", 0));
    }
    let expected: Vec<i32> = (0..100).filter(|i| i % 5 == 0).collect();
    assert_eq!(seen, expected);
}

#[test]
fn skim_no_filter_copies_every_event() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.hipo");
    let out = dir.path().join("skim.hipo");
    write_file(&src, &dict(), 37, Some(5));

    let summary = Chain::open(&src)
        .unwrap()
        .skim(&out, Compression::Lz4PerBank)
        .unwrap();
    assert_eq!(summary.events, 37);
    assert_eq!(Chain::open(&out).unwrap().event_count(), 37);
}

#[test]
fn skim_empty_survivors_is_valid() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.hipo");
    let out = dir.path().join("skim.hipo");
    // No event carries RAW::tag, but the name is in the dict so the filter
    // validates — every event is dropped, yielding a valid 0-event file.
    write_file(&src, &dict(), 10, None);

    let summary = Chain::open(&src)
        .unwrap()
        .with_filter(Filter::require(["RAW::tag"]))
        .unwrap()
        .skim(&out, Compression::Lz4PerBank)
        .unwrap();
    assert_eq!(summary.events, 0);
    assert_eq!(Chain::open(&out).unwrap().event_count(), 0);
}

#[test]
fn with_filter_rejects_unknown_bank() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.hipo");
    write_file(&src, &dict(), 5, Some(1));

    let err = Chain::open(&src)
        .unwrap()
        .with_filter(Filter::require(["NOPE::typo"]))
        .unwrap_err();
    assert!(matches!(err, oxihipo::HipoError::UnknownSchema { .. }));
}
