//! `ReadAt` + `Chain::open_with` from outside the crate.
//!
//! `src/read/inner.rs` has its own in-crate `InMemory` impl proving the read
//! path goes through the seam rather than reaching for `File`. This file
//! proves the *other* half: that a third party can implement the trait and
//! open a chain over it, with no `pub(crate)` in the way. It lives in
//! `tests/` precisely because that compiles as a separate crate — an
//! in-crate test would pass even if `ReadAt` went back to `pub(crate)`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use oxihipo::{Chain, DataType, Dict, HipoError, ReadAt, Result, Schema, Writer};

/// A whole file held in memory, counting the reads it serves so a test can
/// tell "the source was used" from "the source was ignored".
#[derive(Debug)]
struct InMemory {
    bytes: Vec<u8>,
    reads: AtomicU64,
}

impl InMemory {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            reads: AtomicU64::new(0),
        }
    }
}

impl ReadAt for InMemory {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let start = offset as usize;
        let end = start
            .checked_add(buf.len())
            .ok_or_else(|| io_err("offset overflow"))?;
        if end > self.bytes.len() {
            return Err(io_err("read past end of in-memory source"));
        }
        buf.copy_from_slice(&self.bytes[start..end]);
        Ok(())
    }
}

fn io_err(msg: &'static str) -> HipoError {
    HipoError::Io(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, msg))
}

fn write_fixture(path: &std::path::Path) -> Dict {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [("evno".into(), DataType::Long, 1)],
    ));
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        1,
        [
            ("pid".into(), DataType::Int, 1),
            ("px".into(), DataType::Float, 1),
        ],
    ));
    let mut w = Writer::create(path)
        .schemas(&d)
        .max_record_events(37) // several records, so the index is exercised
        .build()
        .unwrap();
    for i in 0..300i64 {
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| r.set("evno", i).map(|_| ()))?;
                Ok(())
            })?;
            ev.bank("REC::Particle", |b| {
                for r_i in 0..=(i % 3) {
                    b.row(|r| {
                        r.set("pid", (11 + r_i) as i32)?;
                        r.set("px", i as f32)?;
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
    d
}

#[test]
fn a_third_party_source_reads_identically_to_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run.hipo");
    write_fixture(&path);

    let from_file = Chain::open(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let len = bytes.len() as u64;
    let src = Arc::new(InMemory::new(bytes));
    let from_memory = Chain::open_with(Arc::clone(&src) as Arc<dyn ReadAt>, len, "memory://run")
        .expect("a chain must open over a caller-supplied source");

    // Metadata parsed out of the source, not the path.
    assert_eq!(from_memory.event_count(), from_file.event_count());
    assert_eq!(from_memory.record_count(), from_file.record_count());
    assert_eq!(from_memory.file_count(), 1);
    assert_eq!(
        from_memory.schemas().len(),
        from_file.schemas().len(),
        "the dictionary must come out of the source too"
    );

    // Every event body, in order, byte for byte the same decisions.
    let mut mem_rows = 0u64;
    let mut mem_evno = Vec::new();
    for ev in from_memory.events().map(Result::unwrap) {
        mem_rows += ev.bank("REC::Particle").map_or(0, |b| b.rows() as u64);
        if let Some(b) = ev.bank("REC::Event") {
            mem_evno.push(b.get::<i64>("evno", 0));
        }
    }
    let mut file_rows = 0u64;
    let mut file_evno = Vec::new();
    for ev in from_file.events().map(Result::unwrap) {
        file_rows += ev.bank("REC::Particle").map_or(0, |b| b.rows() as u64);
        if let Some(b) = ev.bank("REC::Event") {
            file_evno.push(b.get::<i64>("evno", 0));
        }
    }
    assert_eq!(mem_rows, file_rows);
    assert_eq!(mem_evno, file_evno);
    assert_eq!(mem_evno.len(), 300);

    // And it really went through the trait rather than round-tripping to disk.
    assert!(
        src.reads.load(Ordering::Relaxed) > 0,
        "the source served no reads, so the chain read from somewhere else"
    );
}

#[test]
fn the_parallel_paths_work_over_a_third_party_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run.hipo");
    write_fixture(&path);

    let bytes = std::fs::read(&path).unwrap();
    let len = bytes.len() as u64;
    let src = Arc::new(InMemory::new(bytes));
    let chain = Chain::open_with(Arc::clone(&src) as Arc<dyn ReadAt>, len, "memory://run").unwrap();

    // `read_exact_at` takes `&self`, so N workers issue N concurrent reads
    // against one source. Same answer at every thread count.
    let want = 600u64; // (i % 3) + 1 rows over 300 events
    for threads in [1usize, 0, 4] {
        let (rows, stats) = chain
            .par_fold(
                threads,
                || 0u64,
                |acc, ev| *acc += ev.bank("REC::Particle").map_or(0, |b| b.rows() as u64),
                |a, b| a + b,
            )
            .unwrap();
        assert_eq!(rows, want, "threads={threads}");
        assert_eq!(stats.events_yielded, 300, "threads={threads}");
    }

    // The columnar path too — it has its own record materializer.
    let cols = chain
        .read_columns(&[("REC::Particle", &["pid"][..])], None, 0)
        .unwrap();
    assert!(!cols.is_empty());
}

#[test]
fn an_undersized_source_errors_rather_than_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run.hipo");
    write_fixture(&path);

    let bytes = std::fs::read(&path).unwrap();
    let real_len = bytes.len() as u64;

    // A `len` bigger than the source really is: bounds checks read the number
    // we were handed, so the over-read surfaces from the implementation. It
    // must be an `Err`, never a panic.
    let src = Arc::new(InMemory::new(bytes.clone()));
    let over = Chain::open_with(src as Arc<dyn ReadAt>, real_len * 4, "memory://lying");
    if let Ok(chain) = over {
        // Opening may succeed — the header and trailer are where they always
        // were. Walking it must still not panic.
        let _ = chain.events().find_map(Result::err);
    }

    // A `len` far below the header size is rejected outright.
    let tiny = Arc::new(InMemory::new(bytes));
    let err = Chain::open_with(tiny as Arc<dyn ReadAt>, 8, "memory://tiny")
        .expect_err("8 bytes cannot hold a 56-byte file header");
    let msg = err.to_string();
    assert!(
        msg.contains("too small"),
        "expected a size error, got {msg}"
    );
    // And the label rides along, so an error names the source it came from
    // even though nothing ever opened a path.
    assert!(
        msg.contains("memory://tiny"),
        "the label must appear in the error, got {msg}"
    );
}
