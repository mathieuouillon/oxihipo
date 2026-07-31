//! Internal write-side builders.
//!
//! The user-facing write API lives in [`crate::write`] (`Writer`, `BankWriter`,
//! `RowWriter`). These types are the lower-level primitives those builders
//! delegate to — they're exposed at crate scope so callers who want raw
//! byte-level control (e.g. building a record outside the `Writer`) can
//! reach them.

use crate::error::{HipoError, Result};
use crate::schema::{BankScalarType, DataType, Schema};
use crate::wire::constants::*;

/// Build a HIPO bank (a single structure) one row at a time.
///
/// Internal storage is already column-major; [`Self::finish`] is a
/// constant-time serialisation (no transpose).
#[derive(Debug)]
pub struct BankBuilder<'s> {
    schema: &'s Schema,
    /// One buffer per column. Length is always
    /// `rows * entry.ty.size() * entry.length` — the `length` factor is what
    /// makes an array column (`name/T#N`) N times wider than a scalar one.
    columns: Vec<Vec<u8>>,
    rows: u32,
}

impl<'s> BankBuilder<'s> {
    pub fn new(schema: &'s Schema) -> Self {
        let columns = vec![Vec::new(); schema.num_columns()];
        Self {
            schema,
            columns,
            rows: 0,
        }
    }

    pub fn with_row_capacity(schema: &'s Schema, rows: u32) -> Self {
        let columns = schema
            .entries()
            .iter()
            // `* e.length` matters: without it an array column reserves 1/N of
            // what it needs and regrows from there. The row layout in `push_row`
            // below is the mirror of this product, so they must agree.
            .map(|e| Vec::with_capacity(rows as usize * e.ty.size() * e.length as usize))
            .collect();
        Self {
            schema,
            columns,
            rows: 0,
        }
    }

    pub fn schema(&self) -> &Schema {
        self.schema
    }

