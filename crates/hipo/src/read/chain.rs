//! `Chain` — the reader. One or more HIPO files, mmap'd eagerly, with
//! a shared dictionary validated across them.
//!
//! Single-file is just a chain of length 1 (`Chain::open(path)`).
//! Multi-file iteration walks files in input order ([`Chain::events`])
//! or fans out in parallel across every record of every file
//! ([`Chain::par_for_each`], [`Chain::par_reduce`]).
//!
//! Eager open: each file is mmap'd + dict-parsed at construction. mmap
//! is virtual address space (no disk I/O until the first record read),
//! and the dict parse is small (a few KB per file), so opening 100 files
//! costs ≈ 100 ms wall time and ≈ 0 RAM.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::error::{HipoError, Result};
use crate::event::{Event, EventCtx, OwnedEvent};
use crate::read::filter::Filter;
use crate::read::inner::FileInner;
use crate::read::iter::EventIter;
use crate::schema::Dict;
use crate::wire::constants::RECORD_HEADER_SIZE;
use crate::wire::record::{Record, decode_record_into};
use crate::wire::record_header::RecordHeader;

/// One or more HIPO files presented as a single iterable event stream.
///
/// Construct via [`Chain::open`] (single file), [`Chain::open_all`]
/// (explicit list), or [`Chain::open_dir`] (every `*.hipo` in a
/// directory). All files in a chain must share the same dict — this is
/// validated at construction.
pub struct Chain {
    files: Vec<Arc<FileInner>>,
    /// Cumulative event counts. `file_event_offsets[i]` = total events
    /// in files `0..i`; `file_event_offsets[files.len()]` = total.
    file_event_offsets: Vec<u64>,
    dict: Arc<Dict>,
    filter: Option<Filter>,
    record_tags: Option<Vec<u64>>,
}

impl std::fmt::Debug for Chain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chain")
            .field("files", &self.files.len())
            .field("event_count", &self.event_count())
            .field("schemas", &self.dict.len())
            .field("filter", &self.filter)
            .field("record_tags", &self.record_tags)
            .finish()
    }
}

impl Default for Chain {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            file_event_offsets: vec![0],
            dict: Arc::new(Dict::new()),
            filter: None,
            record_tags: None,
        }
    }
}

