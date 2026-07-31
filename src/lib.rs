//! Pure-Rust reader and writer for the HIPO v6 binary container
//! used at Jefferson Lab CLAS12.
//!
//! # Quick start
//!
//! ```no_run
//! use oxihipo::Chain;
//!
//! # fn main() -> oxihipo::Result<()> {
//! let chain = Chain::open("rec.hipo")?;          // single file
//! // or a directory, a glob ("data/*.hipo"), or a list:
//! // let chain = Chain::open(["a.hipo", "b.hipo"])?;
//! for ev in chain.events() {
//!     let ev = ev?;
//!     if let Some(particles) = ev.bank("REC::Particle") {
//!         if let Ok(px) = particles.col::<f32>("px") {
//!             for &x in &*px {
//!                 // ...
//!                 let _ = x;
//!             }
//!         }
//!     }
//! }
//! # Ok(()) }
//! ```
//!
//! See [`prelude`] for the recommended import set.

#![forbid(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]

pub mod error;
pub mod prelude;

mod compress;
mod wire;

pub mod event;
pub mod read;
pub mod schema;
mod tag;
pub mod write;

pub use crate::error::{HipoError, Result};
pub use crate::event::{Bank, BankRow, Composite, Event, EventCtx, OwnedEvent};
pub use crate::read::{
    BankOccupancy, Chain, ChainEventIter, ChainRecordSpan, ChainStats, ColumnBuffers, ColumnData,
    EventIter, Filter, IntoSources, MaterializedColumn, Occupancy, ReadAt, SkimOptions,
    SkimSummary,
};
pub use crate::schema::{BankPatterns, ColumnHandle, DataType, Dict, Schema, SchemaEntry};
pub use crate::tag::{TagRegistry, TagSet};
pub use crate::wire::constants::{Codec, Layout};
pub use crate::write::{BankWriter, Compression, RowWriter, WriteSummary, Writer};
// `Chain::file_header` returns a `&FileHeader` whose `endianness` field is an
// `Endianness`, and both lived behind the private `wire` module. A caller could
// read the integer fields — inference hands over the value — but could not name
// the type to write a signature over it, nor `match` the enum at all, which
// left the accessor effectively unusable downstream.
pub use crate::wire::bytes::Endianness;
pub use crate::wire::file_header::FileHeader;

/// Internal entry points exposed **only** for fuzzing (`fuzz-api` feature).
///
/// Not part of the public API and exempt from semver: the fuzz targets need to
/// feed bytes straight to the record decoder, below `Chain::open`'s file-header
/// layer, so the fuzzer spends its budget on the code that handles
/// attacker-controlled lengths rather than on getting past the header.
#[cfg(feature = "fuzz-api")]
pub mod fuzz_api {
    use crate::wire::record::Record;

    /// A decoded record, for the `read_record` fuzz target.
    #[derive(Debug)]
    pub struct FuzzRecord(Record);

    impl FuzzRecord {
        pub fn event_count(&self) -> u32 {
            self.0.event_count()
        }
        pub fn event(&self, i: u32) -> Option<&[u8]> {
            self.0.event(i)
        }
    }

    /// Decode `bytes` as a standalone record (header at offset 0).
    pub fn decode_record(bytes: &[u8]) -> crate::Result<FuzzRecord> {
        let mut rec = Record::new();
        rec.load(bytes)?;
        Ok(FuzzRecord(rec))
    }

    /// Walk every structure of an event's bytes, touching each header and
    /// payload span — the bounds-checked path a malformed event must survive.
    pub fn walk_structures(event_bytes: &[u8]) {
        let ev = crate::event::Event::new(event_bytes);
        for (hdr, data) in ev.iter_structures() {
            std::hint::black_box((hdr.group, hdr.item, hdr.ty, data.len()));
        }
    }
}

/// Unwrap an `Option<T>`; on `None`, `continue` the enclosing loop.
///
/// A one-line shorthand for the `let-else` pattern that's idiomatic
/// inside event-iteration loops where missing banks / columns are a
/// normal case to skip rather than an error:
///
/// ```ignore
/// for ev in file.events() {
///     let p = oxihipo::or_continue!(ev.bank("REC::Particle"));
///     // use p ...
/// }
/// ```
///
/// `continue` is a statement tied to the syntactic loop, so it can't be
/// expressed via a method like `Option::unwrap_or_else(|| continue)` —
/// the closure can't break its caller's control flow. A macro can,
/// because it expands textually at the call site.
#[macro_export]
macro_rules! or_continue {
    ($opt:expr) => {
        match $opt {
            ::core::option::Option::Some(v) => v,
            ::core::option::Option::None => continue,
        }
    };
}

