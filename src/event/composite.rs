//! `Composite` — heterogeneous rows tagged by an inline format string.
//!
//! Unlike regular banks (column-major, schema-driven), composite banks
//! carry their layout inline:
//!
//! ```text
//! [0..8]           structure header (group/item/type/length-word)
//! [8..8+fmt_len]   format string (one lowercase char per field)
//! [8+fmt_len..]    row-major payload: rows * row_size bytes
//! ```

use crate::error::{HipoError, Result};
use crate::schema::DataType;
use crate::wire::bytes::{read_f32_le, read_f64_le, read_i16_le, read_u32_le, read_u64_le};
use crate::wire::constants::{BANK_STRUCTURE_SIZE, STRUCT_FORMAT_SHIFT, STRUCT_SIZE_MASK};

#[derive(Debug, Clone)]
pub struct CompositeFormat {
    pub fields: Vec<CompositeField>,
    pub row_size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompositeField {
    pub ty: DataType,
    /// Byte offset from the start of a row.
    pub row_offset: u32,
}

impl CompositeFormat {
    /// Parse the inline format string. Unrecognised characters are skipped
    /// (matches the C++ writer's tolerance for stray separators).
    pub fn parse(format: &str) -> Result<Self> {
        let mut fields = Vec::new();
        let mut offset: u32 = 0;
        for c in format.chars() {
            let ty = match c {
                'b' => DataType::Byte,
                's' => DataType::Short,
                'i' => DataType::Int,
                'f' => DataType::Float,
                'd' => DataType::Double,
                'l' => DataType::Long,
                _ => continue,
            };
            fields.push(CompositeField {
                ty,
                row_offset: offset,
            });
            offset += ty.size() as u32;
        }
        if fields.is_empty() {
            return Err(HipoError::SchemaParse(format!(
                "composite format {format:?} has no fields"
            )));
        }
        Ok(Self {
            fields,
            row_size: offset,
        })
    }

    pub fn fields(&self) -> &[CompositeField] {
        &self.fields
    }

    pub fn row_size(&self) -> u32 {
        self.row_size
    }
}

/// Zero-copy view over a composite structure's payload.
///
/// Rows are row-major. Accessors take a `(field_index, row_index)` pair
/// (there is no column name — the format string is positional).
#[derive(Debug, Clone)]
pub struct Composite<'b> {
    format: CompositeFormat,
    data: &'b [u8],
    rows: u32,
}

