//! Allocation contract for the **write** path.
//!
//! The sibling `no_alloc.rs` pins the read path. This pins assembly: building
//! an event out of banks must not allocate per bank and per column, because
//! that is per *event* in a loop that runs for hundreds of thousands of them.
//!
//! Before pooling, the cost was `banks * (2 + per-column growth doublings)`:
//! measured at 15 allocations for a 2-bank event and **666 for a 47-bank
//! CLAS12-shaped one**. The review estimated a flat ~28.
//!
//! The counting window is serialised — the gate is thread-local but the
//! counter is global, so two windows open at once corrupt each other's totals.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use oxihipo::event::{BankBuilder, EventBuilder};
use oxihipo::{DataType, Dict, Schema};

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

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

static COUNT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn count_allocs<F: FnOnce()>(f: F) -> usize {
    let _guard = COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    ALLOCS.store(0, Ordering::Relaxed);
    COUNTING.with(|c| c.set(true));
    f();
    COUNTING.with(|c| c.set(false));
    ALLOCS.load(Ordering::Relaxed)
}

/// `banks` banks of `cols` columns, types round-robin so the widths differ.
fn shape_dict(banks: usize, cols: usize) -> Dict {
    let mut d = Dict::new();
    for b in 0..banks {
        let entries: Vec<(String, DataType, u32)> = (0..cols)
            .map(|c| {
                let ty = match c % 4 {
                    0 => DataType::Int,
                    1 => DataType::Float,
                    2 => DataType::Short,
                    _ => DataType::Byte,
                };
                (format!("c{c}"), ty, 1)
            })
            .collect();
        d.add(Schema::from_columns(
            &format!("B{b}::x") as &str,
            300 + b as u16,
            1,
            entries,
        ));
    }
    d
}

/// Column names are precomputed rather than `format!`ed in the loop: a
/// `format!` per cell allocates, and subtracting an estimate of that from the
/// total is exactly the kind of control that goes wrong quietly. (It did —
/// the first version of this test undercounted by a factor of `banks` and
/// reported the test's own formatting as builder allocations.)
fn fill(bb: &mut BankBuilder<'_>, names: &[String], rows: u32) {
    bb.push_rows(rows);
    for r in 0..rows {
        for (c, name) in names.iter().enumerate() {
            match c % 4 {
                0 => bb.set_i32_at(name, r, (r as i32) + c as i32).unwrap(),
                1 => bb.set_f32_at(name, r, r as f32 * 0.5).unwrap(),
                2 => bb.set_i16_at(name, r, r as i16).unwrap(),
                _ => bb.set_i8_at(name, r, (r % 128) as i8).unwrap(),
            };
        }
    }
}

