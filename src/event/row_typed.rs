//! [`BankRow`] — typed row mapping for a named bank, plus
//! [`BankView`] — handle-cached bank reader for hot-loop reads.
//!
//! Two layers:
//!
//! - **`BankRow`** is the per-row mapping. Implementors are concrete
//!   structs (one per CLAS12 bank) that decode a single row of a bank
//!   into named fields. Each implementor declares the bank's `NAME`,
//!   `(GROUP, ITEM)` identity, an associated `Handles` type, and two
//!   readers: `from_row` (name-lookup per field) and
//!   `from_row_with_handles` (handle-resolved, pointer arithmetic
//!   only).
//!
//! - [`BankView<T>`] is the opt-in hot-loop view obtained from
//!   [`EventCtx::bank_view`](crate::event::EventCtx::bank_view). It
//!   pre-resolves the typed
//!   [`ColumnHandle`](crate::schema::ColumnHandle)s once per record,
//!   then reuses them across every row of every iter call — eliminating
//!   the 12-HashMap-probes-per-row cost that the `EventCtx::rows`
//!   simple path would otherwise incur.
//!
//! Both readers are **infallible**: missing or wrong-type columns
//! yield `T::default()`, matching [`Bank::get`](crate::event::Bank::get).
//!
//! # Picking a path
//!
//! For one-off reads or small per-event row counts, the simple
//! [`EventCtx::rows`](crate::event::EventCtx::rows) path is fine —
//! it resolves columns by name on each call. For hot loops over
//! every row of every event, prefer:
//!
//! ```ignore
//! if let Some(particles) = ev.bank_view::<RecParticleRow>() {
//!     let cal = ev.bank_view::<RecCalorimeterRow>();
//!     for p in particles.iter() {                  // handle-cached
//!         if let Some(cal) = &cal {
//!             for c in cal.iter_for_pindex(p.row_index as i16) {
//!                 // O(1) pindex join after the first call builds
//!                 // the index for this event.
//!             }
//!         }
//!     }
//! }
//! ```

use crate::event::bank::Bank;
use crate::schema::Schema;

/// A typed row mapping for a named bank.
pub trait BankRow: Sized {
    /// Schema name (e.g. `"REC::Particle"`).
    const NAME: &'static str;
    /// Wire-level bank group.
    const GROUP: u16;
    /// Wire-level bank item.
    const ITEM: u8;

    /// Pre-resolved column handles for `Self`'s fields. Built once per
    /// bank instance by [`Self::resolve_handles`]; reused per row by
    /// [`Self::from_row_with_handles`].
    type Handles: Copy + std::fmt::Debug;

    /// Resolve every field's column handle against the bank's schema.
    /// Called once per [`BankView`] construction.
    fn resolve_handles(schema: &Schema) -> Self::Handles;

    /// Decode bank row `row` into `Self` using pre-resolved handles.
    /// Hot-loop path: no name lookups, no type checks; the handles
    /// already validated everything at resolve time.
    fn from_row_with_handles(bank: &Bank<'_>, row: u32, handles: &Self::Handles) -> Self;

    /// Decode bank row `row` into `Self` (one-shot path). Looks up
    /// every field's column index by name; suitable for occasional
    /// reads or warm-up code. For hot loops, use [`BankView::iter`]
    /// which calls [`Self::from_row_with_handles`].
    ///
    /// Default implementation forwards to the handle path with a fresh
    /// per-call resolve.
    fn from_row(bank: &Bank<'_>, row: u32) -> Self {
        let handles = Self::resolve_handles(bank.schema());
        Self::from_row_with_handles(bank, row, &handles)
    }
}

/// Handle-cached typed bank reader.
///
/// Constructed via
/// [`EventCtx::bank_view`](crate::event::EventCtx::bank_view); holds
/// the borrowed [`Bank`] and a one-shot [`BankRow::Handles`]. Re-use
/// the same view across every iteration in an event: each row read
/// becomes a pointer-cast through the cached handles.
#[derive(Debug)]
pub struct BankView<'a, T: BankRow> {
    bank: Bank<'a>,
    handles: T::Handles,
    /// Lazy `pindex → row indices` map, built on first
    /// [`Self::pindex_index`] call. None for banks without a `pindex`
    /// column.
    pindex_index: std::cell::OnceCell<Option<PindexIndex>>,
    /// Same for the `index` column.
    index_index: std::cell::OnceCell<Option<PindexIndex>>,
}