/// Unwrap an `Option<T>`; on `None`, `break` out of the enclosing loop.
///
/// Same idea as [`or_continue!`] but for early exit:
///
/// ```ignore
/// while let Some(rec) = file.event(global_idx) {
///     let p = oxihipo::or_break!(rec.bank("REC::Particle"));
///     // ... if missing, leave the while loop ...
/// }
/// ```
#[macro_export]
macro_rules! or_break {
    ($opt:expr) => {
        match $opt {
            ::core::option::Option::Some(v) => v,
            ::core::option::Option::None => break,
        }
    };
}

/// Define a typed row struct for a named bank and derive its
/// [`BankRow`](crate::event::BankRow) impl — the turn-key path to
/// [`EventCtx::rows`](crate::event::EventCtx::rows), with no
/// hand-written trait impl.
///
/// Each `field: type => "column"` entry maps a struct field to a bank
/// column. The field type must implement
/// [`BankColumnType`](crate::schema::BankColumnType) **and** `Default` —
/// every scalar (`i8`/`i16`/`i32`/`i64`/`f32`/`f64`) and fixed-length
/// array (`[T; N]`). Decoding is infallible: a column missing from the
/// runtime schema, or one whose wire type doesn't match, reads back as
/// `T::default()`.
///
/// ```ignore
/// oxihipo::bank_row! {
///     #[derive(Clone, Copy, Debug, Default)]
///     pub struct RecParticle for "REC::Particle" @ (300, 1) {
///         pid: i32 => "pid",
///         px:  f32 => "px",
///         py:  f32 => "py",
///         pz:  f32 => "pz",
///     }
/// }
///
/// // Then, given an `ev: EventCtx` or `OwnedEvent`:
/// for p in ev.rows::<RecParticle>() {
///     let _ = (p.pid, p.px, p.py, p.pz);
/// }
/// ```
///
/// The generated fields inherit the struct's visibility. Supply your own
/// `#[derive(...)]` (typically `Clone, Copy, Debug` — add `Default` if you
/// construct the struct yourself).
///
/// There is **no column limit**. The internal handle cache is a tuple, which
/// used to cap this at 12 columns because [`BankRow::Handles`] was bound by
/// `Debug` and std stops implementing `Debug` for tuples past 12 elements —
/// enough for `REC::Particle` by exactly one column, and for none of the 16
/// wider CLAS12 banks. The bound is gone; `REC::Calorimeter`'s 28 columns
/// map to one struct.
#[macro_export]
macro_rules! bank_row {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident for $bank:literal @ ($group:literal, $item:literal) {
            $( $field:ident : $ty:ty => $col:literal ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis struct $name {
            $( $vis $field: $ty, )*
        }

        impl $crate::event::BankRow for $name {
            const NAME: &'static str = $bank;
            const GROUP: u16 = $group;
            const ITEM: u8 = $item;

            // One typed handle per field, in declaration order. The same
            // `$(...)*` drives this tuple, `resolve_handles`, and the
            // destructure below, so the order can never drift.
            type Handles = ( $( $crate::schema::ColumnHandle<$ty>, )* );

            fn resolve_handles(schema: &$crate::schema::Schema) -> Self::Handles {
                (
                    $(
                        schema
                            .handle::<$ty>($col)
                            .unwrap_or_else(|_| $crate::schema::ColumnHandle::<$ty>::placeholder()),
                    )*
                )
            }

            #[inline]
            fn from_row_with_handles(
                bank: &$crate::event::Bank<'_>,
                row: u32,
                handles: &Self::Handles,
            ) -> Self {
                // `Handles` is `Copy`, so this copies each handle out.
                let ( $( $field, )* ) = *handles;
                Self {
                    $( $field: bank.read_handle_or_default($field, row), )*
                }
            }
        }
    };
}

/// Re-export of the `mimalloc` crate, available when the
/// `mimalloc-allocator` feature is enabled. Binaries can install it as
/// their global allocator:
///
/// ```ignore
/// #[global_allocator]
/// static GLOBAL: oxihipo::mimalloc::MiMalloc = oxihipo::mimalloc::MiMalloc;
/// ```
///
/// Recommended for allocation-heavy workloads on macOS, where the
/// system allocator underperforms under heavy churn.
#[cfg(feature = "mimalloc-allocator")]
pub use mimalloc;
