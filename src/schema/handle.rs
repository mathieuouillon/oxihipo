//! `ColumnHandle<T>` — pre-resolved, typed column index for hot loops.
//!
//! Resolved once against a [`Schema`] via [`Schema::handle`]; the handle
//! checks type compatibility at resolution time so all `read` calls become
//! a bounds-checked cast with no per-call name lookup.
//!
//! Two layers:
//!
//! - [`BankScalarType`] — the six primitive element types that can live
//!   inside a bank cell (`i8` / `i16` / `i32` / `i64` / `f32` / `f64`).
//!   Encodes the wire-level [`DataType`] for each Rust scalar plus
//!   little-endian byte serialisation. Sealed.
//!
//! - [`BankColumnType`] — what a *column* can hold: either a scalar
//!   directly (length-1 cells) or a fixed-length `[T; N]` array
//!   (length-N cells, declared in schema text as `name/T#N`). Blanket
//!   impls cover both forms automatically.

use std::marker::PhantomData;

use crate::error::{HipoError, Result};
use crate::schema::types::{DataType, Schema};

// ---- BankScalarType --------------------------------------------------

/// Sealed trait — primitive types that can occupy one element of a bank
/// column. Implemented for `i8`, `i16`, `i32`, `i64`, `f32`, `f64`.
///
/// The `bytemuck::Pod` bound lets the reader cast raw bank bytes to
/// `&[T]` / read a single `T` through *safe* `bytemuck` calls instead of
/// hand-rolled `unsafe` pointer reads — every supported element type is a
/// plain-old-data primitive, so this is always satisfiable.
pub trait BankScalarType: Copy + sealed::Sealed + 'static + bytemuck::Pod {
    /// The wire-level [`DataType`] this Rust type corresponds to.
    const DATA_TYPE: DataType;

    /// Little-endian byte serialisation. `dst.len()` must equal
    /// `DATA_TYPE.size()`; the implementation asserts in debug builds.
    #[doc(hidden)]
    fn write_le(self, dst: &mut [u8]);

    /// Write `self` into the row currently being built by `builder`,
    /// targeting scalar column `name`. Used by the array-aware
    /// [`BankColumnType`] dispatch.
    #[doc(hidden)]
    fn set_scalar_in(
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
    impl<T: Sealed, const N: usize> Sealed for [T; N] {}
}

impl BankScalarType for i8 {
    const DATA_TYPE: DataType = DataType::Byte;
    fn write_le(self, dst: &mut [u8]) {
        debug_assert_eq!(dst.len(), 1);
        dst[0] = self as u8;
    }
    fn set_scalar_in(
        self,
        b: &mut crate::event::BankBuilder<'_>,
        name: &str,
    ) -> crate::error::Result<()> {
        b.set_i8(name, self).map(|_| ())
    }
}
impl BankScalarType for i16 {
    const DATA_TYPE: DataType = DataType::Short;
    fn write_le(self, dst: &mut [u8]) {
        dst.copy_from_slice(&self.to_le_bytes());
    }
    fn set_scalar_in(
        self,
        b: &mut crate::event::BankBuilder<'_>,
        name: &str,
    ) -> crate::error::Result<()> {
        b.set_i16(name, self).map(|_| ())
    }
}
impl BankScalarType for i32 {
    const DATA_TYPE: DataType = DataType::Int;
    fn write_le(self, dst: &mut [u8]) {
        dst.copy_from_slice(&self.to_le_bytes());
    }
    fn set_scalar_in(
        self,
        b: &mut crate::event::BankBuilder<'_>,
        name: &str,
    ) -> crate::error::Result<()> {
        b.set_i32(name, self).map(|_| ())
    }
}
impl BankScalarType for i64 {
    const DATA_TYPE: DataType = DataType::Long;
    fn write_le(self, dst: &mut [u8]) {
        dst.copy_from_slice(&self.to_le_bytes());
    }
    fn set_scalar_in(
        self,
        b: &mut crate::event::BankBuilder<'_>,
        name: &str,
    ) -> crate::error::Result<()> {
        b.set_i64(name, self).map(|_| ())
    }
}
impl BankScalarType for f32 {
    const DATA_TYPE: DataType = DataType::Float;
    fn write_le(self, dst: &mut [u8]) {
        dst.copy_from_slice(&self.to_le_bytes());
    }
    fn set_scalar_in(
        self,
        b: &mut crate::event::BankBuilder<'_>,
        name: &str,
    ) -> crate::error::Result<()> {
        b.set_f32(name, self).map(|_| ())
    }
}
impl BankScalarType for f64 {
    const DATA_TYPE: DataType = DataType::Double;
    fn write_le(self, dst: &mut [u8]) {
        dst.copy_from_slice(&self.to_le_bytes());
    }
    fn set_scalar_in(
        self,
        b: &mut crate::event::BankBuilder<'_>,
        name: &str,
    ) -> crate::error::Result<()> {
        b.set_f64(name, self).map(|_| ())
    }
}

// ---- BankColumnType --------------------------------------------------

