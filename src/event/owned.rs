//! `OwnedEvent` — an event handle that owns / shares the underlying
//! storage (via `Arc`).
//!
//! Two storage backends share the public API:
//!
//! 1. **`Bytes`** — the classic path. Holds an `Arc<Vec<u8>>` of full
//!    decompressed event bytes (EventHeader + concatenated bank
//!    structures). `bank(name)` walks the bytes to find the requested
//!    bank.
//! 2. **`ByBank`** — `Lz4ByBank` path. Holds an `Arc<ByBankRecord>` plus
//!    an event index. `bank(name)` looks up the bank's lazy-decompressed
//!    stream and returns a `Bank<'_>` view directly. Banks the user
//!    never asks for stay compressed.
//!
//! Both yield the same `Bank<'a>` API; user code is unaware of the
//! backend except when calling `bytes()` (which is cheap for `Bytes` and
//! incurs a synthesis copy for `ByBank`).

use std::cell::Cell;
use std::sync::Arc;

use crate::event::bank::Bank;
use crate::event::composite::Composite;
use crate::event::ctx::EventCtx;
use crate::event::event::{Event, StructureIter};
use crate::schema::{Dict, Schema};
use crate::wire::by_bank::ByBankRecord;
use crate::wire::constants::{BANK_STRUCTURE_SIZE, EH_SIZE, EVENT_HEADER_SIZE};

/// An event that owns its byte buffer (via `Arc`) and shares the schema
/// dictionary. Cloning is two atomic increments.
#[derive(Debug, Clone)]
pub struct OwnedEvent {
    inner: Inner,
    dict: Arc<Dict>,
    /// Locator for the last bank resolved by name this event, so a per-row
    /// loop (repeated [`Self::get`]) resolves it once: a hit skips both the
    /// dict name-hash (re-validating via the O(1) by-id table) and the
    /// per-call structure walk (rebuilding the `Bank` from the cached byte
    /// range / bank index). `OwnedEvent` owns its buffer, so it can't cache
    /// the borrowed `Bank` itself — the locator is the `Send`-safe,
    /// allocation-free equivalent.
    bank_cache: Cell<Option<CachedBank>>,
}

/// Single-entry per-event resolution cache for [`OwnedEvent::bank`] /
/// [`OwnedEvent::get`]: the bank locator plus the last column index
/// resolved within it (`col == u16::MAX` ⇒ none yet).
#[derive(Debug, Clone, Copy)]
struct CachedBank {
    group: u16,
    item: u8,
    loc: Loc,
    col: u16,
}

#[derive(Debug, Clone, Copy)]
enum Loc {
    /// Byte range of the bank's data within the event bytes.
    Bytes { off: u32, len: u32 },
    /// Resolved bank index into the `ByBankRecord` (present this event).
    ByBank { bank_idx: u32 },
}

#[derive(Debug, Clone)]
enum Inner {
    Bytes {
        payload: Arc<Vec<u8>>,
        start: u32,
        end: u32,
    },
    ByBank {
        record: Arc<ByBankRecord>,
        event_idx: u32,
        /// Lazy synthetic event-bytes blob — built only if `bytes()` /
        /// `structures()` / similar full-event APIs are called.
        synth: Arc<std::sync::OnceLock<Vec<u8>>>,
    },
}

impl OwnedEvent {
    /// Construct from a stand-alone `Vec<u8>` (e.g. test fixtures, writer
    /// round-trips). Wraps in an `Arc` once; from then on cloning is free.
    pub fn new(bytes: Vec<u8>, dict: Arc<Dict>) -> Self {
        let len = bytes.len() as u32;
        Self {
            inner: Inner::Bytes {
                payload: Arc::new(bytes),
                start: 0,
                end: len,
            },
            dict,
            bank_cache: Cell::new(None),
        }
    }

    /// Construct as a slice into a shared payload buffer. Used by
    /// [`EventIter`](crate::read::EventIter); the same `Arc<Vec<u8>>` is
    /// shared across every event in a single record.
    #[inline]
    pub(crate) fn slice(payload: Arc<Vec<u8>>, start: u32, end: u32, dict: Arc<Dict>) -> Self {
        Self {
            inner: Inner::Bytes {
                payload,
                start,
                end,
            },
            dict,
            bank_cache: Cell::new(None),
        }
    }

