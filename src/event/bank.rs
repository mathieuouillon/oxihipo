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
//!
//! # Which accessor?
//!
//! | Need | Method | Returns | On miss |
//! |---|---|---|---|
//! | one cell, lenient | [`Bank::get`] | `T` | `T::default()` |
//! | whole column, strict | [`Bank::col`] | `Result<Cow<[T]>>` | `Err` |
//! | whole column, hot loop | [`Bank::read`] | `Cow<[T]>` | infallible |
//! | one cell, hot loop | [`Bank::read_handle_or_default`] | `T` | `T::default()` |
//! | one row of a runtime-length array column | [`Bank::array_at`] | `Result<Cow<[T]>>` | `Err` |
//!
//! Rule of thumb: [`get`](Bank::get) for occasional lenient reads,
//! [`col`](Bank::col) for bulk strict reads, and resolve a [`ColumnHandle`]
//! once + [`read`](Bank::read) / [`read_handle_or_default`](Bank::read_handle_or_default)
//! when the same column is read across many events. To skip the
//! `ev.bank(name)?` step, the same `get` / `col` exist directly on the event
//! ([`EventCtx::get`](crate::event::EventCtx::get) /
//! [`EventCtx::col`](crate::event::EventCtx::col)). To read several columns
//! per row, prefer a typed row via [`bank_row!`](crate::bank_row) +
//! [`EventCtx::rows`](crate::event::EventCtx::rows).

use std::borrow::Cow;

use crate::error::{HipoError, Result};
use crate::schema::{BankColumnType, ColumnHandle, Schema};

/// Cast `bytes` (exactly `count * size_of::<T>()` bytes) to `&[T]`,
/// borrowing zero-copy when the source is `T`-aligned and falling back to
/// an element-by-element copy otherwise. Both paths go through safe
/// `bytemuck` calls — the alignment fast path is the same single
/// reinterpretation the old hand-rolled `from_raw_parts` did, and the
/// misaligned path matches the C++ reader's per-cell `memcpy`.
#[inline]
fn cast_cells<T: bytemuck::Pod>(bytes: &[u8], count: usize) -> Cow<'_, [T]> {
    if count == 0 {
        return Cow::Borrowed(&[]);
    }
    match bytemuck::try_cast_slice::<u8, T>(bytes) {
        Ok(slice) => Cow::Borrowed(slice),
        Err(_) => {
            let elem = std::mem::size_of::<T>();
            let owned: Vec<T> = (0..count)
                .map(|i| bytemuck::pod_read_unaligned::<T>(&bytes[i * elem..i * elem + elem]))
                .collect();
            Cow::Owned(owned)
        }
    }
}

