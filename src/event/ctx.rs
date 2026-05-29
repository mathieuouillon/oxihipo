//! `EventCtx<'a>` — borrowed event view paired with the schema dictionary.
//!
//! This is what scans hand to user closures. It exposes the
//! "find a bank by name" ergonomics: the user no longer needs to thread a
//! separate `&Schema` clone through their code.
//!
//! Two backends share the same API:
//!
//! - **Bytes** — classic path, wrapping a raw `Event<'a>` byte slice.
//! - **ByBank** — `Lz4ByBank` path, borrowing a shared `ByBankRecord`
//!   plus an event index. `bank(name)` decompresses *only* the
//!   requested bank's stream (lazily, cached on the record).
//!
//! Both variants are `Copy + Clone` and cheap to pass by value.

use std::sync::Arc;

use crate::event::bank::Bank;
use crate::event::composite::Composite;
use crate::event::event::Event;
use crate::event::owned::OwnedEvent;
use crate::schema::{Dict, Schema};
use crate::wire::by_bank::ByBankRecord;

/// Borrowed event view + reference to the file's schema dictionary.
#[derive(Debug, Copy, Clone)]
pub struct EventCtx<'a> {
    backend: Backend<'a>,
    dict: &'a Dict,
}

#[derive(Debug, Copy, Clone)]
enum Backend<'a> {
    Bytes(Event<'a>),
    ByBank {
        record: &'a Arc<ByBankRecord>,
        event_idx: u32,
    },
}

impl<'a> EventCtx<'a> {
    /// Construct over raw event bytes (the classic path).
    pub fn new(event: Event<'a>, dict: &'a Dict) -> Self {
        Self {
            backend: Backend::Bytes(event),
            dict,
        }
    }

    /// Construct over an `Lz4ByBank` record + an event index. Bank
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
        }
    }

    /// The underlying borrowed event bytes.
    ///
    /// Returns an empty `Event<'a>` for `Lz4ByBank` backends — those
    /// don't carry raw event bytes. Callers that need a real byte view
    /// should up-convert through [`OwnedEvent`] (which synthesises bytes
    /// lazily by decompressing every bank).
    pub fn event(&self) -> Event<'a> {
        match self.backend {
            Backend::Bytes(e) => e,
            Backend::ByBank { .. } => Event::new(&[]),
        }
    }

    /// The file's schema dictionary.
    pub fn dict(&self) -> &'a Dict {
        self.dict
    }

    /// Raw event bytes. Empty for `Lz4ByBank` backends — see [`Self::event`].
    pub fn raw(&self) -> &'a [u8] {
        match self.backend {
            Backend::Bytes(e) => e.raw(),
            Backend::ByBank { .. } => &[],
        }
    }

    pub fn tag(&self) -> u32 {
        match self.backend {
            Backend::Bytes(e) => e.tag(),
            Backend::ByBank { record, event_idx } => record.event_tag(event_idx),
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
        }
    }

    /// Decode bank `name`. `None` if the schema isn't in the dict, the
    /// structure isn't in the event, or the bank's data is mis-sized.
    ///
    /// On `Lz4ByBank` records, only the requested bank's LZ4 stream is
    /// decompressed — other banks in the same record remain compressed.
    pub fn bank(&self, name: &str) -> Option<Bank<'a>> {
        let schema = self.dict.get(name)?;
        self.bank_for(schema)
    }

    /// Decode the bank for an already-resolved schema reference.
    pub fn bank_for(&self, schema: &'a Schema) -> Option<Bank<'a>> {
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
        }
    }

    /// Decode by `(group, item)` directly — useful when the dict doesn't
    /// list the bank but you know the wire IDs.
    pub fn bank_by_id(&self, group: u16, item: u8) -> Option<Bank<'a>> {
        let schema = self.dict.get_by_id(group, item)?;
        self.bank_for(schema)
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
        }
    }

    /// Iterate structure headers + payloads, raw — for tools like `dump`
    /// and `stats` that want to see everything.
    ///
    /// **Empty for `Lz4ByBank` backends**: those don't carry raw event
    /// bytes. If you need to enumerate every bank in a ByBank event, use
    /// [`OwnedEvent::structures`] which synthesises bytes lazily.
    pub fn structures(&self) -> crate::event::event::StructureIter<'a> {
        match self.backend {
            Backend::Bytes(e) => e.iter_structures(),
            Backend::ByBank { .. } => Event::new(&[]).iter_structures(),
        }
    }

    /// Decode a composite structure by name (looks up the wire IDs via the
    /// dict, then re-parses the inline format string).
    ///
    /// **Returns `None` for `Lz4ByBank` backends** — composite reconstruction
    /// requires the original structure bytes. Up-convert via `OwnedEvent`
    /// if you need composites on ByBank records.
    pub fn composite(&self, name: &str) -> Option<Composite<'a>> {
        let schema = self.dict.get(name)?;
        self.composite_by_id(schema.group(), schema.item())
    }

    pub fn composite_by_id(&self, group: u16, item: u8) -> Option<Composite<'a>> {
        let event = match self.backend {
            Backend::Bytes(e) => e,
            Backend::ByBank { .. } => return None,
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
    /// For `Lz4ByBank` backends this is `O(1)` — the resulting
    /// `OwnedEvent` shares the same lazy bank cache via `Arc`.
    pub fn to_owned_with(&self, dict: Arc<Dict>) -> OwnedEvent {
        match self.backend {
            Backend::Bytes(e) => OwnedEvent::new(e.raw().to_vec(), dict),
            Backend::ByBank { record, event_idx } => {
                OwnedEvent::by_bank(Arc::clone(record), event_idx, dict)
            }
        }
    }

    // ---- Typed bank rows ------------------------------------------------

    /// Obtain a handle-cached [`BankView`](crate::event::BankView)
    /// for bank `T::NAME`, or `None` if the event lacks the bank.
    ///
    /// The view resolves typed column handles once at construction;
    /// every subsequent row read (via `BankView::iter`, `iter_for_pindex`,
    /// `iter_for_index`, or `row`) skips name lookups and goes straight
    /// to pointer arithmetic. The first `iter_for_pindex` /
    /// `iter_for_index` call builds an inverted index, so cross-bank
    /// joins become O(rows) once + O(matches) per query instead of
    /// O(rows × queries).
    ///
    /// ```ignore
    /// if let Some(particles) = ev.bank_view::<RecParticleRow>() {
    ///     for p in particles.iter() {
    ///         if let Some(cal) = ev.bank_view::<RecCalorimeterRow>() {
    ///             for c in cal.iter_for_pindex(p.row_index as i16) {
    ///                 // …
    ///             }
    ///         }
    ///     }
    /// }
    /// ```
    pub fn bank_view<T: crate::event::BankRow>(&self) -> Option<crate::event::BankView<'a, T>> {
        let bank = self.bank_by_id_raw(T::GROUP, T::ITEM)?;
        Some(crate::event::BankView::new(bank))
    }

    /// Iterate every row of bank `T::NAME` decoded as `T`. Empty when
    /// the event lacks the bank.
    ///
    /// Equivalent to calling [`Self::bank_view`] and iterating the
    /// returned view — the iterator owns the view internally so handle
    /// caching is preserved across rows of a single call. For multiple
    /// independent passes over the same bank in the same event, prefer
    /// the explicit [`Self::bank_view`] handle to share one resolve.
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

    /// Same as [`Self::bank_by_id`] but doesn't require the schema to
    /// be in the dict — looks up the schema (it must be) and returns
    /// the [`Bank`](crate::event::Bank) view via the backend.
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
