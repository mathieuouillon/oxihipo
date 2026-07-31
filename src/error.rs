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

    /// A bank's data exceeded what the HIPO structure header can describe.
    ///
    /// The structure length word carries the size in its low **24 bits**; the
    /// top byte is the composite `header_size` field. Writing a larger bank
    /// used to truncate the size silently — 5,000,000 `Int` rows came back as
    /// 805,696, and exactly 2^24 bytes came back as *zero* while the top byte
    /// re-read as a composite header — so it is refused instead.
    #[error(
        "bank {schema:?} holds {size} bytes of data, over the {max}-byte limit \
         a HIPO structure header can describe (24-bit size field); split it \
         across events or banks"
    )]
    BankTooLarge {
        schema: String,
        size: usize,
        max: usize,
    },

    #[error("corrupt record at offset {offset:#x}: {reason}")]
    CorruptRecord { offset: u64, reason: &'static str },

    /// A tag name that the on-disk `name=bit` text form cannot round-trip.
    ///
    /// Refusing it is the point: before this check, `has\nnewline` was written
    /// and read back as `newline`, and `  padded  ` as `padded` — a different
    /// name, under which every later lookup missed.
    #[error("invalid tag name {name:?}: {reason}")]
    InvalidTagName { name: String, reason: &'static str },

    #[error("event index {index} out of range: the chain has {total} events")]
    EventIndexOutOfRange { index: u64, total: u64 },

    #[error(
        "cannot update the event tag in place: the record at offset {offset:#x} is \
         {compression}-compressed, so the tag lives inside a compressed block — only \
         uncompressed (`Compression::None`) records can be patched in place; rewrite \
         with `skim_tagged` instead"
    )]
    InPlaceTagUnsupported {
        offset: u64,
        compression: &'static str,
    },

    #[error(
        "`for_each_column({bank:?}, {column:?})` cannot be used on a filtered chain: it sweeps \
         whole column streams straight out of the record index, so it would hand back every value \
         in the file rather than only the surviving events' — a plausible number over the wrong \
         event set; use `Chain::read_columns` or `Chain::column_values`, which are also columnar \
         and do honour the filter and the record-tag pushdown"
    )]
    FilterIgnoredByColumnSweep { bank: String, column: String },

    #[error("invalid usage: {what}")]
    InvalidUsage { what: &'static str },

    #[error("compression error: {0}")]
    Compression(&'static str),

    #[error("could not create the reader thread pool: {0}")]
    ThreadPool(String),

    #[error("internal error: {0}")]
    Internal(&'static str),

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

    /// A decode error located to the record it came from.
    ///
    /// Most error variants have nowhere to put a file offset — `Compression`
    /// is a bare `&'static str`, and it is the variant a corrupt LZ4 payload
    /// actually produces. Wrapping is how those get located, the same way
    /// [`Self::Path`] attaches a filename.
    #[error("record at offset {offset:#x}: {source}")]
    AtOffset {
        offset: u64,
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

    /// Rebase a decode error onto the record it came from.
    ///
    /// Most decoder errors are constructed deep inside a record parse, where
    /// the file offset is not known — 58 of the 73 construction sites pass
    /// `offset: 0`. An operator looking at a bad record in the middle of a
    /// 9.1 GB file was told "corrupt record at offset 0x0", which points at
    /// the file header. Applied at the four record-processing entry points,
    /// this turns that into the record's real position.
    ///
    /// Only a **zero** offset is filled in, so the sites that already carry a
    /// real one are left alone. `BadMagic`'s offset is a field position
    /// *within* the record header (`RH_MAGIC_NUMBER`, 28) rather than a file
    /// position, so it is rebased rather than overwritten: `invalid HIPO magic
    /// at offset 0x1c` becomes `0x10c388d1c`.
    ///
    /// Pair with [`Self::with_path`], which names the file. On a multi-file
    /// chain that is the more valuable half — the offset is useless if you do
    /// not know which file it indexes.
    pub fn at_offset(self, record_offset: u64) -> Self {
        match self {
            Self::CorruptRecord { offset: 0, reason } => Self::CorruptRecord {
                offset: record_offset,
                reason,
            },
            Self::BadMagic {
                offset,
                found,
                expected,
            } if offset < crate::wire::constants::RECORD_HEADER_SIZE as u64 => Self::BadMagic {
                offset: record_offset + offset,
                found,
                expected,
            },
            // Everything else has nowhere to put an offset — `Compression` is
            // a bare `&'static str`, and a corrupt LZ4 payload is what most
            // real corruption produces, so leaving these unlocated would miss
            // the common case. Wrap instead, as `with_path` does for the
            // filename.
            already @ (Self::AtOffset { .. } | Self::Path { .. }) => already,
            other => Self::AtOffset {
                offset: record_offset,
                source: Box::new(other),
            },
        }
    }
}

pub type Result<T> = std::result::Result<T, HipoError>;