impl Chain {
    /// Open a HIPO source as a chain: a single `.hipo` file, a directory
    /// of them, or a glob pattern.
    ///
    /// - a **directory** ⇒ every `*.hipo` inside it ([`Self::open_dir`]);
    /// - a path with glob metacharacters (`*`, `?`, `[`) ⇒ every file
    ///   matching it ([`Self::open_glob`]), e.g. `"data/*.hipo"`;
    /// - anything else ⇒ that one file, as a chain of length 1.
    ///
    /// Reach for [`Self::open_dir`] / [`Self::open_glob`] /
    /// [`Self::open_all`] directly when you want the intent spelled out
    /// at the call site — or to open a literal path that contains a `*`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.is_dir() {
            return Self::open_dir(path);
        }
        if let Some(s) = path.to_str()
            && s.contains(['*', '?', '['])
        {
            return Self::open_glob(s);
        }
        Self::open_all([path])
    }

    /// Open every path in `paths`, in order, validating that every
    /// file's dict matches the first file's. Returns
    /// [`HipoError::SchemaParse`] if any file's dict differs.
    pub fn open_all<I, P>(paths: I) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut files: Vec<Arc<FileInner>> = Vec::new();
        for p in paths {
            let inner = FileInner::open(p.as_ref().to_path_buf())?;
            files.push(Arc::new(inner));
        }
        Self::from_inners(files)
    }

    /// Open every `*.hipo` file in `dir` (case-insensitive, sorted by
    /// path). Non-recursive.
    pub fn open_dir(dir: impl AsRef<Path>) -> Result<Self> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir.as_ref())?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("hipo"))
            })
            .collect();
        paths.sort();
        Self::open_all(paths)
    }

    /// Open every file matching the glob `pattern` — for example
    /// `"data/*.hipo"` or `"runs/**/REC_*.hipo"` — sorted by path. All
    /// matched files must share one dictionary, exactly as for
    /// [`Self::open_all`]. A pattern that matches nothing yields an empty
    /// chain; a malformed pattern returns [`HipoError::InvalidGlob`].
    pub fn open_glob(pattern: &str) -> Result<Self> {
        let entries = glob::glob(pattern).map_err(|e| HipoError::InvalidGlob {
            pattern: pattern.to_string(),
            reason: e.to_string(),
        })?;
        let mut paths: Vec<PathBuf> = entries.filter_map(|r| r.ok()).collect();
        paths.sort();
        Self::open_all(paths)
    }

    fn from_inners(files: Vec<Arc<FileInner>>) -> Result<Self> {
        if files.is_empty() {
            return Ok(Self::default());
        }
        // Validate dict equality across files. Equality is structural
        // (every Schema's name / group / item / entries / row_size match).
        let first: &Dict = &files[0].dict;
        for (i, f) in files.iter().enumerate().skip(1) {
            let this: &Dict = &f.dict;
            if this != first {
                return Err(HipoError::SchemaParse(format!(
                    "chain file {i} ({}) has a different dictionary from file 0 ({})",
                    f.path().display(),
                    files[0].path().display(),
                )));
            }
        }
        let dict = Arc::clone(&files[0].dict);
        let mut file_event_offsets = Vec::with_capacity(files.len() + 1);
        file_event_offsets.push(0_u64);
        let mut acc = 0_u64;
        for f in &files {
            acc += f.index.total_events();
            file_event_offsets.push(acc);
        }
        Ok(Self {
            files,
            file_event_offsets,
            dict,
            filter: None,
            record_tags: None,
        })
    }

    // ---- Metadata --------------------------------------------------------

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Total events across every file in the chain.
    pub fn event_count(&self) -> u64 {
        self.file_event_offsets.last().copied().unwrap_or(0)
    }

    pub fn schemas(&self) -> &Dict {
        &self.dict
    }

    /// Cheap `Arc::clone` of the shared dict. One atomic increment.
    pub fn schemas_arc(&self) -> Arc<Dict> {
        Arc::clone(&self.dict)
    }

    /// Iterate the paths in input order.
    pub fn files(&self) -> impl Iterator<Item = &Path> {
        self.files.iter().map(|f| f.path())
    }

    /// Total record count across every file in the chain.
    pub fn record_count(&self) -> usize {
        self.files.iter().map(|f| f.index.record_count()).sum()
    }

    /// File header of the *first* file in the chain (or `None` for an
    /// empty chain). For multi-file chains this is the canonical
    /// header — all files share the same dict by construction.
    pub fn file_header(&self) -> Option<&crate::wire::file_header::FileHeader> {
        self.files.first().map(|f| &f.file_header)
    }

    // ---- Configuration ---------------------------------------------------

    /// Install (or replace) an event filter. Bound against the shared
    /// dict immediately; unknown names are silently dropped (call
    /// [`Filter::validate`] beforehand to fail fast on typos).
    pub fn with_filter(mut self, filter: Filter) -> Self {
        let mut f = filter;
        f.bind(&self.dict);
        if !f.record_tags().is_empty() {
            let mut tags = self.record_tags.unwrap_or_default();
            tags.extend(f.record_tags().iter().copied());
            self.record_tags = Some(tags);
        }
        self.filter = Some(f);
        self
    }

    pub fn with_record_tags(mut self, tags: impl IntoIterator<Item = u64>) -> Self {
        let mut existing = self.record_tags.unwrap_or_default();
        existing.extend(tags);
        self.record_tags = Some(existing);
        self
    }

    // ---- Sequential iteration -------------------------------------------

    /// The sequential reader — an owning [`Iterator`] for the canonical
    /// `for ev in chain.events()` loop. Walks every event of every file
    /// in input order, yielding [`OwnedEvent`] (see that type for the
    /// per-event memory contract: no per-event allocation; the record
    /// buffer is shared by `Arc` and recycled). Composes with the usual
    /// iterator adapters — `filter`, `take`, `map`, and friends.
    pub fn events(&self) -> ChainEventIter {
        ChainEventIter {
            files: self.files.clone(),
            next_file: 0,
            current: None,
            filter: self.filter.clone(),
            record_tags: self.record_tags.clone(),
            finished: false,
        }
    }

    /// Random-access fetch by global event index (0-based, across all
    /// files in input order). `None` if the index is out of range.
    pub fn event(&self, idx: u64) -> Option<OwnedEvent> {
        // Binary search: find the file whose first_event ≤ idx.
        let file_idx = self
            .file_event_offsets
            .partition_point(|&o| o <= idx)
            .checked_sub(1)?;
        if file_idx >= self.files.len() {
            return None;
        }
        let local = idx - self.file_event_offsets[file_idx];
        let inner = &self.files[file_idx];
        let (rec, ev_local) = inner.index.locate(local)?;
        let span = &inner.index.records()[rec];
        let lo = span.file_offset as usize;
        let hi = lo + span.record_length as usize;
        if hi > inner.mmap.len() {
            return None;
        }
        let mut buf = Vec::new();
        let mut offsets = Vec::new();
        let decoded = decode_record_into(&inner.mmap[lo..hi], &mut buf, &mut offsets)
            .expect("decompress well-formed record");
        if ev_local as usize + 1 >= offsets.len() {
            return None;
        }
        let start = decoded.data_start + offsets[ev_local as usize];
        let end = decoded.data_start + offsets[ev_local as usize + 1];
        Some(OwnedEvent::slice(
            Arc::new(buf),
            start,
            end,
            Arc::clone(&self.dict),
        ))
    }

    // ---- Parallel analysis ----------------------------------------------

    /// Run `f` on every event across every file in parallel. Order is
    /// **not preserved**. Use atomics / `Arc<Mutex<_>>` for shared state,
    /// or prefer [`Self::par_reduce`] for accumulator-style work.
    ///
    /// `threads = 0` lets rayon pick (one worker per logical CPU); a value
    /// above the core count is allowed and can help hide page-fault stalls
    /// on a slow filesystem.
    ///
    /// Switches each file's `madvise` to `MADV_NORMAL` for the parallel,
    /// out-of-order access pattern; a later sequential [`Self::events`]
    /// scan on the same `Chain` will not re-assert `MADV_SEQUENTIAL`.
    pub fn par_for_each<F>(&self, threads: usize, f: F) -> Result<ChainStats>
    where
        F: for<'a> Fn(&EventCtx<'a>) + Send + Sync,
    {
        let tasks = self.build_tasks();
        let pool = build_pool(threads)?;
        let filter = self.filter.as_ref();
        let filter_active = filter.is_some_and(|f| f.is_active());
        let events_in = AtomicU64::new(0);
        let events_yielded = AtomicU64::new(0);
        let start = Instant::now();
        let files = &self.files;
        for inner in files {
            inner.advise_parallel();
        }

        pool.install(|| -> Result<()> {
            tasks.par_iter().try_for_each_init::<_, _, _, Result<()>>(
                Record::new,
                |record, &(fi, ri)| {
                    let inner = &files[fi];
                    let span = &inner.index.records()[ri];
                    let lo = span.file_offset as usize;
                    let hi = lo + span.record_length as usize;
                    if hi > inner.mmap.len() {
                        return Err(HipoError::CorruptRecord {
                            offset: span.file_offset,
                            reason: "record extends past EOF",
                        });
                    }
                    record.load(&inner.mmap[lo..hi])?;

                    let mut local_in = 0u64;
                    let mut local_out = 0u64;
                    for ev_idx in 0..record.event_count() {
                        let raw = record.event(ev_idx).expect("event in range");
                        let event = Event::new(raw);
                        local_in += 1;
                        if filter_active
                            && let Some(filt) = filter
                            && !filt.check(&event)
                        {
                            continue;
                        }
                        let ctx = EventCtx::new(event, &inner.dict);
                        f(&ctx);
                        local_out += 1;
                    }
                    events_in.fetch_add(local_in, Ordering::Relaxed);
                    events_yielded.fetch_add(local_out, Ordering::Relaxed);
                    Ok(())
                },
            )
        })?;

        Ok(ChainStats {
            events_in: events_in.load(Ordering::Relaxed),
            events_yielded: events_yielded.load(Ordering::Relaxed),
            records: tasks.len() as u64,
            files: self.files.len(),
            elapsed: start.elapsed(),
        })
    }

    /// Per-thread fold + final reduce across every event. `init` builds an
    /// accumulator per worker; `fold(acc, ev)` adds one event into it;
    /// `combine(a, b)` merges two thread-local accumulators. `combine`
    /// must be associative — events are not visited in order.
    ///
    /// `threads = 0` lets rayon pick. Like [`Self::par_for_each`], this
    /// switches each file's `madvise` to `MADV_NORMAL`.
    pub fn par_reduce<H, InitFn, FoldFn, CombineFn>(
        &self,
        threads: usize,
        init: InitFn,
        fold: FoldFn,
        combine: CombineFn,
    ) -> Result<H>
    where
        H: Send,
        InitFn: Fn() -> H + Send + Sync,
        FoldFn: for<'a> Fn(H, &EventCtx<'a>) -> H + Send + Sync,
        CombineFn: Fn(H, H) -> H + Send + Sync,
    {
        let tasks = self.build_tasks();
        let pool = build_pool(threads)?;
        let filter = self.filter.as_ref();
        let filter_active = filter.is_some_and(|f| f.is_active());
        let files = &self.files;
        for inner in files {
            inner.advise_parallel();
        }

        pool.install(|| -> Result<H> {
            tasks
                .par_iter()
                .try_fold(
                    || (init(), Record::new()),
                    |state: (H, Record), &(fi, ri)| -> Result<(H, Record)> {
                        let (mut acc, mut record) = state;
                        let inner = &files[fi];
                        let span = &inner.index.records()[ri];
                        let lo = span.file_offset as usize;
                        let hi = lo + span.record_length as usize;
                        if hi > inner.mmap.len() {
                            return Err(HipoError::CorruptRecord {
                                offset: span.file_offset,
                                reason: "record extends past EOF",
                            });
                        }
                        record.load(&inner.mmap[lo..hi])?;
                        for ev_idx in 0..record.event_count() {
                            let raw = record.event(ev_idx).expect("event in range");
                            let event = Event::new(raw);
                            if filter_active
                                && let Some(filt) = filter
                                && !filt.check(&event)
                            {
                                continue;
                            }
                            let ctx = EventCtx::new(event, &inner.dict);
                            acc = fold(acc, &ctx);
                        }
                        Ok((acc, record))
                    },
                )
                .map(|r| r.map(|(h, _)| h))
                .try_reduce(&init, |a, b| Ok(combine(a, b)))
        })
    }

    /// Build a flat `(file_idx, record_idx)` task list, after record-tag
    /// pushdown (reads each record header only; no decompression).
    fn build_tasks(&self) -> Vec<(usize, usize)> {
        let mut tasks = Vec::new();
        for (fi, inner) in self.files.iter().enumerate() {
            let records = inner.index.records();
            for (ri, span) in records.iter().enumerate() {
                if let Some(tags) = &self.record_tags {
                    let off = span.file_offset as usize;
                    if off + RECORD_HEADER_SIZE > inner.mmap.len() {
                        continue;
                    }
                    let matches = RecordHeader::parse(&inner.mmap[off..])
                        .map(|h| tags.contains(&h.user_word_1))
                        .unwrap_or(false);
                    if !matches {
                        continue;
                    }
                }
                tasks.push((fi, ri));
            }
        }
        tasks
    }
}

