//! End-to-end round-trip tests across the full Writer → File → events path.

use oxihipo::{Chain, Compression, DataType, Dict, Filter, Schema, Writer};

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

#[test]
fn write_then_scan_array_columns() {
    // Schema with a mix of scalar and array columns. Round-trip through
    // the full writer → file → Chain pipeline and assert every cell
    // matches.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("array_cols.hipo");

    let dict = {
        let mut d = Dict::new();
        d.add(Schema::parse_text("{REC::Traj/100/1}{pid/I,cov/F#9,hits/S#4}").unwrap());
        d
    };

    {
        let mut w = Writer::create(&path).schemas(&dict).build().unwrap();
        for evno in 0..50_i32 {
            w.event(|ev| {
                ev.bank("REC::Traj", |b| {
                    for r in 0..3_i32 {
                        b.row(|r_w| {
                            r_w.set("pid", evno * 100 + r)?;
                            r_w.set(
                                "cov",
                                [
                                    evno as f32 + 0.1 * r as f32 + 0.0,
                                    evno as f32 + 0.1 * r as f32 + 0.1,
                                    evno as f32 + 0.1 * r as f32 + 0.2,
                                    evno as f32 + 0.1 * r as f32 + 0.3,
                                    evno as f32 + 0.1 * r as f32 + 0.4,
                                    evno as f32 + 0.1 * r as f32 + 0.5,
                                    evno as f32 + 0.1 * r as f32 + 0.6,
                                    evno as f32 + 0.1 * r as f32 + 0.7,
                                    evno as f32 + 0.1 * r as f32 + 0.8,
                                ],
                            )?;
                            r_w.set(
                                "hits",
                                [
                                    (evno + r) as i16,
                                    (evno + r + 1) as i16,
                                    (evno + r + 2) as i16,
                                    (evno + r + 3) as i16,
                                ],
                            )?;
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

    let mut seen = 0_i32;
    for ev in file.events() {
        let bank = ev.bank("REC::Traj").unwrap();
        assert_eq!(bank.rows(), 3);
        let pids = bank.col::<i32>("pid").unwrap();
        let cov = bank.col::<[f32; 9]>("cov").unwrap();
        let hits = bank.col::<[i16; 4]>("hits").unwrap();
        for r in 0..3_usize {
            assert_eq!(pids[r], seen * 100 + r as i32);
            let expected_cov: [f32; 9] =
                std::array::from_fn(|i| seen as f32 + 0.1 * r as f32 + i as f32 * 0.1);
            for (a, b) in cov[r].iter().zip(expected_cov.iter()) {
                assert!((a - b).abs() < 1e-5, "cov[{r}][...]={a} expected {b}");
            }
            let expected_hits: [i16; 4] =
                std::array::from_fn(|i| (seen + r as i32 + i as i32) as i16);
            assert_eq!(hits[r], expected_hits);
        }
        seen += 1;
    }
    assert_eq!(seen, 50);
}

#[test]
fn write_then_scan_lz4_chunked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chunked.hipo");

    // ~100 events across several records (max_record_events small enough
    // that we get multiple records, and chunk_size small enough that we
    // get multiple chunks per record).
    {
        let mut w = Writer::create(&path)
            .schemas(&sample_dict())
            .compression(Compression::Lz4Chunked {
                events_per_chunk: 4,
            })
            .max_record_events(30)
            .build()
            .unwrap();
        for evno in 0..100_i64 {
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
                    for i in 0..((evno as i32) % 5 + 1) {
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
    assert_eq!(file.event_count(), 100);
    // Should be > 1 record so we exercise the chunk-boundary case.
    assert!(file.record_count() > 1);

    let mut seen = 0_i64;
    for ev in file.events() {
        let evno = ev.bank("REC::Event").unwrap().col::<i64>("evno").unwrap()[0];
        assert_eq!(evno, seen);
        let p = ev.bank("REC::Particle").unwrap();
        let pids = p.col::<i32>("pid").unwrap();
        // pids start at 11 and march up
        for (i, &pid) in pids.iter().enumerate() {
            assert_eq!(pid, 11 + i as i32);
        }
        seen += 1;
    }
    assert_eq!(seen, 100);
}

#[test]
fn write_then_scan_lz4_by_bank() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("by_bank.hipo");

    {
        let mut w = Writer::create(&path)
            .schemas(&sample_dict())
            .compression(Compression::Lz4ByBank)
            .max_record_events(40)
            .build()
            .unwrap();
        for evno in 0..200_i64 {
            w.event(|ev| {
                ev.bank("REC::Event", |b| {
                    b.row(|r| {
                        r.set("evno", evno)?;
                        r.set("beamE", 10.6_f32)?;
                        Ok(())
                    })?;
                    Ok(())
                })?;
                // Some events also carry REC::Particle, others don't —
                // exercise the presence matrix.
                if evno % 3 != 0 {
                    ev.bank("REC::Particle", |b| {
                        for i in 0..((evno as i32) % 4 + 1) {
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
                }
                Ok(())
            })
            .unwrap();
        }
        w.finish().unwrap();
    }

    let file = Chain::open(&path).unwrap();
    assert_eq!(file.event_count(), 200);
    assert!(file.record_count() >= 5);

    let mut seen = 0_i64;
    for ev in file.events() {
        let evno = ev.bank("REC::Event").unwrap().col::<i64>("evno").unwrap()[0];
        assert_eq!(evno, seen);

        if seen % 3 == 0 {
            // Event without REC::Particle.
            assert!(!ev.has("REC::Particle"));
            assert!(ev.bank("REC::Particle").is_none());
        } else {
            let p = ev.bank("REC::Particle").unwrap();
            let pids = p.col::<i32>("pid").unwrap();
            for (i, &pid) in pids.iter().enumerate() {
                assert_eq!(pid, 11 + i as i32);
            }
        }
        seen += 1;
    }
    assert_eq!(seen, 200);
}

#[test]
fn lz4_by_bank_par_reduce_matches_sequential() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("by_bank_par.hipo");

    {
        let mut w = Writer::create(&path)
            .schemas(&sample_dict())
            .compression(Compression::Lz4ByBank)
            .max_record_events(30)
            .build()
            .unwrap();
        for evno in 0..300_i64 {
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
    let seq_sum: i64 = file
        .events()
        .map(|ev| ev.bank("REC::Event").unwrap().col::<i64>("evno").unwrap()[0])
        .sum();
    let par_sum: i64 = file
        .par_reduce(
            0,
            || 0_i64,
            |acc, ev| acc + ev.bank("REC::Event").unwrap().col::<i64>("evno").unwrap()[0],
            |a, b| a + b,
        )
        .unwrap();
    assert_eq!(seq_sum, (0..300_i64).sum::<i64>());
    assert_eq!(par_sum, seq_sum);
}

#[test]
fn lz4_by_bank_skips_unused_banks() {
    // Verify the partial-decompression contract: a scan that only ever
    // reads REC::Event must NOT inflate REC::Particle's stream. We test
    // this indirectly by exercising the same file with two scans (one
    // touching only REC::Event, one touching both) and asserting both
    // produce correct results — the partial-touch case should still
    // succeed even though REC::Particle bytes are never inflated.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("by_bank_partial.hipo");

    {
        let mut w = Writer::create(&path)
            .schemas(&sample_dict())
            .compression(Compression::Lz4ByBank)
            .max_record_events(50)
            .build()
            .unwrap();
        for evno in 0..150_i64 {
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
        }
        w.finish().unwrap();
    }

    // Scan touching only REC::Event — Particle should never decompress.
    {
        let file = Chain::open(&path).unwrap();
        let mut sum: i64 = 0;
        for ev in file.events() {
            sum += ev.bank("REC::Event").unwrap().col::<i64>("evno").unwrap()[0];
        }
        assert_eq!(sum, (0..150_i64).sum::<i64>());
    }
    // Scan touching both — exercise the full partial-decompression cache.
    {
        let file = Chain::open(&path).unwrap();
        let mut events_total = 0u64;
        let mut particle_total = 0u64;
        for ev in file.events() {
            let _ = ev.bank("REC::Event").unwrap();
            particle_total += ev.bank("REC::Particle").unwrap().rows() as u64;
            events_total += 1;
        }
        assert_eq!(events_total, 150);
        assert_eq!(particle_total, 150 * 5);
    }
}

#[test]
fn lz4_chunked_par_reduce_matches_sequential() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chunked_par.hipo");

    {
        let mut w = Writer::create(&path)
            .schemas(&sample_dict())
            .compression(Compression::Lz4Chunked {
                events_per_chunk: 8,
            })
            .max_record_events(25)
            .build()
            .unwrap();
        for evno in 0..200_i64 {
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
    let seq_sum: i64 = file
        .events()
        .map(|ev| ev.bank("REC::Event").unwrap().col::<i64>("evno").unwrap()[0])
        .sum();

    let par_sum: i64 = file
        .par_reduce(
            0,
            || 0_i64,
            |acc, ev| acc + ev.bank("REC::Event").unwrap().col::<i64>("evno").unwrap()[0],
            |a, b| a + b,
        )
        .unwrap();

    assert_eq!(seq_sum, (0..200_i64).sum::<i64>());
    assert_eq!(par_sum, seq_sum);
}