impl<'a, T: BankRow> BankView<'a, T> {
    /// Wrap a bank with pre-resolved typed column handles.
    pub(crate) fn new(bank: Bank<'a>) -> Self {
        let handles = T::resolve_handles(bank.schema());
        Self {
            bank,
            handles,
            pindex_index: std::cell::OnceCell::new(),
            index_index: std::cell::OnceCell::new(),
        }
    }

    /// The underlying borrowed [`Bank`].
    #[inline]
    pub fn bank(&self) -> &Bank<'a> {
        &self.bank
    }

    /// Row count.
    #[inline]
    pub fn rows(&self) -> u32 {
        self.bank.rows()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bank.is_empty()
    }

    /// Decode row `row` using cached handles. Caller is responsible
    /// for `row < self.rows()`.
    #[inline]
    pub fn row(&self, row: u32) -> T {
        T::from_row_with_handles(&self.bank, row, &self.handles)
    }

    /// Iterate every row using cached handles. Replaces
    /// [`EventCtx::rows`](crate::event::EventCtx::rows)'s per-row
    /// name lookups with pointer arithmetic.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = T> + use<'_, 'a, T> {
        let bank = &self.bank;
        let handles = &self.handles;
        (0..bank.rows()).map(move |r| T::from_row_with_handles(bank, r, handles))
    }

    /// Iterate the rows whose `pindex` column equals `pindex`.
    /// O(rows) on the first call (builds the inverted index); O(1)
    /// per subsequent query.
    pub fn iter_for_pindex(&self, pindex: i16) -> impl Iterator<Item = T> + use<'_, 'a, T> {
        let bank = &self.bank;
        let handles = &self.handles;
        let rows = self.pindex_rows(pindex);
        rows.iter()
            .copied()
            .map(move |r| T::from_row_with_handles(bank, r, handles))
    }

    /// Iterate the rows whose `index` column equals `key`.
    pub fn iter_for_index(&self, key: i16) -> impl Iterator<Item = T> + use<'_, 'a, T> {
        let bank = &self.bank;
        let handles = &self.handles;
        let rows = self.index_rows(key);
        rows.iter()
            .copied()
            .map(move |r| T::from_row_with_handles(bank, r, handles))
    }

    /// Borrow the row indices whose `pindex` column equals `pindex`,
    /// using the cached inverted index. Empty slice when the bank
    /// lacks a `pindex` column, or no row matches.
    pub(crate) fn pindex_rows(&self, pindex: i16) -> &[u32] {
        let idx = self
            .pindex_index
            .get_or_init(|| PindexIndex::build_from_column(&self.bank, "pindex"));
        idx.as_ref().map_or(&[][..], |i| i.rows_for(pindex))
    }

    pub(crate) fn index_rows(&self, key: i16) -> &[u32] {
        let idx = self
            .index_index
            .get_or_init(|| PindexIndex::build_from_column(&self.bank, "index"));
        idx.as_ref().map_or(&[][..], |i| i.rows_for(key))
    }

    /// Cached column handles. Useful when constructing a custom row
    /// reader on top of [`BankRow::from_row_with_handles`].
    #[inline]
    pub fn handles(&self) -> &T::Handles {
        &self.handles
    }
}

/// Inverted `column-value → [row indices]` index for a bank.
///
/// Built lazily on first request from [`BankView::iter_for_pindex`] /
/// `iter_for_index`. Construction is one O(rows) pass; lookups are
/// O(1) into a small hash table. Bank values are `i16` in CLAS12
/// (`pindex` / `index` are `S` in the schema).
#[derive(Debug)]
pub struct PindexIndex {
    table: std::collections::HashMap<i16, Vec<u32>>,
}

impl PindexIndex {
    /// Build the index from `bank`'s column `name`. Returns `None`
    /// when the bank lacks that column.
    fn build_from_column(bank: &Bank<'_>, name: &str) -> Option<Self> {
        let _col = bank.schema().column_index(name)?;
        let mut table: std::collections::HashMap<i16, Vec<u32>> = std::collections::HashMap::new();
        for r in 0..bank.rows() {
            let key: i16 = bank.get(name, r);
            table.entry(key).or_default().push(r);
        }
        Some(Self { table })
    }

    fn rows_for(&self, key: i16) -> &[u32] {
        self.table.get(&key).map_or(&[][..], |v| v.as_slice())
    }
}
