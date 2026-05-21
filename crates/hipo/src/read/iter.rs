//! `EventIter` — owning iterator over a file's events.
//!
//! Yields [`OwnedEvent`]s, so the result is a proper
//! `std::iter::Iterator` and plays with `for` loops:
//!
//! ```no_run
//! use hipo::Chain;
//!
//! # fn main() -> hipo::Result<()> {
//! let chain = Chain::open("rec.hipo")?;
//! for ev in chain.events() {
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
//! Internal corruption (truncated record, bad LZ4 stream, EOF mid-
//! decompress) panics with a clear message. HIPO files in production are
//! write-once and integrity-checked at `File::open`; mid-file corruption
//! is treated as a bug, not a recoverable error.

use std::sync::Arc;

use crate::event::{Event, OwnedEvent};
use crate::read::filter::Filter;
use crate::read::inner::FileInner;
use crate::schema::Dict;
use crate::wire::constants::RECORD_HEADER_SIZE;
use crate::wire::record::decode_record_into;
use crate::wire::record_header::RecordHeader;

pub struct EventIter {
    inner: Arc<FileInner>,
    dict: Arc<Dict>,
    filter: Option<Filter>,
    record_tags: Option<Vec<u64>>,
    /// Currently-loaded record's decompressed payload, shared with any
    /// `OwnedEvent`s already yielded from it.
    cur_payload: Option<Arc<Vec<u8>>>,
    /// Event offsets within the data section.
    event_offsets: Vec<u32>,
    /// Byte offset of the data section within `cur_payload`.
    data_start: u32,
    /// Next record index in `inner.index.records()`.
    next_record: usize,
    /// Next event index inside `event_offsets`.
    next_event: u32,
    /// Recycled buffer recovered when no events from the previous record
    /// remain in flight. Avoids per-record allocations on the steady-
    /// state path.
    scratch: Vec<u8>,
    finished: bool,
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
            cur_payload: None,
            event_offsets: Vec::new(),
            data_start: 0,
            next_record: 0,
            next_event: 0,
            scratch: Vec::new(),
            finished: false,
        }
    }

    /// Recover a `Vec<u8>` for the next record's decompression. Prefers
    /// the previous record's payload (if uniquely held) over the
    /// secondary scratch slot.
    #[inline]
    fn take_buffer(&mut self) -> Vec<u8> {
        if let Some(prev) = self.cur_payload.take() {
            match Arc::try_unwrap(prev) {
                Ok(vec) => return vec,
                Err(arc) => {
                    // Some events still hold the old payload. Drop our
                    // ref; the buffer will be freed when the last event
                    // goes out of scope.
                    drop(arc);
                }
            }
        }
        std::mem::take(&mut self.scratch)
    }

    /// Load the next record that passes the tag filter. Returns false at
    /// EOF.
    fn advance_record(&mut self) -> bool {
        loop {
            let records = self.inner.index.records();
            if self.next_record >= records.len() {
                return false;
            }
            let span = records[self.next_record];
            self.next_record += 1;
            let mmap_len = self.inner.mmap.len();

            if let Some(tags) = &self.record_tags {
                let hdr_off = span.file_offset as usize;
                if hdr_off + RECORD_HEADER_SIZE > mmap_len {
                    panic!("record header past EOF at offset {:#x}", span.file_offset);
                }
                let matches = {
                    let h = RecordHeader::parse(&self.inner.mmap[hdr_off..])
                        .expect("record header parse on well-formed file");
                    tags.contains(&h.user_word_1)
                };
                if !matches {
                    continue;
                }
            }

            let lo = span.file_offset as usize;
            let hi = lo + span.record_length as usize;
            if hi > mmap_len {
                panic!("record extends past EOF at offset {:#x}", span.file_offset);
            }

            // Re-borrow patterns: take the buffer first (mut self), then
            // pass an immutable slice of the mmap into the decoder. The
            // `Arc<FileInner>` keeps the mmap alive throughout.
            let mut buf = self.take_buffer();
            let decoded = {
                let src = &self.inner.mmap[lo..hi];
                decode_record_into(src, &mut buf, &mut self.event_offsets)
                    .expect("decompress well-formed record")
            };
            self.data_start = decoded.data_start;
            self.cur_payload = Some(Arc::new(buf));
            self.next_event = 0;
            return true;
        }
    }
}

impl Iterator for EventIter {
    type Item = OwnedEvent;

    fn next(&mut self) -> Option<OwnedEvent> {
        if self.finished {
            return None;
        }
        loop {
            // Refill the current record if exhausted.
            if self.next_event + 1 >= self.event_offsets.len() as u32 && !self.advance_record() {
                self.finished = true;
                return None;
            }
            let i = self.next_event as usize;
            self.next_event += 1;
            let payload = self.cur_payload.as_ref().expect("record loaded");
            let start = self.data_start + self.event_offsets[i];
            let end = self.data_start + self.event_offsets[i + 1];

            // Filter check on the raw event bytes (cheap; no Bank decode).
            if let Some(filter) = &self.filter {
                let slice = &payload[start as usize..end as usize];
                if !filter.check(&Event::new(slice)) {
                    continue;
                }
            }

            return Some(OwnedEvent::slice(
                Arc::clone(payload),
                start,
                end,
                Arc::clone(&self.dict),
            ));
        }
    }
}
