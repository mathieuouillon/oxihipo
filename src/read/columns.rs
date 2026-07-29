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
//!   `Lz4PerBank`, `Lz4PerColumn`) is reduced to the same
//!   primitive: obtain a [`Bank`] for `(event, bank)`, take `bank.rows()` for
//!   the offset delta and `bank.col_bytes(col)` for the flat column bytes.
//!   The columnar formats never inflate a stream a consumer didn't ask for.
//!
//! Array columns (`T#N`) are emitted as flat scalars with `inner_len = N`;
//! the caller wraps them in a `RegularArray(N)` so the single row-count
//! offsets index them unchanged.

use std::collections::BTreeMap;
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
    /// A fresh empty buffer of the variant matching `dt`. Public counterpart of
    /// the internal `empty`, for callers materializing columns themselves (the
    /// Python binding's composite reader, which has no dictionary schema to
    /// plan from).
    pub fn empty_for(dt: DataType) -> Self {
        Self::empty(dt)
    }

    /// Append one value. Each pushes only into its matching variant; a
    /// mismatched call is a no-op, so a caller driving these from a field's
    /// declared type can't corrupt the buffer.
    pub fn push_i8(&mut self, v: i8) {
        if let Self::I8(b) = self {
            b.push(v);
        }
    }
    pub fn push_i16(&mut self, v: i16) {
        if let Self::I16(b) = self {
            b.push(v);
        }
    }
    pub fn push_i32(&mut self, v: i32) {
        if let Self::I32(b) = self {
            b.push(v);
        }
    }
    pub fn push_i64(&mut self, v: i64) {
        if let Self::I64(b) = self {
            b.push(v);
        }
    }
    pub fn push_f32(&mut self, v: f32) {
        if let Self::F32(b) = self {
            b.push(v);
        }
    }
    pub fn push_f64(&mut self, v: f64) {
        if let Self::F64(b) = self {
            b.push(v);
        }
    }

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
                        // Bounds-checked: the byte range comes from the record's
                        // own offset table, so a corrupted file can point it past
                        // the end of the decompressed stream. Indexing the slice
                        // raw panicked there — "range end index 3400 out of range
                        // for slice of length 3379" — where every other kind of
                        // damage in this reader surfaces as an error. A bank whose
                        // extent does not fit is treated as absent, which is how a
                        // bank that is not there is already reported.
                        let Some(raw) = stream.get(rec.bank_byte_range(e, b)) else {
                            bc.push_event(bp, None);
                            continue;
                        };
                        let bank = Bank::new(bp.schema, raw)?;
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
        record.load_with_header(read_buf, header, Some(&inner.dict))?;
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

/// One requested entry, as `read_columns_at` resolves it: its slot in the
/// caller's `entries` list, and the event's index local to its record.
type EntrySlot = (usize, u32);

/// The requested entries that live in one record, keyed by `(file, record)`.
type EntryGroup = ((usize, usize), Vec<EntrySlot>);

