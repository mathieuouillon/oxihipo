//! Bulk **columnar** extraction — the pure-Rust engine behind the planned
//! Python binding (see `docs/python-binding-design.md`).
//!
//! [`Chain::read_columns`] walks the filtered chain **once** and, for every
//! requested `(bank, column)`, produces two fully-owned buffers:
//!
//! - a flat `content` vector of that column's values across every surviving
//!   event, concatenated in global event order, and
//! - one shared **`i64` offsets** vector per bank (`offsets[e]..offsets[e+1]`
//!   = the rows of event `e`) — exactly an Awkward `ListOffsetArray` /
//!   `Index64` layout.
//!
//! Design commitments carried over from the reconciled design doc:
//!
//! - **Whole columns, not events.** The per-event loop lives here in Rust;
//!   a consumer never iterates events.
//! - **Offsets are `i64`, always** — a full-chain concatenation can exceed
//!   2³¹ rows, and Awkward has no unsigned-64 index.
//! - **Offsets count every *surviving* event**, emitting `0` where a bank is
//!   absent from an event (an empty sub-list) so that columns from different
//!   banks stay length-aligned and `ak.zip`-able. This is why the existing
//!   [`Chain::for_each_column`](super::chain::Chain::for_each_column) can't be
//!   reused: it discards per-event row counts and ignores the filter.
//! - **Filter + record-tag pushdown honored**, exactly like
//!   [`Chain::for_each`](super::chain::Chain::for_each).
//! - **Uniform across storage formats.** Every layout (`None`/`Lz4`/`Gzip`/
//!   `Lz4Chunked`, `Lz4ByBank(V2)`, `Lz4PerColumn`) is reduced to the same
//!   primitive: obtain a [`Bank`] for `(event, bank)`, take `bank.rows()` for
//!   the offset delta and `bank.col_bytes(col)` for the flat column bytes.
//!   The columnar formats never inflate a stream a consumer didn't ask for.
//!
//! Array columns (`T#N`) are emitted as flat scalars with `inner_len = N`;
//! the caller wraps them in a `RegularArray(N)` so the single row-count
//! offsets index them unchanged.

use std::ops::Range;
use std::sync::Arc;

use rayon::prelude::*;

use crate::error::{HipoError, Result};
use crate::event::{Bank, Event};
use crate::schema::{BankColumnType, DataType, Dict, Schema};
use crate::wire::by_bank::ByBankRecord;
use crate::wire::per_column::PerColumnRecord;
use crate::wire::record::Record;

use super::chain::Chain;
use super::filter::Filter;
use super::inner::FileInner;