/// Build a rayon pool. `threads == 0` ⇒ rayon's default (one worker per
/// logical CPU); any other value sets the worker count exactly (values
/// above the core count are allowed — useful to hide filesystem stalls).
fn build_pool(threads: usize) -> Result<rayon::ThreadPool> {
    let builder = if threads == 0 {
        rayon::ThreadPoolBuilder::new()
    } else {
        rayon::ThreadPoolBuilder::new().num_threads(threads)
    };
    builder
        .build()
        .map_err(|_| HipoError::Compression("rayon thread pool init failed"))
}

/// Aggregate counters returned by [`Chain::par_for_each`].
#[derive(Debug, Default, Clone, Copy)]
pub struct ChainStats {
    /// Events visited (before filter).
    pub events_in: u64,
    /// Events that passed the filter and reached the user closure.
    pub events_yielded: u64,
    /// Records actually decompressed (post tag-pushdown).
    pub records: u64,
    /// Number of input files in the chain.
    pub files: usize,
    pub elapsed: Duration,
}

impl ChainStats {
    /// Throughput in thousands of events visited per second.
    pub fn throughput_kev_s(&self) -> f64 {
        let s = self.elapsed.as_secs_f64();
        if s <= 0.0 {
            0.0
        } else {
            self.events_in as f64 / 1000.0 / s
        }
    }
}