/// One event's worth of "the bank isn't here": a single 0-row sub-list in every
/// requested bank.
///
/// Distinct from [`RecordChunk::empty`], which contributes *no* events at all —
/// that is a skipped record, this is a present-but-empty entry. Confusing the
/// two silently shortens the offsets array relative to `entries`.
fn absent_event_chunk(plan: &[BankPlan<'_>]) -> RecordChunk {
    let mut banks: Vec<BankChunk> = plan.iter().map(BankChunk::empty).collect();
    for (bp, bc) in plan.iter().zip(&mut banks) {
        bc.push_event(bp, None);
    }
    RecordChunk { banks }
}

/// Read one record once and materialise a scattered subset of its events.
///
/// The counterpart to [`process_record_columns`], which walks a record start to
/// finish. `wanted` is `(slot, local event index)`, where `slot` is the event's
/// position in the caller's `entries` list; it is carried through untouched so
/// the caller can reassemble in its own order.
///
/// `per_entry` picks the granularity, and it is purely a cost question:
///
/// - `false` — one chunk for the whole group, events appended in `wanted` order.
///   Used when the caller's list is ascending, because then the groups are
///   already in output order and merging is a plain concatenation.
/// - `true` — one chunk per entry, so the caller can reassemble in *its* order
///   by reordering whole chunks instead of permuting packed jagged bytes. This
///   costs a few small allocations per entry, which is invisible next to the
///   record decode it saves but was measurable (+442% on an ascending list)
///   when applied to the case that did not need it.
///
/// Duplicate indices are honoured either way — each occurrence is pushed.
fn process_record_entries(
    inner: &Arc<FileInner>,
    ri: usize,
    plan: &[BankPlan<'_>],
    wanted: &[EntrySlot],
    per_entry: bool,
    record: &mut Record,
    read_buf: &mut Vec<u8>,
) -> Result<Vec<(usize, RecordChunk)>> {
    let span = &inner.index.records()[ri];
    let header = inner.read_record_into(span.file_offset, read_buf)?;
    let mut out = Vec::with_capacity(if per_entry { wanted.len() } else { 1 });

    // One accumulator, emitted after every entry (`per_entry`) or once at the
    // end. `flush` is a macro rather than a closure because the fill loops below
    // already hold `&mut banks`.
    let mut banks: Vec<BankChunk> = plan.iter().map(BankChunk::empty).collect();
    macro_rules! flush {
        ($slot:expr) => {
            if per_entry {
                let fresh = plan.iter().map(BankChunk::empty).collect();
                out.push((
                    $slot,
                    RecordChunk {
                        banks: std::mem::replace(&mut banks, fresh),
                    },
                ));
            }
        };
    }

    if header.compression.is_by_bank() {
        let rec = ByBankRecord::parse(read_buf)?;
        let idxs: Vec<Option<u32>> = plan
            .iter()
            .map(|bp| rec.bank_index(bp.group, bp.item))
            .collect();
        for &(slot, e) in wanted {
            for ((bp, bc), &idx) in plan.iter().zip(&mut banks).zip(&idxs) {
                match idx {
                    Some(b) if e < rec.event_count() && rec.has(e, b) => {
                        let stream = rec.bank_stream(b)?;
                        // Bounds-checked: the byte range comes from the record's
                        // own offset table, so a corrupted file can point it past
                        // the end of the decompressed stream. Indexing the slice
                        // raw panicked there — "range end index 3400 out of range
                        // for slice of length 3379" — where every other kind of
                        // damage in this reader surfaces as an error. A bank whose
                        // extent does not fit is treated as absent, which is how a
                        // bank that is not there is already reported.
                        let Some(raw) = stream.get(rec.bank_byte_range(e, b)) else {
                            bc.push_event(bp, None);
                            continue;
                        };
                        let bank = Bank::new(bp.schema, raw)?;
                        bc.push_event(bp, Some(&bank));
                    }
                    _ => bc.push_event(bp, None),
                }
            }
            flush!(slot);
        }
    } else if header.compression.is_per_column() {
        let rec = PerColumnRecord::parse(read_buf)?;
        let idxs: Vec<Option<u32>> = plan
            .iter()
            .map(|bp| rec.bank_index(bp.group, bp.item))
            .collect();
        for &(slot, e) in wanted {
            for ((bp, bc), &idx) in plan.iter().zip(&mut banks).zip(&idxs) {
                match idx {
                    Some(b) if e < rec.event_count() => {
                        let bank = Bank::new_per_column(bp.schema, &rec, b, e);
                        bc.push_event(bp, Some(&bank));
                    }
                    _ => bc.push_event(bp, None),
                }
            }
            flush!(slot);
        }
    } else {
        record.load_with_header(read_buf, header, Some(&inner.dict))?;
        for &(slot, e) in wanted {
            match record.event(e) {
                Some(raw) => {
                    let event = Event::new(raw);
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
                None => {
                    for (bp, bc) in plan.iter().zip(&mut banks) {
                        bc.push_event(bp, None);
                    }
                }
            }
            flush!(slot);
        }
    }

    if !per_entry && let Some(&(first, _)) = wanted.first() {
        out.push((first, RecordChunk { banks }));
    }
    Ok(out)
}

/// Collect the per-event tag (`EH_TAG`) of every surviving event in one record,
/// in order — the tag-only analogue of [`process_record_columns`]. Reads the
/// tag from the event header or the record directory; never inflates a bank.
#[allow(clippy::too_many_arguments)]
fn record_event_tags(
    inner: &Arc<FileInner>,
    ri: usize,
    file_base: u64,
    filter: Option<&Filter>,
    filter_active: bool,
    range: Option<&Range<u64>>,
    record: &mut Record,
    read_buf: &mut Vec<u8>,
) -> Result<Vec<u32>> {
    let span = &inner.index.records()[ri];
    let global_first = file_base + span.first_event;
    if let Some(r) = range {
        let rec_end = global_first + u64::from(span.event_count);
        if rec_end <= r.start || global_first >= r.end {
            return Ok(Vec::new());
        }
    }

    let header = inner.read_record_into(span.file_offset, read_buf)?;
    let mut tags = Vec::new();
    if header.compression.is_by_bank() {
        let rec = ByBankRecord::parse(read_buf)?;
        for e in 0..rec.event_count() {
            if !in_range(range, global_first + u64::from(e)) {
                continue;
            }
            if filter_active && filter.is_some_and(|f| !f.check_by_bank(&rec, e)) {
                continue;
            }
            tags.push(rec.event_tag(e));
        }
    } else if header.compression.is_per_column() {
        let rec = PerColumnRecord::parse(read_buf)?;
        for e in 0..rec.event_count() {
            if !in_range(range, global_first + u64::from(e)) {
                continue;
            }
            if filter_active && filter.is_some_and(|f| !f.check_per_column(&rec, e)) {
                continue;
            }
            tags.push(rec.event_tag(e));
        }
    } else {
        record.load_with_header(read_buf, header, Some(&inner.dict))?;
        for e in 0..record.event_count() {
            if !in_range(range, global_first + u64::from(e)) {
                continue;
            }
            let Some(raw) = record.event(e) else { continue };
            let event = Event::new(raw);
            if filter_active && filter.is_some_and(|f| !f.check(&event)) {
                continue;
            }
            tags.push(event.tag());
        }
    }
    Ok(tags)
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
                    .map_err(|e| HipoError::ThreadPool(e.to_string()))?;
                pool.install(run)?
            }
        };

        merge_chunks(&plan, chunks)
    }

    /// [`Self::read_columns`] for an explicit list of global event indices,
    /// rather than a contiguous range.
    ///
    /// This is the columnar counterpart to [`Chain::event`]: replaying a list
    /// of interesting events found by an earlier pass. Output is aligned 1:1
    /// with `entries` — element *k* of every bank's offsets describes
    /// `entries[k]` — so the caller's order is preserved and duplicates are
    /// honoured.
    ///
    /// Indices out of range contribute a 0-row sub-list rather than an error,
    /// matching how an absent bank is already reported.
    ///
    /// Order does not matter for speed. Entries are grouped by the record that
    /// holds them, so each record is read and decompressed **once** however the
    /// list is arranged, and the groups run in parallel under the usual
    /// `threads` convention (`0` = rayon's global pool, `1` = sequential, `n` =
    /// an `n`-thread pool). Previously every lookup went through
    /// [`Chain::event`] and its single-slot record cache, so a non-ascending
    /// list re-decoded a whole record *per index* — 256 scattered lookups cost
    /// 7 ms against 13 µs for the same count ascending.
    ///
    /// Like [`Chain::event`], and unlike a range read, this addresses the
    /// file's event stream and does not apply the chain filter.
    pub fn read_columns_at(
        &self,
        selection: &[(&str, &[&str])],
        entries: &[u64],
        threads: usize,
    ) -> Result<Vec<ColumnBuffers>> {
        let plan = build_plan(self.schemas(), selection)?;
        if plan.is_empty() {
            return Ok(Vec::new());
        }
        let files = self.files_inner();
        let offsets = self.event_offsets();

        // Resolve each index to (file, record, local event) once, and group by
        // record. The value carries each entry's *slot* — its position in
        // `entries` — which is what lets the result be reassembled in the
        // caller's order at the end regardless of the order records run in.
        // A `BTreeMap`, not a `HashMap`: the fast path below concatenates groups
        // in iteration order, so that order *is* the output order. Getting it
        // from the container rather than from a sort afterwards makes the
        // invariant structural — and keeps a run reproducible instead of
        // dependent on hash seed.
        let mut groups: BTreeMap<(usize, usize), Vec<EntrySlot>> = BTreeMap::new();
        let mut resolved = 0usize;
        for (slot, &idx) in entries.iter().enumerate() {
            let Some(file_idx) = offsets.partition_point(|&o| o <= idx).checked_sub(1) else {
                continue;
            };
            if file_idx >= files.len() {
                continue;
            }
            let local = idx - offsets[file_idx];
            let Some((rec_idx, ev_local)) = files[file_idx].index.locate(local) else {
                continue;
            };
            resolved += 1;
            groups
                .entry((file_idx, rec_idx))
                .or_default()
                .push((slot, ev_local));
        }

        // Everything in one record (or nothing resolvable): replay through
        // `Chain::event`, as this always did.
        //
        // Grouping exists to stop a record being decoded once per index; with a
        // single record there is nothing to stop, and `Chain::event`'s one-slot
        // cache is *better* than reading it again — a second call with the same
        // list decodes nothing at all, where the grouped path always re-reads.
        // It is never worse either: a cold cache costs the same one decode.
        // Skipping this made a repeated ascending read 3x slower.
        if groups.len() <= 1 {
            let mut chunk = RecordChunk::empty(&plan);
            for &idx in entries {
                let ev = self.event(idx);
                for (bc, bp) in chunk.banks.iter_mut().zip(&plan) {
                    bc.push_event(bp, ev.as_ref().and_then(|e| e.bank(&bp.name)).as_ref());
                }
            }
            return merge_chunks(&plan, vec![chunk]);
        }

        // An ascending, fully-resolved list needs no reassembly: each record's
        // slots are then contiguous and increasing, so visiting records in
        // order and concatenating is already the caller's order. Taking that
        // path avoids a chunk allocation per entry — which cost +442% on
        // exactly this, the list shape the docs tell people to prefer.
        let per_entry = resolved != entries.len() || entries.windows(2).any(|w| w[0] > w[1]);

        // Already ordered by (file, record) — and within a group the slots are
        // ascending too, having been pushed in `entries` order.
        let work: Vec<EntryGroup> = groups.into_iter().collect();
        let run_one =
            |record: &mut Record, read_buf: &mut Vec<u8>, key: &(usize, usize), w: &[EntrySlot]| {
                process_record_entries(&files[key.0], key.1, &plan, w, per_entry, record, read_buf)
            };

        // One group means one record to read, so there is nothing to run in
        // parallel — and a small interactive `entries=[..]` inside one record is
        // the normal case. Going through rayon anyway would make it pay for a
        // `ThreadPoolBuilder::build()` it cannot use.
        let filled: Vec<Vec<(usize, RecordChunk)>> = if threads == 1 || work.len() < 2 {
            let mut record = Record::new();
            let mut read_buf = Vec::new();
            work.iter()
                .map(|(key, w)| run_one(&mut record, &mut read_buf, key, w))
                .collect::<Result<_>>()?
        } else {
            let run = || {
                work.par_iter()
                    .map_init(
                        || (Record::new(), Vec::new()),
                        |(record, read_buf), (key, w)| run_one(record, read_buf, key, w),
                    )
                    .collect::<Result<Vec<_>>>()
            };
            if threads == 0 {
                run()?
            } else {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(threads)
                    .build()
                    .map_err(|e| HipoError::ThreadPool(e.to_string()))?;
                pool.install(run)?
            }
        };

        if !per_entry {
            // Ascending and fully resolved: the per-record chunks are already
            // in the caller's order, so concatenating them is the answer.
            let chunks = filled.into_iter().flatten().map(|(_, c)| c).collect();
            return merge_chunks(&plan, chunks);
        }

        // Otherwise scatter back into caller order. A slot left empty is an
        // index that resolved to no event (out of range) — it still contributes
        // one 0-row sub-list, so every bank's offsets stay the same length as
        // `entries`.
        let mut slots: Vec<Option<RecordChunk>> = (0..entries.len()).map(|_| None).collect();
        for (slot, chunk) in filled.into_iter().flatten() {
            slots[slot] = Some(chunk);
        }
        let chunks = slots
            .into_iter()
            .map(|c| c.unwrap_or_else(|| absent_event_chunk(&plan)))
            .collect();
        merge_chunks(&plan, chunks)
    }

    /// Every surviving event's per-event tag (`EH_TAG`), in global event order —
    /// the tag column aligned 1:1 with [`Self::read_columns`] over the same
    /// `range` and chain filter. Cheap: the tag is read from the event header or
    /// the record directory, never inflating a bank. `threads`: `0` = rayon's
    /// global pool, `1` = sequential, `n` = an `n`-thread pool.
    pub fn event_tags(&self, range: Option<Range<u64>>, threads: usize) -> Result<Vec<u32>> {
        let tasks = self.build_tasks();
        let files = self.files_inner();
        let offsets = self.event_offsets();
        let filter = self.filter_ref();
        let filter_active = filter.is_some_and(Filter::is_active);
        let range = range.as_ref();

        let run_one = |record: &mut Record, read_buf: &mut Vec<u8>, fi: usize, ri: usize| {
            record_event_tags(
                &files[fi],
                ri,
                offsets[fi],
                filter,
                filter_active,
                range,
                record,
                read_buf,
            )
        };

        let per_record: Vec<Vec<u32>> = if threads == 1 {
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
                    .map_err(|e| HipoError::ThreadPool(e.to_string()))?;
                pool.install(run)?
            }
        };

        Ok(per_record.into_iter().flatten().collect())
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

    /// Decompressed payload bytes per record, in [`Self::record_spans`] order.
    /// Reads each record's 56-byte header (small positioned reads, no payload)
    /// — used to size byte-based streaming batches (`iterate("200 MB")`).
    /// Errors on a corrupt/truncated header.
    pub fn record_decompressed_sizes(&self) -> Result<Vec<u64>> {
        // One header read per record, and `iterate(step_size="200 MB")` calls
        // this before it can plan a single batch — so on a many-record chain the
        // sequential version is a visible stall before any data moves. The reads
        // are independent; collecting from an *indexed* parallel iterator keeps
        // record order, which the batching relies on.
        let spans: Vec<(usize, u64)> = self
            .files_inner()
            .iter()
            .enumerate()
            .flat_map(|(fi, inner)| {
                inner
                    .index
                    .records()
                    .iter()
                    .map(move |span| (fi, span.file_offset))
            })
            .collect();
        spans
            .into_par_iter()
            .map(|(fi, off)| {
                let header = self.files_inner()[fi].read_record_header(off)?;
                Ok(header.decompressed_payload_size() as u64)
            })
            .collect::<Result<Vec<u64>>>()
    }
}

