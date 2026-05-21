//! `ColumnHandle<T>` — pre-resolved, typed column index for hot loops.
//!
//! Resolved once against a [`Schema`] via [`Schema::handle`]; the handle
//! checks type compatibility at resolution time so all `read` calls become
//! a bounds-checked cast with no per-call name lookup.

use std::marker::PhantomData;

use crate::error::{HipoError, Result};
use crate::schema::types::{DataType, Schema};

/// Sealed trait — types that can occupy a bank column.
///
/// Implemented for `i8`, `i16`, `i32`, `i64`, `f32`, `f64`. We seal the
/// trait so users cannot add fishy implementations that violate the layout
/// invariants of the bank.
pub trait BankColumnType: Copy + sealed::Sealed + 'static {
    /// The wire-level [`DataType`] this Rust type corresponds to.
    const DATA_TYPE: DataType;

    /// Write `self` into the row currently being built by `builder`,
    /// targeting column `name`. Used by the writer's
    /// [`RowWriter::set`](crate::write::RowWriter::set) so callers don't
    /// have to remember per-type setter names.
    #[doc(hidden)]
    fn set_in(
        self,
        builder: &mut crate::event::BankBuilder<'_>,
        name: &str,
    ) -> crate::error::Result<()>;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for i8 {}
    impl Sealed for i16 {}
    impl Sealed for i32 {}
    impl Sealed for i64 {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
}

impl BankColumnType for i8 {
    const DATA_TYPE: DataType = DataType::Byte;
    fn set_in(
        self,
        builder: &mut crate::event::BankBuilder<'_>,
        name: &str,
    ) -> crate::error::Result<()> {
        builder.set_i8(name, self).map(|_| ())
    }
}
impl BankColumnType for i16 {
    const DATA_TYPE: DataType = DataType::Short;
    fn set_in(
        self,
        builder: &mut crate::event::BankBuilder<'_>,
        name: &str,
    ) -> crate::error::Result<()> {
        builder.set_i16(name, self).map(|_| ())
    }
}
impl BankColumnType for i32 {
    const DATA_TYPE: DataType = DataType::Int;
    fn set_in(
        self,
        builder: &mut crate::event::BankBuilder<'_>,
        name: &str,
    ) -> crate::error::Result<()> {
        builder.set_i32(name, self).map(|_| ())
    }
}
impl BankColumnType for i64 {
    const DATA_TYPE: DataType = DataType::Long;
    fn set_in(
        self,
        builder: &mut crate::event::BankBuilder<'_>,
        name: &str,
    ) -> crate::error::Result<()> {
        builder.set_i64(name, self).map(|_| ())
    }
}
impl BankColumnType for f32 {
    const DATA_TYPE: DataType = DataType::Float;
    fn set_in(
        self,
        builder: &mut crate::event::BankBuilder<'_>,
        name: &str,
    ) -> crate::error::Result<()> {
        builder.set_f32(name, self).map(|_| ())
    }
}
impl BankColumnType for f64 {
    const DATA_TYPE: DataType = DataType::Double;
    fn set_in(
        self,
        builder: &mut crate::event::BankBuilder<'_>,
        name: &str,
    ) -> crate::error::Result<()> {
        builder.set_f64(name, self).map(|_| ())
    }
}

/// Pre-resolved, typed column index.
///
/// Cheap to copy (it's effectively a `u16`). Construct via
/// [`Schema::handle`] or [`Bank::handle`](crate::event::Bank::handle).
#[derive(Debug, Clone, Copy)]
pub struct ColumnHandle<T> {
    col: u16,
    _phantom: PhantomData<fn() -> T>,
}

impl<T: BankColumnType> ColumnHandle<T> {
    /// Resolve against a schema, verifying that the column exists and that
    /// its on-wire type matches `T`.
    pub(crate) fn resolve(schema: &Schema, name: &str) -> Result<Self> {
        let col = schema.require_column(name)?;
        let actual = schema.entries()[col].ty;
        if actual != T::DATA_TYPE {
            return Err(HipoError::TypeMismatch {
                schema: schema.name().to_string(),
                column: name.to_string(),
                expected: T::DATA_TYPE.name(),
                actual: actual.name(),
            });
        }
        Ok(Self {
            col: u16::try_from(col).expect("schema column index fits in u16"),
            _phantom: PhantomData,
        })
    }

    /// 0-based column index inside the schema's `entries()`.
    #[inline]
    pub fn column_index(&self) -> usize {
        self.col as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::types::Schema;

    fn sample_schema() -> Schema {
        Schema::from_columns(
            "REC::Particle",
            300,
            1,
            [
                ("pid".into(), DataType::Int),
                ("px".into(), DataType::Float),
                ("charge".into(), DataType::Byte),
            ],
        )
    }

    #[test]
    fn resolves_matching_type() {
        let s = sample_schema();
        let h = ColumnHandle::<i32>::resolve(&s, "pid").unwrap();
        assert_eq!(h.column_index(), 0);
        let h = ColumnHandle::<f32>::resolve(&s, "px").unwrap();
        assert_eq!(h.column_index(), 1);
    }

    #[test]
    fn rejects_type_mismatch() {
        let s = sample_schema();
        let err = ColumnHandle::<i32>::resolve(&s, "px").unwrap_err();
        assert!(matches!(err, HipoError::TypeMismatch { .. }));
    }

    #[test]
    fn rejects_missing_column() {
        let s = sample_schema();
        let err = ColumnHandle::<i32>::resolve(&s, "missing").unwrap_err();
        assert!(matches!(err, HipoError::UnknownColumn { .. }));
    }
}
