//! `BankWriter` — closure target for building one bank inside an event.

use crate::error::Result;
use crate::event::BankBuilder;
use crate::write::row::RowWriter;

/// Used inside a [`Writer::event`](super::Writer::event) closure to attach
/// rows to one bank.
///
/// `'b` is the lifetime of the borrow (scoped to the `event` closure).
/// `'a` is the lifetime of the schema reference held inside the
/// underlying [`BankBuilder`] (typically the writer's dict).
#[derive(Debug)]
pub struct BankWriter<'b, 'a> {
    builder: BankBuilder<'a>,
    _scope: std::marker::PhantomData<&'b mut ()>,
}

impl<'b, 'a> BankWriter<'b, 'a> {
    pub(crate) fn new(builder: BankBuilder<'a>) -> Self {
        Self {
            builder,
            _scope: std::marker::PhantomData,
        }
    }

    pub(crate) fn into_inner(self) -> BankBuilder<'a> {
        self.builder
    }

    /// Append a row, then run `f` to populate its columns.
    pub fn row<F>(&mut self, f: F) -> Result<&mut Self>
    where
        F: for<'r> FnOnce(&mut RowWriter<'r, 'a>) -> Result<()>,
    {
        self.builder.push_row();
        let mut row = RowWriter::new(&mut self.builder);
        f(&mut row)?;
        Ok(self)
    }

    /// Append `n` zero-filled rows (faster than `n` individual `row` calls
    /// when the row contents are filled in a later random-access pass).
    pub fn push_rows(&mut self, n: u32) -> &mut Self {
        self.builder.push_rows(n);
        self
    }

    pub fn rows(&self) -> u32 {
        self.builder.rows()
    }

    pub fn is_empty(&self) -> bool {
        self.builder.is_empty()
    }
}
