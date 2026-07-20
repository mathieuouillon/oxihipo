//! `Chain` — the reader. One or more HIPO files, opened with a shared
//! dictionary validated across them.
//!
//! Single-file is just a chain of length 1 (`Chain::open(path)`).
//! Multi-file iteration walks files in input order ([`Chain::events`])
//! or fans out in parallel across every record of every file
//! ([`Chain::for_each`]).
//!
//! Streaming open: each file's header, dictionary, and trailer index are
//! parsed at construction (small positioned reads); record payloads are
//! never mapped or read whole — they stream in one record at a time into a
//! recycled buffer. Opening 100 files costs ≈ 0 RAM, and scanning a
//! 10–100 GB file holds only one record (per worker) resident, not the file.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::error::{HipoError, Result};
use crate::event::bank::Bank;
use crate::event::{Event, EventCtx, OwnedEvent};
use crate::read::filter::Filter;
use crate::read::inner::FileInner;
use crate::read::iter::EventIter;
use crate::read::source::IntoSources;
use crate::schema::Dict;
use crate::tag::TagRegistry;
use crate::wire::by_bank::ByBankRecord;
use crate::wire::bytes::write_u32_le;
use crate::wire::constants::EH_TAG;
use crate::wire::per_column::PerColumnRecord;
use crate::wire::record::{Record, decode_record_into};
use crate::write::{Compression, WriteSummary, Writer};

/// One or more HIPO files presented as a single iterable event stream.
///
/// Construct via [`Chain::open`] — its single argument accepts a file, a
/// directory, a glob pattern, or an explicit list of paths (see
/// [`IntoSources`]). All files in a chain must share the same dict — this
/// is validated at construction.
#[derive(Clone)]
pub struct Chain {
    files: Vec<Arc<FileInner>>,
    /// Cumulative event counts. `file_event_offsets[i]` = total events
    /// in files `0..i`; `file_event_offsets[files.len()]` = total.
    file_event_offsets: Vec<u64>,
    dict: Arc<Dict>,
    /// Name↔bit tag registry (first non-empty across the chain's files);
    /// empty if none of them carry one.
    tag_registry: Arc<TagRegistry>,
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
            tag_registry: Arc::new(TagRegistry::new()),
            filter: None,
            record_tags: None,
        }
    }
}

impl Chain {
    /// Open a HIPO source as a chain. The single argument covers every
    /// input shape — see [`IntoSources`]:
    ///
    /// - a single `.hipo` **file** ⇒ a chain of length 1;
    /// - a **directory** ⇒ every `*.hipo` inside it (sorted);
    /// - a **glob** pattern ⇒ every file matching it, e.g. `"data/*.hipo"`;
    /// - an explicit **list** of paths (`&[_]` / `Vec<_>` / `[_; N]`) ⇒
    ///   those files, in order.
    ///
    /// All files in the resulting chain must share one dictionary; this is
    /// validated at construction and returns [`HipoError::SchemaParse`] if
    /// any file's dict differs from the first.
    ///
    /// ```no_run
    /// # use oxihipo::Chain;
    /// # fn main() -> oxihipo::Result<()> {
    /// let one  = Chain::open("run.hipo")?;          // single file
    /// let dir  = Chain::open("/data/run5042")?;     // every *.hipo in a dir
    /// let glob = Chain::open("/data/*.hipo")?;       // glob pattern
    /// let list = Chain::open(["a.hipo", "b.hipo"])?; // explicit list
    /// # Ok(()) }
    /// ```
    pub fn open<S: IntoSources>(src: S) -> Result<Self> {
        Self::from_paths(src.into_sources()?)
    }

