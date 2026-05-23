//! Schema types — `Schema`, `DataType`, `Dict`, typed `ColumnHandle`.

mod dict;
pub(crate) mod handle;
mod parse;
mod types;

pub use dict::Dict;
pub use handle::{BankColumnType, BankScalarType, ColumnHandle};
pub use types::{DataType, Schema, SchemaEntry, SchemaIndex};
