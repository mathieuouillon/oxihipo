//! `Bank<'b>` — typed, zero-copy view over a column-major data buffer.
//!
//! A bank is the "rows" view of one structure inside an event, paired with a
//! [`Schema`]. Its data section stores columns contiguously:
//! `[col0_row0, col0_row1, ..., col0_rowN-1, col1_row0, ...]`.
//!
//! Typed column access goes through the generic [`Self::col`] method:
//!
//! ```ignore
//! let px = bank.col::<f32>("px")?;     // Cow<[f32]>; deref → &[f32]
//! let pid = bank.col::<i32>("pid")?;
//! for &x in &*px { /* ... */ }
//! ```
//!
//! On little-endian targets (every supported one) the typed cast is
//! zero-copy.

use std::borrow::Cow;

use crate::error::{HipoError, Result};
use crate::schema::{BankColumnType, ColumnHandle, DataType, Schema};

/// A read-only bank backed by a borrowed byte slice.
#[derive(Debug, Clone)]
pub struct Bank<'b> {
    schema: &'b Schema,
    /// The bank's data section (excluding the 8-byte structure header).
    data: &'b [u8],
    rows: u32,
}

impl<'b> Bank<'b> {
    /// Wrap a data buffer with a schema. `data.len()` must be a multiple
    /// of `schema.row_size()` — anything else is a corrupt bank.
    pub fn new(schema: &'b Schema, data: &'b [u8]) -> Result<Self> {
        let row_size = schema.row_size();
        if row_size == 0 {
            return Ok(Self {
                schema,
                data,
                rows: 0,
            });
        }
        if !data.len().is_multiple_of(row_size as usize) {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "bank data size is not a multiple of row size",
            });
        }
        let rows = (data.len() as u32) / row_size;
        Ok(Self { schema, data, rows })
    }

    /// Construct without checking `data.len()` divisibility — useful when
    /// the structure header reports an unrelated padding byte. Truncates to
    /// the largest whole-row prefix.
    pub fn new_lossy(schema: &'b Schema, data: &'b [u8]) -> Self {
        let row_size = schema.row_size().max(1) as usize;
        let rows = (data.len() / row_size) as u32;
        Self {
            schema,
            data: &data[..rows as usize * row_size],
            rows,
        }
    }

    pub fn schema(&self) -> &'b Schema {
        self.schema
    }

    pub fn rows(&self) -> u32 {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// Raw bytes of column `col`. Length is `rows * ty.size()`.
    pub fn col_bytes(&self, col: usize) -> &'b [u8] {
        let entry = &self.schema.entries()[col];
        let start = self.schema.column_byte_offset(col, self.rows) as usize;
        let len = self.rows as usize * entry.ty.size();
        &self.data[start..start + len]
    }

    /// Borrow column `name` as `Cow<[T]>`. Generic over the bank-column-type
    /// trait.
    ///
    /// **Performance contract:** On little-endian targets (every supported
    /// platform) the return is `Cow::Borrowed` — zero-copy — whenever the
    /// column's bytes happen to be aligned to `T`. This is virtually always
    /// the case for 4-byte types (`i32`, `f32`, `i16`, `i8`). For 8-byte
    /// types (`i64`, `f64`) it depends on the byte layout of the containing
    /// event; misaligned columns fall back to a one-shot copy via
    /// `read_unaligned` — matching the C++ reader's memcpy semantics.
    pub fn col<T: BankColumnType>(&self, name: &str) -> Result<Cow<'b, [T]>> {
        let col = self.schema.require_column(name)?;
        self.cast_column::<T>(col)
    }

    /// Same as [`Self::col`] but by column index — saves a name lookup.
    pub fn col_at<T: BankColumnType>(&self, col: usize) -> Result<Cow<'b, [T]>> {
        self.cast_column::<T>(col)
    }

    /// Read using a pre-resolved typed handle. Constant-time after handle
    /// construction; this is the recommended hot-loop path.
    ///
    /// Infallible — the handle's column index and type were verified at
    /// resolution time. In debug builds, asserts the handle's schema is
    /// compatible with the bank's schema; release builds trust the
    /// handle. Passing a handle resolved against a different schema is a
    /// programming error.
    pub fn read<T: BankColumnType>(&self, handle: ColumnHandle<T>) -> Cow<'b, [T]> {
        let col = handle.column_index();
        debug_assert!(
            col < self.schema.entries().len(),
            "ColumnHandle out of bounds for this bank's schema"
        );
        debug_assert_eq!(
            self.schema.entries()[col].ty,
            T::DATA_TYPE,
            "ColumnHandle resolved against a different schema"
        );
        self.cast_column_unchecked::<T>(col)
    }

    /// Resolve a column handle against this bank's schema. Equivalent to
    /// `bank.schema().handle::<T>(name)` — kept here for ergonomics.
    pub fn handle<T: BankColumnType>(&self, name: &str) -> Result<ColumnHandle<T>> {
        self.schema.handle::<T>(name)
    }

    /// Read one cell as `T`. Infallible: returns `T::default()` (`0` /
    /// `0.0`) if the column is missing, the wire type doesn't match `T`,
    /// or the row is out of range.
    ///
    /// Type-inferred from the binding site:
    /// ```ignore
    /// let pid: i32 = bank.get("pid", row);
    /// let px:  f32 = bank.get("px",  row);
    /// ```
    ///
    /// Use [`Self::col`] / [`Self::read`] for strict access (error on
    /// missing column / type mismatch) and bulk reads.
    #[inline]
    pub fn get<T: BankColumnType + Default>(&self, name: &str, row: u32) -> T {
        let Some(col) = self.schema.column_index(name) else {
            return T::default();
        };
        let entry = &self.schema.entries()[col];
        if entry.ty != T::DATA_TYPE || row >= self.rows {
            return T::default();
        }
        let offset = self.schema.cell_byte_offset(col, row, self.rows) as usize;
        // SAFETY: `col` came from `column_index` so it's a valid entry;
        // `row < self.rows`; the bank's data buffer is exactly
        // `rows * row_size` bytes and the entry's row_offset places the
        // cell entirely within bounds. We read unaligned because event
        // boundaries inside a record don't guarantee `T`'s natural
        // alignment for 8-byte primitives.
        unsafe { (self.data.as_ptr().add(offset) as *const T).read_unaligned() }
    }

    /// Borrow a single row's view.
    pub fn row(&self, row: u32) -> Option<RowView<'b>> {
        if row >= self.rows {
            return None;
        }
        Some(RowView {
            schema: self.schema,
            data: self.data,
            rows: self.rows,
            row,
        })
    }

    /// Iterate rows.
    pub fn rows_iter(&self) -> impl Iterator<Item = RowView<'b>> {
        let schema = self.schema;
        let data = self.data;
        let rows = self.rows;
        (0..rows).map(move |row| RowView {
            schema,
            data,
            rows,
            row,
        })
    }

    fn check_type(&self, col: usize, expected: DataType) -> Result<()> {
        let actual = self.schema.entries()[col].ty;
        if actual != expected {
            return Err(HipoError::TypeMismatch {
                schema: self.schema.name().to_string(),
                column: self.schema.entries()[col].name.clone(),
                expected: expected.name(),
                actual: actual.name(),
            });
        }
        Ok(())
    }

    /// Cast a column's bytes to a typed slice with full type-checking.
    /// Used by [`Self::col`] / [`Self::col_at`] where the column index
    /// came from a name lookup.
    #[inline]
    fn cast_column<T: BankColumnType>(&self, col: usize) -> Result<Cow<'b, [T]>> {
        self.check_type(col, T::DATA_TYPE)?;
        Ok(self.cast_column_unchecked::<T>(col))
    }

    /// Cast a column's bytes to a typed slice **without** the type check.
    /// Caller is responsible for ensuring the column's wire type matches
    /// `T` — typically by going through [`ColumnHandle`].
    #[inline]
    fn cast_column_unchecked<T: BankColumnType>(&self, col: usize) -> Cow<'b, [T]> {
        let bytes = self.col_bytes(col);
        let rows = self.rows as usize;
        if rows == 0 {
            return Cow::Borrowed(&[]);
        }
        let ptr = bytes.as_ptr();
        if (ptr as usize).is_multiple_of(std::mem::align_of::<T>()) {
            // SAFETY: rows > 0; `bytes.len() == rows * size_of::<T>()` by
            // construction; alignment just checked; HIPO files are
            // little-endian, matching all supported targets.
            let slice = unsafe { std::slice::from_raw_parts(ptr as *const T, rows) };
            Cow::Borrowed(slice)
        } else {
            // Misaligned — copy with read_unaligned. Same fallback the C++
            // reader uses via memcpy.
            let mut owned = Vec::with_capacity(rows);
            let elem = std::mem::size_of::<T>();
            for i in 0..rows {
                // SAFETY: bounds: `i * elem + elem <= bytes.len()` because
                // `bytes.len() == rows * elem` and `i < rows`. The unaligned
                // read is sound for any T: Copy.
                let v = unsafe { (ptr.add(i * elem) as *const T).read_unaligned() };
                owned.push(v);
            }
            Cow::Owned(owned)
        }
    }
}

