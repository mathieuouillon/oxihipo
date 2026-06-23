//! [`BankRow`] — typed row mapping for a named bank, plus the
//! crate-internal `BankView` handle-cache that backs the public row
//! accessors.
//!
//! Two layers:
//!
//! - **`BankRow`** is the per-row mapping. Implementors are concrete
//!   structs (one per CLAS12 bank) that decode a single row of a bank
//!   into named fields. Each implementor declares the bank's `NAME`,
//!   `(GROUP, ITEM)` identity, an associated `Handles` type, and two
//!   readers: `from_row` (name-lookup per field) and
//!   `from_row_with_handles` (handle-resolved, pointer arithmetic
//!   only). Define one with the [`bank_row!`](crate::bank_row) macro.
//!
//! - **`BankView<T>`** (crate-internal) is the handle-cached reader
//!   that backs [`EventCtx::rows`](crate::event::EventCtx::rows) and the
//!   `rows_for_*` accessors. It pre-resolves the typed
//!   [`ColumnHandle`](crate::schema::ColumnHandle)s once per record,
//!   then reuses them across every row — eliminating the
//!   12-HashMap-probes-per-row cost a name-lookup path would incur.
//!
//! Both readers are **infallible**: missing or wrong-type columns
//! yield `T::default()`, matching [`Bank::get`](crate::event::Bank::get).
//!
//! ```ignore
//! // Define the row structs once with `bank_row!`, then iterate; the
//! // handle cache is resolved once per `rows` call and reused per row.
//! for p in ev.rows::<RecParticle>() {
//!     for c in ev.rows_for_pindex::<RecCalorimeter>(p.row_index as i16) {
//!         // O(1) pindex join after the first call builds the index.
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
    /// Called once per [`EventCtx::rows`](crate::event::EventCtx::rows)
    /// call, then reused for every row.
    fn resolve_handles(schema: &Schema) -> Self::Handles;

    /// Decode bank row `row` into `Self` using pre-resolved handles.
    /// Hot-loop path: no name lookups, no type checks; the handles
    /// already validated everything at resolve time.
    fn from_row_with_handles(bank: &Bank<'_>, row: u32, handles: &Self::Handles) -> Self;

    /// Decode bank row `row` into `Self` (one-shot path). Looks up
    /// every field's column index by name; suitable for occasional
    /// reads or warm-up code. For hot loops, use
    /// [`EventCtx::rows`](crate::event::EventCtx::rows), which resolves
    /// the handles once and calls [`Self::from_row_with_handles`] per row.
    ///
    /// Default implementation forwards to the handle path with a fresh
    /// per-call resolve.
    fn from_row(bank: &Bank<'_>, row: u32) -> Self {
        let handles = Self::resolve_handles(bank.schema());
        Self::from_row_with_handles(bank, row, &handles)
    }
}

/// Handle-cached typed bank reader (crate-internal).
///
/// Backs [`EventCtx::rows`](crate::event::EventCtx::rows) and the
/// `rows_for_*` accessors; holds the borrowed [`Bank`] and a one-shot
/// [`BankRow::Handles`], reused across every row of one accessor call so
/// each row read becomes a pointer-cast through the cached handles.
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

    /// Row count.
    #[inline]
    pub fn rows(&self) -> u32 {
        self.bank.rows()
    }

    /// Decode row `row` using cached handles. Caller is responsible
    /// for `row < self.rows()`.
    #[inline]
    pub fn row(&self, row: u32) -> T {
        T::from_row_with_handles(&self.bank, row, &self.handles)
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
}

/// Inverted `column-value → [row indices]` index for a bank.
///
/// Built lazily on first request from
/// [`EventCtx::rows_for_pindex`](crate::event::EventCtx::rows_for_pindex) /
/// [`rows_for_index`](crate::event::EventCtx::rows_for_index).
/// Construction is one O(rows) pass; lookups are O(1) into a small hash
/// table. Bank values are `i16` in CLAS12 (`pindex` / `index` are `S` in
/// the schema).
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
