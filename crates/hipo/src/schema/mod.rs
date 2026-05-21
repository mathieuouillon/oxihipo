//! Schema types — `Schema`, `DataType`, `Dict`, typed `ColumnHandle`.

mod dict;
mod handle;
mod parse;
mod types;

pub use dict::Dict;
pub use handle::{BankColumnType, ColumnHandle};
pub use types::{DataType, Schema, SchemaEntry, SchemaIndex};