    /// Construct from a shared `Lz4ByBank` record + an event index.
    /// Banks decompress lazily on first access.
    #[inline]
    pub(crate) fn by_bank(record: Arc<ByBankRecord>, event_idx: u32, dict: Arc<Dict>) -> Self {
        Self {
            inner: Inner::ByBank {
                record,
                event_idx,
                synth: Arc::new(std::sync::OnceLock::new()),
            },
            dict,
            bank_cache: Cell::new(None),
        }
    }

    /// Return the event's serialised bytes (EventHeader + bank
    /// structures). For `Bytes`-backed events this is zero-copy. For
    /// `ByBank`-backed events the bytes are **synthesised on first
    /// call** — every bank in the event is decompressed, then a
    /// canonical EventHeader+BankStructure blob is built. Subsequent
    /// calls are zero-copy.
    pub fn bytes(&self) -> &[u8] {
        match &self.inner {
            Inner::Bytes {
                payload,
                start,
                end,
            } => &payload[*start as usize..*end as usize],
            Inner::ByBank {
                record,
                event_idx,
                synth,
            } => synth.get_or_init(|| synthesize_event_bytes(record, *event_idx)),
        }
    }

    pub fn dict(&self) -> &Arc<Dict> {
        &self.dict
    }

    pub fn tag(&self) -> u32 {
        match &self.inner {
            Inner::Bytes { .. } => self.as_event().tag(),
            Inner::ByBank {
                record, event_idx, ..
            } => record.event_tag(*event_idx),
        }
    }

    pub fn size(&self) -> u32 {
        match &self.inner {
            Inner::Bytes { start, end, .. } => end - start,
            Inner::ByBank { .. } => self.bytes().len() as u32,
        }
    }

