//! Read side — `Chain`, `Filter`, parallel iteration.
//!
//! `Chain` is the sole reader entry point. Single-file is just a chain
//! of length 1 (`Chain::open(path)`).

pub mod chain;
mod filter;
mod inner;
mod iter;

pub use chain::{Chain, ChainEventIter, ChainStats};
pub use filter::Filter;
pub use iter::EventIter;
