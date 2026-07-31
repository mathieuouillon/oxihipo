//! Regression test: the `for ev in file.events()` hot loop must be
//! zero-allocation in steady state.
//!
//! Per the design contract:
//! - Per event: 2 `Arc::clone`s (atomic increments, not allocations).
//! - Per record: one decompression into a recycled `Vec<u8>` (via
//!   `Arc::try_unwrap`) and a refill of a recycled event-offsets `Vec<u32>`.
//!
//! Implementation note: this binary installs a counting wrapper around the
//! system allocator. Counting is armed by a *thread-local* flag, set only on
//! the thread running [`count_allocs`], so allocations on background threads —
//! rayon's global pool (spawned when `Chain::open` fans file opens across
//! workers) and the test harness — are never attributed to the measurement.
//! The global `ALLOCS` counter is therefore only ever touched by the measuring
//! thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use oxihipo::{Chain, DataType, Dict, Schema, Writer};

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Armed only on the thread inside [`count_allocs`]. `const`-init means
    /// TLS access allocates nothing, so it is safe to read from within the
    /// allocator without reentrancy.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

/// Whether the *current* thread is inside a counting window.
#[inline]
fn counting() -> bool {
    COUNTING.try_with(|c| c.get()).unwrap_or(false)
}

struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if counting() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if counting() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if counting() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn build_fixture_with(
    path: &std::path::Path,
    events: i32,
    max_record_events: u32,
    compression: oxihipo::Compression,
) {
    let mut dict = Dict::new();
    dict.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [
            ("evno".into(), DataType::Long, 1),
            ("beamE".into(), DataType::Float, 1),
        ],
    ));
    let mut w = Writer::create(path)
        .schemas(&dict)
        .compression(compression)
        .max_record_events(max_record_events)
        .build()
        .unwrap();
    for evno in 0..events as i64 {
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

fn build_fixture(path: &std::path::Path, events: i32, max_record_events: u32) {
    build_fixture_with(path, events, max_record_events, oxihipo::Compression::Lz4);
}

/// Serialises the counting windows.
///
/// The gate (`COUNTING`) is thread-local but the counter (`ALLOCS`) is global,
/// so two windows open at once on different threads clobber each other: one
/// test's `store(0)` zeroes the other's running total, and each attributes the
/// other's allocations to itself. `cargo test` runs the tests in this file on
/// separate threads, so that is not hypothetical — it surfaced as
/// `column_iter_and_read_into_are_alloc_free_on_a_mixed_width_bank` failing
/// once a third counting test was added, having passed with two purely by
/// luck of scheduling.
///
/// Taken *before* arming the gate, so the lock's own bookkeeping is never
/// counted. Poisoning is ignored: a panicking test has already failed, and its
/// leftover count is discarded by the next `store(0)` anyway.
static COUNT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run a closure with allocation counting enabled; returns the count.
fn count_allocs<F: FnOnce()>(f: F) -> usize {
    let _guard = COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    ALLOCS.store(0, Ordering::Relaxed);
    COUNTING.with(|c| c.set(true));
    f();
    COUNTING.with(|c| c.set(false));
    ALLOCS.load(Ordering::Relaxed)
}

#[test]
fn iter_alloc_contract() {
    let dir = tempfile::tempdir().unwrap();
    let small = dir.path().join("small.hipo");
    let big = dir.path().join("big.hipo");
    build_fixture(&small, 1000, 200); //  5 records of 200 events
    build_fixture(&big, 5000, 100); // 50 records of 100 events

    // ---- Test 1: steady-state recycling is alloc-free.
    //
    // When the user drops each event immediately, the iterator can
    // recover the previous record's payload `Vec` via `Arc::try_unwrap`
    // and reuse it. After warmup (where buffers grow to their stable
    // size), the inner loop must do zero heap allocations.
    {
        let file = Chain::open(&small).unwrap();
        let mut iter = file.events().map(Result::unwrap);
        for _ in 0..200 {
            let _ = iter.next(); // warmup
        }
        let allocs = count_allocs(|| {
            for _ev in iter.by_ref() {
                // Drop immediately — keeps the Arc count down so the
                // next record can recycle the buffer.
            }
        });
        assert!(
            allocs <= 4,
            "steady-state iteration must be alloc-free; got {allocs}"
        );
    }

    // ---- Test 2: collect-path scales with records, not events.
    //
    // When the user collects events into a `Vec`, the previous record's
    // payload stays alive (held by the collected `OwnedEvent`s) so the
    // iterator allocates a fresh `Vec<u8>` per record. The allocation
    // rate must be O(records), not O(events).
    let small_file = Chain::open(&small).unwrap();
    let big_file = Chain::open(&big).unwrap();
    let _: Vec<_> = small_file.events().map(Result::unwrap).take(50).collect(); // warmup
    let _: Vec<_> = big_file.events().map(Result::unwrap).take(50).collect();

    let mut collected_small = Vec::with_capacity(1000);
    let allocs_small = count_allocs(|| {
        for ev in small_file.events().map(Result::unwrap) {
            collected_small.push(ev);
        }
    });
    let mut collected_big = Vec::with_capacity(5000);
    let allocs_big = count_allocs(|| {
        for ev in big_file.events().map(Result::unwrap) {
            collected_big.push(ev);
        }
    });

    assert_eq!(collected_small.len(), 1000);
    assert_eq!(collected_big.len(), 5000);

    // big has 10× the records of small (50 vs 5). If allocations were
    // per-record, allocs_big / allocs_small ≈ 10. If they were
    // per-event, the same ratio (events and records both scaled 10×).
    // The interesting check is *absolute* allocations per event:
    let per_event_small = allocs_small as f64 / collected_small.len() as f64;
    let per_event_big = allocs_big as f64 / collected_big.len() as f64;
    assert!(
        per_event_small < 0.2 && per_event_big < 0.2,
        "collect-path should allocate << 1× per event; got \
         {per_event_small:.3} (small) and {per_event_big:.3} (big)"
    );

    // Sanity: collected events still decode correctly (the Arc keeps
    // the underlying payload buffer alive across iterator advance).
    let evno = collected_big[1234]
        .bank("REC::Event")
        .unwrap()
        .col::<i64>("evno")
        .unwrap()[0];
    assert!((0..5000).contains(&evno));

    // ---- Test 3: the same contract on the split codecs.
    allocation_scales_with_records_not_events(dir.path());
}

/// The per-event allocation contract, on **every** codec.
///
/// `Chain::events` documents "no per-event allocation; the record buffer is
/// shared by `Arc` and recycled". That was measured only on the blob codecs.
/// `Lz4PerBank` and `Lz4PerColumn` store banks separately and hand out events
/// that borrow from the shared record, so the claim should hold for them too —
/// it did not: every by-bank event heap-allocated an empty `Arc<OnceLock>` for
/// a synthetic blob it usually never built. 800 events cost 852 allocations.
///
/// Rather than count against a fixed budget (which would just re-encode
/// whatever the numbers happen to be), this compares two files with the **same
/// record count** and 4× the events. Per-record work cancels; anything that
/// scales with events shows up as a 4× gap.
///
/// Called from `iter_alloc_contract` rather than being its own `#[test]`:
/// `ALLOCS` is a single global counter, so two tests measuring concurrently
/// attribute each other's allocations and both readings become meaningless.
fn allocation_scales_with_records_not_events(dir: &std::path::Path) {
    for codec in [
        oxihipo::Compression::None,
        oxihipo::Compression::Lz4,
        oxihipo::Compression::Lz4PerBank,
        oxihipo::Compression::Lz4PerColumn,
    ] {
        // Both files hold 4 records. Only the events per record differ.
        let few = dir.join(format!("{codec:?}_few.hipo"));
        let many = dir.join(format!("{codec:?}_many.hipo"));
        build_fixture_with(&few, 800, 200, codec);
        build_fixture_with(&many, 3200, 800, codec);

        let measure = |path: &std::path::Path| -> usize {
            let file = Chain::open(path).unwrap();
            // Warm up: first-touch growth of the recycled buffers is per-file,
            // not per-event, and would otherwise land inside the window.
            for ev in file.events().map(Result::unwrap) {
                std::hint::black_box(ev.bank("REC::Event").map(|b| b.get::<i64>("evno", 0)));
            }
            let file = Chain::open(path).unwrap();
            count_allocs(|| {
                for ev in file.events().map(Result::unwrap) {
                    std::hint::black_box(ev.bank("REC::Event").map(|b| b.get::<i64>("evno", 0)));
                }
            })
        };

        let a = measure(&few);
        let b = measure(&many);
        println!("{codec:?}: 800 events -> {a} allocs, 3200 events -> {b} allocs (4 records each)");
        assert!(
            b <= a + 8,
            "{codec:?}: allocations scale with events, not records: \
             800 events cost {a}, 3200 cost {b}"
        );
    }
}

/// A bank whose row is **not** a multiple of 4 bytes, like every real CLAS12
/// `REC::Particle`: `charge/B` and `status/S` sit between the floats, giving
/// row_size 43.
///
/// This matters because HIPO packs rows with no padding, so a bank's column
/// offsets inherit the row width. A schema built only from 4-byte columns —
/// which is what the rest of this file and `benches/read.rs` use — keeps every
/// column 4-aligned by construction, and so can never exercise the unaligned
/// path in `Bank::col`. Real files do, on most banks.
fn build_mixed_fixture(path: &std::path::Path, events: i32) {
    let mut dict = Dict::new();
    dict.add(Schema::from_columns(
        "REC::Particle",
        300,
        31,
        [
            ("pid".into(), DataType::Int, 1),       //  4
            ("px".into(), DataType::Float, 1),      //  4
            ("py".into(), DataType::Float, 1),      //  4
            ("pz".into(), DataType::Float, 1),      //  4
            ("vx".into(), DataType::Float, 1),      //  4
            ("vy".into(), DataType::Float, 1),      //  4
            ("vz".into(), DataType::Float, 1),      //  4
            ("vt".into(), DataType::Float, 1),      //  4
            ("charge".into(), DataType::Byte, 1),   //  1  <- breaks 4-alignment
            ("beta".into(), DataType::Float, 1),    //  4
            ("chi2pid".into(), DataType::Float, 1), //  4
            ("status".into(), DataType::Short, 1),  //  2
        ], // = 43
    ));
    let mut w = Writer::create(path)
        .schemas(&dict)
        .compression(oxihipo::Compression::Lz4)
        .max_record_events(200)
        .build()
        .unwrap();
    for e in 0..events {
        w.event(|ev| {
            ev.bank("REC::Particle", |b| {
                // A few rows per event, as a real DST has.
                for r in 0..(1 + (e % 4)) {
                    b.row(|w| {
                        w.set("pid", 11i32)?;
                        w.set("px", 0.5_f32 + r as f32)?;
                        w.set("py", -0.2_f32)?;
                        w.set("pz", 2.0_f32)?;
                        w.set("vx", 0.0_f32)?;
                        w.set("vy", 0.0_f32)?;
                        w.set("vz", -3.0_f32)?;
                        w.set("vt", 0.0_f32)?;
                        w.set("charge", -1i8)?;
                        w.set("beta", 0.99_f32)?;
                        w.set("chi2pid", 1.5_f32)?;
                        w.set("status", -2000i16)?;
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

/// The allocation-free column accessors must stay allocation-free on a bank
/// whose rows are not a multiple of the column width.
///
/// `Bank::col` / `Bank::read` return a slice, so on a misaligned column they
/// have no choice but to copy into an owned `Vec` — and a column is misaligned
/// whenever the row width is not a multiple of `size_of::<T>()`, which is most
/// real CLAS12 banks (`REC::Particle` is 43 bytes). `Bank::iter` and
/// `Bank::read_into` exist precisely so that a full-column pass need not pay
/// that, and this pins both.
///
/// The pre-existing alloc contract in this file could not have caught the
/// regression these guard against: its window only drives `iter.next()`, and
/// its fixture is `evno/L, beamE/F` — row 12, so every column is 4-aligned by
/// construction and the fallback is unreachable.
#[test]
fn column_iter_and_read_into_are_alloc_free_on_a_mixed_width_bank() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.hipo");
    build_mixed_fixture(&path, 2000);

    let file = Chain::open(&path).unwrap();
    let schema = file.schemas().get("REC::Particle").unwrap();
    let px = schema.handle::<f32>("px").unwrap();
    let pz = schema.handle::<f32>("pz").unwrap();

    // Warm up so the window measures column access, not buffer growth.
    let mut warm = 0usize;
    for ev in file.events().map(Result::unwrap).take(400) {
        if let Some(b) = ev.bank("REC::Particle") {
            warm += b.iter(px).count();
        }
    }
    assert!(warm > 0, "fixture produced no rows");

    // ---- `iter`: no allocation at all.
    let mut sum = 0.0f64;
    let allocs = count_allocs(|| {
        for ev in file.events().map(Result::unwrap) {
            let Some(b) = ev.bank("REC::Particle") else {
                continue;
            };
            sum += b.iter(px).map(f64::from).sum::<f64>();
            sum += b.iter(pz).map(f64::from).sum::<f64>();
        }
    });
    assert!(sum != 0.0, "columns read as all-zero — fixture is wrong");
    assert!(
        allocs <= 32,
        "Bank::iter allocated {allocs} times over the scan; it must read in          place. See ColumnIter in src/event/bank.rs."
    );

    // ---- `read_into` with a hoisted buffer: one allocation, not one per event.
    let mut buf: Vec<f32> = Vec::new();
    let file2 = Chain::open(&path).unwrap();
    for ev in file2.events().map(Result::unwrap).take(400) {
        if let Some(b) = ev.bank("REC::Particle") {
            b.read_into(px, &mut buf); // warm the buffer to steady size
        }
    }
    let mut rows = 0usize;
    let allocs = count_allocs(|| {
        for ev in file2.events().map(Result::unwrap) {
            let Some(b) = ev.bank("REC::Particle") else {
                continue;
            };
            rows += b.read_into(px, &mut buf);
        }
    });
    assert!(rows > 0);
    assert!(
        allocs <= 32,
        "Bank::read_into allocated {allocs} times with a hoisted buffer; it          must reuse the caller's capacity"
    );
}

/// `iter` and `read_into` must agree with `col`, on both alignments.
///
/// They take different paths — element-wise, bulk memcpy, and the borrowed
/// fast path — so a divergence would be silent wrong numbers rather than a
/// crash. The mixed-width fixture exercises the misaligned path; a 4-aligned
/// bank would only ever prove the easy one.
#[test]
fn the_three_column_accessors_agree() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agree.hipo");
    build_mixed_fixture(&path, 200);

    let file = Chain::open(&path).unwrap();
    let schema = file.schemas().get("REC::Particle").unwrap();
    let px = schema.handle::<f32>("px").unwrap();

    let mut buf = Vec::new();
    let mut checked = 0usize;
    for ev in file.events().map(Result::unwrap) {
        let Some(b) = ev.bank("REC::Particle") else {
            continue;
        };
        let via_col: Vec<f32> = b.col::<f32>("px").unwrap().to_vec();
        let via_iter: Vec<f32> = b.iter(px).collect();
        b.read_into(px, &mut buf);

        assert_eq!(via_col, via_iter, "iter disagrees with col");
        assert_eq!(via_col, buf, "read_into disagrees with col");
        assert!(!via_col.is_empty());
        checked += via_col.len();
    }
    assert!(checked > 100, "only checked {checked} cells");
}

/// `iter` is an `ExactSizeIterator` and a `DoubleEndedIterator`; both must
/// agree with the row count rather than with the byte length.
#[test]
fn column_iter_length_and_reverse() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rev.hipo");
    build_mixed_fixture(&path, 40);

    let file = Chain::open(&path).unwrap();
    let schema = file.schemas().get("REC::Particle").unwrap();
    let px = schema.handle::<f32>("px").unwrap();

    for ev in file.events().map(Result::unwrap) {
        let Some(b) = ev.bank("REC::Particle") else {
            continue;
        };
        let n = b.rows() as usize;
        assert_eq!(b.iter(px).len(), n);
        let fwd: Vec<f32> = b.iter(px).collect();
        let mut rev: Vec<f32> = b.iter(px).rev().collect();
        rev.reverse();
        assert_eq!(fwd, rev, "reverse iteration disagrees");
    }
}

/// `OwnedEvent::size()` must answer from the record directory, not by
/// reassembling the event.
///
/// A correctness test cannot catch this: routing the split codecs through
/// `bytes()` returns exactly the right number, just after inflating every bank
/// stream in the record. Verified — reverting the implementation leaves
/// `tests/all_formats.rs::size_agrees_across_codecs_and_matches_the_uncompressed_span`
/// green. What separates the two is that the slow route *allocates*: `bytes()`
/// builds an owned reassembly per event, so the allocation count scales with
/// events instead of records.
///
/// So this is the guard for the performance property, and the `all_formats`
/// test is the guard for the numeric one. Neither alone is sufficient.
#[test]
fn size_does_not_reassemble_the_event_on_split_codecs() {
    let dir = tempfile::tempdir().unwrap();

    for (name, compression) in [
        ("perbank", oxihipo::Compression::Lz4PerBank),
        ("percolumn", oxihipo::Compression::Lz4PerColumn),
    ] {
        let path = dir.path().join(format!("size_{name}.hipo"));
        // 2000 events over 20 records: if `size()` allocates per event the
        // count lands in the thousands; per record it stays in the dozens.
        build_fixture_with(&path, 2000, 100, compression);

        let chain = Chain::open(&path).unwrap();
        // Warm anything lazily built at open so it is not attributed below.
        let mut warm = 0u64;
        for ev in chain.events() {
            warm += u64::from(ev.unwrap().size());
        }
        assert!(warm > 0);

        let mut total = 0u64;
        let allocs = count_allocs(|| {
            let chain = Chain::open(&path).unwrap();
            for ev in chain.events() {
                total += u64::from(ev.unwrap().size());
            }
        });
        assert!(total > 0, "{name}: fixture produced no events");

        // Streaming 2000 events over 20 records costs a bounded number of
        // allocations (record buffers, the per-record directory, the chain
        // open itself). The threshold is far below 2000 so it cannot be met
        // by an implementation that allocates once per event, and far above
        // the ~60-100 the record path actually needs.
        assert!(
            allocs < 600,
            "{name}: size() allocated {allocs} times for 2000 events over 20 records \
             — that is per-event, so it is reassembling rather than reading the directory"
        );
    }
}