impl<'b> Composite<'b> {
    /// Decode from the **full** structure bytes (including the 8-byte
    /// structure header).
    pub fn from_structure(bytes: &'b [u8]) -> Result<Self> {
        if bytes.len() < BANK_STRUCTURE_SIZE {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "composite structure shorter than header",
            });
        }
        let length_word = read_u32_le(bytes, 4);
        let data_size = (length_word & STRUCT_SIZE_MASK) as usize;
        let format_size = ((length_word >> STRUCT_FORMAT_SHIFT) & 0xFF) as usize;
        if format_size == 0 {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "composite structure has zero-length format string",
            });
        }
        let data_start = BANK_STRUCTURE_SIZE;
        let format_end = data_start + format_size;
        if format_end > bytes.len() || data_start + data_size > bytes.len() {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "composite structure truncated",
            });
        }
        let format_bytes = &bytes[data_start..format_end];
        let format_str = std::str::from_utf8(format_bytes)
            .map_err(|_| HipoError::SchemaParse("composite format is not valid UTF-8".into()))?;
        let format = CompositeFormat::parse(format_str.trim_end_matches('\0'))?;
        let payload = &bytes[format_end..data_start + data_size];
        Self::from_parts(format, payload)
    }

    pub fn from_parts(format: CompositeFormat, data: &'b [u8]) -> Result<Self> {
        if format.row_size == 0 {
            return Ok(Self {
                format,
                data: &[],
                rows: 0,
            });
        }
        if !data.len().is_multiple_of(format.row_size as usize) {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "composite data size is not a multiple of row size",
            });
        }
        let rows = (data.len() as u32) / format.row_size;
        Ok(Self { format, data, rows })
    }

    pub fn format(&self) -> &CompositeFormat {
        &self.format
    }

    pub fn rows(&self) -> u32 {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    pub fn fields(&self) -> &[CompositeField] {
        &self.format.fields
    }

    fn offset(&self, field: usize, row: u32) -> usize {
        let f = &self.format.fields[field];
        row as usize * self.format.row_size as usize + f.row_offset as usize
    }

    pub fn i8(&self, field: usize, row: u32) -> i8 {
        self.data[self.offset(field, row)] as i8
    }

    pub fn i16(&self, field: usize, row: u32) -> i16 {
        read_i16_le(self.data, self.offset(field, row))
    }

    pub fn i32(&self, field: usize, row: u32) -> i32 {
        let off = self.offset(field, row);
        match self.format.fields[field].ty {
            DataType::Byte => self.data[off] as i8 as i32,
            DataType::Short => read_i16_le(self.data, off) as i32,
            DataType::Int => read_u32_le(self.data, off) as i32,
            _ => 0,
        }
    }

    pub fn i64(&self, field: usize, row: u32) -> i64 {
        let off = self.offset(field, row);
        match self.format.fields[field].ty {
            DataType::Long => read_u64_le(self.data, off) as i64,
            DataType::Int => read_u32_le(self.data, off) as i32 as i64,
            DataType::Short => read_i16_le(self.data, off) as i64,
            DataType::Byte => self.data[off] as i8 as i64,
            _ => 0,
        }
    }

    pub fn f32(&self, field: usize, row: u32) -> f32 {
        let off = self.offset(field, row);
        match self.format.fields[field].ty {
            DataType::Float => read_f32_le(self.data, off),
            DataType::Double => read_f64_le(self.data, off) as f32,
            _ => 0.0,
        }
    }

    pub fn f64(&self, field: usize, row: u32) -> f64 {
        let off = self.offset(field, row);
        match self.format.fields[field].ty {
            DataType::Double => read_f64_le(self.data, off),
            DataType::Float => f64::from(read_f32_le(self.data, off)),
            _ => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::bytes::write_u32_le;

    #[test]
    fn parse_format_ilf() {
        let f = CompositeFormat::parse("ilf").unwrap();
        assert_eq!(f.fields.len(), 3);
        assert_eq!(f.fields[0].ty, DataType::Int);
        assert_eq!(f.fields[1].ty, DataType::Long);
        assert_eq!(f.fields[2].ty, DataType::Float);
        assert_eq!(f.fields[0].row_offset, 0);
        assert_eq!(f.fields[1].row_offset, 4);
        assert_eq!(f.fields[2].row_offset, 12);
        assert_eq!(f.row_size, 16);
    }

    #[test]
    fn parse_format_skips_separators() {
        let f = CompositeFormat::parse("i,l,f,").unwrap();
        assert_eq!(f.fields.len(), 3);
        assert_eq!(f.row_size, 16);
    }

    #[test]
    fn parse_format_empty_errors() {
        let err = CompositeFormat::parse("xyz").unwrap_err();
        assert!(matches!(err, HipoError::SchemaParse(_)));
    }

    fn build_composite_bytes(group: u16, item: u8, format: &str, rows_data: &[u8]) -> Vec<u8> {
        let format_bytes = format.as_bytes();
        let data_size = format_bytes.len() + rows_data.len();
        let mut out = Vec::with_capacity(BANK_STRUCTURE_SIZE + data_size);
        out.extend_from_slice(&group.to_le_bytes());
        out.push(item);
        out.push(11);
        let length_word = (data_size as u32) | ((format_bytes.len() as u32) << 24);
        let mut len_bytes = [0u8; 4];
        write_u32_le(&mut len_bytes, 0, length_word);
        out.extend_from_slice(&len_bytes);
        out.extend_from_slice(format_bytes);
        out.extend_from_slice(rows_data);
        out
    }

    #[test]
    fn composite_from_structure_round_trip() {
        let mut rows = Vec::new();
        rows.extend_from_slice(&11i32.to_le_bytes());
        rows.extend_from_slice(&0.5f32.to_le_bytes());
        rows.extend_from_slice(&22i32.to_le_bytes());
        rows.extend_from_slice(&((-0.25f32).to_le_bytes()));

        let bytes = build_composite_bytes(123, 45, "if", &rows);
        let comp = Composite::from_structure(&bytes).unwrap();
        assert_eq!(comp.rows(), 2);
        assert_eq!(comp.i32(0, 0), 11);
        assert_eq!(comp.f32(1, 0), 0.5);
        assert_eq!(comp.i32(0, 1), 22);
        assert_eq!(comp.f32(1, 1), -0.25);
    }

    #[test]
    fn composite_rejects_truncated() {
        let bytes = build_composite_bytes(1, 1, "i", &[0, 0]);
        let err = Composite::from_structure(&bytes).unwrap_err();
        assert!(matches!(err, HipoError::CorruptRecord { .. }));
    }

    #[test]
    fn composite_rejects_zero_format_length() {
        let mut bytes = build_composite_bytes(1, 1, "i", &[0; 4]);
        bytes[7] = 0;
        bytes[4] = 0;
        bytes[5] = 0;
        let err = Composite::from_structure(&bytes).unwrap_err();
        assert!(matches!(err, HipoError::CorruptRecord { .. }));
    }

    #[test]
    fn composite_mixed_types() {
        let mut rows = Vec::new();
        rows.push(0xABu8);
        rows.extend_from_slice(&1234i16.to_le_bytes());
        rows.extend_from_slice(&0xDEADBEEFCAFEi64.to_le_bytes());
        rows.extend_from_slice(&(-42i32).to_le_bytes());
        rows.extend_from_slice(&1.75f32.to_le_bytes());
        rows.extend_from_slice(&0.125f64.to_le_bytes());
        assert_eq!(rows.len(), 27);

        let bytes = build_composite_bytes(1, 1, "bslifd", &rows);
        let comp = Composite::from_structure(&bytes).unwrap();
        assert_eq!(comp.format().row_size, 27);
        assert_eq!(comp.rows(), 1);
        assert_eq!(comp.i8(0, 0), 0xABu8 as i8);
        assert_eq!(comp.i16(1, 0), 1234);
        assert_eq!(comp.i64(2, 0), 0xDEADBEEFCAFE_i64);
        assert_eq!(comp.i32(3, 0), -42);
        assert_eq!(comp.f32(4, 0), 1.75);
        assert_eq!(comp.f64(5, 0), 0.125);
    }
}