/// Concatenate the ordered per-record chunks into one [`ColumnBuffers`] per
/// bank: prefix-sum the row counts into shared `i64` offsets and append each
/// column's values.
/// Concatenate per-record chunks into one buffer set per bank.
///
/// # Errors
///
/// [`HipoError::CorruptRecord`] if the assembled buffers violate the contract
/// `ColumnBuffers` promises its caller: offsets starting at 0 and
/// non-decreasing, and each column holding exactly `total_rows * inner_len`
/// values. A per-column record whose row counts and column payloads disagree
/// produces exactly that, and this used to be a `debug_assert` — so a release
/// build handed the caller buffers whose data length did not match its own
/// offsets, and slicing a row read the wrong values instead of failing.
///
/// Found by `tests/mutation_sweep.rs` on the `Lz4PerColumn` path, and only in a
/// debug build; the release runs it had been checked against could not see it.
fn merge_chunks(plan: &[BankPlan<'_>], chunks: Vec<RecordChunk>) -> Result<Vec<ColumnBuffers>> {
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

    let sound = out.iter().all(|b| {
        b.offsets.first() == Some(&0)
            && b.offsets.windows(2).all(|w| w[0] <= w[1])
            && b.columns
                .iter()
                .all(|c| c.data.len() as i64 == b.total_rows() * i64::from(c.inner_len))
    });
    if !sound {
        return Err(HipoError::CorruptRecord {
            offset: 0,
            reason: "record's row counts and column payloads disagree; the \
                     assembled columns cannot be sliced per event",
        });
    }

    Ok(out)
}