// ---- ChainEventIter ------------------------------------------------------

/// Owning iterator over a chain's events. Lazily *advances* to the next
/// file but does not open it — files were opened at chain construction.
#[derive(Debug)]
pub struct ChainEventIter {
    files: Vec<Arc<FileInner>>,
    next_file: usize,
    current: Option<EventIter>,
    filter: Option<Filter>,
    record_tags: Option<Vec<u64>>,
    finished: bool,
}

impl ChainEventIter {
    fn open_next(&mut self) -> bool {
        if self.next_file >= self.files.len() {
            return false;
        }
        let inner = Arc::clone(&self.files[self.next_file]);
        self.next_file += 1;
        let dict = Arc::clone(&inner.dict);
        let iter = EventIter::new(inner, dict, self.filter.clone(), self.record_tags.clone());
        self.current = Some(iter);
        true
    }
}

impl Iterator for ChainEventIter {
    type Item = OwnedEvent;

    fn next(&mut self) -> Option<OwnedEvent> {
        if self.finished {
            return None;
        }
        loop {
            if self.current.is_none() && !self.open_next() {
                self.finished = true;
                return None;
            }
            match self.current.as_mut().expect("just opened").next() {
                Some(item) => return Some(item),
                None => {
                    self.current = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_chain_metadata() {
        let chain = Chain::default();
        assert_eq!(chain.file_count(), 0);
        assert_eq!(chain.event_count(), 0);
        assert!(chain.is_empty());
        assert!(chain.events().next().is_none());
    }
}
