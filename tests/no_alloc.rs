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

/// Run a closure with allocation counting enabled; returns the count.
fn count_allocs<F: FnOnce()>(f: F) -> usize {
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
