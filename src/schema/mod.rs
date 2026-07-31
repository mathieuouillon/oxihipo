//! Schema types — `Schema`, `DataType`, `Dict`, typed `ColumnHandle`.

mod dict;
pub(crate) mod handle;
mod parse;
mod patterns;
mod types;

pub use dict::Dict;
pub use handle::{BankColumnType, BankScalarType, ColumnHandle};
pub use patterns::BankPatterns;
pub use types::{DataType, Schema, SchemaEntry, SchemaIndex};