/// A column's values, tagged by wire element type. Each variant holds a flat
/// vector of scalars; array columns (`T#N`) store `rows * N` scalars and the
/// grouping is recovered from [`MaterializedColumn::inner_len`].
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnData {
    I8(Vec<i8>),
    I16(Vec<i16>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
}

impl ColumnData {
    /// A fresh empty buffer of the variant matching `dt`.
    fn empty(dt: DataType) -> Self {
        match dt {
            DataType::Byte => ColumnData::I8(Vec::new()),
            DataType::Short => ColumnData::I16(Vec::new()),
            DataType::Int => ColumnData::I32(Vec::new()),
            DataType::Long => ColumnData::I64(Vec::new()),
            DataType::Float => ColumnData::F32(Vec::new()),
            DataType::Double => ColumnData::F64(Vec::new()),
        }
    }

    /// The wire element type of the stored scalars.
    pub fn data_type(&self) -> DataType {
        match self {
            ColumnData::I8(_) => DataType::Byte,
            ColumnData::I16(_) => DataType::Short,
            ColumnData::I32(_) => DataType::Int,
            ColumnData::I64(_) => DataType::Long,
            ColumnData::F32(_) => DataType::Float,
            ColumnData::F64(_) => DataType::Double,
        }
    }

    /// Number of scalar elements (`rows * inner_len` for the column).
    pub fn len(&self) -> usize {
        match self {
            ColumnData::I8(v) => v.len(),
            ColumnData::I16(v) => v.len(),
            ColumnData::I32(v) => v.len(),
            ColumnData::I64(v) => v.len(),
            ColumnData::F32(v) => v.len(),
            ColumnData::F64(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reserve capacity in the underlying vector.
    fn reserve(&mut self, additional: usize) {
        match self {
            ColumnData::I8(v) => v.reserve(additional),
            ColumnData::I16(v) => v.reserve(additional),
            ColumnData::I32(v) => v.reserve(additional),
            ColumnData::I64(v) => v.reserve(additional),
            ColumnData::F32(v) => v.reserve(additional),
            ColumnData::F64(v) => v.reserve(additional),
        }
    }

    /// Append raw little-endian bytes as scalars of this variant's type.
    /// `bytes.len()` must be a multiple of the scalar size (guaranteed by
    /// [`Bank::col_bytes`], which returns `rows * ty.size() * length` bytes).
    fn push_bytes(&mut self, bytes: &[u8]) {
        match self {
            ColumnData::I8(v) => extend_cast(v, bytes),
            ColumnData::I16(v) => extend_cast(v, bytes),
            ColumnData::I32(v) => extend_cast(v, bytes),
            ColumnData::I64(v) => extend_cast(v, bytes),
            ColumnData::F32(v) => extend_cast(v, bytes),
            ColumnData::F64(v) => extend_cast(v, bytes),
        }
    }

    /// Move `other`'s scalars onto the end of `self`. Both must be the same
    /// variant (guaranteed: chunks are built from the same column plan).
    fn append(&mut self, other: ColumnData) {
        match (self, other) {
            (ColumnData::I8(a), ColumnData::I8(mut b)) => a.append(&mut b),
            (ColumnData::I16(a), ColumnData::I16(mut b)) => a.append(&mut b),
            (ColumnData::I32(a), ColumnData::I32(mut b)) => a.append(&mut b),
            (ColumnData::I64(a), ColumnData::I64(mut b)) => a.append(&mut b),
            (ColumnData::F32(a), ColumnData::F32(mut b)) => a.append(&mut b),
            (ColumnData::F64(a), ColumnData::F64(mut b)) => a.append(&mut b),
            _ => unreachable!("ColumnData variant mismatch while merging record chunks"),
        }
    }

    /// Reinterpret the flat scalars as `Vec<T>` (`T` is a
    /// [`BankColumnType`], possibly an array cell `[S; N]`). The scalar
    /// element type must match `T::DATA_TYPE`. Copies once; the zero-copy
    /// path for consumers is [`Chain::read_columns`] itself.
    fn into_typed<T: BankColumnType>(self) -> Result<Vec<T>> {
        fn conv<S: bytemuck::Pod, T: bytemuck::Pod>(v: Vec<S>) -> Result<Vec<T>> {
            bytemuck::try_cast_slice::<S, T>(&v)
                .map(<[T]>::to_vec)
                .map_err(|_| HipoError::CorruptRecord {
                    offset: 0,
                    reason: "column length is not a whole multiple of the requested cell size",
                })
        }
        if self.data_type() != T::DATA_TYPE {
            return Err(HipoError::TypeMismatch {
                schema: String::new(),
                column: String::new(),
                expected: T::DATA_TYPE.name(),
                actual: self.data_type().name(),
            });
        }
        match self {
            ColumnData::I8(v) => conv::<i8, T>(v),
            ColumnData::I16(v) => conv::<i16, T>(v),
            ColumnData::I32(v) => conv::<i32, T>(v),
            ColumnData::I64(v) => conv::<i64, T>(v),
            ColumnData::F32(v) => conv::<f32, T>(v),
            ColumnData::F64(v) => conv::<f64, T>(v),
        }
    }
}

/// Cast `bytes` to `&[S]` and append; falls back to an element-wise
/// unaligned read when the source isn't `S`-aligned (LZ4 output need not be).
#[inline]
fn extend_cast<S: bytemuck::Pod>(v: &mut Vec<S>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    match bytemuck::try_cast_slice::<u8, S>(bytes) {
        Ok(s) => v.extend_from_slice(s),
        Err(_) => {
            let elem = std::mem::size_of::<S>();
            v.extend(
                (0..bytes.len() / elem)
                    .map(|i| bytemuck::pod_read_unaligned::<S>(&bytes[i * elem..i * elem + elem])),
            );
        }
    }
}

/// One column of a materialized bank.
#[derive(Debug, Clone, PartialEq)]
pub struct MaterializedColumn {
    /// Column name as declared in the schema.
    pub name: String,
    /// Wire element type.
    pub data_type: DataType,
    /// Elements per row: `1` for a scalar column, `N` for a `T#N` array
    /// column. The caller wraps the flat `data` in a `RegularArray(inner_len)`.
    pub inner_len: u32,
    /// Flat values across every surviving event.
    pub data: ColumnData,
}

/// The materialized columns of one bank plus its shared jagged offsets.
///
/// Invariants (asserted before return): `offsets[0] == 0`, `offsets` is
/// non-decreasing with `offsets.len() == surviving_events + 1`, and for every
/// column `data.len() == offsets.last() * inner_len`.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnBuffers {
    /// Requested bank name.
    pub bank: String,
    /// Shared per-event row offsets (`i64` / Awkward `Index64`).
    pub offsets: Vec<i64>,
    /// One entry per requested column, in request order.
    pub columns: Vec<MaterializedColumn>,
}

impl ColumnBuffers {
    /// Number of events represented (`offsets.len() - 1`).
    pub fn event_count(&self) -> usize {
        self.offsets.len() - 1
    }

    /// Total rows across all events (`offsets.last()`).
    pub fn total_rows(&self) -> i64 {
        *self.offsets.last().unwrap_or(&0)
    }
}

/// A record's position in the chain, without any decompression. Feeds the
/// planned streaming cursor (`iterate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainRecordSpan {
    pub file_index: usize,
    pub record_index: usize,
    /// Global index of this record's first event.
    pub global_event_start: u64,
    pub event_count: u32,
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// A resolved column within a bank plan.
struct ColPlan {
    col_idx: usize,
    data_type: DataType,
    inner_len: u32,
    name: String,
}

/// A resolved bank + its requested columns. Borrows the `&Schema` out of the
/// chain's shared `Dict`, so it is `Send + Sync` and safe to share across
/// rayon workers.
struct BankPlan<'a> {
    name: String,
    group: u16,
    item: u8,
    schema: &'a Schema,
    cols: Vec<ColPlan>,
}

/// Resolve the `(bank, columns)` selection against the dictionary up front,
/// so a bad name fails before any I/O. Empty `cols` means "all columns".
fn build_plan<'a>(dict: &'a Dict, selection: &[(&str, &[&str])]) -> Result<Vec<BankPlan<'a>>> {
    let mut plan = Vec::with_capacity(selection.len());
    for &(bank, cols) in selection {
        let schema = dict.require(bank)?;
        let indices: Vec<usize> = if cols.is_empty() {
            (0..schema.num_columns()).collect()
        } else {
            cols.iter()
                .map(|&c| schema.require_column(c))
                .collect::<Result<_>>()?
        };
        let cols = indices
            .into_iter()
            .map(|ci| {
                let e = &schema.entries()[ci];
                ColPlan {
                    col_idx: ci,
                    data_type: e.ty,
                    inner_len: e.length,
                    name: e.name.clone(),
                }
            })
            .collect();
        plan.push(BankPlan {
            name: bank.to_string(),
            group: schema.group(),
            item: schema.item(),
            schema,
            cols,
        });
    }
    Ok(plan)
}

