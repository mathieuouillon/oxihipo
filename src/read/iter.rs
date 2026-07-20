//! `EventIter` — owning iterator over a file's events.
//!
//! Yields `Result<`[`OwnedEvent`]`>`, so the result is a proper
//! `std::iter::Iterator` and plays with `for` loops:
//!
//! ```no_run
//! use oxihipo::Chain;
//!
//! # fn main() -> oxihipo::Result<()> {
//! let chain = Chain::open("rec.hipo")?;
//! for ev in chain.events() {
//!     let ev = ev?;
//!     if let Some(p) = ev.bank("REC::Particle") {
//!         let _ = p.rows();
//!     }
//! }
//! # Ok(()) }
//! ```
//!
//! # Performance
//!
//! Per event: **2 atomic increments** (the payload `Arc` and the dict
//! `Arc`) plus a filter check. Zero heap allocations per event.
//!
//! Per record: one decompression into a `Vec<u8>` that's **recycled**
//! when the previous record's payload is uniquely held (i.e. the user
//! processed and dropped its events promptly). The recovery uses
//! `Arc::try_unwrap`; if the user is collecting events into a `Vec`, the
//! old buffer stays alive and we allocate a fresh one for the next
//! record. Steady-state allocations: zero.
//!
//! # Error model
//!
//! [`Chain::events`](crate::Chain::events) yields `Result<OwnedEvent>`:
//! internal corruption (truncated record, bad LZ4 stream, EOF
//! mid-decompress) is surfaced as an `Err` — after which iteration ends —
//! instead of aborting the process. Propagate it with `?`, or
//! `.map(Result::unwrap)` when the input is a write-once, integrity-checked
//! production file where mid-file corruption would be a bug.

use std::sync::Arc;

use crate::error::Result;
use crate::event::{Event, OwnedEvent};
use crate::read::filter::Filter;
use crate::read::inner::FileInner;
use crate::schema::Dict;
use crate::wire::by_bank::ByBankRecord;
use crate::wire::per_column::PerColumnRecord;
use crate::wire::record::decode_record_into;

pub struct EventIter {
    inner: Arc<FileInner>,
    dict: Arc<Dict>,
    filter: Option<Filter>,
    record_tags: Option<Vec<u64>>,
    /// Currently-loaded record's state. The iterator switches between
    /// "bytes-backed" (every compression except the by-bank format) and
    /// "by-bank-backed" depending on the record's compression tag.
    cur: CurrentRecord,
    /// Recycled across records to avoid per-record allocation in the
    /// Bytes-backed path. The `Bytes` variant of `CurrentRecord` borrows
    /// nothing from here — the table is swapped in and out via
    /// `mem::take` / `mem::swap`.
    offsets_scratch: Vec<u32>,
    /// Next record index in `inner.index.records()`.
    next_record: usize,
    /// Next event index inside the current record.
    next_event: u32,
    /// Recycled buffer recovered when no events from the previous record
    /// remain in flight. Avoids per-record allocations on the steady-
    /// state path (Bytes-backed path only).
    scratch: Vec<u8>,
    /// Recycled buffer holding the current record's raw (still-compressed)
    /// bytes, `pread` from the file each record. Reused across records so a
    /// scan never holds more than one record's bytes resident.
    read_buf: Vec<u8>,
    finished: bool,
}

enum CurrentRecord {
    None,
    Bytes {
        /// Currently-loaded record's decompressed payload, shared with any
        /// `OwnedEvent`s already yielded from it.
        payload: Arc<Vec<u8>>,
        /// Event offsets within the data section. Owned here so the
        /// iterator can recover it back to `offsets_scratch` on
        /// `advance_record`.
        event_offsets: Vec<u32>,
        /// Byte offset of the data section within `payload`.
        data_start: u32,
    },
    ByBank {
        record: Arc<ByBankRecord>,
        /// `event_count` for the by-bank record (cached locally).
        event_count: u32,
    },
    PerColumn {
        record: Arc<PerColumnRecord>,
        event_count: u32,
    },
}

