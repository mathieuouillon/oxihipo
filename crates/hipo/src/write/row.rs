//! `RowWriter` — closure target for setting one row's columns.

use crate::error::Result;
use crate::event::BankBuilder;
use crate::schema::BankColumnType;

/// Used inside a [`BankWriter::row`](super::BankWriter::row) closure. Calls
/// dispatch to the right wire-level setter based on the value's type.
#[derive(Debug)]
pub struct RowWriter<'r, 'a> {
    builder: &'r mut BankBuilder<'a>,
}

impl<'r, 'a> RowWriter<'r, 'a> {
    pub(crate) fn new(builder: &'r mut BankBuilder<'a>) -> Self {
        Self { builder }
    }

    /// Set column `name` to `value`. The bank's wire-level type must match
    /// the value's Rust type (e.g. an `i32` value into a `Float` column
    /// returns [`HipoError::TypeMismatch`](crate::HipoError::TypeMismatch)).
    pub fn set<T: BankColumnType>(&mut self, name: &str, value: T) -> Result<&mut Self> {
        value.set_in(self.builder, name)?;
        Ok(self)
    }
}