/// A read-only bank backed by a borrowed byte slice.
///
/// `Copy` — it's two references plus a row count, so callers (and the
/// per-event bank cache on [`EventCtx`](crate::event::EventCtx)) can pass it
/// around freely.
#[derive(Debug, Clone, Copy)]
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

    /// Raw bytes of column `col`. Length is
    /// `rows * ty.size() * length` (array columns are stored
    /// element-major within each row). Internal: used by `cast_column`.
    pub(crate) fn col_bytes(&self, col: usize) -> &'b [u8] {
        let entry = &self.schema.entries()[col];
        let start = self.schema.column_byte_offset(col, self.rows) as usize;
        let len = self.rows as usize * entry.ty.size() * entry.length as usize;
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
            "ColumnHandle resolved against a different schema (type)"
        );
        debug_assert_eq!(
            self.schema.entries()[col].length,
            T::LENGTH,
            "ColumnHandle resolved against a different schema (length)"
        );
        self.cast_column_unchecked::<T>(col)
    }

    /// Read one cell through a pre-resolved [`ColumnHandle`].
    /// `T::default()` if the handle is a
    /// [placeholder](ColumnHandle::placeholder) or `row` is out of
    /// range. Skips name lookup, type check, and length check — the
    /// handle already validated everything at resolve time.
    ///
    /// This is the hot-path scalar accessor used by the typed-row
    /// catalog ([`BankRow::from_row_with_handles`](crate::event::BankRow)).
    #[inline]
    pub fn read_handle_or_default<T: BankColumnType + Default>(
        &self,
        handle: ColumnHandle<T>,
        row: u32,
    ) -> T {
        if handle.is_placeholder() || row >= self.rows {
            return T::default();
        }
        let offset = self
            .schema
            .cell_byte_offset(handle.column_index(), row, self.rows) as usize;
        // Safe unaligned read: the handle was resolved against this
        // schema and `row < self.rows`, so the cell is in bounds.
        let size = std::mem::size_of::<T>();
        bytemuck::pod_read_unaligned::<T>(&self.data[offset..offset + size])
    }

    /// Read one cell as `T`. Infallible: returns `T::default()` if the
    /// column is missing, the wire type doesn't match `T`'s element
    /// type, the column's per-row length doesn't match `T::LENGTH`, or
    /// the row is out of range.
    ///
    /// Type-inferred from the binding site:
    /// ```ignore
    /// let pid: i32        = bank.get("pid", row);    // scalar column
    /// let px:  f32        = bank.get("px",  row);    // scalar column
    /// let cov: [f32; 16]  = bank.get("cov", row);    // array column (F#16)
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
        if entry.ty != T::DATA_TYPE || entry.length != T::LENGTH || row >= self.rows {
            return T::default();
        }
        let offset = self.schema.cell_byte_offset(col, row, self.rows) as usize;
        // Safe unaligned read: `col` came from `column_index`, `row <
        // self.rows`, and the type/length checks above guarantee the
        // `size_of::<T>()` bytes at `offset` lie within the cell. For
        // `T = [U; N]` this reads `N * size_of::<U>()` bytes — exactly
        // the row's array.
        let size = std::mem::size_of::<T>();
        bytemuck::pod_read_unaligned::<T>(&self.data[offset..offset + size])
    }

    /// Read one row's array bytes for an array column with runtime
    /// length. Returns a borrowed slice when alignment permits, otherwise
    /// a fresh owned copy. Errors if the column doesn't exist, the wire
    /// type doesn't match `T`, or `row` is out of range.
    ///
    /// This is the runtime escape hatch: useful when the array length
    /// isn't known at compile time (e.g. a generic dump tool walking a
    /// dictionary). For known-N hot loops, prefer
    /// [`Self::get`]::<[T; N]> or [`Self::read`]::<[T; N]>.
    pub fn array_at<T: crate::schema::BankScalarType>(
        &self,
        name: &str,
        row: u32,
    ) -> Result<Cow<'b, [T]>> {
        let col = self.schema.require_column(name)?;
        let entry = &self.schema.entries()[col];
        if entry.ty != T::DATA_TYPE {
            return Err(HipoError::TypeMismatch {
                schema: self.schema.name().to_string(),
                column: name.to_string(),
                expected: T::DATA_TYPE.name(),
                actual: entry.ty.name(),
            });
        }
        if row >= self.rows {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Bank::array_at: row index out of range",
            });
        }
        let len = entry.length as usize;
        let elem = std::mem::size_of::<T>();
        let offset = self.schema.cell_byte_offset(col, row, self.rows) as usize;
        let bytes = &self.data[offset..offset + len * elem];
        Ok(cast_cells::<T>(bytes, len))
    }

    /// Check both element type AND per-row length against the typed
    /// view requested by `T`. Length checks ensure that
    /// `bank.col::<f32>(name)` rejects an array column (`length > 1`)
    /// and that `bank.col::<[f32; 32]>(name)` rejects a scalar or
    /// wrong-N column.
    fn check_column<T: BankColumnType>(&self, col: usize) -> Result<()> {
        let entry = &self.schema.entries()[col];
        if entry.ty != T::DATA_TYPE {
            return Err(HipoError::TypeMismatch {
                schema: self.schema.name().to_string(),
                column: entry.name.clone(),
                expected: T::DATA_TYPE.name(),
                actual: entry.ty.name(),
            });
        }
        if entry.length != T::LENGTH {
            return Err(HipoError::ColumnLengthMismatch {
                schema: self.schema.name().to_string(),
                column: entry.name.clone(),
                expected: T::LENGTH,
                actual: entry.length,
            });
        }
        Ok(())
    }

    /// Cast a column's bytes to a typed slice with full type-checking.
    /// Used by [`Self::col`] where the column index came from a name
    /// lookup.
    #[inline]
    fn cast_column<T: BankColumnType>(&self, col: usize) -> Result<Cow<'b, [T]>> {
        self.check_column::<T>(col)?;
        Ok(self.cast_column_unchecked::<T>(col))
    }

    /// Cast a column's bytes to a typed slice **without** the type check.
    /// Caller is responsible for ensuring the column's wire type matches
    /// `T` — typically by going through [`ColumnHandle`].
    #[inline]
    fn cast_column_unchecked<T: BankColumnType>(&self, col: usize) -> Cow<'b, [T]> {
        // `col_bytes` returns exactly `rows * size_of::<T>()` bytes for the
        // matching `T`, so `cast_cells` borrows zero-copy on aligned data
        // (the common case for 4-byte types) and copies otherwise.
        cast_cells::<T>(self.col_bytes(col), self.rows as usize)
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
                ("pid".into(), DataType::Int, 1),
                ("px".into(), DataType::Float, 1),
                ("charge".into(), DataType::Byte, 1),
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
    fn type_mismatch_errors() {
        let (s, data) = make_bank_data(2);
        let bank = Bank::new(&s, &data).unwrap();
        let err = bank.col::<f32>("pid").unwrap_err();
        assert!(matches!(err, HipoError::TypeMismatch { .. }));
    }

    /// Build a bank with `px/F#3` and three known rows. Used by the
    /// array-column read tests.
    fn make_array_bank() -> (Schema, Vec<u8>) {
        use crate::wire::bytes::write_u32_le;
        let s = Schema::from_columns(
            "X",
            1,
            1,
            [
                ("pid".into(), DataType::Int, 1u32),
                ("px".into(), DataType::Float, 3u32),
            ],
        );
        // 3 rows. Column-major layout:
        //   pid column: 3 * 4 = 12 bytes
        //   px column:  3 * 12 = 36 bytes (3 floats per row)
        let rows: u32 = 3;
        let pid_bytes = rows as usize * 4;
        let px_bytes = rows as usize * 12;
        let mut data = vec![0u8; pid_bytes + px_bytes];
        // Write pid column: 11, 22, 33.
        write_u32_le(&mut data, 0, 11);
        write_u32_le(&mut data, 4, 22);
        write_u32_le(&mut data, 8, 33);
        // Write px column: [0.1,0.2,0.3], [1.0,1.1,1.2], [2.0,2.1,2.2].
        let rows_data: [[f32; 3]; 3] = [[0.1, 0.2, 0.3], [1.0, 1.1, 1.2], [2.0, 2.1, 2.2]];
        let mut off = pid_bytes;
        for row in &rows_data {
            for v in row {
                data[off..off + 4].copy_from_slice(&v.to_le_bytes());
                off += 4;
            }
        }
        (s, data)
    }

    #[test]
    fn col_array_typed() {
        let (s, data) = make_array_bank();
        let bank = Bank::new(&s, &data).unwrap();
        let arrays = bank.col::<[f32; 3]>("px").unwrap();
        assert_eq!(arrays.len(), 3);
        assert_eq!(arrays[0], [0.1, 0.2, 0.3]);
        assert_eq!(arrays[2], [2.0, 2.1, 2.2]);
    }

    #[test]
    fn get_array_typed() {
        let (s, data) = make_array_bank();
        let bank = Bank::new(&s, &data).unwrap();
        let row1: [f32; 3] = bank.get("px", 1);
        assert_eq!(row1, [1.0, 1.1, 1.2]);
    }

    #[test]
    fn get_array_wrong_length_returns_default() {
        let (s, data) = make_array_bank();
        let bank = Bank::new(&s, &data).unwrap();
        // px is F#3; ask for [f32; 4] → default ([0; 4]).
        let row: [f32; 4] = bank.get("px", 0);
        assert_eq!(row, [0.0; 4]);
    }

    #[test]
    fn array_at_runtime() {
        let (s, data) = make_array_bank();
        let bank = Bank::new(&s, &data).unwrap();
        let row2 = bank.array_at::<f32>("px", 2).unwrap();
        assert_eq!(&*row2, &[2.0, 2.1, 2.2]);
        // Type mismatch.
        let err = bank.array_at::<i32>("px", 0).unwrap_err();
        assert!(matches!(err, HipoError::TypeMismatch { .. }));
        // Out of range.
        let err = bank.array_at::<f32>("px", 99).unwrap_err();
        assert!(matches!(err, HipoError::CorruptRecord { .. }));
    }

    #[test]
    fn col_array_wrong_length_rejected() {
        let (s, data) = make_array_bank();
        let bank = Bank::new(&s, &data).unwrap();
        let err = bank.col::<[f32; 4]>("px").unwrap_err();
        assert!(matches!(err, HipoError::ColumnLengthMismatch { .. }));
        // Calling scalar col on an array column also rejected.
        let err = bank.col::<f32>("px").unwrap_err();
        assert!(matches!(err, HipoError::ColumnLengthMismatch { .. }));
    }

    #[test]
    fn handle_resolution_checks_length() {
        let (s, _) = make_array_bank();
        // Correct N.
        let _ok = s.handle::<[f32; 3]>("px").unwrap();
        // Wrong N.
        let err = s.handle::<[f32; 4]>("px").unwrap_err();
        assert!(matches!(err, HipoError::ColumnLengthMismatch { .. }));
        // Scalar handle on array column.
        let err = s.handle::<f32>("px").unwrap_err();
        assert!(matches!(err, HipoError::ColumnLengthMismatch { .. }));
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
        let s = Schema::from_columns("X", 1, 1, [("a".into(), DataType::Int, 1)]);
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
            [
                ("tag".into(), DataType::Byte, 1),
                ("v".into(), DataType::Long, 1),
            ],
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
