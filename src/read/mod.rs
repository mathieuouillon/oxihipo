//! Read side — `Chain`, `Filter`, parallel iteration.
//!
//! `Chain` is the sole reader entry point. Single-file is just a chain
//! of length 1 (`Chain::open(path)`).

pub mod chain;
mod columns;
mod filter;
mod inner;
mod iter;
mod source;

pub use chain::{Chain, ChainEventIter, ChainStats};
pub use columns::{ChainRecordSpan, ColumnBuffers, ColumnData, MaterializedColumn};
pub use filter::Filter;
pub use iter::EventIter;
pub use source::IntoSources;
