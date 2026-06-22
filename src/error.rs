//! Error model. One enum, no dynamic allocation on the hot path.

use std::path::PathBuf;

/// Errors produced anywhere in the HIPO Rust library.
///
/// Variants are designed to be cheap to construct on the cold path; we never
/// build an error on the inner read loop unless something is genuinely wrong.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum HipoError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("file is too small: {actual} bytes (need at least {min})")]
    FileTooSmall { actual: u64, min: u64 },

    #[error(
        "invalid HIPO magic at offset {offset:#x}: got {found:#010x}, expected {expected:#010x}"
    )]
    BadMagic {
        offset: u64,
        found: u32,
        expected: u32,
    },

    #[error(
        "unsupported HIPO version: {version} (this build supports up to {})",
        crate::wire::constants::HIPO_VERSION
    )]
    UnsupportedVersion { version: u32 },

    #[error("big-endian HIPO files are not supported (endian magic at offset {offset:#x})")]
    UnsupportedEndianness { offset: u64 },

    #[error("unknown compression type: {0}")]
    UnknownCompression(u32),

    #[error("schema {name:?} not found in dictionary")]
    UnknownSchema { name: String },

    #[error("schema {schema:?} has no column {column:?}")]
    UnknownColumn { schema: String, column: String },

    #[error(
        "type mismatch in {schema:?}.{column:?}: bank stores {actual:?}, asked for {expected:?}"
    )]
    TypeMismatch {
        schema: String,
        column: String,
        expected: &'static str,
        actual: &'static str,
    },

    #[error(
        "column length mismatch in {schema:?}.{column:?}: schema declares length {expected}, got {actual}"
    )]
    ColumnLengthMismatch {
        schema: String,
        column: String,
        expected: u32,
        actual: u32,
    },

    #[error("corrupt record at offset {offset:#x}: {reason}")]
    CorruptRecord { offset: u64, reason: &'static str },

    #[error("compression error: {0}")]
    Compression(&'static str),

    #[error("decompression overflow: produced {produced} bytes, buffer holds {capacity}")]
    DecompressOverflow { produced: usize, capacity: usize },

    #[error("decompression underflow: produced {produced} bytes, expected {expected}")]
    DecompressUnderflow { produced: usize, expected: usize },

    #[error("schema parse error: {0}")]
    SchemaParse(String),

    #[error("invalid glob pattern {pattern:?}: {reason}")]
    InvalidGlob { pattern: String, reason: String },

    #[error("file {path:?}: {source}")]
    Path {
        path: PathBuf,
        #[source]
        source: Box<HipoError>,
    },
}

impl HipoError {
    /// Attach a path to an existing error. Useful when reading a chain of
    /// files — the inner error doesn't carry path context.
    pub fn with_path(self, path: impl Into<PathBuf>) -> Self {
        Self::Path {
            path: path.into(),
            source: Box::new(self),
        }
    }
}

pub type Result<T> = std::result::Result<T, HipoError>;
