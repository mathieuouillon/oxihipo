//! `RowView` lives in `bank.rs` to share its private cell-access helpers.
//! This module re-exports it so callers can import from
//! `crate::event::row::RowView` if they like.

#[allow(unused_imports)]
pub use crate::event::bank::RowView;
