//! Event / Bank — zero-copy decoded views.
//!
//! Three views live here:
//!
//! - [`Event<'a>`] — bare borrowed bytes; the low-level primitive.
//! - [`EventCtx<'a>`] — `Event` paired with a `&Dict`; the high-level
//!   handle yielded by scans. Adds `bank("name")` ergonomics.
//! - [`OwnedEvent`] — detached `Vec<u8>` plus `Arc<Dict>`; for storage and
//!   cross-thread shipping.

pub(crate) mod bank;
pub(crate) mod build;
pub(crate) mod composite;
pub(crate) mod ctx;
#[allow(clippy::module_inception)]
pub(crate) mod event;
pub(crate) mod owned;
pub(crate) mod row_typed;

pub use bank::Bank;
pub use build::{BankBuilder, EventBuilder};
pub use composite::{Composite, CompositeField, CompositeFormat};
pub use ctx::EventCtx;
pub use event::{Event, StructureHeader, StructureIter};
pub use owned::OwnedEvent;
pub use row_typed::BankRow;
pub(crate) use row_typed::BankView;
