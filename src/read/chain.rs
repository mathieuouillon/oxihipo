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

use std::collections::{HashMap, hash_map::Entry};
use std::fs::{File, OpenOptions};
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
use crate::wire::constants::{CompressionType, EH_TAG, RECORD_HEADER_SIZE};
use crate::wire::per_column::PerColumnRecord;
use crate::wire::record::{Record, decode_record_into};
use crate::wire::record_header::RecordHeader;
use crate::write::{Compression, WriteSummary, Writer};

/// One or more HIPO files presented as a single iterable event stream.
///
/// Construct via [`Chain::open`] — its single argument accepts a file, a
/// directory, a glob pattern, or an explicit list of paths (see
/// [`IntoSources`]). Files in a chain must not *contradict* each other's
/// dictionaries — see [`Chain::open`] for exactly what is checked.
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
    /// User key/value config from the dictionary record (first non-empty).
    config: Arc<Vec<(String, String)>>,
    filter: Option<Filter>,
    record_tags: Option<Vec<u64>>,
    /// Last record decoded by [`Self::event`]. Shared across clones so a
    /// cloned chain inherits the warm record. `Mutex` (not `RefCell`) keeps
    /// `Chain: Sync`; the lock is held only for the lookup/insert, and is
    /// negligible next to a record decode.
    record_cache: Arc<std::sync::Mutex<Option<RecordCache>>>,
}

/// One decoded record, kept so consecutive [`Chain::event`] calls that land in
/// the same record don't re-read and re-decompress it.
///
/// Random access previously cost a full record decode *per call*: fetching M
/// events from one record was O(M x record-decompress). Real access patterns
/// are clustered (a sorted list of interesting event indices), so a
/// single-record cache turns all but the first hit into a slice.
#[derive(Debug)]
struct RecordCache {
    file_idx: usize,
    rec_idx: usize,
    decoded: CachedRecord,
}

#[derive(Debug)]
enum CachedRecord {
    /// Classic record: shared decompressed payload + per-event offsets.
    Bytes {
        payload: Arc<Vec<u8>>,
        offsets: Vec<u32>,
        data_start: u32,
    },
    ByBank(Arc<ByBankRecord>),
    PerColumn(Arc<PerColumnRecord>),
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
            config: Arc::new(Vec::new()),
            filter: None,
            record_tags: None,
            record_cache: Arc::new(std::sync::Mutex::new(None)),
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
    /// Dictionaries are checked for **contradiction**, not for equality: a
    /// later file that declares a bank the first file also declares, but with a
    /// different layout, returns [`HipoError::SchemaParse`], because reading
    /// them together would decode columns against the wrong schema.
    ///
    /// A file that merely *lacks* a bank another file has is accepted — that is
    /// a subset, not a conflict, and it is what a chain of runs with different
    /// detectors looks like. Such a bank is then absent from that file's events,
    /// and [`Filter::require`] rejects them there.
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