// ---- RowView --------------------------------------------------------------

use crate::wire::bytes::{read_f32_le, read_f64_le, read_i16_le, read_u32_le, read_u64_le};

/// Single-row view, with lossy/coerced scalar accessors.
///
/// Missing columns return a zero default rather than an error — mirroring
/// the C++ `noexcept` helpers. For strict access use [`Bank::col`] +
/// indexing.
#[derive(Debug, Clone, Copy)]
pub struct RowView<'b> {
    schema: &'b Schema,
    data: &'b [u8],
    rows: u32,
    row: u32,
}

impl<'b> RowView<'b> {
    pub fn schema(&self) -> &'b Schema {
        self.schema
    }

    pub fn row(&self) -> u32 {
        self.row
    }

    #[inline]
    fn cell_offset(&self, col: usize) -> usize {
        self.schema.cell_byte_offset(col, self.row, self.rows) as usize
    }

    #[inline]
    pub fn i32(&self, name: &str) -> Option<i32> {
        let col = self.schema.column_index(name)?;
        let entry = &self.schema.entries()[col];
        let off = self.cell_offset(col);
        Some(match entry.ty {
            DataType::Byte => self.data[off] as i8 as i32,
            DataType::Short => read_i16_le(self.data, off) as i32,
            DataType::Int => read_u32_le(self.data, off) as i32,
            _ => return None,
        })
    }

    #[inline]
    pub fn i64(&self, name: &str) -> Option<i64> {
        let col = self.schema.column_index(name)?;
        let entry = &self.schema.entries()[col];
        let off = self.cell_offset(col);
        Some(match entry.ty {
            DataType::Long => read_u64_le(self.data, off) as i64,
            DataType::Int => read_u32_le(self.data, off) as i32 as i64,
            DataType::Short => read_i16_le(self.data, off) as i64,
            DataType::Byte => self.data[off] as i8 as i64,
            _ => return None,
        })
    }

    #[inline]
    pub fn i16(&self, name: &str) -> Option<i16> {
        let col = self.schema.column_index(name)?;
        let entry = &self.schema.entries()[col];
        let off = self.cell_offset(col);
        Some(match entry.ty {
            DataType::Byte => self.data[off] as i8 as i16,
            DataType::Short => read_i16_le(self.data, off),
            _ => return None,
        })
    }

    #[inline]
    pub fn i8(&self, name: &str) -> Option<i8> {
        let col = self.schema.column_index(name)?;
        let entry = &self.schema.entries()[col];
        if !matches!(entry.ty, DataType::Byte) {
            return None;
        }
        Some(self.data[self.cell_offset(col)] as i8)
    }

    #[inline]
    pub fn f32(&self, name: &str) -> Option<f32> {
        let col = self.schema.column_index(name)?;
        let entry = &self.schema.entries()[col];
        let off = self.cell_offset(col);
        Some(match entry.ty {
            DataType::Float => read_f32_le(self.data, off),
            DataType::Double => read_f64_le(self.data, off) as f32,
            _ => return None,
        })
    }

    #[inline]
    pub fn f64(&self, name: &str) -> Option<f64> {
        let col = self.schema.column_index(name)?;
        let entry = &self.schema.entries()[col];
        let off = self.cell_offset(col);
        Some(match entry.ty {
            DataType::Double => read_f64_le(self.data, off),
            DataType::Float => f64::from(read_f32_le(self.data, off)),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{DataType, Schema};

    fn make_bank_data(rows: u32) -> (Schema, Vec<u8>) {
        let s = Schema::from_columns(
            "REC::Particle",
            300,
            1,
            [
                ("pid".into(), DataType::Int),
                ("px".into(), DataType::Float),
                ("charge".into(), DataType::Byte),
            ],
        );

        let mut data = Vec::new();
        for i in 0..rows {
            data.extend_from_slice(&((i as i32) * 10).to_le_bytes());
        }
        for i in 0..rows {
            let v = i as f32 + 0.5;
            data.extend_from_slice(&v.to_le_bytes());
        }
        for i in 0..rows {
            data.push((i as i8 - 1) as u8);
        }
        (s, data)
    }

    #[test]
    fn rows_inferred_from_size() {
        let (s, data) = make_bank_data(5);
        let bank = Bank::new(&s, &data).unwrap();
        assert_eq!(bank.rows(), 5);
    }

    #[test]
    fn generic_col_returns_typed_slices() {
        let (s, data) = make_bank_data(4);
        let bank = Bank::new(&s, &data).unwrap();
        let pids = bank.col::<i32>("pid").unwrap();
        assert_eq!(&*pids, &[0, 10, 20, 30]);
        let pxs = bank.col::<f32>("px").unwrap();
        assert_eq!(&*pxs, &[0.5, 1.5, 2.5, 3.5]);
        let chg = bank.col::<i8>("charge").unwrap();
        assert_eq!(&*chg, &[-1, 0, 1, 2]);
    }

    #[test]
    fn handle_round_trip() {
        let (s, data) = make_bank_data(3);
        let bank = Bank::new(&s, &data).unwrap();
        let h = s.handle::<f32>("px").unwrap();
        assert_eq!(&*bank.read(h), &[0.5, 1.5, 2.5]);
    }

    #[test]
    fn get_scalar_type_inferred() {
        let (s, data) = make_bank_data(4);
        let bank = Bank::new(&s, &data).unwrap();

        // Type-inferred from the binding.
        let pid0: i32 = bank.get("pid", 0);
        let pid3: i32 = bank.get("pid", 3);
        let px2: f32 = bank.get("px", 2);
        let chg1: i8 = bank.get("charge", 1);
        assert_eq!(pid0, 0);
        assert_eq!(pid3, 30);
        assert_eq!(px2, 2.5);
        assert_eq!(chg1, 0);
    }

    #[test]
    fn get_defaults_on_miss_or_mismatch() {
        let (s, data) = make_bank_data(2);
        let bank = Bank::new(&s, &data).unwrap();

        // Unknown column → default.
        let missing: i32 = bank.get("nope", 0);
        assert_eq!(missing, 0);

        // Type mismatch (pid is i32, ask for f32) → default.
        let mismatched: f32 = bank.get("pid", 0);
        assert_eq!(mismatched, 0.0);

        // Row out of range → default.
        let oob: i32 = bank.get("pid", 99);
        assert_eq!(oob, 0);
    }

    #[test]
    fn row_view_scalar() {
        let (s, data) = make_bank_data(3);
        let bank = Bank::new(&s, &data).unwrap();
        let row = bank.row(1).unwrap();
        assert_eq!(row.i32("pid"), Some(10));
        assert_eq!(row.f32("px"), Some(1.5));
        assert_eq!(row.i32("nope"), None);
    }

    #[test]
    fn rows_iter_lengths() {
        let (s, data) = make_bank_data(4);
        let bank = Bank::new(&s, &data).unwrap();
        let rows: Vec<u32> = bank.rows_iter().map(|r| r.row()).collect();
        assert_eq!(rows, vec![0, 1, 2, 3]);
    }

    #[test]
    fn type_mismatch_errors() {
        let (s, data) = make_bank_data(2);
        let bank = Bank::new(&s, &data).unwrap();
        let err = bank.col::<f32>("pid").unwrap_err();
        assert!(matches!(err, HipoError::TypeMismatch { .. }));
    }

    #[test]
    fn corrupt_size_rejected() {
        let (s, mut data) = make_bank_data(3);
        data.push(0);
        let err = Bank::new(&s, &data).unwrap_err();
        assert!(matches!(err, HipoError::CorruptRecord { .. }));
    }

    #[test]
    fn empty_bank() {
        let s = Schema::from_columns("X", 1, 1, [("a".into(), DataType::Int)]);
        let bank = Bank::new(&s, &[]).unwrap();
        assert_eq!(bank.rows(), 0);
        assert_eq!(&*bank.col::<i32>("a").unwrap(), &[] as &[i32]);
    }

    #[test]
    fn misaligned_8byte_col_copies_into_owned() {
        // Build a buffer where the i64 column starts at an odd offset.
        let s = Schema::from_columns(
            "Y",
            1,
            1,
            [("tag".into(), DataType::Byte), ("v".into(), DataType::Long)],
        );
        // 2 rows: row_size = 1 + 8 = 9 bytes; column-major: 2 bytes of tag,
        // then 16 bytes of long. The long column starts at byte 2 — not
        // 8-aligned, so cast falls back to copy.
        let mut data = Vec::new();
        data.push(0x11_u8);
        data.push(0x22_u8);
        data.extend_from_slice(&0x1122_3344_5566_7788_i64.to_le_bytes());
        data.extend_from_slice(&(-1_i64).to_le_bytes());
        let bank = Bank::new(&s, &data).unwrap();
        let vs = bank.col::<i64>("v").unwrap();
        assert_eq!(vs.len(), 2);
        assert_eq!(vs[0], 0x1122_3344_5566_7788);
        assert_eq!(vs[1], -1);
    }
}
