//! Wire-format primitives — constants, byte readers, headers, record
//! decompression. Crate-private; never re-exported.
//!
//! Many constants here are forensic / wire-format completeness — they
//! exist so the crate can be a reference for the HIPO v6 format even if
//! the current code paths don't read them.

#![allow(dead_code)]

pub mod by_bank;
pub mod bytes;
pub mod constants;
pub mod event_index;
pub mod file_header;
pub mod record;
pub mod record_header;
