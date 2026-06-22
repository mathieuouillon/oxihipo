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
//! // or: let chain = Chain::open_all(["a.hipo", "b.hipo"])?;
//! for ev in chain.events() {
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
pub mod write;

pub use crate::error::{HipoError, Result};
pub use crate::event::{Bank, BankRow, BankView, Composite, Event, EventCtx, OwnedEvent, RowView};
pub use crate::read::{Chain, ChainEventIter, ChainStats, EventIter, Filter, TryChainEventIter};
pub use crate::schema::{ColumnHandle, DataType, Dict, Schema, SchemaEntry};
pub use crate::write::{BankWriter, Compression, RowWriter, WriteSummary, Writer, WriterOptions};

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
