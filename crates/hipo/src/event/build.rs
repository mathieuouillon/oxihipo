//! Internal write-side builders.
//!
//! The user-facing write API lives in [`crate::write`] (`Writer`, `BankWriter`,
//! `RowWriter`). These types are the lower-level primitives those builders
//! delegate to — they're exposed at crate scope so callers who want raw
//! byte-level control (e.g. building a record outside the `Writer`) can
//! reach them.

use crate::error::{HipoError, Result};
use crate::schema::{DataType, Schema};
use crate::wire::bytes::write_u32_le;
use crate::wire::constants::*;

/// Build a HIPO bank (a single structure) one row at a time.
///
/// Internal storage is already column-major; [`Self::finish`] is a
/// constant-time serialisation (no transpose).
#[derive(Debug)]
pub struct BankBuilder<'s> {
    schema: &'s Schema,
    /// One buffer per column. Length is always `rows * entry.ty.size()`.
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
            .map(|e| Vec::with_capacity(rows as usize * e.ty.size()))
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
            col.extend(std::iter::repeat_n(0u8, entry.ty.size()));
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
        let actual = self.schema.entries()[col].ty;
        if actual != expected {
            return Err(HipoError::TypeMismatch {
                schema: self.schema.name().to_string(),
                column: name.to_string(),
                expected: expected.name(),
                actual: actual.name(),
            });
        }
        Ok(col)
    }

    fn last_row(&self) -> Result<u32> {
        self.rows.checked_sub(1).ok_or(HipoError::CorruptRecord {
            offset: 0,
            reason: "set_* called before push_row()",
        })
    }

    pub fn set_i32(&mut self, name: &str, value: i32) -> Result<&mut Self> {
        let row = self.last_row()?;
        self.set_i32_at(name, row, value)
    }

    pub fn set_i32_at(&mut self, name: &str, row: u32, value: i32) -> Result<&mut Self> {
        let col = self.check_col(name, DataType::Int)?;
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
        let col = self.check_col(name, DataType::Long)?;
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
        let col = self.check_col(name, DataType::Short)?;
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
        let col = self.check_col(name, DataType::Byte)?;
        let bytes = &mut self.columns[col];
        bytes[row as usize] = value as u8;
        Ok(self)
    }

    pub fn set_f32(&mut self, name: &str, value: f32) -> Result<&mut Self> {
        let row = self.last_row()?;
        self.set_f32_at(name, row, value)
    }

    pub fn set_f32_at(&mut self, name: &str, row: u32, value: f32) -> Result<&mut Self> {
        let col = self.check_col(name, DataType::Float)?;
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
        let col = self.check_col(name, DataType::Double)?;
        let bytes = &mut self.columns[col];
        let off = row as usize * 8;
        bytes[off..off + 8].copy_from_slice(&value.to_le_bytes());
        Ok(self)
    }

    /// Serialise as `[structure header | column-major data]`. Byte-
    /// compatible with what [`Bank::new`](crate::event::Bank::new) decodes.
    pub fn finish(self) -> Vec<u8> {
        let data_size: usize = self.columns.iter().map(|c| c.len()).sum();
        let mut out = Vec::with_capacity(BANK_STRUCTURE_SIZE + data_size);
        out.extend_from_slice(&self.schema.group().to_le_bytes());
        out.push(self.schema.item());
        out.push(11); // basic bank type code (matches C++ writer)
        out.extend_from_slice(&(data_size as u32).to_le_bytes());
        for col in self.columns {
            out.extend_from_slice(&col);
        }
        out
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

    pub fn with_tag(mut self, tag: u32) -> Self {
        self.tag = tag;
        self
    }

    pub fn tag(&self) -> u32 {
        self.tag
    }

    pub fn set_tag(&mut self, tag: u32) -> &mut Self {
        self.tag = tag;
        self
    }

    pub fn add_bank_bytes(&mut self, bank_bytes: &[u8]) -> &mut Self {
        self.structures.extend_from_slice(bank_bytes);
        self
    }

    pub fn add(&mut self, bank: BankBuilder<'_>) -> &mut Self {
        let bytes = bank.finish();
        self.add_bank_bytes(&bytes)
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

    pub fn finish(self) -> Vec<u8> {
        let total = EVENT_HEADER_SIZE + self.structures.len();
        let mut out = vec![0u8; total];
        out[0..4].copy_from_slice(b"EVNT");
        write_u32_le(&mut out, EH_SIZE, total as u32);
        write_u32_le(&mut out, EH_TAG, self.tag);
        out[EVENT_HEADER_SIZE..].copy_from_slice(&self.structures);
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
                ("pid".into(), DataType::Int),
                ("px".into(), DataType::Float),
                ("charge".into(), DataType::Byte),
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
        let bytes = b.finish();

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
        let bytes = b.finish();
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
        assert!(matches!(err, HipoError::CorruptRecord { .. }));
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
        let mut eb = EventBuilder::new().with_tag(7);
        eb.add(b);
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
    fn event_builder_multiple_banks() {
        let s1 = schema();
        let s2 = Schema::from_columns("REC::Event", 300, 30, [("evno".into(), DataType::Long)]);

        let mut b1 = BankBuilder::new(&s1);
        b1.push_row().set_i32("pid", 1).unwrap();
        let mut b2 = BankBuilder::new(&s2);
        b2.push_row().set_i64("evno", 99).unwrap();

        let mut eb = EventBuilder::new().with_tag(0);
        eb.add(b1).add(b2);
        assert_eq!(eb.structure_count(), 2);
        let bytes = eb.finish();

        let event = Event::new(&bytes);
        assert_eq!(event.iter_structures().count(), 2);
        assert!(event.has(300, 1));
        assert!(event.has(300, 30));
    }
}
