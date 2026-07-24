//! `Schema`, `SchemaEntry`, `DataType`, plus the `(group, item)` sparse
//! lookup table used by [`Dict`](crate::schema::Dict).

use crate::error::{HipoError, Result};

/// Wire-level data type for a single column. Matches the C++ `enum Type`
/// values byte-for-byte so dictionaries round-trip without translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum DataType {
    Byte = 1,
    Short = 2,
    Int = 3,
    Float = 4,
    Double = 5,
    Long = 8,
}

impl DataType {
    /// Type letter as it appears in schema text ("pid/I" -> Int).
    pub const fn letter(self) -> char {
        match self {
            Self::Byte => 'B',
            Self::Short => 'S',
            Self::Int => 'I',
            Self::Float => 'F',
            Self::Double => 'D',
            Self::Long => 'L',
        }
    }

    pub const fn from_letter(c: char) -> Option<Self> {
        match c {
            'B' => Some(Self::Byte),
            'S' => Some(Self::Short),
            'I' => Some(Self::Int),
            'F' => Some(Self::Float),
            'D' => Some(Self::Double),
            'L' => Some(Self::Long),
            _ => None,
        }
    }

    pub const fn size(self) -> usize {
        match self {
            Self::Byte => 1,
            Self::Short => 2,
            Self::Int | Self::Float => 4,
            Self::Double | Self::Long => 8,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Byte => "byte",
            Self::Short => "short",
            Self::Int => "int",
            Self::Float => "float",
            Self::Double => "double",
            Self::Long => "long",
        }
    }

    pub const fn from_type_id(id: u8) -> Option<Self> {
        match id {
            1 => Some(Self::Byte),
            2 => Some(Self::Short),
            3 => Some(Self::Int),
            4 => Some(Self::Float),
            5 => Some(Self::Double),
            8 => Some(Self::Long),
            _ => None,
        }
    }
}

/// One column in a schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaEntry {
    pub name: String,
    pub ty: DataType,
    /// Per-row offset within a single row (sum of preceding columns'
    /// total sizes, i.e. `ty.size() * length`). In column-major layout
    /// the bank-level offset of cell `(col, row)` is
    /// `total_rows * row_offset + row_idx * ty.size() * length`.
    pub row_offset: u32,
    /// Number of elements per cell. `1` for ordinary scalar columns;
    /// `N` for an array column declared in schema text as `name/T#N`.
    /// A cell's bytes are `ty.size() * length` contiguous bytes laid
    /// out as `[element_0, element_1, …, element_{length-1}]`.
    pub length: u32,
}

/// Layout of a single bank.
///
/// Cheap to clone but typically passed by reference. Decoders look up
/// columns by index (constant time) once they've translated a name.
///
/// A name → column-index map is built at construction time and stored
/// alongside the entries, so [`Self::column_index`] is `O(1)` rather
/// than a linear scan. Memory cost: a small `HashMap` per schema (≈ 5–20
/// entries in practice).
#[derive(Debug, Clone)]
pub struct Schema {
    name: String,
    group: u16,
    item: u8,
    entries: Vec<SchemaEntry>,
    row_size: u32,
    /// Cached name → entry index. Always derivable from `entries`; rebuilt
    /// after any structural change (`add_column`).
    by_name: std::collections::HashMap<String, u16>,
}

// Manually implemented so two schemas with the same entries compare equal
// regardless of the `by_name` cache's iteration order or capacity.
impl PartialEq for Schema {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.group == other.group
            && self.item == other.item
            && self.entries == other.entries
            && self.row_size == other.row_size
    }
}
impl Eq for Schema {}

impl Default for Schema {
    fn default() -> Self {
        Self::new("", 0, 0)
    }
}

impl Schema {
    pub fn new(name: impl Into<String>, group: u16, item: u8) -> Self {
        Self {
            name: name.into(),
            group,
            item,
            entries: Vec::new(),
            row_size: 0,
            by_name: std::collections::HashMap::new(),
        }
    }

