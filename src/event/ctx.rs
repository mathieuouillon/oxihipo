//! `EventCtx<'a>` — borrowed event view paired with the schema dictionary.
//!
//! This is what scans hand to user closures. It exposes the
//! "find a bank by name" ergonomics: the user no longer needs to thread a
//! separate `&Schema` clone through their code.
//!
//! Two backends share the same API:
//!
//! - **Bytes** — classic path, wrapping a raw `Event<'a>` byte slice.
//! - **ByBank** — by-bank path, borrowing a shared `ByBankRecord`
//!   plus an event index. `bank(name)` decompresses *only* the
//!   requested bank's stream (lazily, cached on the record).
//!
//! Both backends are cheap to copy; the `EventCtx` wrapper adds a small
//! per-event cache of the last bank resolved by name (see [`EventCtx::bank`]),
//! so it is `Clone` but not `Copy`.

use std::cell::Cell;
use std::sync::Arc;

use crate::event::bank::Bank;
use crate::event::composite::Composite;
use crate::event::event::Event;
use crate::event::owned::OwnedEvent;
use crate::schema::{Dict, Schema};
use crate::wire::by_bank::ByBankRecord;
use crate::wire::per_column::PerColumnRecord;

/// Borrowed event view + reference to the file's schema dictionary.
#[derive(Debug, Clone)]
pub struct EventCtx<'a> {
    backend: Backend<'a>,
    dict: &'a Dict,
    /// Per-event resolution cache: the last bank resolved by name, plus the
    /// last column index resolved within it (`u16::MAX` = none). A per-row
    /// loop over one bank/column (repeated [`Self::get`]) resolves both once
    /// and then reads through a handle — no per-cell name lookup at all.
    cache: Cell<Option<(Bank<'a>, u16)>>,
}

#[derive(Debug, Copy, Clone)]
enum Backend<'a> {
    Bytes(Event<'a>),
    ByBank {
        record: &'a Arc<ByBankRecord>,
        event_idx: u32,
    },
    PerColumn {
        record: &'a Arc<PerColumnRecord>,
        event_idx: u32,
    },
}

impl<'a> EventCtx<'a> {
    /// Construct over raw event bytes (the classic path).
    pub fn new(event: Event<'a>, dict: &'a Dict) -> Self {
        Self {
            backend: Backend::Bytes(event),
            dict,
            cache: Cell::new(None),
        }
    }

    /// Construct over a by-bank record + an event index. Bank
    /// streams stay compressed until `bank(name)` requests one.
    ///
    /// Takes `&'a Arc<ByBankRecord>` (not `&ByBankRecord`) so
    /// `to_owned_with` can clone the Arc safely without unsafe code.
    pub(crate) fn new_by_bank(
        record: &'a Arc<ByBankRecord>,
        event_idx: u32,
        dict: &'a Dict,
    ) -> Self {
        Self {
            backend: Backend::ByBank { record, event_idx },
            dict,
            cache: Cell::new(None),
        }
    }

    /// Construct over an `Lz4PerColumn` record + an event index. Column
    /// streams stay compressed until a column is actually read.
    pub(crate) fn new_per_column(
        record: &'a Arc<PerColumnRecord>,
        event_idx: u32,
        dict: &'a Dict,
    ) -> Self {
        Self {
            backend: Backend::PerColumn { record, event_idx },
            dict,
            cache: Cell::new(None),
        }
    }