    pub fn rows(&self) -> u32 {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// Append a zero-filled row; subsequent `set_*` calls modify it.
    pub fn push_row(&mut self) -> &mut Self {
        for (col, entry) in self.columns.iter_mut().zip(self.schema.entries()) {
            col.extend(std::iter::repeat_n(
                0u8,
                entry.ty.size() * entry.length as usize,
            ));
        }
        self.rows += 1;
        self
    }

    pub fn push_rows(&mut self, n: u32) -> &mut Self {
        for _ in 0..n {
            self.push_row();
        }
        self
    }

    /// Build over caller-owned column buffers instead of fresh ones.
    ///
    /// The buffers are cleared and resized to the schema's column count, so
    /// any set can be handed to any schema. Together with
    /// [`Self::into_buffers`] this makes a free list possible: the writer
    /// recycles one set across every bank of every event, which is what takes
    /// a 47-bank CLAS12-shaped event from 666 allocations to none.
    pub fn with_buffers(schema: &'s Schema, mut buffers: Vec<Vec<u8>>) -> Self {
        buffers.truncate(schema.num_columns());
        for c in buffers.iter_mut() {
            c.clear();
        }
        buffers.resize_with(schema.num_columns(), Vec::new);
        Self {
            schema,
            columns: buffers,
            rows: 0,
        }
    }

    /// Reclaim the column buffers, keeping their capacity, to hand to the
    /// next [`Self::with_buffers`].
    pub fn into_buffers(self) -> Vec<Vec<u8>> {
        self.columns
    }

    /// Reset to zero rows without freeing column buffers (re-use the
    /// builder across records).
    pub fn reset(&mut self) {
        for col in self.columns.iter_mut() {
            col.clear();
        }
        self.rows = 0;
    }

    fn check_col(&self, name: &str, expected: DataType) -> Result<usize> {
        let col = self.schema.require_column(name)?;
        let entry = &self.schema.entries()[col];
        if entry.ty != expected {
            return Err(HipoError::TypeMismatch {
                schema: self.schema.name().to_string(),
                column: name.to_string(),
                expected: expected.name(),
                actual: entry.ty.name(),
            });
        }
        Ok(col)
    }

    /// Like [`Self::check_col`] but also requires the column to be a
    /// scalar (length == 1). Scalar setters use this to fail loudly when
    /// they're aimed at an array column.
    fn check_col_scalar(&self, name: &str, expected: DataType) -> Result<usize> {
        let col = self.check_col(name, expected)?;
        let entry = &self.schema.entries()[col];
        if entry.length != 1 {
            return Err(HipoError::ColumnLengthMismatch {
                schema: self.schema.name().to_string(),
                column: name.to_string(),
                expected: 1,
                actual: entry.length,
            });
        }
        Ok(col)
    }

    fn last_row(&self) -> Result<u32> {
        self.rows.checked_sub(1).ok_or(HipoError::InvalidUsage {
            what: "BankBuilder: set_* called before push_row()",
        })
    }

    pub fn set_i32(&mut self, name: &str, value: i32) -> Result<&mut Self> {
        let row = self.last_row()?;
        self.set_i32_at(name, row, value)
    }

    /// Bounds-check a random-access row index. The `set_*_at` setters index
    /// the column buffer directly, so an out-of-range `row` would slice past
    /// the end — the one failure mode a random-access writer actually hits.
    /// Every other error here is a `Result`, so this is too.
    #[inline]
    fn check_row(&self, row: u32) -> Result<()> {
        if row >= self.rows {
            return Err(HipoError::InvalidUsage {
                what: "BankBuilder: row index out of range (call push_rows first)",
            });
        }
        Ok(())
    }

    pub fn set_i32_at(&mut self, name: &str, row: u32, value: i32) -> Result<&mut Self> {
        let col = self.check_col_scalar(name, DataType::Int)?;
        self.check_row(row)?;
        let bytes = &mut self.columns[col];
        let off = row as usize * 4;
        bytes[off..off + 4].copy_from_slice(&value.to_le_bytes());
        Ok(self)
    }

    pub fn set_i64(&mut self, name: &str, value: i64) -> Result<&mut Self> {
        let row = self.last_row()?;
        self.set_i64_at(name, row, value)
    }

    pub fn set_i64_at(&mut self, name: &str, row: u32, value: i64) -> Result<&mut Self> {
        let col = self.check_col_scalar(name, DataType::Long)?;
        self.check_row(row)?;
        let bytes = &mut self.columns[col];
        let off = row as usize * 8;
        bytes[off..off + 8].copy_from_slice(&value.to_le_bytes());
        Ok(self)
    }

    pub fn set_i16(&mut self, name: &str, value: i16) -> Result<&mut Self> {
        let row = self.last_row()?;
        self.set_i16_at(name, row, value)
    }

    pub fn set_i16_at(&mut self, name: &str, row: u32, value: i16) -> Result<&mut Self> {
        let col = self.check_col_scalar(name, DataType::Short)?;
        self.check_row(row)?;
        let bytes = &mut self.columns[col];
        let off = row as usize * 2;
        bytes[off..off + 2].copy_from_slice(&value.to_le_bytes());
        Ok(self)
    }

    pub fn set_i8(&mut self, name: &str, value: i8) -> Result<&mut Self> {
        let row = self.last_row()?;
        self.set_i8_at(name, row, value)
    }

    pub fn set_i8_at(&mut self, name: &str, row: u32, value: i8) -> Result<&mut Self> {
        let col = self.check_col_scalar(name, DataType::Byte)?;
        self.check_row(row)?;
        let bytes = &mut self.columns[col];
        bytes[row as usize] = value as u8;
        Ok(self)
    }

    pub fn set_f32(&mut self, name: &str, value: f32) -> Result<&mut Self> {
        let row = self.last_row()?;
        self.set_f32_at(name, row, value)
    }

    pub fn set_f32_at(&mut self, name: &str, row: u32, value: f32) -> Result<&mut Self> {
        let col = self.check_col_scalar(name, DataType::Float)?;
        self.check_row(row)?;
        let bytes = &mut self.columns[col];
        let off = row as usize * 4;
        bytes[off..off + 4].copy_from_slice(&value.to_le_bytes());
        Ok(self)
    }

    pub fn set_f64(&mut self, name: &str, value: f64) -> Result<&mut Self> {
        let row = self.last_row()?;
        self.set_f64_at(name, row, value)
    }

    pub fn set_f64_at(&mut self, name: &str, row: u32, value: f64) -> Result<&mut Self> {
        let col = self.check_col_scalar(name, DataType::Double)?;
        self.check_row(row)?;
        let bytes = &mut self.columns[col];
        let off = row as usize * 8;
        bytes[off..off + 8].copy_from_slice(&value.to_le_bytes());
        Ok(self)
    }

    /// Write an array of `T` into the last-pushed row's `name` slot.
    /// The schema must declare `name` as an array column of element
    /// type `T` and length `values.len()`.
    pub fn set_array<T: BankScalarType>(&mut self, name: &str, values: &[T]) -> Result<&mut Self> {
        let row = self.last_row()?;
        self.set_array_at(name, row, values)
    }

    /// Like [`Self::set_array`] but at an explicit row index.
    pub fn set_array_at<T: BankScalarType>(
        &mut self,
        name: &str,
        row: u32,
        values: &[T],
    ) -> Result<&mut Self> {
        let col = self.check_col(name, T::DATA_TYPE)?;
        self.check_row(row)?;
        let entry = &self.schema.entries()[col];
        let expected_len = entry.length as usize;
        if values.len() != expected_len {
            return Err(HipoError::ColumnLengthMismatch {
                schema: self.schema.name().to_string(),
                column: name.to_string(),
                expected: entry.length,
                actual: values.len() as u32,
            });
        }
        let item_size = T::DATA_TYPE.size();
        let row_bytes = item_size * expected_len;
        let off = row as usize * row_bytes;
        let bytes = &mut self.columns[col];
        for (i, v) in values.iter().enumerate() {
            let start = off + i * item_size;
            v.write_le(&mut bytes[start..start + item_size]);
        }
        Ok(self)
    }

    /// Serialise as `[structure header | column-major data]`. Byte-
    /// compatible with what [`Bank::new`](crate::event::Bank::new) decodes.
    ///
    /// # Errors
    ///
    /// [`HipoError::BankTooLarge`] once the bank's data reaches 2^24 bytes.
    /// The structure length word holds the size in its low 24 bits and the
    /// composite `header_size` in its top byte, so a larger bank cannot be
    /// described at all.
    ///
    /// This used to be written anyway, truncated and without an error:
    /// 5,000,000 `Int` rows read back as 805,696, and exactly 2^24 bytes read
    /// back as **zero** rows with the overflowed top byte reinterpreted as a
    /// composite header. `Writer::finish` returned `Ok` in both cases, so the
    /// loss was invisible until the file was read.
    /// Serialise into a caller-owned buffer: the 8-byte structure header
    /// followed by the column data, **appended** to `out`.
    ///
    /// Takes `&self`, which is the whole point. [`Self::finish`] consumes the
    /// builder, so a builder could never be reused and [`Self::reset`] had no
    /// possible caller. Pair this with `reset` to assemble every event of a
    /// file through one set of builders: measured over 64 events of the
    /// realistic CLAS12 shape (47 banks x 8 columns x 3 rows), assembly goes
    /// from 666 allocations per event to none.
    pub fn finish_into(&self, out: &mut Vec<u8>) -> Result<()> {
        let data_size: usize = self.columns.iter().map(|c| c.len()).sum();
        if data_size > STRUCT_SIZE_MASK as usize {
            return Err(HipoError::BankTooLarge {
                schema: self.schema.name().to_string(),
                size: data_size,
                max: STRUCT_SIZE_MASK as usize,
            });
        }
        out.reserve(BANK_STRUCTURE_SIZE + data_size);
        out.extend_from_slice(&self.schema.group().to_le_bytes());
        out.push(self.schema.item());
        out.push(11); // basic bank type code (matches C++ writer)
        out.extend_from_slice(&(data_size as u32).to_le_bytes());
        for col in &self.columns {
            out.extend_from_slice(col);
        }
        Ok(())
    }

    /// Serialise into a fresh `Vec`. Thin wrapper over [`Self::finish_into`];
    /// prefer that one in a loop, where the allocation is per event.
    pub fn finish(self) -> Result<Vec<u8>> {
        let data_size: usize = self.columns.iter().map(|c| c.len()).sum();
        let mut out = Vec::with_capacity(BANK_STRUCTURE_SIZE + data_size);
        self.finish_into(&mut out)?;
        Ok(out)
    }
}

/// Build a HIPO event by composing one or more banks.
///
/// Events are a 16-byte header followed by a run of structures. The total
/// event size is written into the header at [`Self::finish`] so callers
/// can recover it via `Event::size`.
#[derive(Debug, Default)]
pub struct EventBuilder {
    structures: Vec<u8>,
    tag: u32,
}

impl EventBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the per-event tag (`EH_TAG`). Accepts a raw `u32` or a
    /// [`TagSet`](crate::TagSet) / named `tag_flags!` flag.
    pub fn with_tag(mut self, tag: impl Into<u32>) -> Self {
        self.tag = tag.into();
        self
    }

