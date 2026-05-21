//! End-to-end round-trip tests across the full Writer → File → events path.

use hipo::{Chain, Compression, DataType, Dict, Filter, Schema, Writer};

fn sample_dict() -> Dict {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        1,
        [
            ("pid".into(), DataType::Int),
            ("px".into(), DataType::Float),
            ("py".into(), DataType::Float),
            ("charge".into(), DataType::Byte),
        ],
    ));
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [
            ("evno".into(), DataType::Long),
            ("beamE".into(), DataType::Float),
        ],
    ));
    d
}

#[test]
fn write_then_scan_basic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rt.hipo");

    {
        let mut w = Writer::create(&path)
            .schemas(&sample_dict())
            .compression(Compression::Lz4)
            .build()
            .unwrap();
        for evno in 0..50_i64 {
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
                    for i in 0..(evno as i32 % 5 + 1) {
                        b.row(|r| {
                            r.set("pid", 11 + i)?;
                            r.set("px", i as f32 * 0.1)?;
                            r.set("py", -i as f32 * 0.1)?;
                            r.set("charge", (i as i8) - 1)?;
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

    let file = Chain::open(&path).unwrap();
    assert_eq!(file.event_count(), 50);
    assert_eq!(file.schemas().len(), 2);

    let mut total_particles = 0u64;
    let mut events_seen = 0u64;
    for ev in file.events() {
        events_seen += 1;
        let particles = ev.bank("REC::Particle").unwrap();
        total_particles += particles.rows() as u64;
        let evno = ev.bank("REC::Event").unwrap().col::<i64>("evno").unwrap()[0];
        assert!(evno < 50);
    }
    assert_eq!(events_seen, 50);
    assert!(total_particles >= 50);
}

#[test]
fn write_then_filter_require() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rt.hipo");

    let dict = {
        let mut d = sample_dict();
        // Add a schema that only some events will carry.
        d.add(Schema::from_columns(
            "RAW::tag",
            500,
            1,
            [("v".into(), DataType::Int)],
        ));
        d
    };

    {
        let mut w = Writer::create(&path).schemas(&dict).build().unwrap();
        for i in 0..20_i64 {
            w.event(|ev| {
                ev.bank("REC::Event", |b| {
                    b.row(|r| {
                        r.set("evno", i)?;
                        r.set("beamE", 10.6_f32)?;
                        Ok(())
                    })?;
                    Ok(())
                })?;
                if i % 3 == 0 {
                    ev.bank("RAW::tag", |b| {
                        b.row(|r| {
                            r.set("v", i as i32)?;
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

    let file = Chain::open(&path)
        .unwrap()
        .with_filter(Filter::require(["RAW::tag"]));

    let mut seen = 0u64;
    for ev in file.events() {
        assert!(ev.has("RAW::tag"));
        seen += 1;
    }
    // i = 0, 3, 6, 9, 12, 15, 18 → 7 events
    assert_eq!(seen, 7);
}

#[test]
fn random_access_via_event() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rt.hipo");

    {
        let mut w = Writer::create(&path)
            .schemas(&sample_dict())
            .max_record_events(5)
            .build()
            .unwrap();
        for evno in 0..23_i64 {
            w.event(|ev| {
                ev.bank("REC::Event", |b| {
                    b.row(|r| {
                        r.set("evno", evno)?;
                        r.set("beamE", 10.6_f32)?;
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

    let file = Chain::open(&path).unwrap();
    let owned = file.event(17).unwrap();
    let evno = owned
        .bank("REC::Event")
        .unwrap()
        .col::<i64>("evno")
        .unwrap()[0];
    assert_eq!(evno, 17);
}

#[test]
fn column_handle_matches_col() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rt.hipo");

    {
        let mut w = Writer::create(&path)
            .schemas(&sample_dict())
            .build()
            .unwrap();
        w.event(|ev| {
            ev.bank("REC::Particle", |b| {
                for i in 0..5_i32 {
                    b.row(|r| {
                        r.set("pid", i * 11)?;
                        r.set("px", i as f32 * 0.5)?;
                        r.set("py", i as f32 * -0.25)?;
                        r.set("charge", (i as i8) - 2)?;
                        Ok(())
                    })?;
                }
                Ok(())
            })?;
            Ok(())
        })
        .unwrap();
        w.finish().unwrap();
    }

    let file = Chain::open(&path).unwrap();
    let schema = file.schemas().require("REC::Particle").unwrap().clone();
    let h_pid = schema.handle::<i32>("pid").unwrap();
    let h_px = schema.handle::<f32>("px").unwrap();
    let h_charge = schema.handle::<i8>("charge").unwrap();

    for ev in file.events() {
        let bank = ev.bank("REC::Particle").unwrap();
        let pid_named = bank.col::<i32>("pid").unwrap();
        let pid_handle = bank.read(h_pid);
        assert_eq!(pid_named, pid_handle);

        let px_named = bank.col::<f32>("px").unwrap();
        let px_handle = bank.read(h_px);
        assert_eq!(px_named, px_handle);

        let chg_named = bank.col::<i8>("charge").unwrap();
        let chg_handle = bank.read(h_charge);
        assert_eq!(chg_named, chg_handle);
    }
}

#[test]
fn owned_event_crosses_thread_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rt.hipo");

    {
        let mut w = Writer::create(&path)
            .schemas(&sample_dict())
            .build()
            .unwrap();
        for evno in 0..3_i64 {
            w.event(|ev| {
                ev.bank("REC::Event", |b| {
                    b.row(|r| {
                        r.set("evno", evno + 100)?;
                        r.set("beamE", 10.6_f32)?;
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

    let file = Chain::open(&path).unwrap();
    let owned = file.event(1).unwrap();

    let h = std::thread::spawn(move || {
        let bank = owned.bank("REC::Event").unwrap();
        bank.col::<i64>("evno").unwrap()[0]
    });
    assert_eq!(h.join().unwrap(), 101);
}

#[test]
fn chain_scans_multiple_files() {
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");

    for (path, range) in [(&p1, 0..3_i64), (&p2, 100..105)] {
        let mut w = Writer::create(path)
            .schemas(&sample_dict())
            .build()
            .unwrap();
        for evno in range {
            w.event(|ev| {
                ev.bank("REC::Event", |b| {
                    b.row(|r| {
                        r.set("evno", evno)?;
                        r.set("beamE", 1.0_f32)?;
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

    let chain = Chain::open_all([&p1, &p2]).unwrap();
    let mut evnos = Vec::new();
    for ev in chain.events() {
        evnos.push(ev.bank("REC::Event").unwrap().col::<i64>("evno").unwrap()[0]);
    }
    assert_eq!(evnos, vec![0, 1, 2, 100, 101, 102, 103, 104]);
}

#[test]
fn events_iter_supports_plain_for_loop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rt.hipo");

    {
        let mut w = Writer::create(&path)
            .schemas(&sample_dict())
            .max_record_events(7)
            .build()
            .unwrap();
        for evno in 0..30_i64 {
            w.event(|ev| {
                ev.bank("REC::Event", |b| {
                    b.row(|r| {
                        r.set("evno", evno + 1)?;
                        r.set("beamE", 10.6_f32)?;
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

    let file = Chain::open(&path).unwrap();
    let mut count = 0u64;
    let mut sum = 0_i64;
    for ev in file.events() {
        count += 1;
        sum += ev.bank("REC::Event").unwrap().col::<i64>("evno").unwrap()[0];
    }
    assert_eq!(count, 30);
    // sum of 1..=30
    assert_eq!(sum, 465);
}

#[test]
fn events_iter_honors_filter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rt.hipo");

    let mut d = sample_dict();
    d.add(Schema::from_columns(
        "RAW::tag",
        500,
        1,
        [("v".into(), DataType::Int)],
    ));

    {
        let mut w = Writer::create(&path).schemas(&d).build().unwrap();
        for i in 0..20_i64 {
            w.event(|ev| {
                ev.bank("REC::Event", |b| {
                    b.row(|r| {
                        r.set("evno", i)?;
                        r.set("beamE", 1.0_f32)?;
                        Ok(())
                    })?;
                    Ok(())
                })?;
                if i % 4 == 0 {
                    ev.bank("RAW::tag", |b| {
                        b.row(|r| r.set("v", i as i32).map(|_| ()))?;
                        Ok(())
                    })?;
                }
                Ok(())
            })
            .unwrap();
        }
        w.finish().unwrap();
    }

    let file = Chain::open(&path)
        .unwrap()
        .with_filter(Filter::require(["RAW::tag"]));
    let mut count = 0;
    for _ev in file.events() {
        count += 1;
    }
    // i = 0, 4, 8, 12, 16 → 5 events
    assert_eq!(count, 5);
}

#[test]
fn events_iter_break_early() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rt.hipo");

    {
        let mut w = Writer::create(&path)
            .schemas(&sample_dict())
            .build()
            .unwrap();
        for evno in 0..50_i64 {
            w.event(|ev| {
                ev.bank("REC::Event", |b| {
                    b.row(|r| {
                        r.set("evno", evno)?;
                        r.set("beamE", 1.0_f32)?;
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

    let file = Chain::open(&path).unwrap();
    let mut count = 0u64;
    for _ev in file.events() {
        count += 1;
        if count == 5 {
            break;
        }
    }
    assert_eq!(count, 5);
}

#[test]
fn events_iter_counts_all_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rt.hipo");

    {
        let mut w = Writer::create(&path)
            .schemas(&sample_dict())
            .max_record_events(50)
            .build()
            .unwrap();
        for evno in 0..500_i64 {
            w.event(|ev| {
                ev.bank("REC::Event", |b| {
                    b.row(|r| {
                        r.set("evno", evno)?;
                        r.set("beamE", 10.6_f32)?;
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

    // 500 events across 10 records (max_record_events = 50): the `for`
    // loop must walk every event of every record.
    let file = Chain::open(&path).unwrap();
    assert_eq!(file.events().count(), 500);
}