    /// The underlying borrowed event bytes.
    ///
    /// Returns an empty `Event<'a>` for by-bank backends — those
    /// don't carry raw event bytes. Callers that need a real byte view
    /// should up-convert through [`OwnedEvent`] (which synthesises bytes
    /// lazily by decompressing every bank).
    pub fn event(&self) -> Event<'a> {
        match self.backend {
            Backend::Bytes(e) => e,
            Backend::ByBank { .. } | Backend::PerColumn { .. } => Event::new(&[]),
        }
    }

    /// The file's schema dictionary.
    pub fn dict(&self) -> &'a Dict {
        self.dict
    }

    /// Raw event bytes. Empty for by-bank backends — see [`Self::event`].
    pub fn raw(&self) -> &'a [u8] {
        match self.backend {
            Backend::Bytes(e) => e.raw(),
            Backend::ByBank { .. } | Backend::PerColumn { .. } => &[],
        }
    }

    pub fn tag(&self) -> u32 {
        match self.backend {
            Backend::Bytes(e) => e.tag(),
            Backend::ByBank { record, event_idx } => record.event_tag(event_idx),
            Backend::PerColumn { record, event_idx } => record.event_tag(event_idx),
        }
    }

    pub fn size(&self) -> u32 {
        match self.backend {
            Backend::Bytes(e) => e.size(),
            Backend::ByBank { record, event_idx } => {
                // Synthetic size: EventHeader + bank structures present.
                let mut total = crate::wire::constants::EVENT_HEADER_SIZE as u32;
                for b in 0..record.num_banks() as u32 {
                    if record.has(event_idx, b) {
                        total += crate::wire::constants::BANK_STRUCTURE_SIZE as u32
                            + record.bank_size(event_idx, b);
                    }
                }
                total
            }
            Backend::PerColumn { record, event_idx } => {
                let mut total = crate::wire::constants::EVENT_HEADER_SIZE as u32;
                for b in 0..record.num_banks() as u32 {
                    if record.has(event_idx, b) {
                        total += crate::wire::constants::BANK_STRUCTURE_SIZE as u32
                            + record.bank_size(event_idx, b);
                    }
                }
                total
            }
        }
    }

    /// Decode bank `name`. `None` if the schema isn't in the dict, the
    /// structure isn't in the event, or the bank's data is mis-sized.
    ///
    /// On by-bank records, only the requested bank's LZ4 stream is
    /// decompressed — other banks in the same record remain compressed.
    pub fn bank(&self, name: &str) -> Option<Bank<'a>> {
        // Per-event cache: a per-row loop over one bank resolves it once,
        // not on every call. The cached bank borrows `'a` (not `&self`), so
        // it survives across calls.
        if let Some((b, _)) = self.cache.get()
            && b.schema().name() == name
        {
            return Some(b);
        }
        let schema = self.dict.get(name)?;
        let bank = self.bank_for(schema)?;
        self.cache.set(Some((bank, u16::MAX)));
        Some(bank)
    }

    /// Decode the bank for an already-resolved schema reference. Internal:
    /// backs [`Self::bank`] and the typed-row accessors.
    pub(crate) fn bank_for(&self, schema: &'a Schema) -> Option<Bank<'a>> {
        match self.backend {
            Backend::Bytes(e) => {
                let (_, data) = e.find(schema.group(), schema.item())?;
                Bank::new(schema, data).ok()
            }
            Backend::ByBank { record, event_idx } => {
                let bank_idx = record.bank_index(schema.group(), schema.item())?;
                if !record.has(event_idx, bank_idx) {
                    return None;
                }
                let stream = record.bank_stream(bank_idx).ok()?;
                let range = record.bank_byte_range(event_idx, bank_idx);
                Bank::new(schema, &stream[range]).ok()
            }
            Backend::PerColumn { record, event_idx } => {
                let bank_idx = record.bank_index(schema.group(), schema.item())?;
                if !record.has(event_idx, bank_idx) {
                    return None;
                }
                if record.is_opaque(bank_idx) {
                    // Opaque bank: one whole-bank stream, served contiguously.
                    let stream = record.column_stream(bank_idx, 0).ok()?;
                    let range = record.bank_byte_range(event_idx, bank_idx);
                    Bank::new(schema, &stream[range]).ok()
                } else {
                    Some(Bank::new_per_column(schema, record, bank_idx, event_idx))
                }
            }
        }
    }

    /// Read one cell of bank `bank`, column `col`, at `row` — collapsing
    /// `ev.bank(bank)?.get(col, row)` into a single call.
    ///
    /// Infallible: returns `T::default()` when the bank is absent, the
    /// column is missing, the wire type doesn't match `T`, or `row` is out
    /// of range.
    ///
    /// **Performance:** the resolved bank *and* column are cached per event,
    /// so a per-row loop over one bank/column reads through a pre-resolved
    /// handle — no name lookup per cell. Repeated same-bank/column `get` is
    /// as fast as a hoisted `bank.get` (often faster, since `bank.get` looks
    /// the column up by name on every call). Switching banks or columns
    /// re-resolves; the typed [`bank_row!`](crate::bank_row) + [`Self::rows`]
    /// path is the equivalent for reading several columns per row.
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
        // Column cache: reuse the last column index resolved in this bank,
        // so a per-row loop reads through a handle with no name lookup.
        let ci = match self.cache.get() {
            Some((cb, cci))
                if cci != u16::MAX
                    && cb.schema().name() == bank
                    && entries
                        .get(cci as usize)
                        .is_some_and(|e| e.name.as_str() == col) =>
            {
                cci
            }
            _ => {
                let Some(ci) = b.schema().column_index(col) else {
                    return T::default();
                };
                let ci = ci as u16;
                self.cache.set(Some((b, ci)));
                ci
            }
        };
        let entry = &entries[ci as usize];
        if entry.ty != T::DATA_TYPE || entry.length != T::LENGTH || row >= b.rows() {
            return T::default();
        }
        b.read_handle_or_default(crate::schema::ColumnHandle::<T>::from_index(ci), row)
    }

    /// Borrow column `col` of bank `bank` as `Cow<'a, [T]>` in one call.
    ///
    /// Strict like [`Bank::col`](crate::event::Bank::col): errors on a
    /// type/length mismatch. A missing bank yields an empty borrowed slice
    /// (`Ok`), so callers can iterate uniformly without distinguishing
    /// "absent" from "present but empty" — use [`Self::has`] if that
    /// distinction matters.
    pub fn col<T: crate::schema::BankColumnType>(
        &self,
        bank: &str,
        col: &str,
    ) -> crate::Result<std::borrow::Cow<'a, [T]>> {
        match self.bank(bank) {
            Some(b) => b.col::<T>(col),
            None => Ok(std::borrow::Cow::Borrowed(&[])),
        }
    }

    /// True if the event contains a structure for the named schema.
    pub fn has(&self, name: &str) -> bool {
        let Some(schema) = self.dict.get(name) else {
            return false;
        };
        match self.backend {
            Backend::Bytes(e) => e.has(schema.group(), schema.item()),
            Backend::ByBank { record, event_idx } => {
                let Some(bank_idx) = record.bank_index(schema.group(), schema.item()) else {
                    return false;
                };
                record.has(event_idx, bank_idx)
            }
            Backend::PerColumn { record, event_idx } => {
                let Some(bank_idx) = record.bank_index(schema.group(), schema.item()) else {
                    return false;
                };
                record.has(event_idx, bank_idx)
            }
        }
    }

    /// Iterate structure headers + payloads — for tools like `dump` and
    /// `stats`, and for any "touch every bank" pass.
    ///
    /// Works for the `Bytes` and by-bank backends: by-bank events
    /// gather their banks straight from the per-bank decompressed (lazily
    /// cached) streams with no event-blob synthesis. **Empty for
    /// `Lz4PerColumn`** — those carry no contiguous bytes here; use
    /// [`OwnedEvent::structures`], which reassembles each bank from its
    /// columns lazily.
    pub fn structures(&self) -> crate::event::event::StructureIter<'a> {
        match self.backend {
            Backend::Bytes(e) => e.iter_structures(),
            Backend::ByBank { record, event_idx } => {
                crate::event::event::StructureIter::new_by_bank(record, event_idx)
            }
            Backend::PerColumn { .. } => Event::new(&[]).iter_structures(),
        }
    }

    /// Decode a composite structure by name (looks up the wire IDs via the
    /// dict, then re-parses the inline format string).
    ///
    /// **Returns `None` for by-bank backends** — composite reconstruction
    /// requires the original structure bytes. Up-convert via `OwnedEvent`
    /// if you need composites on ByBank records.
    pub fn composite(&self, name: &str) -> Option<Composite<'a>> {
        let schema = self.dict.get(name)?;
        self.composite_by_id(schema.group(), schema.item())
    }

    pub(crate) fn composite_by_id(&self, group: u16, item: u8) -> Option<Composite<'a>> {
        let event = match self.backend {
            Backend::Bytes(e) => e,
            Backend::ByBank { .. } | Backend::PerColumn { .. } => return None,
        };
        for (pos, hdr, data) in event.iter_structures_with_offset() {
            if hdr.group == group && hdr.item == item && hdr.header_size > 0 {
                let end = pos + crate::wire::constants::BANK_STRUCTURE_SIZE + data.len();
                let bytes = &event.raw()[pos..end];
                return Composite::from_structure(bytes).ok();
            }
        }
        None
    }

    /// Detach into an [`OwnedEvent`] (copies event bytes, shares the dict
    /// via `Arc`). Used to send events across thread boundaries or store
    /// them in collections.
    ///
    /// The dict must already be wrapped in an `Arc` — provided so callers
    /// don't have to clone the entire dict per event.
    ///
    /// For by-bank backends this is `O(1)` — the resulting
    /// `OwnedEvent` shares the same lazy bank cache via `Arc`.
    pub fn to_owned_with(&self, dict: Arc<Dict>) -> OwnedEvent {
        match self.backend {
            Backend::Bytes(e) => OwnedEvent::new(e.raw().to_vec(), dict),
            Backend::ByBank { record, event_idx } => {
                OwnedEvent::by_bank(Arc::clone(record), event_idx, dict)
            }
            Backend::PerColumn { record, event_idx } => {
                OwnedEvent::per_column(Arc::clone(record), event_idx, dict)
            }
        }
    }

    // ---- Typed bank rows ------------------------------------------------

    /// Internal: handle-cached [`BankView`](crate::event::BankView) for
    /// bank `T::NAME`, or `None` if the event lacks the bank. Backs
    /// [`Self::rows`] / [`Self::rows_for_pindex`] / [`Self::rows_for_index`],
    /// which resolve typed column handles once and reuse them across the
    /// rows of a single call.
    pub(crate) fn bank_view<T: crate::event::BankRow>(
        &self,
    ) -> Option<crate::event::BankView<'a, T>> {
        let bank = self.bank_by_id_raw(T::GROUP, T::ITEM)?;
        Some(crate::event::BankView::new(bank))
    }

    /// Iterate every row of bank `T::NAME` decoded as `T`. Empty when
    /// the event lacks the bank.
    ///
    /// Resolves the bank's typed column handles once and reuses them
    /// across the rows of this call, so each row read is pointer
    /// arithmetic with no per-cell name lookup.
    pub fn rows<T: crate::event::BankRow>(&self) -> RowsIter<'a, T> {
        RowsIter::new(self.bank_view::<T>())
    }

    /// Iterate the rows of bank `T::NAME` whose `pindex` column
    /// equals `pindex`. Empty if the bank doesn't exist or lacks a
    /// `pindex` column.
    ///
    /// Builds an inverted `pindex → rows` index on first call per
    /// event; subsequent queries are O(matches).
    pub fn rows_for_pindex<T: crate::event::BankRow>(&self, pindex: i16) -> RowsForKeyIter<'a, T> {
        RowsForKeyIter::new(self.bank_view::<T>(), Key::Pindex(pindex))
    }

    /// Iterate the rows of bank `T::NAME` whose `index` column equals
    /// `key`. Symmetric to [`Self::rows_for_pindex`].
    pub fn rows_for_index<T: crate::event::BankRow>(&self, key: i16) -> RowsForKeyIter<'a, T> {
        RowsForKeyIter::new(self.bank_view::<T>(), Key::Index(key))
    }

    /// Resolve a bank by `(group, item)` via the dict (the schema must be
    /// present) and return the [`Bank`](crate::event::Bank) view from the
    /// backend. Internal helper behind the typed-row accessors.
    fn bank_by_id_raw(&self, group: u16, item: u8) -> Option<crate::event::Bank<'a>> {
        // The dict-driven path is the path the rest of the API takes;
        // we just go through it.
        let schema = self.dict.get_by_id(group, item)?;
        self.bank_for(schema)
    }
}