/// What a bank column can hold: either a scalar (length-1 cell) or a
/// fixed-length `[T; N]` array (length-N cell).
///
/// Blanket impls cover:
/// - every `T: BankScalarType` (so `bank.col::<f32>(…)` keeps working
///   for scalar columns), with `LENGTH = 1`.
/// - every `[T; N] where T: BankScalarType` (so `bank.col::<[f32; 32]>(…)`
///   works for an array column declared `name/F#32`), with `LENGTH = N`.
///
/// Sealed; downstream crates can't add fishy implementations.
///
/// The `bytemuck::Pod` bound covers both forms (scalars are `Pod`;
/// `[T; N]` is `Pod` when `T` is) and is what makes the column casts in
/// [`Bank`](crate::event::Bank) safe.
pub trait BankColumnType: Copy + sealed::Sealed + 'static + bytemuck::Pod {
    /// Wire-level element type. For arrays, this is the *element* type,
    /// not the array type — `[f32; 32]::DATA_TYPE == DataType::Float`.
    const DATA_TYPE: DataType;

    /// Number of elements per cell. `1` for scalar `T`; `N` for `[T; N]`.
    const LENGTH: u32;

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

impl<T: BankScalarType> BankColumnType for T {
    const DATA_TYPE: DataType = <T as BankScalarType>::DATA_TYPE;
    const LENGTH: u32 = 1;
    fn set_in(
        self,
        builder: &mut crate::event::BankBuilder<'_>,
        name: &str,
    ) -> crate::error::Result<()> {
        self.set_scalar_in(builder, name)
    }
}

impl<T: BankScalarType, const N: usize> BankColumnType for [T; N] {
    const DATA_TYPE: DataType = <T as BankScalarType>::DATA_TYPE;
    const LENGTH: u32 = N as u32;
    fn set_in(
        self,
        builder: &mut crate::event::BankBuilder<'_>,
        name: &str,
    ) -> crate::error::Result<()> {
        builder.set_array(name, &self).map(|_| ())
    }
}

// ---- ColumnHandle ----------------------------------------------------

/// Pre-resolved, typed column index.
///
/// Cheap to copy (it's effectively a `u16`). Construct via
/// [`Schema::handle`].
#[derive(Debug, Clone, Copy)]
pub struct ColumnHandle<T> {
    col: u16,
    _phantom: PhantomData<fn() -> T>,
}

impl<T: BankColumnType> ColumnHandle<T> {
    /// Resolve against a schema, verifying that the column exists, that
    /// its on-wire type matches `T`'s element type, **and** that its
    /// per-row length matches `T::LENGTH` (so a scalar handle won't
    /// resolve against an array column or vice versa).
    pub(crate) fn resolve(schema: &Schema, name: &str) -> Result<Self> {
        let col = schema.require_column(name)?;
        let entry = &schema.entries()[col];
        if entry.ty != T::DATA_TYPE {
            return Err(HipoError::TypeMismatch {
                schema: schema.name().to_string(),
                column: name.to_string(),
                expected: T::DATA_TYPE.name(),
                actual: entry.ty.name(),
            });
        }
        if entry.length != T::LENGTH {
            return Err(HipoError::ColumnLengthMismatch {
                schema: schema.name().to_string(),
                column: name.to_string(),
                expected: T::LENGTH,
                actual: entry.length,
            });
        }
        Ok(Self {
            col: u16::try_from(col).expect("schema column index fits in u16"),
            _phantom: PhantomData,
        })
    }

    /// Build a handle from an already-resolved column index, **without**
    /// re-checking type or length. The caller must have verified the
    /// column's `ty`/`length` match `T` (e.g. the per-event `ev.get`
    /// column cache, which validates against `entries()` before reading).
    #[inline]
    pub(crate) fn from_index(col: u16) -> Self {
        Self {
            col,
            _phantom: PhantomData,
        }
    }

    /// 0-based column index inside the schema's `entries()`.
    #[inline]
    pub fn column_index(&self) -> usize {
        self.col as usize
    }

    /// Per-row element count of this handle's column.
    #[inline]
    pub fn length(&self) -> u32 {
        T::LENGTH
    }

    /// Sentinel handle that represents "column not present in this
    /// schema". Reads through a placeholder via
    /// [`Bank::read_handle_or_default`](crate::event::Bank::read_handle_or_default)
    /// return `T::default()` — matching the infallible contract of
    /// [`Bank::get`](crate::event::Bank::get) for missing columns.
    ///
    /// Lets a typed-row catalog keep handle resolution infallible: an
    /// absent column resolves to a placeholder rather than aborting the
    /// whole row read.
    pub const fn placeholder() -> Self {
        Self {
            col: u16::MAX,
            _phantom: PhantomData,
        }
    }

    /// True if this handle is a [`Self::placeholder`].
    #[inline]
    pub fn is_placeholder(&self) -> bool {
        self.col == u16::MAX
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
                ("pid".into(), DataType::Int, 1),
                ("px".into(), DataType::Float, 1),
                ("charge".into(), DataType::Byte, 1),
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