    /// Construct a schema from an iterator of `(name, type, length)`
    /// triples. `length == 1` is a scalar column; `length > 1` is a
    /// fixed-length array (declared as `name/T#N` in schema text). Pass
    /// `1` for scalar columns.
    pub fn from_columns(
        name: impl Into<String>,
        group: u16,
        item: u8,
        columns: impl IntoIterator<Item = (String, DataType, u32)>,
    ) -> Self {
        let mut s = Self::new(name, group, item);
        let mut offset: u32 = 0;
        for (col_name, ty, length) in columns {
            let length = length.max(1);
            // Stop at u16::MAX columns rather than panicking on hostile
            // dictionary text; such a schema is invalid and simply won't match
            // any bank. (A valid schema has a handful of columns.)
            let Ok(idx) = u16::try_from(s.entries.len()) else {
                break;
            };
            s.by_name.insert(col_name.clone(), idx);
            s.entries.push(SchemaEntry {
                name: col_name,
                ty,
                row_offset: offset,
                length,
            });
            // Saturating so a crafted `#N` / column count can't overflow the
            // running offset (which would debug-panic / release-wrap into a
            // bogus layout). Identical to `+=` for any valid schema, whose row
            // size is far below u32::MAX (HIPO bank sizes are 24-bit).
            offset = offset.saturating_add((ty.size() as u32).saturating_mul(length));
        }
        s.row_size = offset;
        s
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn group(&self) -> u16 {
        self.group
    }

    pub fn item(&self) -> u8 {
        self.item
    }

    pub fn entries(&self) -> &[SchemaEntry] {
        &self.entries
    }

    pub fn num_columns(&self) -> usize {
        self.entries.len()
    }

    pub fn row_size(&self) -> u32 {
        self.row_size
    }

    pub fn bytes_for_rows(&self, rows: u32) -> u64 {
        u64::from(self.row_size) * u64::from(rows)
    }

    /// Index of the column with `name`, or `None` if it doesn't exist.
    ///
    /// O(1) via a name → index `HashMap` cached at construction time. For
    /// per-event hot loops, [`Self::handle`] gives a zero-cost typed
    /// reference and is still preferred.
    #[inline]
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.by_name.get(name).map(|&i| i as usize)
    }

    pub fn require_column(&self, name: &str) -> Result<usize> {
        self.column_index(name)
            .ok_or_else(|| HipoError::UnknownColumn {
                schema: self.name.clone(),
                column: name.to_string(),
            })
    }

    /// Resolve a typed column handle. Returns a zero-cost `(col_index,
    /// PhantomData<T>)` that callers reuse across events to skip the
    /// per-event name lookup:
    ///
    /// ```ignore
    /// let h_px = schema.handle::<f32>("px")?;
    /// for ev in chain.events() {
    ///     let ev = ev?;
    ///     if let Some(b) = ev.bank("REC::Particle") {
    ///         let px = b.read(h_px);   // Cow<[f32]>, no name lookup
    ///         // ...
    ///     }
    /// }
    /// ```
    pub fn handle<T: super::handle::BankColumnType>(
        &self,
        name: &str,
    ) -> Result<super::handle::ColumnHandle<T>> {
        super::handle::ColumnHandle::resolve(self, name)
    }

    #[inline]
    pub fn column_byte_offset(&self, col: usize, total_rows: u32) -> u64 {
        u64::from(total_rows) * u64::from(self.entries[col].row_offset)
    }

    #[inline]
    pub fn cell_byte_offset(&self, col: usize, row: u32, total_rows: u32) -> u64 {
        let entry = &self.entries[col];
        u64::from(total_rows) * u64::from(entry.row_offset)
            + u64::from(row) * entry.ty.size() as u64 * u64::from(entry.length)
    }

    /// Per-row element count for column `col`. `1` for a scalar
    /// column, `N` for an `T#N` array column.
    #[inline]
    pub fn column_length(&self, col: usize) -> u32 {
        self.entries[col].length
    }
}

/// Fast `(group, item) -> schema_id` index for a [`Dict`](crate::schema::Dict).
///
/// HIPO group is `u16` and item is `u8`. We build a flat sparse `Vec<u16>`
/// indexed by `(group << 8) | item` lazily as schemas are added — about
/// 2-3× faster than a `HashMap` when it dominates the inner loop.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SchemaIndex {
    table: Vec<u16>,
    max_key: u32,
}