    /// Open every resolved path in parallel, then validate dict equality.
    fn from_paths(paths: Vec<PathBuf>) -> Result<Self> {
        // Each `FileInner::open` is a latency-bound round-trip — a file open
        // plus small positioned reads of the header, embedded dictionary, and
        // trailer index. Opening a long chain (a run is often split into a
        // hundred-plus files) one at a time on a network filesystem serialises
        // those round-trips into many seconds of startup before the first event
        // is read, so fan the opens across rayon's pool. Collecting from an
        // *indexed* parallel iterator into `Result<Vec<_>>` preserves input
        // order — leaving file order, and thus global event offsets, unchanged —
        // and short-circuits on the first error (dropping any files already
        // opened). Concurrency is bounded by the rayon pool, so this never
        // opens more than a poolful of descriptors at once.
        let files: Vec<Arc<FileInner>> = paths
            .into_par_iter()
            .map(|p| FileInner::open(p).map(Arc::new))
            .collect::<Result<Vec<_>>>()?;
        Self::from_inners(files)
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
        // The tag registry travels with the dict. Prefer the first non-empty
        // one so chaining an untagged file alongside a tagged one (same dict)
        // still exposes the names, rather than letting file 0 blank them.
        let tag_registry = files
            .iter()
            .map(|f| &f.tag_registry)
            .find(|r| !r.is_empty())
            .map(Arc::clone)
            .unwrap_or_else(|| Arc::clone(&files[0].tag_registry));
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
            tag_registry,
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

    /// The file's persisted tag registry — the name↔bit table written by
    /// [`WriterBuilder::tag_names`](crate::write::WriterBuilder::tag_names).
    /// Empty if the file carries none. Lets a reader resolve tag names without
    /// the original `tag_flags!` declaration:
    ///
    /// ```no_run
    /// # use oxihipo::{Chain, Filter};
    /// # fn main() -> oxihipo::Result<()> {
    /// let chain = Chain::open("run.hipo")?;
    /// if let Some(mask) = chain.tag_registry().mask("dvcs") {
    ///     let dvcs = chain.with_filter(Filter::new().event_tag_any(mask))?;
    ///     # let _ = dvcs;
    /// }
    /// # Ok(()) }
    /// ```
    pub fn tag_registry(&self) -> &TagRegistry {
        &self.tag_registry
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

    /// Install (or replace) an event filter, validated and bound against the
    /// shared dict. Returns [`HipoError::UnknownSchema`] if a required bank
    /// name isn't in the dictionary — a fail-fast guard against typos that
    /// would otherwise silently drop every event.
    pub fn with_filter(mut self, filter: Filter) -> Result<Self> {
        let mut f = filter;
        f.validate(&self.dict)?;
        f.bind(&self.dict);
        if !f.record_tags().is_empty() {
            let mut tags = self.record_tags.unwrap_or_default();
            tags.extend(f.record_tags().iter().copied());
            self.record_tags = Some(tags);
        }
        self.filter = Some(f);
        Ok(self)
    }

    // ---- Sequential iteration -------------------------------------------

    /// The sequential reader — an owning [`Iterator`] for the canonical
    /// `for ev in chain.events()` loop. Walks every event of every file
    /// in input order, yielding `Result<OwnedEvent>`: a corrupt or
    /// truncated record surfaces as an `Err` (after which iteration ends)
    /// instead of panicking, so untrusted or partially written input is
    /// safe to stream. Composes with the usual iterator adapters —
    /// `filter`, `take`, `map`, and friends.
    ///
    /// See [`OwnedEvent`] for the per-event memory contract: no per-event
    /// allocation; the record buffer is shared by `Arc` and recycled.
    ///
    /// ```no_run
    /// use oxihipo::Chain;
    ///
    /// # fn main() -> oxihipo::Result<()> {
    /// let chain = Chain::open("rec.hipo")?;
    /// for ev in chain.events() {
    ///     let ev = ev?;               // propagate corruption as an error
    ///     let _ = ev.bank("REC::Particle");
    /// }
    /// # Ok(()) }
    /// ```
    pub fn events(&self) -> ChainEventIter {
        self.make_event_iter()
    }

    fn make_event_iter(&self) -> ChainEventIter {
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
        // Stream just this one record into a local buffer.
        let mut raw = Vec::new();
        let header = inner.read_record_into(span.file_offset, &mut raw).ok()?;
        if header.compression.is_by_bank() {
            let by_bank = ByBankRecord::parse(&raw).ok()?;
            if ev_local >= by_bank.event_count() {
                return None;
            }
            return Some(OwnedEvent::by_bank(
                by_bank,
                ev_local,
                Arc::clone(&self.dict),
            ));
        }
        if header.compression.is_per_column() {
            let per_column = PerColumnRecord::parse(&raw).ok()?;
            if ev_local >= per_column.event_count() {
                return None;
            }
            return Some(OwnedEvent::per_column(
                per_column,
                ev_local,
                Arc::clone(&self.dict),
            ));
        }
        let mut payload = Vec::new();
        let mut offsets = Vec::new();
        let decoded = decode_record_into(&raw, &mut payload, &mut offsets)
            .expect("decompress well-formed record");
        if ev_local as usize + 1 >= offsets.len() {
            return None;
        }
        let start = decoded.data_start + offsets[ev_local as usize];
        let end = decoded.data_start + offsets[ev_local as usize + 1];
        Some(OwnedEvent::slice(
            Arc::new(payload),
            start,
            end,
            Arc::clone(&self.dict),
        ))
    }

    // ---- Column-major scan -----------------------------------------------

    /// Visit every value of `bank`.`column` across the whole chain, as
    /// contiguous chunks of `T` — the *column-major* full read.
    ///
    /// - For `Lz4PerColumn` inputs this decompresses only that one column's
    ///   stream per record and hands you **all its values at once** — no
    ///   per-event work and no whole-event reassembly. It is the fastest way
    ///   to sweep a single column across a file (histogramming, column
    ///   statistics) and sidesteps the row-major [`OwnedEvent::structures`]
    ///   reassembly cost entirely.
    /// - For any other format it falls back to reading the column per event.
    ///
    /// `visit` is called one or more times with chunks of values; chunk
    /// boundaries are unspecified (per-record for `Lz4PerColumn`, per-event
    /// otherwise), so use it for order-independent work. Errors if
    /// `bank`/`column` is absent from the dictionary or `T` doesn't match
    /// the column's wire type and per-row length.
    pub fn for_each_column<T, F>(&self, bank: &str, column: &str, mut visit: F) -> Result<()>
    where
        T: crate::schema::BankColumnType,
        F: FnMut(&[T]),
    {
        let schema = self.dict.require(bank)?;
        // Validates the element type *and* per-row length against the column.
        let handle = schema.handle::<T>(column)?;
        let col_idx = handle.column_index();
        let (group, item) = (schema.group(), schema.item());
        let elem = std::mem::size_of::<T>();

        let mut raw = Vec::new();
        let mut payload = Vec::new();
        let mut offsets = Vec::new();
        let mut scratch: Vec<T> = Vec::new();
        for inner in &self.files {
            for span in inner.index.records() {
                let header = inner.read_record_into(span.file_offset, &mut raw)?;
                if header.compression.is_per_column() {
                    let rec = PerColumnRecord::parse(&raw)?;
                    let Some(b) = rec.bank_index(group, item) else {
                        continue;
                    };
                    if rec.is_opaque(b) {
                        // Opaque bank: read the column per event out of the
                        // whole-bank stream.
                        let stream = rec.column_stream(b, 0)?;
                        for e in 0..rec.event_count() {
                            if rec.has(e, b) {
                                let r = rec.bank_byte_range(e, b);
                                if let Ok(bk) = Bank::new(schema, &stream[r]) {
                                    visit(&bk.read(handle));
                                }
                            }
                        }
                    } else if (col_idx as u16) < rec.num_columns(b) {
                        // Columnar: the whole column, all events, in one slice.
                        let stream = rec.column_stream(b, col_idx as u16)?;
                        if elem > 0 && stream.len() >= elem {
                            let n = stream.len() / elem;
                            let bytes = &stream[..n * elem];
                            match bytemuck::try_cast_slice::<u8, T>(bytes) {
                                Ok(s) => visit(s),
                                Err(_) => {
                                    scratch.clear();
                                    scratch.extend((0..n).map(|i| {
                                        bytemuck::pod_read_unaligned::<T>(
                                            &bytes[i * elem..i * elem + elem],
                                        )
                                    }));
                                    visit(&scratch);
                                }
                            }
                        }
                    }
                    continue;
                }
                // Fallback (Bytes / ByBank / chunked): decode + per-event read.
                payload.clear();
                offsets.clear();
                let decoded = decode_record_into(&raw, &mut payload, &mut offsets)?;
                for w in offsets.windows(2) {
                    let s = (decoded.data_start + w[0]) as usize;
                    let e = (decoded.data_start + w[1]) as usize;
                    if let Some((_, data)) = Event::new(&payload[s..e]).find(group, item)
                        && let Ok(bk) = Bank::new(schema, data)
                    {
                        visit(&bk.read(handle));
                    }
                }
            }
        }
        Ok(())
    }

    // ---- Skim ------------------------------------------------------------

    /// Copy every event that survives the chain's filter into a new HIPO
    /// file at `dst`, re-encoded with `compression`, and return a
    /// [`WriteSummary`] of what was written.
    ///
    /// The chain's [`Filter`] (set via [`Self::with_filter`]) and any
    /// record-tag pushdown apply on the read side, so only matching events
    /// are written. The output carries the same dictionary **and tag registry**
    /// as the input and preserves each event's tag; multiple input files merge
    /// into one output. Reading stops and the error is returned on the first
    /// corrupt record (this uses the fallible [`Self::events`] internally).
    ///
    /// ```no_run
    /// use oxihipo::{Chain, Compression, Filter};
    ///
    /// # fn main() -> oxihipo::Result<()> {
    /// let summary = Chain::open("run.hipo")?
    ///     .with_filter(Filter::require(["REC::Particle"]))?
    ///     .skim("electrons.hipo", Compression::Lz4PerColumn)?;
    /// println!("wrote {} events", summary.events);
    /// # Ok(()) }
    /// ```
    ///
    /// Note: per-*record* user tags (`user_word_1`/`user_word_2`) are **not**
    /// carried over — output records are renumbered and tagged `0`. Filtering
    /// the result by a record tag would therefore match nothing. (Per-event
    /// tags are preserved; only the coarser record-level tags are dropped.)
    pub fn skim(&self, dst: impl AsRef<Path>, compression: Compression) -> Result<WriteSummary> {
        self.skim_with(dst, compression, |_| {})
    }

    /// Like [`Self::skim`], but calls `progress` after each event is written
    /// with the running count of events written so far — drive a progress
    /// bar (or any reporting) from it without the library taking on a
    /// progress-bar dependency.
    ///
    /// ```no_run
    /// use oxihipo::{Chain, Compression};
    ///
    /// # fn main() -> oxihipo::Result<()> {
    /// let chain = Chain::open("run.hipo")?;
    /// let total = chain.event_count();
    /// let summary = chain.skim_with("out.hipo", Compression::Lz4PerColumn, |n| {
    ///     if n % 100_000 == 0 {
    ///         eprintln!("  {n}/{total}");
    ///     }
    /// })?;
    /// # let _ = summary;
    /// # Ok(()) }
    /// ```
    pub fn skim_with(
        &self,
        dst: impl AsRef<Path>,
        compression: Compression,
        mut progress: impl FnMut(u64),
    ) -> Result<WriteSummary> {
        let mut w = Writer::create(dst)
            .schemas(self.schemas())
            .tag_registry(self.tag_registry())
            .compression(compression)
            .build()?;
        let mut written = 0u64;
        for ev in self.events() {
            w.append_raw(ev?.bytes())?;
            written += 1;
            progress(written);
        }
        w.finish()
    }

    /// Copy the (filtered) chain to `dst` like [`Self::skim`], but **retag**
    /// every event: `tag_fn` is called on each surviving event and its return
    /// (a raw `u32` or a [`TagSet`](crate::TagSet)) overwrites the event's
    /// per-event `EH_TAG`. `tag_names` records the output's [`TagRegistry`] —
    /// pass a `tag_flags!` type's `NAMES` so the DST is self-describing, or
    /// `&[]` for none. The source file's own
    /// registry is **not** carried over, since the closure defines a fresh tag
    /// scheme.
    ///
    /// This closes the select→label→write→reread loop: filter the chain, label
    /// each survivor, and the written DST rereads with
    /// [`Filter::event_tag_any`](crate::read::Filter::event_tag_any) (or
    /// `filtered(event_tag="…")` from Python). Retagging touches only the event
    /// header — banks are copied through unchanged (no decode/re-encode of the
    /// payload beyond the target compression), so it is as cheap as [`Self::skim`].
    ///
    /// ```no_run
    /// use oxihipo::{Chain, Compression};
    /// oxihipo::tag_flags! { pub Cat { Dvcs = 0, Sidis = 1 } }
    ///
    /// # fn main() -> oxihipo::Result<()> {
    /// let chain = Chain::open("run.hipo")?;
    /// chain.skim_tagged("tagged.hipo", Compression::Lz4PerColumn, Cat::NAMES, |ev| {
    ///     // classify from the event's banks…
    ///     if ev.bank("REC::Particle").is_some() { Cat::Dvcs } else { Cat::Sidis }
    /// })?;
    /// // …then reread by name: Chain::open("tagged.hipo")? has the Cat registry.
    /// # Ok(()) }
    /// ```
    pub fn skim_tagged<T, F>(
        &self,
        dst: impl AsRef<Path>,
        compression: Compression,
        tag_names: &[(&str, u32)],
        mut tag_fn: F,
    ) -> Result<WriteSummary>
    where
        T: Into<u32>,
        F: FnMut(&EventCtx<'_>) -> T,
    {
        let mut w = Writer::create(dst)
            .schemas(self.schemas())
            .tag_names(tag_names)
            .compression(compression)
            .build()?;
        let mut buf = Vec::new();
        for ev in self.events() {
            let ev = ev?;
            let tag: u32 = tag_fn(&ev.ctx()).into();
            buf.clear();
            buf.extend_from_slice(ev.bytes());
            // Overwrite EH_TAG (event-header byte 8) in the copy; the writer
            // reads it back from here to build the per-column / by-bank tag
            // directory as well as the event header.
            write_u32_le(&mut buf, EH_TAG, tag);
            w.append_raw(&buf)?;
        }
        w.finish()
    }

    // ---- Event processing -----------------------------------------------

    /// Run `f` on every event across every file. The execution mode is
    /// selected entirely by `threads` — the **only** difference between
    /// single- and multi-threaded is this argument:
    ///
    /// - `threads == 1` → **sequential**, in input order, on the calling
    ///   thread (no rayon pool, no thread spawn).
    /// - `threads == 0` → **parallel** on rayon's process-wide pool (one
    ///   worker per logical CPU by default, or whatever a caller-installed
    ///   global pool configures — reused across calls, no per-call spin-up).
    /// - `threads == n` (`n > 1`) → **parallel** with exactly `n` workers
    ///   (a value above the core count is allowed and can hide page-fault
    ///   stalls on a slow filesystem).
    ///
    /// Event order is preserved only for `threads == 1`; the parallel
    /// modes visit events out of order, so use atomics or a `Mutex` in `f`
    /// for shared state. Returns aggregate [`ChainStats`].
    ///
    /// The parallel modes switch each file's `madvise` to `MADV_NORMAL`
    /// and prefetch the records they'll touch; the sequential mode leaves
    /// the `MADV_SEQUENTIAL` advice set at open.
    ///
    /// ```no_run
    /// use std::sync::atomic::{AtomicU64, Ordering};
    /// use oxihipo::Chain;
    ///
    /// # fn main() -> oxihipo::Result<()> {
    /// let chain = Chain::open("rec.hipo")?;
    /// let rows = AtomicU64::new(0);
    /// // `threads = 0` → all cores; pass `1` for the identical single-
    /// // threaded scan.
    /// chain.for_each(0, |ev| {
    ///     if let Some(b) = ev.bank("REC::Particle") {
    ///         rows.fetch_add(b.rows() as u64, Ordering::Relaxed);
    ///     }
    /// })?;
    /// println!("{} REC::Particle rows", rows.load(Ordering::Relaxed));
    /// # Ok(()) }
    /// ```
    pub fn for_each<F>(&self, threads: usize, f: F) -> Result<ChainStats>
    where
        F: for<'a> Fn(&EventCtx<'a>) + Send + Sync,
    {
        let tasks = self.build_tasks();
        let filter = self.filter.as_ref();
        let filter_active = filter.is_some_and(|f| f.is_active());
        let events_in = AtomicU64::new(0);
        let events_yielded = AtomicU64::new(0);
        let start = Instant::now();
        let files = &self.files;

        if threads == 1 {
            // Single-threaded: walk records in input order on this thread,
            // reusing the record-read and decompression scratch buffers.
            let mut record = Record::new();
            let mut read_buf = Vec::new();
            for &(fi, ri) in &tasks {
                process_record(
                    &files[fi],
                    ri,
                    filter,
                    filter_active,
                    &mut record,
                    &mut read_buf,
                    &f,
                    &events_in,
                    &events_yielded,
                )?;
            }
        } else {
            // Parallel: stream records across a rayon pool, out of order.
            // Each worker `pread`s a record into its own recycled buffer, so
            // resident memory is bounded by (workers × one record), never the
            // file size. On Unix `pread` is concurrency-safe on the shared
            // descriptor; elsewhere `FileInner` serialises behind a `Mutex`.
            let run = || -> Result<()> {
                tasks.par_iter().try_for_each_init::<_, _, _, Result<()>>(
                    || (Record::new(), Vec::new()),
                    |(record, read_buf), &(fi, ri)| {
                        process_record(
                            &files[fi],
                            ri,
                            filter,
                            filter_active,
                            record,
                            read_buf,
                            &f,
                            &events_in,
                            &events_yielded,
                        )
                    },
                )
            };
            if threads == 0 {
                // Reuse rayon's process-wide pool (lazily initialised, shared
                // across calls) rather than spinning up and tearing down a
                // fresh pool every time.
                run()?;
            } else {
                build_pool(threads)?.install(run)?;
            }
        }

        Ok(ChainStats {
            events_in: events_in.load(Ordering::Relaxed),
            events_yielded: events_yielded.load(Ordering::Relaxed),
            records: tasks.len() as u64,
            files: self.files.len(),
            elapsed: start.elapsed(),
        })
    }

    /// Borrow the opened files in input order. The column materializer
    /// indexes `files_inner()[fi]` for a `(file_idx, record_idx)` task.
    pub(crate) fn files_inner(&self) -> &[Arc<FileInner>] {
        &self.files
    }

    /// Cumulative per-file event offsets: `event_offsets()[fi]` is the global
    /// index of file `fi`'s first event, so local event `e` of record span
    /// `span` in file `fi` has global index `event_offsets()[fi] +
    /// span.first_event + e`.
    pub(crate) fn event_offsets(&self) -> &[u64] {
        &self.file_event_offsets
    }

    /// The bound event filter, if any.
    pub(crate) fn filter_ref(&self) -> Option<&Filter> {
        self.filter.as_ref()
    }

    /// Build a flat `(file_idx, record_idx)` task list, after record-tag
    /// pushdown (reads each record header only; no decompression).
    pub(crate) fn build_tasks(&self) -> Vec<(usize, usize)> {
        let mut tasks = Vec::new();
        for (fi, inner) in self.files.iter().enumerate() {
            let records = inner.index.records();
            for (ri, span) in records.iter().enumerate() {
                if let Some(tags) = &self.record_tags {
                    let matches = inner
                        .read_record_header(span.file_offset)
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

/// Stream record `ri` of `inner` and call `f` on every (post-filter)
/// event, accumulating per-record counts into the shared atomics. Shared
/// by the sequential and parallel arms of [`Chain::for_each`]. `read_buf`
/// holds the raw record bytes (`pread` into it, reused across calls) and
/// `record` is the decompression scratch for the bytes-backed path — both
/// recycled, so the resident footprint is one record, not the file.
#[allow(clippy::too_many_arguments)]
fn process_record<F>(
    inner: &Arc<FileInner>,
    ri: usize,
    filter: Option<&Filter>,
    filter_active: bool,
    record: &mut Record,
    read_buf: &mut Vec<u8>,
    f: &F,
    events_in: &AtomicU64,
    events_yielded: &AtomicU64,
) -> Result<()>
where
    F: for<'a> Fn(&EventCtx<'a>) + Send + Sync,
{
    let span = &inner.index.records()[ri];
    let header = inner.read_record_into(span.file_offset, read_buf)?;
    let mut local_in = 0u64;
    let mut local_out = 0u64;

    if header.compression.is_by_bank() {
        // Lazy per-bank decompression — `f` only inflates banks it touches.
        let by_bank = ByBankRecord::parse(read_buf)?;
        for ev_idx in 0..by_bank.event_count() {
            local_in += 1;
            if filter_active
                && let Some(filt) = filter
                && !filt.check_by_bank(&by_bank, ev_idx)
            {
                continue;
            }
            f(&EventCtx::new_by_bank(&by_bank, ev_idx, &inner.dict));
            local_out += 1;
        }
    } else if header.compression.is_per_column() {
        // Lazy per-column decompression — `f` only inflates columns it reads.
        let per_column = PerColumnRecord::parse(read_buf)?;
        for ev_idx in 0..per_column.event_count() {
            local_in += 1;
            if filter_active
                && let Some(filt) = filter
                && !filt.check_per_column(&per_column, ev_idx)
            {
                continue;
            }
            f(&EventCtx::new_per_column(&per_column, ev_idx, &inner.dict));
            local_out += 1;
        }
    } else {
        record.load_with_header(read_buf, header)?;
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
            f(&EventCtx::new(event, &inner.dict));
            local_out += 1;
        }
    }
    events_in.fetch_add(local_in, Ordering::Relaxed);
    events_yielded.fetch_add(local_out, Ordering::Relaxed);
    Ok(())
}

/// Build a rayon pool with exactly `threads` workers (values above the core
/// count are allowed — useful to hide filesystem stalls). Only called with
/// `threads > 1`; the `threads == 0` path reuses rayon's global pool.
fn build_pool(threads: usize) -> Result<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|_| HipoError::Compression("rayon thread pool init failed"))
}

/// Aggregate counters returned by [`Chain::for_each`].
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

    /// Advance the stream. A corrupt or truncated record surfaces as a
    /// single `Some(Err)` (after which iteration ends), never a panic.
    fn next_result(&mut self) -> Option<Result<OwnedEvent>> {
        if self.finished {
            return None;
        }
        loop {
            if self.current.is_none() && !self.open_next() {
                self.finished = true;
                return None;
            }
            match self.current.as_mut().expect("just opened").next_result() {
                Some(Ok(ev)) => return Some(Ok(ev)),
                Some(Err(e)) => {
                    self.finished = true;
                    return Some(Err(e));
                }
                None => {
                    self.current = None;
                }
            }
        }
    }
}

impl Iterator for ChainEventIter {
    type Item = Result<OwnedEvent>;

    fn next(&mut self) -> Option<Result<OwnedEvent>> {
        self.next_result()
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