    /// [`open`](Self::open), for a file whose 56-byte header is unusable.
    ///
    /// The normal path parses that header first, so a file missing it cannot be
    /// opened at all — even though nothing important is in it. It holds the
    /// magic, the version, record counts, where the dictionary starts and where
    /// the trailer is, and every one of those is re-derivable: each record
    /// carries its own header and magic, so the records can simply be found.
    ///
    /// Use this only after [`open`](Self::open) has failed. It trusts less and
    /// therefore checks less: it locates the first structure that parses as a
    /// record and claims a length fitting inside the file, reads the dictionary
    /// from it if one is there, and indexes the rest by scanning.
    ///
    /// **The dictionary may not survive.** It lives in the record right after
    /// the header, so damage that took the header often took it too. Then the
    /// events are still readable as bytes — `skim`-style verbatim copying works
    /// — but their banks have no names or column types, because those appear
    /// nowhere else in the file. The returned chain has an empty dictionary in
    /// that case rather than a guess.
    pub fn open_salvage<P: AsRef<std::path::Path>>(path: P) -> Result<Self> {
        let inner = FileInner::open_salvage(path.as_ref().to_path_buf())?;
        Self::from_inners(vec![Arc::new(inner)])
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
        let dict = Arc::new(union_dict(&files)?);
        // The tag registry travels with the dict. Prefer the first non-empty
        // one so chaining an untagged file alongside a tagged one (same dict)
        // still exposes the names, rather than letting file 0 blank them.
        let tag_registry = files
            .iter()
            .map(|f| &f.tag_registry)
            .find(|r| !r.is_empty())
            .map(Arc::clone)
            .unwrap_or_else(|| Arc::clone(&files[0].tag_registry));
        // User config travels with the dict too; prefer the first non-empty.
        let config = files
            .iter()
            .map(|f| &f.config)
            .find(|c| !c.is_empty())
            .map(Arc::clone)
            .unwrap_or_else(|| Arc::clone(&files[0].config));
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
            config,
            filter: None,
            record_tags: None,
            record_cache: Arc::new(std::sync::Mutex::new(None)),
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

    /// The user key/value configuration written into the dictionary record —
    /// the `(32555,…)` "run config" store shared with the C++/Java writers — in
    /// file order. Empty if the file carries none.
    pub fn user_config(&self) -> &[(String, String)] {
        &self.config
    }

    /// Look up a single user-config value by key (see [`Self::user_config`]).
    pub fn config(&self, key: &str) -> Option<&str> {
        self.config
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
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
        // Replace, do not accumulate. `self.filter` is assigned wholesale just
        // below, so extending the record tags made the two halves of one filter
        // disagree: a second `with_filter` narrowed every other clause while
        // *widening* the record-tag pushdown, because the tags are consumed as a
        // union in `build_tasks`. Composition belongs to the caller, which now
        // hands over an already-merged filter.
        self.record_tags = if f.record_tags().is_empty() {
            None
        } else {
            Some(f.record_tags().to_vec())
        };
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
    /// allocation; the record buffer is shared by `Arc` and recycled. This
    /// holds on every codec — verified by `tests/no_alloc.rs`, which reads two
    /// files with the same record count and 4× the events and checks the
    /// allocation count does not move. What it costs per *record* differs:
    /// `Lz4PerBank` and `Lz4PerColumn` parse a directory of per-bank tables,
    /// so they allocate roughly an order of magnitude more per record than the
    /// blob codecs — but still nothing per event.
    ///
    /// One caveat on the split codecs: they store banks separately, so there is
    /// no event blob to hand back and the whole-event views
    /// ([`OwnedEvent::bytes`], [`OwnedEvent::composite`], and `structures` on
    /// `Lz4PerColumn`) must **synthesise** one — a single allocation per event,
    /// cached, and only on first use. Bank access by name
    /// ([`OwnedEvent::bank`], [`OwnedEvent::get`]) never goes through it.
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
        let (rec_idx, ev_local) = inner.index.locate(local)?;

        // Serve from the cached record when this call lands in the same one as
        // the last (the common case for a sorted list of event indices): a hit
        // is a slice / index, with no read and no decompression.
        let mut guard = self.record_cache.lock().ok()?;
        let hit = guard
            .as_ref()
            .is_some_and(|c| c.file_idx == file_idx && c.rec_idx == rec_idx);
        if !hit {
            let span = &inner.index.records()[rec_idx];
            let mut raw = Vec::new();
            let header = inner.read_record_into(span.file_offset, &mut raw).ok()?;
            let decoded = if header.compression.is_by_bank() {
                CachedRecord::ByBank(ByBankRecord::parse(&raw).ok()?)
            } else if header.compression.is_per_column() {
                CachedRecord::PerColumn(PerColumnRecord::parse(&raw).ok()?)
            } else {
                let mut payload = Vec::new();
                let mut offsets = Vec::new();
                // Degrade to `None` on a corrupt record rather than panicking —
                // the documented contract is `None` on failure, not an abort.
                let d =
                    decode_record_into(&raw, &mut payload, &mut offsets, Some(&self.dict)).ok()?;
                CachedRecord::Bytes {
                    payload: Arc::new(payload),
                    offsets,
                    data_start: d.data_start,
                }
            };
            *guard = Some(RecordCache {
                file_idx,
                rec_idx,
                decoded,
            });
        }

        let cache = guard.as_ref()?;
        match &cache.decoded {
            CachedRecord::ByBank(rec) => {
                if ev_local >= rec.event_count() {
                    return None;
                }
                Some(OwnedEvent::by_bank(
                    Arc::clone(rec),
                    ev_local,
                    Arc::clone(&self.dict),
                ))
            }
            CachedRecord::PerColumn(rec) => {
                if ev_local >= rec.event_count() {
                    return None;
                }
                Some(OwnedEvent::per_column(
                    Arc::clone(rec),
                    ev_local,
                    Arc::clone(&self.dict),
                ))
            }
            CachedRecord::Bytes {
                payload,
                offsets,
                data_start,
            } => {
                if ev_local as usize + 1 >= offsets.len() {
                    return None;
                }
                let start = data_start + offsets[ev_local as usize];
                let end = data_start + offsets[ev_local as usize + 1];
                Some(OwnedEvent::slice(
                    Arc::clone(payload),
                    start,
                    end,
                    Arc::clone(&self.dict),
                ))
            }
        }
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
    ///
    /// # This does **not** apply the chain filter
    ///
    /// It walks the record index directly, so a filter set with
    /// [`Chain::with_filter`] — and the record-tag pushdown — are both ignored:
    /// you get every value in the file. That is deliberate, because the
    /// per-column fast path reads whole column streams and has no per-event
    /// predicate to apply, but it is a trap: a caller that filters and then
    /// sweeps gets a plausible number over the wrong event set, silently.
    ///
    /// Use [`Chain::read_columns`] when a filter is in play. It is also
    /// columnar, honours the filter and the tag pushdown, and costs little more
    /// on the formats this method was written for.
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
                                // Bounds-checked for the same reason as in
                                // `read_columns`: the range comes from the file's
                                // own offset table, and a corrupt one indexed the
                                // slice out of range and panicked.
                                let r = rec.bank_byte_range(e, b);
                                if let Some(Ok(bk)) =
                                    stream.get(r).map(|raw| Bank::new(schema, raw))
                                {
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
                if header.compression.is_by_bank() {
                    // By-bank: one LZ4 stream per bank plus a directory, so the
                    // record has no single decompressible payload and the
                    // fallback below cannot touch it. Inflate just this bank's
                    // stream and read the column out of it per event — the same
                    // shape as the per-column opaque path above.
                    let rec = ByBankRecord::parse(&raw)?;
                    let Some(b) = rec.bank_index(group, item) else {
                        continue;
                    };
                    let stream = rec.bank_stream(b)?;
                    for e in 0..rec.event_count() {
                        if rec.has(e, b) {
                            // Bounds-checked, as in the by-bank branch above: a
                            // corrupt offset table indexed this slice out of range
                            // and panicked. An exhaustive byte-flip test found this
                            // second site after the first three were fixed.
                            let r = rec.bank_byte_range(e, b);
                            if let Some(Ok(bk)) = stream.get(r).map(|raw| Bank::new(schema, raw)) {
                                visit(&bk.read(handle));
                            }
                        }
                    }
                    continue;
                }
                // Fallback (whole-record payloads: None / Lz4 / Lz4Best / Gzip):
                // decode + per-event read.
                payload.clear();
                offsets.clear();
                let decoded =
                    decode_record_into(&raw, &mut payload, &mut offsets, Some(&self.dict))?;
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

    // ---- In-place tag update --------------------------------------------

    /// Overwrite one event's per-event tag (`EH_TAG`) **in place** on disk,
    /// without rewriting the file — a single 4-byte write. Requires write
    /// permission on the underlying file (the open fails with an I/O error
    /// otherwise). `tag` is a raw `u32` or a [`TagSet`](crate::TagSet).
    ///
    /// **Only uncompressed records (`Compression::None`) can be patched.** For
    /// every compressed format the tag lives inside a compressed block, so
    /// changing it needs the record re-encoded — this returns
    /// [`HipoError::InPlaceTagUnsupported`]; use [`Self::skim_tagged`] to rewrite
    /// those. An out-of-range `global_idx` returns
    /// [`HipoError::EventIndexOutOfRange`]. The event header magic is verified
    /// before the write, so a bad offset can never corrupt the file.
    ///
    /// The change is visible to later reads (through this or a fresh `Chain`)
    /// immediately — records are streamed fresh on every read.
    ///
    /// ```no_run
    /// # use oxihipo::Chain;
    /// # fn main() -> oxihipo::Result<()> {
    /// let chain = Chain::open("run.hipo")?; // written with Compression::None
    /// chain.set_event_tag(42, 0b0000_0001_u32)?; // flag event 42
    /// # Ok(()) }
    /// ```
    pub fn set_event_tag(&self, global_idx: u64, tag: impl Into<u32>) -> Result<()> {
        self.set_event_tags([(global_idx, tag.into())]).map(|_| ())
    }

    /// Batch [`Self::set_event_tag`]: patch many events, grouping by file and
    /// record so each file is opened once. **Every** update is validated (index
    /// in range, record uncompressed) *before* any write, so a bad update fails
    /// the whole batch without a partial change. Returns the number patched.
    ///
    /// ```no_run
    /// # use oxihipo::Chain;
    /// # fn main() -> oxihipo::Result<()> {
    /// let chain = Chain::open("run.hipo")?;
    /// chain.set_event_tags([(10, 1_u32), (20, 2), (30, 4)])?;
    /// # Ok(()) }
    /// ```
    pub fn set_event_tags<I>(&self, updates: I) -> Result<usize>
    where
        I: IntoIterator<Item = (u64, u32)>,
    {
        let total = self.event_count();

        // Pass 0 — resolve every update to (file, record offset, local event),
        // erroring on an out-of-range index before touching the disk.
        let mut targets: Vec<TagPatch> = Vec::new();
        for (global_idx, tag) in updates {
            let file_idx = self
                .file_event_offsets
                .partition_point(|&o| o <= global_idx)
                .checked_sub(1)
                .filter(|&fi| fi < self.files.len())
                .ok_or(HipoError::EventIndexOutOfRange {
                    index: global_idx,
                    total,
                })?;
            let local = global_idx - self.file_event_offsets[file_idx];
            let inner = &self.files[file_idx];
            let (rec, ev_local) =
                inner
                    .index
                    .locate(local)
                    .ok_or(HipoError::EventIndexOutOfRange {
                        index: global_idx,
                        total,
                    })?;
            targets.push(TagPatch {
                file_idx,
                record_offset: inner.index.records()[rec].file_offset,
                ev_local,
                tag,
            });
        }
        if targets.is_empty() {
            return Ok(0);
        }

        // Pass 1 — open a read+write handle per distinct file (this is the
        // permission gate) and read+validate each distinct record's layout
        // (uncompressed, event count). No writes yet, so a compressed record
        // or a permission error aborts the whole batch cleanly.
        let mut fds: HashMap<usize, File> = HashMap::new();
        let mut layouts: HashMap<(usize, u64), RecordLayout> = HashMap::new();
        for t in &targets {
            if let Entry::Vacant(slot) = fds.entry(t.file_idx) {
                let path = self.files[t.file_idx].path();
                let f = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .map_err(|e| HipoError::Io(e).with_path(path.to_path_buf()))?;
                slot.insert(f);
            }
            if let Entry::Vacant(slot) = layouts.entry((t.file_idx, t.record_offset)) {
                let layout = read_record_layout(&fds[&t.file_idx], t.record_offset)?;
                slot.insert(layout);
            }
        }

        // Pass 2 — apply. Verify the event-header magic at each computed offset
        // before writing the 4-byte tag, so a layout miscomputation surfaces as
        // an error rather than a corrupted event.
        for t in &targets {
            let fd = &fds[&t.file_idx];
            let layout = &layouts[&(t.file_idx, t.record_offset)];
            let event_start = t.record_offset + layout.event_start_in_record(t.ev_local)?;
            let mut magic = [0u8; 4];
            read_exact_at(fd, event_start, &mut magic)?;
            if &magic != b"EVNT" {
                return Err(HipoError::CorruptRecord {
                    offset: event_start,
                    reason: "event header magic mismatch during in-place tag patch",
                });
            }
            write_all_at(fd, event_start + EH_TAG as u64, &t.tag.to_le_bytes())?;
        }
        Ok(targets.len())
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
                    None,
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
                            None,
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

    /// [`for_each`](Self::for_each) over one **global event range**.
    ///
    /// Reads only the records the range touches, so the cost is proportional to
    /// the slice rather than the file. That is the difference between an index
    /// that can be exploited and one that cannot: skipping records was already
    /// computable from a summary, but the only way to *read* a subset from
    /// outside was [`event`](Self::event) per index, which re-seeks for every
    /// event — measured at 55 kev/s against this path's 6,400 on a CLAS12 DST,
    /// so a caller skipping 85% of events still came out 4.5x slower.
    ///
    /// `range` is in the same **pre-filter** global index space as
    /// [`read_columns`](Self::read_columns) and `--events A..B`: a filter bound
    /// with [`with_filter`](Self::with_filter) still applies, and still drops
    /// events *within* the range, but does not renumber it.
    ///
    /// A record straddling either end is read once and its out-of-range events
    /// dropped, so `events_in` counts what the range holds, not what the
    /// records do.
    ///
    /// ```no_run
    /// # use oxihipo::Chain;
    /// # use std::sync::atomic::{AtomicU64, Ordering};
    /// # fn main() -> oxihipo::Result<()> {
    /// let chain = Chain::open("rec.hipo")?;
    /// let n = AtomicU64::new(0);
    /// // Only the records holding events 1_000..2_000 are touched.
    /// chain.for_each_range(1_000..2_000, 0, |_| {
    ///     n.fetch_add(1, Ordering::Relaxed);
    /// })?;
    /// assert_eq!(n.load(Ordering::Relaxed), 1_000);
    /// # Ok(()) }
    /// ```
    pub fn for_each_range<F>(
        &self,
        range: std::ops::Range<u64>,
        threads: usize,
        f: F,
    ) -> Result<ChainStats>
    where
        F: for<'a> Fn(&EventCtx<'a>) + Send + Sync,
    {
        self.for_each_ranges(std::slice::from_ref(&range), threads, f)
    }

    /// [`for_each`](Self::for_each) over several **global event ranges** at once.
    ///
    /// Reads only the records those ranges touch, so the cost follows the slices
    /// rather than the file. Measured on a 3 GB CLAS12 file, reading 21,506
    /// events spread over 89 ranges (warm, best of three):
    ///
    /// | | `-j 1` | `-j 16` |
    /// |---|---|---|
    /// | **all ranges, one call** | 0.24 s | **0.11 s** |
    /// | one call per range | 0.62 s | 0.80 s |
    /// | [`event`](Self::event) per index | 0.61 s | — |
    ///
    /// **Pass every range in one call.** Each call rebuilds the record task list
    /// and pays a rayon dispatch, so looping over ranges spends its time on
    /// bookkeeping — and at 16 threads that overhead makes the loop *slower*
    /// than running it single-threaded.
    ///
    /// Against `event(idx)` per index over the same events this is 2.6x quicker
    /// on one thread and **5.9x on sixteen**. The single-threaded margin is
    /// modest because `event` caches the record it last inflated, so contiguous
    /// access was already reasonable; what this adds is parallelism across
    /// records, which per-index reading cannot have.
    ///
    /// Ranges may overlap and need not be sorted; they are clamped, merged and
    /// ordered internally, so no event is visited twice. Indices are the same
    /// **pre-filter** space as [`read_columns`](Self::read_columns): a filter
    /// bound with [`with_filter`](Self::with_filter) still drops events *within*
    /// a range without renumbering it.
    ///
    /// ```no_run
    /// # use oxihipo::Chain;
    /// # fn main() -> oxihipo::Result<()> {
    /// let chain = Chain::open("rec.hipo")?;
    /// // The surviving ranges an index produced, read in one pass.
    /// let stats = chain.for_each_ranges(&[0..500, 9_000..9_250], 0, |_| {})?;
    /// assert_eq!(stats.events_in, 750);
    /// # Ok(()) }
    /// ```
    pub fn for_each_ranges<F>(
        &self,
        ranges: &[std::ops::Range<u64>],
        threads: usize,
        f: F,
    ) -> Result<ChainStats>
    where
        F: for<'a> Fn(&EventCtx<'a>) + Send + Sync,
    {
        let start = Instant::now();
        let total = self.event_count();

        // Clamp, drop the empty and inverted, sort, merge.
        //
        // **Sorting** is load bearing: the per-record slice below is found with
        // `partition_point`, which needs the list ordered to mean anything.
        //
        // **Merging is only an optimisation**, and the first version of this
        // comment claimed otherwise — that it stopped an event being visited
        // twice when ranges overlap. It does not, and a mutation test proved
        // it: an event is visited once per *record*, and `holds` is a
        // membership test, so a duplicate range changes nothing but the length
        // of the slice each record scans. Merging keeps that slice short.
        let mut wanted: Vec<(u64, u64)> = ranges
            .iter()
            .map(|r| (r.start.min(total), r.end.min(total)))
            .filter(|(lo, hi)| lo < hi)
            .collect();
        wanted.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(wanted.len());
        for (lo, hi) in wanted {
            match merged.last_mut() {
                Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
                _ => merged.push((lo, hi)),
            }
        }
        if merged.is_empty() {
            return Ok(ChainStats {
                events_in: 0,
                events_yielded: 0,
                records: 0,
                files: self.files.len(),
                elapsed: start.elapsed(),
            });
        }

        // One pass over the records, keeping those any range touches together
        // with the (contiguous, because `merged` is sorted) slice that applies.
        let mut tasks: Vec<(usize, usize, u64, usize, usize)> = Vec::new();
        for &(fi, ri) in self.build_tasks().iter() {
            let span = &self.files[fi].index.records()[ri];
            let rec_start = self.file_event_offsets[fi] + span.first_event;
            let rec_end = rec_start + u64::from(span.event_count);
            let first = merged.partition_point(|&(_, hi)| hi <= rec_start);
            let last = merged.partition_point(|&(lo, _)| lo < rec_end);
            if first < last {
                tasks.push((fi, ri, rec_start, first, last));
            }
        }

        let filter = self.filter.as_ref();
        let filter_active = filter.is_some_and(|f| f.is_active());
        let events_in = AtomicU64::new(0);
        let events_yielded = AtomicU64::new(0);
        let files = &self.files;
        let win = |rec_start: u64, a: usize, b: usize| EventWindow {
            record_start: rec_start,
            ranges: &merged[a..b],
        };

        if threads == 1 {
            let mut record = Record::new();
            let mut read_buf = Vec::new();
            for &(fi, ri, rs, a, b) in &tasks {
                process_record(
                    &files[fi],
                    ri,
                    filter,
                    filter_active,
                    Some(win(rs, a, b)),
                    &mut record,
                    &mut read_buf,
                    &f,
                    &events_in,
                    &events_yielded,
                )?;
            }
        } else {
            let run = || -> Result<()> {
                tasks.par_iter().try_for_each_init::<_, _, _, Result<()>>(
                    || (Record::new(), Vec::new()),
                    |(record, read_buf), &(fi, ri, rs, a, b)| {
                        process_record(
                            &files[fi],
                            ri,
                            filter,
                            filter_active,
                            Some(win(rs, a, b)),
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

/// Which global event indices a partially-overlapping record should yield.
///
/// [`Chain::for_each_range`] selects records that *intersect* the requested
/// range, so its first and last record usually hold events on both sides of the
/// boundary. This carries the record's own global start alongside the range so
/// those are dropped without reaching `f`.
#[derive(Debug, Clone, Copy)]
struct EventWindow<'r> {
    /// Global index of this record's first event.
    record_start: u64,
    /// The requested ranges that touch *this* record, in the same **pre-filter**
    /// global index space `read_columns(range)` and `--events A..B` use.
    ///
    /// A slice rather than one pair because a record can straddle several of
    /// them — merged and sorted by the caller, and usually one element.
    ranges: &'r [(u64, u64)],
}

impl EventWindow<'_> {
    /// Whether local event `ev_idx` of this record is inside the request.
    ///
    /// Takes `u64` so the three record layouts — whose event counters are
    /// `u32` and `usize` — can all hand it their index without a cast at each
    /// call site.
    fn holds(&self, ev_idx: u64) -> bool {
        let g = self.record_start + ev_idx;
        self.ranges.iter().any(|&(lo, hi)| g >= lo && g < hi)
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
    window: Option<EventWindow<'_>>,
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
            // Outside the request: not counted, not yielded. A record that
            // straddles the boundary is read once and its out-of-range events
            // dropped here.
            if let Some(w) = window
                && !w.holds(u64::from(ev_idx))
            {
                continue;
            }
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
            // Outside the request: not counted, not yielded. A record that
            // straddles the boundary is read once and its out-of-range events
            // dropped here.
            if let Some(w) = window
                && !w.holds(u64::from(ev_idx))
            {
                continue;
            }
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
        record.load_with_header(read_buf, header, Some(&inner.dict))?;
        for ev_idx in 0..record.event_count() {
            // Outside the request: not counted, not yielded. A record that
            // straddles the boundary is read once and its out-of-range events
            // dropped here.
            if let Some(w) = window
                && !w.holds(ev_idx as u64)
            {
                continue;
            }
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
        .map_err(|e| HipoError::ThreadPool(e.to_string()))
}

// ---- In-place tag update helpers -----------------------------------------

/// One resolved in-place tag update: which file, which record, which local
/// event, and the new tag value.
struct TagPatch {
    file_idx: usize,
    record_offset: u64,
    ev_local: u32,
    tag: u32,
}

/// The byte geometry of one *uncompressed* record, enough to locate any event's
/// header on disk without decompressing anything.
struct RecordLayout {
    /// Byte offset of the data section, relative to the record's file offset.
    data_start_in_record: u64,
    /// Cumulative event byte offsets within the data section (`event_count + 1`
    /// entries; the first is 0).
    event_offsets: Vec<u32>,
}

impl RecordLayout {
    /// Byte offset of event `ev`'s header, relative to the record's file offset.
    fn event_start_in_record(&self, ev: u32) -> Result<u64> {
        let off = *self
            .event_offsets
            .get(ev as usize)
            .ok_or(HipoError::CorruptRecord {
                offset: 0,
                reason: "event index past record event count during in-place tag patch",
            })?;
        Ok(self.data_start_in_record + u64::from(off))
    }
}

/// Read + validate the header and index array of the record at `record_offset`,
/// returning its [`RecordLayout`]. Errors if the record is compressed (only
/// uncompressed records are patchable in place) or malformed.
fn read_record_layout(fd: &File, record_offset: u64) -> Result<RecordLayout> {
    let mut hdr = [0u8; RECORD_HEADER_SIZE];
    read_exact_at(fd, record_offset, &mut hdr)?;
    let header = RecordHeader::parse(&hdr)?;
    if !matches!(header.compression, CompressionType::None) {
        return Err(HipoError::InPlaceTagUnsupported {
            offset: record_offset,
            compression: compression_label(header.compression),
        });
    }
    // The index array (`4 * event_count` bytes of per-event sizes) must fit in
    // the record's payload — bound the allocation against a hostile header.
    if u64::from(header.index_array_length) > header.payload_bytes() {
        return Err(HipoError::CorruptRecord {
            offset: record_offset,
            reason: "index array larger than record payload",
        });
    }
    let ia_len = header.index_array_length as usize;
    let mut index = vec![0u8; ia_len];
    read_exact_at(
        fd,
        record_offset + u64::from(header.header_length),
        &mut index,
    )?;

    let n = ia_len / 4;
    let mut event_offsets = Vec::with_capacity(n + 1);
    event_offsets.push(0u32);
    let mut acc = 0u32;
    for i in 0..n {
        let size = u32::from_le_bytes(index[i * 4..i * 4 + 4].try_into().expect("4 bytes"));
        acc = acc.saturating_add(size);
        event_offsets.push(acc);
    }

    let data_start_in_record = u64::from(header.header_length)
        + u64::from(header.index_array_length)
        + u64::from(header.user_header_length)
        + u64::from(header.user_header_padding);
    Ok(RecordLayout {
        data_start_in_record,
        event_offsets,
    })
}

/// The chain's dictionary: every bank any file declares, checked for conflicts.
///
/// A chain used to require every file's dictionary to be *equal* to file 0's.
/// That comparison was also order-sensitive — `Dict` derives `PartialEq` over a
/// positional `Vec<Schema>` plus index tables whose values are insertion
/// indices — so a glob over a real run period could fail on files describing the
/// same banks in a different order, which nothing about the format makes
/// meaningful. Worse, a genuinely heterogeneous set — a pass-2 cook that
/// added a bank, an MC file carrying `MC::Lund` — was refused outright with no
/// way to opt out, though a bank simply being absent from a file is something
/// the read path already handles (it yields an empty entry).
///
/// So take the union instead, and reject only what a reader cannot survive:
///
/// - the **same name** describing different layouts. Column offsets would be
///   read against the wrong schema.
/// - the same **`(group, item)`** used for different banks. This is the sharp
///   one: the columnar path resolves banks by id from *this* dictionary
///   ([`super::columns`]), so a collision would decode one file's bytes using
///   another file's schema and silently return wrong numbers rather than fail.
///
/// File 0's banks come first and later files append, so for a chain that opens
/// today the union is element-for-element what file 0's dictionary already was.
fn union_dict(files: &[Arc<FileInner>]) -> Result<Dict> {
    let mut union = Dict::new();
    // (group, item) -> the name that claimed it, for the collision check.
    let mut ids: HashMap<(u16, u8), &str> = HashMap::new();

    for f in files {
        for schema in f.dict.iter() {
            if let Some(seen) = union.get(schema.name()) {
                if seen != schema {
                    return Err(HipoError::SchemaParse(format!(
                        "chain file {} declares bank {} with a different layout \
                         than an earlier file; reading them together would \
                         decode columns against the wrong schema",
                        f.path().display(),
                        schema.name(),
                    )));
                }
                continue;
            }
            if let Some(&owner) = ids.get(&(schema.group(), schema.item()))
                && owner != schema.name()
            {
                return Err(HipoError::SchemaParse(format!(
                    "chain file {} declares bank {} with id ({}, {}), which an \
                     earlier file already uses for {}; banks are located by id, \
                     so one would be decoded as the other",
                    f.path().display(),
                    schema.name(),
                    schema.group(),
                    schema.item(),
                    owner,
                )));
            }
            let stored = union.add(schema.clone());
            ids.insert((stored.group(), stored.item()), schema.name());
        }
    }
    Ok(union)
}

/// Lower-case wire name of a record compression, for error messages.
fn compression_label(c: CompressionType) -> &'static str {
    match c {
        CompressionType::None => "none",
        CompressionType::Lz4 => "lz4",
        CompressionType::Lz4Best => "lz4best",
        CompressionType::Gzip => "gzip",
        CompressionType::Lz4PerBank => "lz4perbank",
        CompressionType::Lz4PerColumn => "lz4percolumn",
    }
}

/// Positioned read. On Unix this is `pread` (no shared cursor). Elsewhere it
/// seeks the passed handle — callers use a private handle, so this is safe.
#[cfg(unix)]
fn read_exact_at(f: &File, offset: u64, buf: &mut [u8]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, offset).map_err(HipoError::Io)
}

/// Positioned write. On Unix this is `pwrite` (no shared cursor).
#[cfg(unix)]
fn write_all_at(f: &File, offset: u64, buf: &[u8]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, offset).map_err(HipoError::Io)
}

#[cfg(not(unix))]
fn read_exact_at(mut f: &File, offset: u64, buf: &mut [u8]) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    f.seek(SeekFrom::Start(offset)).map_err(HipoError::Io)?;
    f.read_exact(buf).map_err(HipoError::Io)
}

#[cfg(not(unix))]
fn write_all_at(mut f: &File, offset: u64, buf: &[u8]) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    f.seek(SeekFrom::Start(offset)).map_err(HipoError::Io)?;
    f.write_all(buf).map_err(HipoError::Io)
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