impl SchemaIndex {
    const MISSING: u16 = u16::MAX;

    pub fn new() -> Self {
        Self {
            table: Vec::new(),
            max_key: 0,
        }
    }

    #[inline]
    pub fn key(group: u16, item: u8) -> u32 {
        (u32::from(group) << 8) | u32::from(item)
    }

    pub fn insert(&mut self, group: u16, item: u8, idx: u16) {
        let k = Self::key(group, item);
        if k as usize >= self.table.len() {
            self.table.resize((k + 1) as usize, Self::MISSING);
            self.max_key = k;
        }
        self.table[k as usize] = idx;
    }

    #[inline]
    pub fn get(&self, group: u16, item: u8) -> Option<u16> {
        let k = Self::key(group, item) as usize;
        let v = *self.table.get(k)?;
        (v != Self::MISSING).then_some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_type_roundtrip() {
        for ty in [
            DataType::Byte,
            DataType::Short,
            DataType::Int,
            DataType::Float,
            DataType::Double,
            DataType::Long,
        ] {
            assert_eq!(DataType::from_letter(ty.letter()), Some(ty));
            assert_eq!(DataType::from_type_id(ty as u8), Some(ty));
        }
    }

    #[test]
    fn schema_layout_columnar() {
        let s = Schema::from_columns(
            "REC::Particle",
            300,
            1,
            [
                ("pid".into(), DataType::Int, 1),
                ("px".into(), DataType::Float, 1),
                ("py".into(), DataType::Float, 1),
                ("charge".into(), DataType::Byte, 1),
            ],
        );
        assert_eq!(s.row_size(), 4 + 4 + 4 + 1);
        assert_eq!(s.bytes_for_rows(10), 130);
        assert_eq!(s.column_byte_offset(0, 10), 0);
        assert_eq!(s.column_byte_offset(1, 10), 40);
        assert_eq!(s.column_byte_offset(2, 10), 80);
        assert_eq!(s.column_byte_offset(3, 10), 120);
        assert_eq!(s.cell_byte_offset(2, 5, 10), 100);
    }

    #[test]
    fn schema_layout_with_array_columns() {
        // a/I (scalar, 4 B), b/F#4 (16 B per row), c/B#2 (2 B per row).
        // row_size = 4 + 16 + 2 = 22 bytes.
        let s = Schema::from_columns(
            "X",
            1,
            1,
            [
                ("a".into(), DataType::Int, 1u32),
                ("b".into(), DataType::Float, 4u32),
                ("c".into(), DataType::Byte, 2u32),
            ],
        );
        assert_eq!(s.row_size(), 22);
        assert_eq!(s.column_length(0), 1);
        assert_eq!(s.column_length(1), 4);
        assert_eq!(s.column_length(2), 2);
        // For 10 rows:
        //   col 0 starts at 0 (10 * 4 = 40 bytes)
        //   col 1 starts at 40 (10 * 16 = 160 bytes)
        //   col 2 starts at 200 (10 * 2 = 20 bytes)
        assert_eq!(s.column_byte_offset(0, 10), 0);
        assert_eq!(s.column_byte_offset(1, 10), 40);
        assert_eq!(s.column_byte_offset(2, 10), 200);
        // Cell offset: col 1 row 5 = 40 + 5 * 16 = 120.
        assert_eq!(s.cell_byte_offset(1, 5, 10), 120);
    }

    #[test]
    fn schema_index_lookup() {
        let mut idx = SchemaIndex::new();
        idx.insert(300, 1, 7);
        idx.insert(120, 2, 0);
        assert_eq!(idx.get(300, 1), Some(7));
        assert_eq!(idx.get(120, 2), Some(0));
        assert_eq!(idx.get(300, 2), None);
        assert_eq!(idx.get(0, 0), None);
    }

    #[test]
    fn missing_column_error() {
        let s = Schema::from_columns("X", 1, 1, [("a".into(), DataType::Int, 1)]);
        assert_eq!(s.column_index("a"), Some(0));
        assert!(s.column_index("b").is_none());
        let err = s.require_column("b").unwrap_err();
        assert!(err.to_string().contains("\"b\""));
    }
}