    /// Borrow as an `EventCtx<'_>`. For `ByBank` events this is **O(1)**
    /// — the returned `EventCtx` carries the same lazy bank cache and
    /// `ctx.bank(name)` will only decompress the requested bank.
    pub fn ctx(&self) -> EventCtx<'_> {
        match &self.inner {
            Inner::Bytes { .. } => EventCtx::new(self.as_event(), &self.dict),
            Inner::ByBank {
                record, event_idx, ..
            } => EventCtx::new_by_bank(record, *event_idx, &self.dict),
        }
    }

    pub fn bank(&self, name: &str) -> Option<Bank<'_>> {
        // Fast path: same bank as the last call this event? Re-validate the
        // cached id by name (cheap O(1) by-id lookup + a string compare),
        // then rebuild the `Bank` from the cached locator — no name-hash, no
        // structure walk.
        if let Some(c) = self.bank_cache.get()
            && let Some(schema) = self.dict.get_by_id(c.group, c.item)
            && schema.name() == name
        {
            return self.rebuild(schema, c.loc);
        }
        let schema = self.dict.get(name)?;
        self.resolve_and_cache(schema)
    }

    /// Resolve a bank from scratch and remember its locator for the next
    /// same-name call this event.
    fn resolve_and_cache<'a>(&'a self, schema: &'a Schema) -> Option<Bank<'a>> {
        let (g, i) = (schema.group(), schema.item());
        match &self.inner {
            Inner::Bytes {
                payload,
                start,
                end,
            } => {
                let ev_bytes = &payload[*start as usize..*end as usize];
                let (_, data) = Event::new(ev_bytes).find(g, i)?;
                let off = (data.as_ptr().addr() - ev_bytes.as_ptr().addr()) as u32;
                let bank = Bank::new(schema, data).ok()?;
                self.bank_cache.set(Some(CachedBank {
                    group: g,
                    item: i,
                    loc: Loc::Bytes {
                        off,
                        len: data.len() as u32,
                    },
                    col: u16::MAX,
                }));
                Some(bank)
            }
            Inner::ByBank {
                record, event_idx, ..
            } => {
                let bank_idx = record.bank_index(g, i)?;
                if !record.has(*event_idx, bank_idx) {
                    return None;
                }
                let stream = record.bank_stream(bank_idx).ok()?;
                let range = record.bank_byte_range(*event_idx, bank_idx);
                let bank = Bank::new(schema, &stream[range]).ok()?;
                self.bank_cache.set(Some(CachedBank {
                    group: g,
                    item: i,
                    loc: Loc::ByBank { bank_idx },
                    col: u16::MAX,
                }));
                Some(bank)
            }
        }
    }

    /// Rebuild a `Bank` from a cached locator (a hit). The bank was present
    /// when cached and the event is immutable, so no presence re-check is
    /// needed.
    fn rebuild<'a>(&'a self, schema: &'a Schema, loc: Loc) -> Option<Bank<'a>> {
        match (&self.inner, loc) {
            (
                Inner::Bytes {
                    payload,
                    start,
                    end,
                },
                Loc::Bytes { off, len },
            ) => {
                let ev_bytes = &payload[*start as usize..*end as usize];
                let s = off as usize;
                let e = s.checked_add(len as usize)?;
                if e > ev_bytes.len() {
                    return None;
                }
                Bank::new(schema, &ev_bytes[s..e]).ok()
            }
            (
                Inner::ByBank {
                    record, event_idx, ..
                },
                Loc::ByBank { bank_idx },
            ) => {
                let stream = record.bank_stream(bank_idx).ok()?;
                let range = record.bank_byte_range(*event_idx, bank_idx);
                Bank::new(schema, &stream[range]).ok()
            }
            // Inner/Loc kind mismatch can't happen (one event, one backend).
            _ => None,
        }
    }

    /// Decode the bank for an already-resolved schema reference. Internal:
    /// backs [`Self::bank`] and the typed-row accessors.
    pub(crate) fn bank_for<'a>(&'a self, schema: &'a Schema) -> Option<Bank<'a>> {
        match &self.inner {
            Inner::Bytes { .. } => self.ctx().bank_for(schema),
            Inner::ByBank {
                record, event_idx, ..
            } => {
                let bank_idx = record.bank_index(schema.group(), schema.item())?;
                if !record.has(*event_idx, bank_idx) {
                    return None;
                }
                let stream = record.bank_stream(bank_idx).ok()?;
                let range = record.bank_byte_range(*event_idx, bank_idx);
                Bank::new(schema, &stream[range]).ok()
            }
        }
    }

    /// Read one cell of bank `bank`, column `col`, at `row`. Infallible;
    /// see [`EventCtx::get`](crate::event::EventCtx::get).
    #[inline]
    pub fn get<T: crate::schema::BankColumnType + Default>(
        &self,
        bank: &str,
        col: &str,
        row: u32,
    ) -> T {
        let Some(b) = self.bank(bank) else {
            return T::default();
        };
        let entries = b.schema().entries();
        // Column cache: reuse the last column index resolved in this bank.
        let ci = match self.bank_cache.get() {
            Some(c)
                if c.col != u16::MAX
                    && c.group == b.schema().group()
                    && c.item == b.schema().item()
                    && entries
                        .get(c.col as usize)
                        .is_some_and(|e| e.name.as_str() == col) =>
            {
                c.col
            }
            _ => {
                let Some(ci) = b.schema().column_index(col) else {
                    return T::default();
                };
                let ci = ci as u16;
                if let Some(mut c) = self.bank_cache.get() {
                    c.col = ci;
                    self.bank_cache.set(Some(c));
                }
                ci
            }
        };
        let entry = &entries[ci as usize];
        if entry.ty != T::DATA_TYPE || entry.length != T::LENGTH || row >= b.rows() {
            return T::default();
        }
        b.read_handle_or_default(crate::schema::ColumnHandle::<T>::from_index(ci), row)
    }

    /// Borrow column `col` of bank `bank` as `Cow<'_, [T]>` (tied to
    /// `&self`). See [`EventCtx::col`](crate::event::EventCtx::col).
    pub fn col<T: crate::schema::BankColumnType>(
        &self,
        bank: &str,
        col: &str,
    ) -> crate::Result<std::borrow::Cow<'_, [T]>> {
        match self.bank(bank) {
            Some(b) => b.col::<T>(col),
            None => Ok(std::borrow::Cow::Borrowed(&[])),
        }
    }

    pub fn has(&self, name: &str) -> bool {
        let Some(schema) = self.dict.get(name) else {
            return false;
        };
        match &self.inner {
            Inner::Bytes { .. } => self.as_event().has(schema.group(), schema.item()),
            Inner::ByBank {
                record, event_idx, ..
            } => {
                let Some(bank_idx) = record.bank_index(schema.group(), schema.item()) else {
                    return false;
                };
                record.has(*event_idx, bank_idx)
            }
        }
    }

    /// Iterate structure headers + payloads. For `ByBank` events this
    /// triggers full synthesis (decompresses every bank in the event).
    pub fn structures(&self) -> StructureIter<'_> {
        self.as_event().iter_structures()
    }

    /// Decode a composite structure by name.
    pub fn composite(&self, name: &str) -> Option<Composite<'_>> {
        self.ctx().composite(name)
    }

    /// Internal: handle-cached [`BankView`](crate::event::BankView) for
    /// bank `T::NAME`. Backs [`Self::rows`] and the `rows_for_*` accessors.
    pub(crate) fn bank_view<T: crate::event::BankRow>(
        &self,
    ) -> Option<crate::event::BankView<'_, T>> {
        let schema = self.dict.get_by_id(T::GROUP, T::ITEM)?;
        let bank = self.bank_for(schema)?;
        Some(crate::event::BankView::new(bank))
    }

    /// Iterate every row of bank `T::NAME` decoded as `T`. See
    /// [`EventCtx::rows`](crate::event::EventCtx::rows).
    pub fn rows<T: crate::event::BankRow>(&self) -> crate::event::ctx::RowsIter<'_, T> {
        crate::event::ctx::RowsIter::new(self.bank_view::<T>())
    }

    /// Iterate the rows of bank `T::NAME` whose `pindex` column
    /// equals `pindex`. See
    /// [`EventCtx::rows_for_pindex`](crate::event::EventCtx::rows_for_pindex).
    pub fn rows_for_pindex<T: crate::event::BankRow>(
        &self,
        pindex: i16,
    ) -> crate::event::ctx::RowsForKeyIter<'_, T> {
        crate::event::ctx::RowsForKeyIter::new_pindex(self.bank_view::<T>(), pindex)
    }

    /// Iterate the rows of bank `T::NAME` whose `index` column equals
    /// `key`. See
    /// [`EventCtx::rows_for_index`](crate::event::EventCtx::rows_for_index).
    pub fn rows_for_index<T: crate::event::BankRow>(
        &self,
        key: i16,
    ) -> crate::event::ctx::RowsForKeyIter<'_, T> {
        crate::event::ctx::RowsForKeyIter::new_index(self.bank_view::<T>(), key)
    }

    #[inline]
    fn as_event(&self) -> Event<'_> {
        Event::new(self.bytes())
    }
}