// ---- Owning iterators returned by EventCtx::rows / rows_for_* ------

/// Owning iterator: holds the [`BankView<T>`] so its cached handles
/// survive every row read. Yielded by [`EventCtx::rows`].
#[derive(Debug)]
pub struct RowsIter<'a, T: crate::event::BankRow> {
    view: Option<crate::event::BankView<'a, T>>,
    next: u32,
    end: u32,
}

impl<'a, T: crate::event::BankRow> RowsIter<'a, T> {
    pub(crate) fn new(view: Option<crate::event::BankView<'a, T>>) -> Self {
        let end = view.as_ref().map_or(0, |v| v.rows());
        Self { view, next: 0, end }
    }
}

impl<'a, T: crate::event::BankRow> Iterator for RowsIter<'a, T> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        let v = self.view.as_ref()?;
        if self.next >= self.end {
            return None;
        }
        let row = v.row(self.next);
        self.next += 1;
        Some(row)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let rem = (self.end - self.next) as usize;
        (rem, Some(rem))
    }
}

impl<T: crate::event::BankRow> ExactSizeIterator for RowsIter<'_, T> {}

#[derive(Copy, Clone, Debug)]
enum Key {
    Pindex(i16),
    Index(i16),
}

/// Owning iterator over the rows whose `pindex` (or `index`) column
/// matches a given key. Yielded by [`EventCtx::rows_for_pindex`] /
/// [`EventCtx::rows_for_index`].
#[derive(Debug)]
pub struct RowsForKeyIter<'a, T: crate::event::BankRow> {
    view: Option<crate::event::BankView<'a, T>>,
    key: Key,
    next_idx: usize,
}

impl<'a, T: crate::event::BankRow> RowsForKeyIter<'a, T> {
    fn new(view: Option<crate::event::BankView<'a, T>>, key: Key) -> Self {
        Self {
            view,
            key,
            next_idx: 0,
        }
    }

    pub(crate) fn new_pindex(view: Option<crate::event::BankView<'a, T>>, p: i16) -> Self {
        Self::new(view, Key::Pindex(p))
    }

    pub(crate) fn new_index(view: Option<crate::event::BankView<'a, T>>, k: i16) -> Self {
        Self::new(view, Key::Index(k))
    }
}

impl<'a, T: crate::event::BankRow> Iterator for RowsForKeyIter<'a, T> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        let v = self.view.as_ref()?;
        let rows = match self.key {
            Key::Pindex(p) => v.pindex_rows(p),
            Key::Index(k) => v.index_rows(k),
        };
        if self.next_idx >= rows.len() {
            return None;
        }
        let r = rows[self.next_idx];
        self.next_idx += 1;
        Some(v.row(r))
    }
}