// ---------------------------------------------------------------------------
// Per-record chunk (the unit of parallel work)
// ---------------------------------------------------------------------------

/// One bank's contribution from a single record.
struct BankChunk {
    /// Row count per surviving event of this record (offset deltas).
    row_counts: Vec<u32>,
    /// One flat buffer per requested column.
    columns: Vec<ColumnData>,
}

/// A record's contribution to every requested bank, in plan order.
struct RecordChunk {
    banks: Vec<BankChunk>,
}

impl RecordChunk {
    /// An empty contribution (record skipped, e.g. entirely out of range).
    fn empty(plan: &[BankPlan<'_>]) -> Self {
        RecordChunk {
            banks: plan.iter().map(BankChunk::empty).collect(),
        }
    }
}

impl BankChunk {
    fn empty(bp: &BankPlan<'_>) -> Self {
        BankChunk {
            row_counts: Vec::new(),
            columns: bp
                .cols
                .iter()
                .map(|c| ColumnData::empty(c.data_type))
                .collect(),
        }
    }

    /// Record a surviving event's `bank`, appending its columns. `bank` is
    /// `None` when the bank is absent from this event → a `0`-row sub-list.
    fn push_event(&mut self, bp: &BankPlan<'_>, bank: Option<&Bank<'_>>) {
        match bank {
            Some(b) => {
                self.row_counts.push(b.rows());
                for (sink, cp) in self.columns.iter_mut().zip(&bp.cols) {
                    sink.push_bytes(b.col_bytes(cp.col_idx));
                }
            }
            None => self.row_counts.push(0),
        }
    }
}

/// Whether a local event survives the range + presence filter, given its
/// global index. Bank presence is checked per compression backend by the
/// caller; this only covers `range` and the (bound) event filter's cheap
/// per-event checks are applied at the call site.
#[inline]
fn in_range(range: Option<&Range<u64>>, global: u64) -> bool {
    match range {
        Some(r) => global >= r.start && global < r.end,
        None => true,
    }
}

/// Materialize every requested bank/column for one record.
#[allow(clippy::too_many_arguments)]
fn process_record_columns(
    inner: &Arc<FileInner>,
    ri: usize,
    file_base: u64,
    plan: &[BankPlan<'_>],
    filter: Option<&Filter>,
    filter_active: bool,
    range: Option<&Range<u64>>,
    record: &mut Record,
    read_buf: &mut Vec<u8>,
) -> Result<RecordChunk> {
    let span = &inner.index.records()[ri];
    let global_first = file_base + span.first_event;

    // Skip the whole record (no I/O) when it can't intersect the range.
    if let Some(r) = range {
        let rec_end = global_first + u64::from(span.event_count);
        if rec_end <= r.start || global_first >= r.end {
            return Ok(RecordChunk::empty(plan));
        }
    }

    let header = inner.read_record_into(span.file_offset, read_buf)?;
    let mut banks: Vec<BankChunk> = plan.iter().map(BankChunk::empty).collect();

    if header.compression.is_by_bank() {
        let rec = ByBankRecord::parse(read_buf)?;
        let idxs: Vec<Option<u32>> = plan
            .iter()
            .map(|bp| rec.bank_index(bp.group, bp.item))
            .collect();
        for e in 0..rec.event_count() {
            if !in_range(range, global_first + u64::from(e)) {
                continue;
            }
            if filter_active && filter.is_some_and(|f| !f.check_by_bank(&rec, e)) {
                continue;
            }
            for ((bp, bc), &idx) in plan.iter().zip(&mut banks).zip(&idxs) {
                match idx {
                    Some(b) if rec.has(e, b) => {
                        let stream = rec.bank_stream(b)?;
                        let bank = Bank::new(bp.schema, &stream[rec.bank_byte_range(e, b)])?;
                        bc.push_event(bp, Some(&bank));
                    }
                    _ => bc.push_event(bp, None),
                }
            }
        }
    } else if header.compression.is_per_column() {
        let rec = PerColumnRecord::parse(read_buf)?;
        let idxs: Vec<Option<u32>> = plan
            .iter()
            .map(|bp| rec.bank_index(bp.group, bp.item))
            .collect();
        for e in 0..rec.event_count() {
            if !in_range(range, global_first + u64::from(e)) {
                continue;
            }
            if filter_active && filter.is_some_and(|f| !f.check_per_column(&rec, e)) {
                continue;
            }
            for ((bp, bc), &idx) in plan.iter().zip(&mut banks).zip(&idxs) {
                match idx {
                    Some(b) => {
                        let bank = Bank::new_per_column(bp.schema, &rec, b, e);
                        bc.push_event(bp, Some(&bank));
                    }
                    None => bc.push_event(bp, None),
                }
            }
        }
    } else {
        record.load_with_header(read_buf, header)?;
        for e in 0..record.event_count() {
            if !in_range(range, global_first + u64::from(e)) {
                continue;
            }
            let Some(raw) = record.event(e) else { continue };
            let event = Event::new(raw);
            if filter_active && filter.is_some_and(|f| !f.check(&event)) {
                continue;
            }
            for (bp, bc) in plan.iter().zip(&mut banks) {
                match event.find(bp.group, bp.item) {
                    Some((_, data)) => {
                        let bank = Bank::new(bp.schema, data)?;
                        bc.push_event(bp, Some(&bank));
                    }
                    None => bc.push_event(bp, None),
                }
            }
        }
    }

    Ok(RecordChunk { banks })
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

impl Chain {
    /// Bulk columnar extraction over the **filtered** chain in a single pass.
    ///
    /// For every `(bank, columns)` in `selection` (empty `columns` = all
    /// columns of that bank), returns one [`ColumnBuffers`] carrying the
    /// bank's shared `i64` offsets and each column's flat values, in global
    /// event order. `range` restricts to global event indices `[start, stop)`
    /// (`None` = whole chain). `threads`: `0` = rayon's global pool, `1` =
    /// sequential, `n` = an `n`-thread pool. Honors the chain filter and
    /// record-tag pushdown. A corrupt record aborts with `Err` — never a
    /// short or misaligned result.
    pub fn read_columns(
        &self,
        selection: &[(&str, &[&str])],
        range: Option<Range<u64>>,
        threads: usize,
    ) -> Result<Vec<ColumnBuffers>> {
        let plan = build_plan(self.schemas(), selection)?;
        if plan.is_empty() {
            return Ok(Vec::new());
        }

        let tasks = self.build_tasks();
        let files = self.files_inner();
        let offsets = self.event_offsets();
        let filter = self.filter_ref();
        let filter_active = filter.is_some_and(Filter::is_active);
        let range = range.as_ref();

        let run_one = |record: &mut Record, read_buf: &mut Vec<u8>, fi: usize, ri: usize| {
            process_record_columns(
                &files[fi],
                ri,
                offsets[fi],
                &plan,
                filter,
                filter_active,
                range,
                record,
                read_buf,
            )
        };

        let chunks: Vec<RecordChunk> = if threads == 1 {
            let mut record = Record::new();
            let mut read_buf = Vec::new();
            tasks
                .iter()
                .map(|&(fi, ri)| run_one(&mut record, &mut read_buf, fi, ri))
                .collect::<Result<_>>()?
        } else {
            let run = || {
                tasks
                    .par_iter()
                    .map_init(
                        || (Record::new(), Vec::new()),
                        |(record, read_buf), &(fi, ri)| run_one(record, read_buf, fi, ri),
                    )
                    .collect::<Result<Vec<_>>>()
            };
            if threads == 0 {
                run()?
            } else {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(threads)
                    .build()
                    .map_err(|_| HipoError::Compression("rayon thread pool init failed"))?;
                pool.install(run)?
            }
        };

        Ok(merge_chunks(&plan, chunks))
    }

    /// Typed single-column read: `(offsets, content)` where `content` is a
    /// `Vec<T>` (`T` scalar or array cell `[S; N]`). Validates `T` against the
    /// schema (type + per-row length) before reading. Convenience wrapper over
    /// [`Self::read_columns`]; it reinterprets the buffer once (the zero-copy
    /// path is `read_columns`).
    pub fn read_column_typed<T: BankColumnType>(
        &self,
        bank: &str,
        column: &str,
        range: Option<Range<u64>>,
    ) -> Result<(Vec<i64>, Vec<T>)> {
        // Validate element type + array length up front (clear error on
        // mismatch, before any I/O).
        let _ = self.schemas().require(bank)?.handle::<T>(column)?;
        let mut bufs = self.read_columns(&[(bank, &[column])], range, 0)?;
        let mut buf = bufs.pop().expect("one bank requested");
        let offsets = std::mem::take(&mut buf.offsets);
        let data = buf.columns.pop().expect("one column requested").data;
        Ok((offsets, data.into_typed::<T>()?))
    }

    /// Flat values of one column across the (filtered) chain — offsets
    /// dropped. Equivalent to the values half of [`Self::read_column_typed`].
    pub fn column_values<T: BankColumnType>(
        &self,
        bank: &str,
        column: &str,
        range: Option<Range<u64>>,
    ) -> Result<Vec<T>> {
        Ok(self.read_column_typed::<T>(bank, column, range)?.1)
    }

    /// Every record's position in the chain, without decompression — the
    /// entry↔record map the streaming cursor slices `step_size` against.
    pub fn record_spans(&self) -> Vec<ChainRecordSpan> {
        let offsets = self.event_offsets();
        let mut out = Vec::new();
        for (fi, inner) in self.files_inner().iter().enumerate() {
            let base = offsets[fi];
            for (ri, span) in inner.index.records().iter().enumerate() {
                out.push(ChainRecordSpan {
                    file_index: fi,
                    record_index: ri,
                    global_event_start: base + span.first_event,
                    event_count: span.event_count,
                });
            }
        }
        out
    }
}

/// Concatenate the ordered per-record chunks into one [`ColumnBuffers`] per
/// bank: prefix-sum the row counts into shared `i64` offsets and append each
/// column's values.
fn merge_chunks(plan: &[BankPlan<'_>], chunks: Vec<RecordChunk>) -> Vec<ColumnBuffers> {
    let mut out: Vec<ColumnBuffers> = plan
        .iter()
        .map(|bp| ColumnBuffers {
            bank: bp.name.clone(),
            offsets: vec![0],
            columns: bp
                .cols
                .iter()
                .map(|c| MaterializedColumn {
                    name: c.name.clone(),
                    data_type: c.data_type,
                    inner_len: c.inner_len,
                    data: ColumnData::empty(c.data_type),
                })
                .collect(),
        })
        .collect();

    let mut running = vec![0i64; plan.len()];
    for chunk in chunks {
        for (bi, bc) in chunk.banks.into_iter().enumerate() {
            let ob = &mut out[bi];
            ob.offsets.reserve(bc.row_counts.len());
            for rc in bc.row_counts {
                running[bi] += i64::from(rc);
                ob.offsets.push(running[bi]);
            }
            for (dst, src) in ob.columns.iter_mut().zip(bc.columns) {
                dst.data.reserve(src.len());
                dst.data.append(src);
            }
        }
    }

    debug_assert!(out.iter().all(|b| {
        b.offsets.first() == Some(&0)
            && b.offsets.windows(2).all(|w| w[0] <= w[1])
            && b.columns
                .iter()
                .all(|c| c.data.len() as i64 == b.total_rows() * i64::from(c.inner_len))
    }));

    out
}