impl CurrentRecord {
    fn event_count(&self) -> u32 {
        match self {
            Self::None => 0,
            Self::Bytes { event_offsets, .. } => event_offsets.len().saturating_sub(1) as u32,
            Self::ByBank { event_count, .. } => *event_count,
            Self::PerColumn { event_count, .. } => *event_count,
        }
    }
}

impl std::fmt::Debug for EventIter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventIter")
            .field("next_record", &self.next_record)
            .field("next_event", &self.next_event)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl EventIter {
    pub(crate) fn new(
        inner: Arc<FileInner>,
        dict: Arc<Dict>,
        filter: Option<Filter>,
        record_tags: Option<Vec<u64>>,
    ) -> Self {
        let mut filter = filter;
        if let Some(f) = filter.as_mut() {
            f.bind(&inner.dict);
        }
        Self {
            inner,
            dict,
            filter,
            record_tags,
            cur: CurrentRecord::None,
            offsets_scratch: Vec::new(),
            next_record: 0,
            next_event: 0,
            scratch: Vec::new(),
            read_buf: Vec::new(),
            finished: false,
        }
    }

    /// Recover a `Vec<u8>` for the next record's decompression and a
    /// recycled `Vec<u32>` for event offsets. Prefers the previous
    /// record's payload (if uniquely held) over the secondary scratch
    /// slot.
    #[inline]
    fn take_bytes_buffers(&mut self) -> (Vec<u8>, Vec<u32>) {
        let prev = std::mem::replace(&mut self.cur, CurrentRecord::None);
        let mut offsets = std::mem::take(&mut self.offsets_scratch);
        match prev {
            CurrentRecord::Bytes {
                payload,
                event_offsets,
                ..
            } => {
                // Always recover the offsets vector (it's owned here).
                offsets = event_offsets;
                offsets.clear();
                let payload_vec = match Arc::try_unwrap(payload) {
                    Ok(v) => v,
                    Err(_arc) => {
                        // Some events still hold the old payload — fall
                        // back to scratch (a fresh Vec only if scratch
                        // is empty).
                        std::mem::take(&mut self.scratch)
                    }
                };
                (payload_vec, offsets)
            }
            _ => (std::mem::take(&mut self.scratch), offsets),
        }
    }

    /// Load the next record that passes the tag filter. `Ok(true)` =
    /// loaded, `Ok(false)` = EOF, `Err` = corruption (truncated span,
    /// unparseable header, bad LZ4 stream). The fallible signature is what
    /// lets [`Self::next_result`] surface corruption as a recoverable
    /// `Result` instead of aborting the process.
    fn advance_record(&mut self) -> Result<bool> {
        loop {
            let records = self.inner.index.records();
            if self.next_record >= records.len() {
                return Ok(false);
            }
            let span = records[self.next_record];
            self.next_record += 1;

            // Record-tag pushdown: peek just the header (a small positioned
            // read) and skip non-matching records without streaming the
            // whole payload.
            if let Some(tags) = &self.record_tags {
                let h = self.inner.read_record_header(span.file_offset)?;
                if !tags.contains(&h.user_word_1) {
                    continue;
                }
            }

            // Stream the whole record into the recycled `read_buf`.
            let mut read_buf = std::mem::take(&mut self.read_buf);
            let header = self
                .inner
                .read_record_into(span.file_offset, &mut read_buf)?;

            if header.compression.is_by_bank() {
                // ByBank: parse the directory eagerly, copying out just the
                // compressed section; bank streams inflate lazily on first
                // `ev.bank(name)`. `read_buf` is free to recycle afterwards.
                // Recover the previous record's Bytes-path scratch first.
                let prev = std::mem::replace(&mut self.cur, CurrentRecord::None);
                if let CurrentRecord::Bytes {
                    payload,
                    event_offsets,
                    ..
                } = prev
                {
                    self.offsets_scratch = event_offsets;
                    self.offsets_scratch.clear();
                    if let Ok(v) = Arc::try_unwrap(payload) {
                        self.scratch = v;
                    }
                }
                let by_bank = ByBankRecord::parse(&read_buf)?;
                self.read_buf = read_buf;
                let event_count = by_bank.event_count();
                self.cur = CurrentRecord::ByBank {
                    record: by_bank,
                    event_count,
                };
                self.next_event = 0;
                return Ok(true);
            }

            if header.compression.is_per_column() {
                // PerColumn: like ByBank, parse the directory eagerly and
                // leave every column stream compressed until it is read.
                let prev = std::mem::replace(&mut self.cur, CurrentRecord::None);
                if let CurrentRecord::Bytes {
                    payload,
                    event_offsets,
                    ..
                } = prev
                {
                    self.offsets_scratch = event_offsets;
                    self.offsets_scratch.clear();
                    if let Ok(v) = Arc::try_unwrap(payload) {
                        self.scratch = v;
                    }
                }
                let per_column = PerColumnRecord::parse(&read_buf)?;
                self.read_buf = read_buf;
                let event_count = per_column.event_count();
                self.cur = CurrentRecord::PerColumn {
                    record: per_column,
                    event_count,
                };
                self.next_event = 0;
                return Ok(true);
            }

            // Bytes-backed path. Reclaim the decompression buffers, then
            // decode the streamed record into them.
            let (mut buf, mut event_offsets) = self.take_bytes_buffers();
            let decoded = decode_record_into(&read_buf, &mut buf, &mut event_offsets)?;
            let data_start = decoded.data_start;
            self.read_buf = read_buf;
            self.cur = CurrentRecord::Bytes {
                payload: Arc::new(buf),
                event_offsets,
                data_start,
            };
            self.next_event = 0;
            return Ok(true);
        }
    }

    /// Fallible iteration core: `Some(Ok(ev))` per event, `Some(Err)` once
    /// on the first corrupt record (after which iteration ends), then
    /// `None`. The [`Iterator`] impl and
    /// [`Chain::events`](crate::Chain::events) both funnel through this.
    pub(crate) fn next_result(&mut self) -> Option<Result<OwnedEvent>> {
        if self.finished {
            return None;
        }
        loop {
            // Refill the current record if exhausted.
            if self.next_event >= self.cur.event_count() {
                match self.advance_record() {
                    Ok(true) => {}
                    Ok(false) => {
                        self.finished = true;
                        return None;
                    }
                    Err(e) => {
                        self.finished = true;
                        return Some(Err(e));
                    }
                }
            }
            let i = self.next_event;
            self.next_event += 1;

            match &self.cur {
                CurrentRecord::None => unreachable!("advance_record Ok(true) ⇒ Some current"),
                CurrentRecord::Bytes {
                    payload,
                    event_offsets,
                    data_start,
                } => {
                    let start = *data_start + event_offsets[i as usize];
                    let end = *data_start + event_offsets[i as usize + 1];
                    if let Some(filter) = &self.filter {
                        let slice = &payload[start as usize..end as usize];
                        if !filter.check(&Event::new(slice)) {
                            continue;
                        }
                    }
                    return Some(Ok(OwnedEvent::slice(
                        Arc::clone(payload),
                        start,
                        end,
                        Arc::clone(&self.dict),
                    )));
                }
                CurrentRecord::ByBank { record, .. } => {
                    if let Some(filter) = &self.filter
                        && !filter.check_by_bank(record, i)
                    {
                        continue;
                    }
                    return Some(Ok(OwnedEvent::by_bank(
                        Arc::clone(record),
                        i,
                        Arc::clone(&self.dict),
                    )));
                }
                CurrentRecord::PerColumn { record, .. } => {
                    if let Some(filter) = &self.filter
                        && !filter.check_per_column(record, i)
                    {
                        continue;
                    }
                    return Some(Ok(OwnedEvent::per_column(
                        Arc::clone(record),
                        i,
                        Arc::clone(&self.dict),
                    )));
                }
            }
        }
    }
}

impl Iterator for EventIter {
    type Item = Result<OwnedEvent>;

    fn next(&mut self) -> Option<Result<OwnedEvent>> {
        self.next_result()
    }
}
