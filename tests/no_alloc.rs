//! Regression test: the `for ev in file.events()` hot loop must be
//! zero-allocation in steady state.
//!
//! Per the design contract:
//! - Per event: 2 `Arc::clone`s (atomic increments, not allocations).
//! - Per record: one decompression into a recycled `Vec<u8>` (via
//!   `Arc::try_unwrap`) and a refill of a recycled event-offsets `Vec<u32>`.
//!
//! Implementation note: this binary installs a counting wrapper around
//! the system allocator. The counter is a *global* (the allocator API
//! doesn't expose per-call context), so concurrent allocations from any
//! thread are attributed to whoever owns the counting window. We
//! therefore combine all measurements into a single `#[test]` function
//! that runs entirely on the test main thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use oxhipo::{Chain, DataType, Dict, Schema, Writer};

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);

struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn build_fixture(path: &std::path::Path, events: i32, max_record_events: u32) {
    let mut dict = Dict::new();
    dict.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [
            ("evno".into(), DataType::Long),
            ("beamE".into(), DataType::Float),
        ],
    ));
    let mut w = Writer::create(path)
        .schemas(&dict)
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

/// Run a closure with allocation counting enabled; returns the count.
fn count_allocs<F: FnOnce()>(f: F) -> usize {
    ALLOCS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    f();
    COUNTING.store(false, Ordering::Relaxed);
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
        let mut iter = file.events();
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
    let _: Vec<_> = small_file.events().take(50).collect(); // warmup
    let _: Vec<_> = big_file.events().take(50).collect();

    let mut collected_small = Vec::with_capacity(1000);
    let allocs_small = count_allocs(|| {
        for ev in small_file.events() {
            collected_small.push(ev);
        }
    });
    let mut collected_big = Vec::with_capacity(5000);
    let allocs_big = count_allocs(|| {
        for ev in big_file.events() {
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
}