    pub fn tag(&self) -> u32 {
        self.tag
    }

    /// Set the per-event tag (`EH_TAG`) in place; accepts `u32` or a
    /// [`TagSet`](crate::TagSet).
    pub fn set_tag(&mut self, tag: impl Into<u32>) -> &mut Self {
        self.tag = tag.into();
        self
    }

    pub fn add_bank_bytes(&mut self, bank_bytes: &[u8]) -> &mut Self {
        self.structures.extend_from_slice(bank_bytes);
        self
    }

    /// Serialise `bank` and append it.
    ///
    /// # Errors
    ///
    /// Whatever [`BankBuilder::finish`] returns — in practice
    /// [`HipoError::BankTooLarge`] for a bank at or past 2^24 bytes, which was
    /// previously appended with a truncated size word and no error.
    pub fn add(&mut self, bank: BankBuilder<'_>) -> Result<&mut Self> {
        let bytes = bank.finish()?;
        Ok(self.add_bank_bytes(&bytes))
    }

    pub fn structure_count(&self) -> usize {
        let mut pos = 0;
        let mut count = 0;
        while pos + BANK_STRUCTURE_SIZE <= self.structures.len() {
            let length = u32::from_le_bytes(
                self.structures[pos + 4..pos + 8]
                    .try_into()
                    .expect("4-byte slice fits in [u8; 4]"),
            ) & STRUCT_SIZE_MASK;
            pos += BANK_STRUCTURE_SIZE + length as usize;
            count += 1;
        }
        count
    }