/// Assemble one event with a **pooled** builder set, the way the writer does.
fn assemble_pooled(
    pool: &mut [BankBuilder<'_>],
    ev: &mut EventBuilder,
    out: &mut Vec<u8>,
    names: &[String],
    rows: u32,
) {
    ev.reset();
    for bb in pool.iter_mut() {
        bb.reset();
        fill(bb, names, rows);
        ev.add_bank(bb).unwrap();
    }
    out.clear();
    ev.finish_into(out);
}

#[test]
fn event_assembly_is_allocation_free_once_the_builders_are_pooled() {
    // The realistic CLAS12 DST shape: 47 banks, 8 columns, 3 rows.
    for (label, banks, cols, rows) in [
        ("tiny 2x4x1", 2usize, 4usize, 1u32),
        ("wide 1x28x40", 1, 28, 40),
        ("clas12-10", 10, 10, 5),
        ("clas12-47", 47, 8, 3),
    ] {
        let dict = shape_dict(banks, cols);
        let names: Vec<String> = (0..cols).map(|c| format!("c{c}")).collect();
        let schemas: Vec<&Schema> = dict.iter().collect();
        let mut pool: Vec<BankBuilder<'_>> = schemas.iter().map(|s| BankBuilder::new(s)).collect();
        let mut ev = EventBuilder::new();
        let mut out = Vec::new();

        // Warm the pool: the first events legitimately grow every buffer.
        for _ in 0..3 {
            assemble_pooled(&mut pool, &mut ev, &mut out, &names, rows);
        }
        let warm_len = out.len();
        assert!(warm_len > 0, "{label}: produced no bytes");

        // Steady state. Nothing inside the window allocates on its own, so
        // this is the builders' count with no subtraction.
        const EVENTS: usize = 64;
        let builder_allocs = count_allocs(|| {
            for _ in 0..EVENTS {
                assemble_pooled(&mut pool, &mut ev, &mut out, &names, rows);
            }
        });

        assert_eq!(
            out.len(),
            warm_len,
            "{label}: event size drifted between iterations"
        );
        assert_eq!(
            builder_allocs, 0,
            "{label}: pooled assembly allocated {builder_allocs} times over {EVENTS} events — \
             the builders must reuse their buffers"
        );
    }
}

/// The same four shapes through the **unpooled** path, for the record.
///
/// `BankBuilder::new` per bank, `finish` per bank (consuming, so no reuse is
/// possible), and `EventBuilder::finish` per event. This is not a regression
/// gate — it documents what the pooled path is worth, and it fails loudly if
/// the old numbers were wrong.
#[test]
fn unpooled_assembly_allocates_per_bank_and_per_column() {
    let mut report = Vec::new();
    for (label, banks, cols, rows) in [
        ("tiny 2x4x1", 2usize, 4usize, 1u32),
        ("wide 1x28x40", 1, 28, 40),
        ("clas12-10", 10, 10, 5),
        ("clas12-47", 47, 8, 3),
    ] {
        let dict = shape_dict(banks, cols);
        let names: Vec<String> = (0..cols).map(|c| format!("c{c}")).collect();
        let schemas: Vec<&Schema> = dict.iter().collect();

        const EVENTS: usize = 64;
        let mut sink = 0usize;
        let allocs = count_allocs(|| {
            for _ in 0..EVENTS {
                let mut ev = EventBuilder::new();
                for s in &schemas {
                    let mut bb = BankBuilder::new(s);
                    fill(&mut bb, &names, rows);
                    let bytes = bb.finish().unwrap();
                    ev.add_bank_bytes(&bytes);
                }
                sink += ev.finish().len();
            }
        });
        assert!(sink > 0);
        report.push((label, allocs as f64 / EVENTS as f64));
    }

    for (label, per_event) in &report {
        println!("  unpooled {label:14} {per_event:8.2} allocations/event");
    }

    // The realistic CLAS12 shape is the one that matters, and it is the one
    // the review under-estimated at ~28.
    let clas47 = report.iter().find(|(l, _)| *l == "clas12-47").unwrap().1;
    assert!(
        clas47 > 400.0,
        "expected several hundred allocations/event unpooled on a 47-bank event, measured {clas47}"
    );
    // ...and every shape must cost more unpooled than pooled, which is 0.
    assert!(report.iter().all(|(_, n)| *n > 1.0));
}

/// The same property through the public `Writer::event` closure API.
///
/// The builder-level test proves the primitives *can* be allocation-free;
/// this proves the writer uses them. Different claims: `Writer::event` used to
/// construct a fresh `EventWriter`, a fresh `BankBuilder` per bank and a fresh
/// `Vec` per bank and per event, so the pooled primitives could exist and
/// change nothing for ordinary callers.
///
/// Measured as a **slope**, not an average. Writing a file has a large fixed
/// cost — `Writer::create` clones the dictionary and serialises it into the
/// dictionary record, ~1,400 allocations for a 47-schema dict — and dividing
/// that by the event count produces a number that says nothing about
/// per-event behaviour. (It said 7.16/event here, all of it setup.) The slope
/// between `n` and `2n` events cancels the constant exactly.
#[test]
fn writer_event_recycles_its_builders_across_events() {
    use oxihipo::{Compression, Writer};

    const BANKS: usize = 47;
    const COLS: usize = 8;
    const ROWS: u32 = 3;
    const N: usize = 200;

    let dict = shape_dict(BANKS, COLS);
    let names: Vec<String> = (0..COLS).map(|c| format!("c{c}")).collect();
    let bank_names: Vec<String> = (0..BANKS).map(|b| format!("B{b}::x")).collect();
    let dir = tempfile::tempdir().unwrap();

    let write_n = |path: &std::path::Path, n: usize| {
        let mut w = Writer::create(path)
            .schemas(&dict)
            .compression(Compression::None)
            .max_record_events(u32::MAX)
            .max_record_bytes(usize::MAX)
            .build()
            .unwrap();
        for _ in 0..n {
            w.event(|ev| {
                for bn in &bank_names {
                    ev.bank(bn, |b| {
                        for r in 0..ROWS {
                            b.row(|w2| {
                                for (c, name) in names.iter().enumerate() {
                                    match c % 4 {
                                        0 => w2.set(name, r as i32)?,
                                        1 => w2.set(name, r as f32)?,
                                        2 => w2.set(name, r as i16)?,
                                        _ => w2.set(name, r as i8)?,
                                    };
                                }
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
        w.finish().unwrap()
    };

    // Warm anything lazily built.
    assert_eq!(write_n(&dir.path().join("warm.hipo"), 8).events, 8);

    let a1 = count_allocs(|| {
        assert_eq!(write_n(&dir.path().join("n.hipo"), N).events, N as u64);
    });
    let a2 = count_allocs(|| {
        assert_eq!(
            write_n(&dir.path().join("2n.hipo"), 2 * N).events,
            2 * N as u64
        );
    });
    let slope = (a2 as f64 - a1 as f64) / N as f64;
    println!(
        "  Writer::event marginal cost {slope:.3} allocations/event ({a1} at n={N}, {a2} at n={})",
        2 * N
    );

    // Unpooled, assembly alone cost 666 allocations per event for this shape
    // (see `unpooled_assembly_allocates_per_bank_and_per_column`). Pooled, the
    // marginal cost measures 0.015/event — the record buffer's geometric
    // growth — with the assembly share at exactly 0.
    assert!(
        slope < 0.5,
        "Writer::event costs {slope:.3} allocations per additional event \
         ({a1} at n={N}, {a2} at n={}); the pool is not being reused",
        2 * N
    );
}