/// Build a synthetic EventHeader + BankStructure blob for one event of
/// an `Lz4ByBank` record. Decompresses every bank that the event has —
/// expensive, used only when callers explicitly ask for raw bytes
/// (e.g. `OwnedEvent::bytes()`, `ev.structures()`, recook flows).
fn synthesize_event_bytes(record: &ByBankRecord, event_idx: u32) -> Vec<u8> {
    // First pass: figure out total size so we allocate once.
    let mut total = EVENT_HEADER_SIZE;
    for b in 0..record.num_banks() as u32 {
        if !record.has(event_idx, b) {
            continue;
        }
        total += BANK_STRUCTURE_SIZE + record.bank_size(event_idx, b) as usize;
    }
    let mut out = vec![0u8; EVENT_HEADER_SIZE];
    out[0..4].copy_from_slice(b"EVNT");
    // EH_TAG = event_tag, EH_RESERVED = 0; size patched at end.
    crate::wire::bytes::write_u32_le(
        &mut out,
        crate::wire::constants::EH_TAG,
        record.event_tag(event_idx),
    );

    out.reserve(total - EVENT_HEADER_SIZE);
    for b in 0..record.num_banks() as u32 {
        if !record.has(event_idx, b) {
            continue;
        }
        let (group, item, data_type) = record.descriptor(b);
        let size = record.bank_size(event_idx, b);

        // BankStructure header — 8 bytes: u16 group, u8 item, u8 type, u32 length.
        out.extend_from_slice(&group.to_le_bytes());
        out.push(item);
        out.push(data_type);
        out.extend_from_slice(&size.to_le_bytes());

        // Bank data — decompress this bank's stream (lazy / cached) and
        // copy our event's slice.
        if size > 0 {
            // `bank_stream` returns Result; for synthesis we panic on
            // failure because the only failures are corruption that
            // would already have triggered at iterator construction.
            let stream = record
                .bank_stream(b)
                .expect("Lz4ByBank: bank stream decompression failed during synthesis");
            let range = record.bank_byte_range(event_idx, b);
            out.extend_from_slice(&stream[range]);
        }
    }
    let total = out.len() as u32;
    crate::wire::bytes::write_u32_le(&mut out, EH_SIZE, total);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::build::EventBuilder;
    use crate::schema::{DataType, Schema};
    use crate::wire::bytes::write_u32_le;
    use crate::wire::constants::BANK_STRUCTURE_SIZE;

    fn build_event_bytes(schema: &Schema, pid: i32) -> Vec<u8> {
        // Structure: 8-byte header + column-major data (1 row, 1 col i32)
        let mut struct_bytes = vec![0u8; BANK_STRUCTURE_SIZE + 4];
        struct_bytes[0..2].copy_from_slice(&schema.group().to_le_bytes());
        struct_bytes[2] = schema.item();
        struct_bytes[3] = 11;
        write_u32_le(&mut struct_bytes, 4, 4);
        struct_bytes[BANK_STRUCTURE_SIZE..].copy_from_slice(&pid.to_le_bytes());

        let mut eb = EventBuilder::new();
        eb.add_bank_bytes(&struct_bytes);
        eb.finish()
    }

    #[test]
    fn owned_event_round_trip() {
        let mut dict = Dict::new();
        let schema = Schema::from_columns("X", 1, 1, [("pid".into(), DataType::Int, 1)]);
        dict.add(schema.clone());
        let bytes = build_event_bytes(&schema, 42);
        let owned = OwnedEvent::new(bytes, Arc::new(dict));

        let bank = owned.bank("X").unwrap();
        assert_eq!(&*bank.col::<i32>("pid").unwrap(), &[42]);
        assert!(owned.has("X"));
    }

    #[test]
    fn owned_event_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<OwnedEvent>();
    }

    #[test]
    fn cross_thread_round_trip() {
        let mut dict = Dict::new();
        let schema = Schema::from_columns("X", 1, 1, [("pid".into(), DataType::Int, 1)]);
        dict.add(schema.clone());
        let bytes = build_event_bytes(&schema, 99);
        let owned = OwnedEvent::new(bytes, Arc::new(dict));

        let h =
            std::thread::spawn(move || owned.bank("X").map(|b| b.col::<i32>("pid").map(|c| c[0])));
        let result = h.join().unwrap().unwrap().unwrap();
        assert_eq!(result, 99);
    }

    #[test]
    fn slice_constructor_shares_buffer() {
        let mut dict = Dict::new();
        let schema = Schema::from_columns("X", 1, 1, [("pid".into(), DataType::Int, 1)]);
        dict.add(schema.clone());
        let bytes = build_event_bytes(&schema, 7);
        let len = bytes.len() as u32;
        let payload = Arc::new(bytes);
        let dict_arc = Arc::new(dict);

        let a = OwnedEvent::slice(Arc::clone(&payload), 0, len, Arc::clone(&dict_arc));
        let b = OwnedEvent::slice(Arc::clone(&payload), 0, len, Arc::clone(&dict_arc));
        assert_eq!(
            a.bank("X").unwrap().col::<i32>("pid").unwrap(),
            b.bank("X").unwrap().col::<i32>("pid").unwrap()
        );
        // Three Arc holders: a, b, and `payload` itself.
        assert_eq!(Arc::strong_count(&payload), 3);
    }
}