    pub fn finished_size(&self) -> usize {
        EVENT_HEADER_SIZE + self.structures.len()
    }

    /// Clear the structures and the tag, keeping the buffer's capacity, so
    /// the next event reuses it instead of reallocating.
    pub fn reset(&mut self) {
        self.structures.clear();
        self.tag = 0;
    }

    /// Serialise `bank` and append it, without the intermediate `Vec` that
    /// [`BankBuilder::finish`] would allocate.
    pub fn add_bank(&mut self, bank: &BankBuilder<'_>) -> Result<&mut Self> {
        bank.finish_into(&mut self.structures)?;
        Ok(self)
    }

    /// Serialise the event into a caller-owned buffer, **appended** to `out`.
    ///
    /// The counterpart to [`BankBuilder::finish_into`]: with both, an entire
    /// file's events can be assembled through one buffer.
    pub fn finish_into(&self, out: &mut Vec<u8>) {
        let total = EVENT_HEADER_SIZE + self.structures.len();
        out.reserve(total);
        out.extend_from_slice(b"EVNT");
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&self.tag.to_le_bytes());
        // EH_RESERVED. The old `vec![0u8; total]` supplied these four bytes
        // implicitly and nothing else ever writes them — dropping the
        // zero-fill without this line silently emits garbage in a real wire
        // field (`wire::endian` byte-swaps it as a u32 on big-endian
        // conversion).
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&self.structures);
    }

    pub fn finish(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(EVENT_HEADER_SIZE + self.structures.len());
        self.finish_into(&mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::bank::Bank;
    use crate::event::event::Event;
    use crate::schema::{DataType, Schema};

    fn schema() -> Schema {
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
    fn bank_builder_round_trip() {
        let s = schema();
        let mut b = BankBuilder::with_row_capacity(&s, 3);
        b.push_row()
            .set_i32("pid", 11)
            .unwrap()
            .set_f32("px", 0.5)
            .unwrap()
            .set_i8("charge", -1)
            .unwrap();
        b.push_row()
            .set_i32("pid", 22)
            .unwrap()
            .set_f32("px", 1.5)
            .unwrap()
            .set_i8("charge", 1)
            .unwrap();
        b.push_row()
            .set_i32("pid", 33)
            .unwrap()
            .set_f32("px", 2.5)
            .unwrap()
            .set_i8("charge", 0)
            .unwrap();
        assert_eq!(b.rows(), 3);
        let bytes = b.finish().unwrap();

        assert_eq!(u16::from_le_bytes([bytes[0], bytes[1]]), 300);
        assert_eq!(bytes[2], 1);
        assert_eq!(bytes[3], 11);
        let data_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(data_size as usize, bytes.len() - BANK_STRUCTURE_SIZE);

        let bank = Bank::new(&s, &bytes[BANK_STRUCTURE_SIZE..]).unwrap();
        assert_eq!(bank.rows(), 3);
        assert_eq!(&*bank.col::<i32>("pid").unwrap(), &[11, 22, 33]);
        assert_eq!(&*bank.col::<f32>("px").unwrap(), &[0.5, 1.5, 2.5]);
        assert_eq!(&*bank.col::<i8>("charge").unwrap(), &[-1, 1, 0]);
    }

    #[test]
    fn bank_builder_random_access_set() {
        let s = schema();
        let mut b = BankBuilder::new(&s);
        b.push_rows(3);
        b.set_i32_at("pid", 2, 33).unwrap();
        b.set_i32_at("pid", 1, 22).unwrap();
        b.set_i32_at("pid", 0, 11).unwrap();
        let bytes = b.finish().unwrap();
        let bank = Bank::new(&s, &bytes[BANK_STRUCTURE_SIZE..]).unwrap();
        assert_eq!(&*bank.col::<i32>("pid").unwrap(), &[11, 22, 33]);
    }

    #[test]
    fn bank_builder_type_mismatch_errors() {
        let s = schema();
        let mut b = BankBuilder::new(&s);
        b.push_row();
        let err = b.set_f32("pid", 1.0).unwrap_err();
        assert!(matches!(err, HipoError::TypeMismatch { .. }));
    }

    #[test]
    fn bank_builder_missing_column_errors() {
        let s = schema();
        let mut b = BankBuilder::new(&s);
        b.push_row();
        let err = b.set_i32("nope", 1).unwrap_err();
        assert!(matches!(err, HipoError::UnknownColumn { .. }));
    }

    #[test]
    fn bank_builder_set_before_push_errors() {
        let s = schema();
        let mut b = BankBuilder::new(&s);
        let err = b.set_i32("pid", 1).unwrap_err();
        // Writer-API misuse, not record corruption — calling a setter before
        // `push_row` says nothing about any file's bytes.
        assert!(matches!(err, HipoError::InvalidUsage { .. }), "{err:?}");
    }

    #[test]
    fn event_builder_round_trip() {
        let s = schema();
        let mut b = BankBuilder::new(&s);
        b.push_row()
            .set_i32("pid", 42)
            .unwrap()
            .set_f32("px", 1.75)
            .unwrap()
            .set_i8("charge", 1)
            .unwrap();
        let mut eb = EventBuilder::new().with_tag(7u32);
        eb.add(b).unwrap();
        let bytes = eb.finish();

        let event = Event::new(&bytes);
        assert_eq!(event.size() as usize, bytes.len());
        assert_eq!(event.tag(), 7);
        let (hdr, data) = event.find(300, 1).unwrap();
        assert_eq!(hdr.group, 300);
        assert_eq!(hdr.item, 1);
        let bank = Bank::new(&s, data).unwrap();
        assert_eq!(bank.rows(), 1);
        assert_eq!(&*bank.col::<i32>("pid").unwrap(), &[42]);
        assert_eq!(&*bank.col::<f32>("px").unwrap(), &[1.75]);
    }

    #[test]
    fn bank_builder_with_array_columns() {
        // px/F#3 (3 floats per row), pid/I (scalar).
        let s = Schema::from_columns(
            "X",
            1,
            1,
            [
                ("pid".into(), DataType::Int, 1u32),
                ("px".into(), DataType::Float, 3u32),
            ],
        );
        let mut b = BankBuilder::with_row_capacity(&s, 3);
        b.push_row()
            .set_i32("pid", 11)
            .unwrap()
            .set_array("px", &[0.1f32, 0.2, 0.3])
            .unwrap();
        b.push_row()
            .set_i32("pid", 22)
            .unwrap()
            .set_array("px", &[1.0f32, 1.1, 1.2])
            .unwrap();
        b.push_row()
            .set_i32("pid", 33)
            .unwrap()
            .set_array("px", &[2.0f32, 2.1, 2.2])
            .unwrap();
        let bytes = b.finish().unwrap();

        let bank = Bank::new(&s, &bytes[BANK_STRUCTURE_SIZE..]).unwrap();
        assert_eq!(bank.rows(), 3);

        // Read as Cow<[[f32; 3]]>.
        let arrays = bank.col::<[f32; 3]>("px").unwrap();
        assert_eq!(arrays.len(), 3);
        assert_eq!(arrays[0], [0.1, 0.2, 0.3]);
        assert_eq!(arrays[1], [1.0, 1.1, 1.2]);
        assert_eq!(arrays[2], [2.0, 2.1, 2.2]);

        // Per-row get returns the full array.
        let row1: [f32; 3] = bank.get("px", 1);
        assert_eq!(row1, [1.0, 1.1, 1.2]);

        // Runtime escape hatch: array_at.
        let row2 = bank.array_at::<f32>("px", 2).unwrap();
        assert_eq!(&*row2, &[2.0, 2.1, 2.2]);

        // The scalar column still works.
        assert_eq!(&*bank.col::<i32>("pid").unwrap(), &[11, 22, 33]);
    }

    #[test]
    fn bank_builder_array_wrong_length_errors() {
        let s = Schema::from_columns("X", 1, 1, [("px".into(), DataType::Float, 3u32)]);
        let mut b = BankBuilder::new(&s);
        b.push_row();
        let err = b.set_array("px", &[0.1f32, 0.2]).unwrap_err(); // need 3, gave 2
        assert!(matches!(err, HipoError::ColumnLengthMismatch { .. }));
    }

    #[test]
    fn bank_builder_scalar_set_on_array_column_errors() {
        let s = Schema::from_columns("X", 1, 1, [("px".into(), DataType::Float, 3u32)]);
        let mut b = BankBuilder::new(&s);
        b.push_row();
        // set_f32 is scalar — should be rejected on an F#3 column.
        let err = b.set_f32("px", 0.5).unwrap_err();
        assert!(matches!(err, HipoError::ColumnLengthMismatch { .. }));
    }

    #[test]
    fn row_writer_set_array_via_const_generic() {
        // Through the RowWriter::set ergonomic path: set("name", [arr]).
        // Verifies the BankColumnType blanket impl for [T; N] dispatches
        // correctly.
        let s = Schema::from_columns("X", 1, 1, [("v".into(), DataType::Float, 4u32)]);
        let mut b = BankBuilder::new(&s);
        b.push_row();
        // Via the trait method (what RowWriter::set calls under the hood):
        <[f32; 4] as crate::schema::BankColumnType>::set_in([0.25, 0.5, 0.75, 1.0], &mut b, "v")
            .unwrap();
        let bytes = b.finish().unwrap();
        let bank = Bank::new(&s, &bytes[BANK_STRUCTURE_SIZE..]).unwrap();
        let row0: [f32; 4] = bank.get("v", 0);
        assert_eq!(row0, [0.25, 0.5, 0.75, 1.0]);
    }

    #[test]
    fn event_builder_multiple_banks() {
        let s1 = schema();
        let s2 = Schema::from_columns("REC::Event", 300, 30, [("evno".into(), DataType::Long, 1)]);

        let mut b1 = BankBuilder::new(&s1);
        b1.push_row().set_i32("pid", 1).unwrap();
        let mut b2 = BankBuilder::new(&s2);
        b2.push_row().set_i64("evno", 99).unwrap();

        let mut eb = EventBuilder::new().with_tag(0u32);
        eb.add(b1).unwrap().add(b2).unwrap();
        assert_eq!(eb.structure_count(), 2);
        let bytes = eb.finish();

        let event = Event::new(&bytes);
        assert_eq!(event.iter_structures().count(), 2);
        assert!(event.has(300, 1));
        assert!(event.has(300, 30));
    }

    /// `with_row_capacity` must account for `entry.length`.
    ///
    /// It reserved `rows * ty.size()`, dropping the per-row element count, so
    /// an array column such as `cov/F#16` got 1/16th of what it needed and
    /// regrew from there. Output was always correct — only the allocation was
    /// wrong — so the guard has to be on capacity, not on bytes.
    #[test]
    fn with_row_capacity_reserves_for_array_columns() {
        let schema = Schema::from_columns(
            "T::Test",
            1000,
            1,
            [
                ("cov".into(), DataType::Float, 16),
                ("px".into(), DataType::Float, 1),
            ],
        );
        const ROWS: u32 = 1000;
        let b = BankBuilder::with_row_capacity(&schema, ROWS);

        // 16 f32 per row for `cov`, 1 for `px`.
        assert!(
            b.columns[0].capacity() >= ROWS as usize * 4 * 16,
            "array column reserved {} bytes, needs {}",
            b.columns[0].capacity(),
            ROWS as usize * 4 * 16
        );
        assert!(b.columns[1].capacity() >= ROWS as usize * 4);

        // And filling to that many rows must not have reallocated: capacity
        // was already enough. This is what actually fails on the old code.
        let cap_before = b.columns[0].capacity();
        let mut b = b;
        b.push_rows(ROWS);
        assert_eq!(
            b.columns[0].capacity(),
            cap_before,
            "the array column regrew, so the reservation was short"
        );
        assert_eq!(b.rows(), ROWS);
        // Sanity: the buffer really is 16 floats per row wide, so the
        // capacity assertion above is measuring the right thing.
        assert_eq!(b.columns[0].len(), ROWS as usize * 4 * 16);
    }
}
