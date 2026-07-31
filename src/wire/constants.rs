//! HIPO format constants — direct port of `hipo4/constants.h`.
//!
//! Stable for HIPO version 6. Any change here is a wire-format change and
//! must be coordinated with the C++ implementation.

// --- Magic numbers ---
pub const HIPO_FILE_UNIQUE_WORD: u32 = 0x4F50_4948; // "HIPO" in LE
pub const HEADER_MAGIC: u32 = 0xc0da_0100; // little-endian marker
pub const HEADER_MAGIC_BE: u32 = 0x0001_dac0; // big-endian marker

// --- Header sizes ---
pub const FILE_HEADER_WORDS: usize = 14;
pub const RECORD_HEADER_WORDS: usize = 14;
pub const FILE_HEADER_SIZE: usize = FILE_HEADER_WORDS * 4; // 56 bytes
pub const RECORD_HEADER_SIZE: usize = RECORD_HEADER_WORDS * 4; // 56 bytes
pub const EVENT_HEADER_SIZE: usize = 16;
pub const BANK_STRUCTURE_SIZE: usize = 8;

// The C++ reader reads 80-byte chunks (header + first 24 bytes of payload).
// We expose this so callers don't have to recompute it.
pub const RECORD_HEADER_PROBE_SIZE: usize = 80;

// --- File header field offsets (byte offsets) ---
pub const FH_UNIQUE_WORD: usize = 0;
pub const FH_FILE_NUMBER: usize = 4;
pub const FH_HEADER_LENGTH: usize = 8;
pub const FH_RECORD_COUNT: usize = 12;
pub const FH_INDEX_ARRAY_LEN: usize = 16;
pub const FH_BIT_INFO: usize = 20;
pub const FH_USER_HEADER_LEN: usize = 24;
pub const FH_MAGIC_NUMBER: usize = 28;
pub const FH_USER_REGISTER: usize = 32; // u64
pub const FH_TRAILER_POS: usize = 40; // u64
pub const FH_USER_INT1: usize = 48;
pub const FH_USER_INT2: usize = 52;

// --- Record header field offsets (byte offsets) ---
pub const RH_RECORD_LENGTH: usize = 0;
pub const RH_RECORD_NUMBER: usize = 4;
pub const RH_HEADER_LENGTH: usize = 8;
pub const RH_EVENT_COUNT: usize = 12;
pub const RH_INDEX_ARRAY_LEN: usize = 16;
pub const RH_BIT_INFO: usize = 20;
pub const RH_USER_HEADER_LEN: usize = 24;
pub const RH_MAGIC_NUMBER: usize = 28;
pub const RH_DATA_LENGTH: usize = 32;
pub const RH_COMP_WORD: usize = 36;
pub const RH_USER_WORD1: usize = 40; // u64
pub const RH_USER_WORD2: usize = 48; // u64

// --- Event header field offsets ---
pub const EH_MAGIC: usize = 0;
pub const EH_SIZE: usize = 4;
pub const EH_TAG: usize = 8;
pub const EH_RESERVED: usize = 12;

// --- Dictionary identifiers ---
pub const DICT_GROUP: u16 = 120;
pub const DICT_ITEM: u8 = 2;
pub const DICT_JSON_ITEM: u8 = 1;
/// Event-tag name↔bit registry, stored as an extra text bank in the
/// dictionary record (an oxihipo extension — see [`crate::TagRegistry`]).
/// Additive: readers that don't know this item skip it, so a file carrying
/// it stays readable by the stock and C++ `hipo4` readers.
pub const TAG_REGISTRY_ITEM: u8 = 3;
pub const CONFIG_GROUP: u16 = 32555;
pub const CONFIG_KEY_ITEM: u8 = 1;
pub const CONFIG_STRING_ITEM: u8 = 2;
pub const FILE_INDEX_GROUP: u16 = 32111;
pub const FILE_INDEX_ITEM: u8 = 1;

// --- Bit-info word layout ---
pub const BITINFO_VERSION_MASK: u32 = 0x0000_00FF;
pub const BITINFO_VERSION_BITS: u32 = 8;
pub const BITINFO_HAS_DICTIONARY_BIT: u32 = 8;
pub const BITINFO_HAS_FIRST_EVENT_BIT: u32 = 9;
pub const BITINFO_TRAILER_WITH_INDEX_BIT: u32 = 10;
pub const BITINFO_PAD1_SHIFT: u32 = 20;
pub const BITINFO_PAD2_SHIFT: u32 = 22;
pub const BITINFO_PAD3_SHIFT: u32 = 24;
pub const BITINFO_PAD_MASK: u32 = 0x3;
pub const BITINFO_HEADER_TYPE_SHIFT: u32 = 28;

// --- Compression word layout ---
pub const COMP_TYPE_MASK: u32 = 0xF000_0000;
pub const COMP_TYPE_SHIFT: u32 = 28;

// --- split-codec extension-format versions -------------------------------
//
// These are a **cross-implementation contract**, not an internal version
// counter. The `hipo-cpp` and `hipo-java` `feature/bybank-bycolumn-compression`
// branches document and implement exactly these two numbers. 0.7.0 raised them
// to 3 and 2 when it appended the composite `header_size` table; `hipo-java`
// then failed to decode and `hipo-cpp` segfaulted, and 0.7.1 reverted it.
//
// **Do not bump them to describe a format addition.** The directory tables are
// append-only and the readers detect the optional tail by *length*, so a reader
// that predates a table simply never looks that far. That is what lets the
// format grow while the byte stays fixed — and it was verified by patching only
// the version byte back on a 0.7.0 file, after which both other
// implementations read it perfectly.

/// The `ext_format_version` byte written into every `Lz4PerBank` record.
pub const EXT_FORMAT_VERSION_BY_BANK: u8 = 2;

/// The `ext_format_version` byte written into every `Lz4PerColumn` record.
pub const EXT_FORMAT_VERSION_PER_COLUMN: u8 = 1;

/// Versions `ByBankRecord::parse` accepts. `3` is only ever seen in files
/// written by 0.7.0; it is read, never written.
pub const EXT_FORMAT_ACCEPT_BY_BANK: [u8; 2] = [EXT_FORMAT_VERSION_BY_BANK, 3];

/// Versions `PerColumnRecord::parse` accepts. `2` is only ever seen in files
/// written by 0.7.0; it is read, never written.
pub const EXT_FORMAT_ACCEPT_PER_COLUMN: [u8; 2] = [EXT_FORMAT_VERSION_PER_COLUMN, 2];
pub const COMP_TYPE_BYTE: u32 = 0x0000_000F; // after shift
pub const COMP_LENGTH_MASK: u32 = 0x0FFF_FFFF;

// --- Bank/node structure word layout ---
pub const STRUCT_SIZE_MASK: u32 = 0x00FF_FFFF;
pub const STRUCT_FORMAT_MASK: u32 = 0xFF00_0000;
pub const STRUCT_FORMAT_SHIFT: u32 = 24;
pub const STRUCT_FORMAT_BYTE: u32 = 0x0000_00FF;

pub const HIPO_VERSION: u32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HeaderType {
    EvioRecord = 0,
    EvioFile = 1,
    EvioExtFile = 2,
    HipoRecord = 4,
    HipoFile = 5,
    HipoExtFile = 6,
    HipoTrailer = 7,
}

impl HeaderType {
    pub const fn from_bit_info(bit_info: u32) -> Option<Self> {
        match (bit_info >> BITINFO_HEADER_TYPE_SHIFT) & 0xF {
            0 => Some(Self::EvioRecord),
            1 => Some(Self::EvioFile),
            2 => Some(Self::EvioExtFile),
            4 => Some(Self::HipoRecord),
            5 => Some(Self::HipoFile),
            6 => Some(Self::HipoExtFile),
            7 => Some(Self::HipoTrailer),
            _ => None,
        }
    }
}

/// What squeezes the bytes.
///
/// Orthogonal to [`Layout`], which decides *what* gets squeezed separately.
/// The pair is carried on the wire as a single 4-bit record-header tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    None,
    Lz4,
    /// LZ4 high-compression. ~10-15% smaller than [`Self::Lz4`] at ~4x the
    /// write cost; identical to decode.
    Lz4Hc,
    Gzip,
    /// Zstandard. The level is a writer-side choice only — any level decodes
    /// through the same path — so unlike LZ4/LZ4-HC it costs one tag, not two.
    Zstd,
}

/// How the record's bytes are grouped before compression.
///
/// Orthogonal to [`Codec`]. `PerBank` and `PerColumn` are what make partial
/// decompression possible: a reader inflates only the banks or columns it
/// touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// One stream for the whole record. Every read inflates everything.
    PerChunk,
    /// One stream per bank type, plus an event x bank presence directory.
    /// `ev.bank(name)` inflates only that bank. Layout in `wire/by_bank.rs`.
    PerBank,
    /// One stream per `(bank, column)`, laid out cross-event contiguous.
    /// Reading one column inflates only that column, and homogeneous columns
    /// compress better than a bank's interleaved bytes. Layout in
    /// `wire/per_column.rs`.
    PerColumn,
}

/// The 4-bit record-header tag: one value per (codec, layout) pair.
///
/// # This is a cross-implementation contract
///
/// Tags 0-3, 6 and 7 are implemented by `hipo-cpp` and `hipo-java` and must
/// never be reassigned. The rest are oxihipo extensions those readers reject
/// as unknown, which is the correct outcome — they cannot decode them.
///
/// Tags **4 and 5** previously carried `Lz4Chunked` and `Lz4ByBank` v1, both
/// removed during 0.x, and were rejected at parse. They are reused here for
/// Zstd deliberately: a zstd frame starts with the magic `0xFD2FB528`, so a
/// stale file carrying one of those old tags fails loudly at the frame check
/// rather than decoding as something plausible. Any other codec in those slots
/// would not have that property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CompressionType {
    None = 0,
    Lz4 = 1,
    Lz4Best = 2,
    Gzip = 3,
    ZstdPerChunk = 4,
    ZstdPerBank = 5,
    Lz4PerBank = 6,
    Lz4PerColumn = 7,
    ZstdPerColumn = 8,
    Lz4FastPerBank = 9,
    Lz4FastPerColumn = 10,
    GzipPerBank = 11,
    GzipPerColumn = 12,
    NonePerBank = 13,
    NonePerColumn = 14,
    // 15 is unassigned.
}

impl CompressionType {
    pub const fn from_word(comp_word: u32) -> Option<Self> {
        Self::from_tag(((comp_word >> COMP_TYPE_SHIFT) & COMP_TYPE_BYTE) as u8)
    }

    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::None),
            1 => Some(Self::Lz4),
            2 => Some(Self::Lz4Best),
            3 => Some(Self::Gzip),
            4 => Some(Self::ZstdPerChunk),
            5 => Some(Self::ZstdPerBank),
            6 => Some(Self::Lz4PerBank),
            7 => Some(Self::Lz4PerColumn),
            8 => Some(Self::ZstdPerColumn),
            9 => Some(Self::Lz4FastPerBank),
            10 => Some(Self::Lz4FastPerColumn),
            11 => Some(Self::GzipPerBank),
            12 => Some(Self::GzipPerColumn),
            13 => Some(Self::NonePerBank),
            14 => Some(Self::NonePerColumn),
            _ => None,
        }
    }

    /// The tag for a (codec, layout) pair. Every one of the 15 pairs has one.
    pub const fn for_pair(codec: Codec, layout: Layout) -> Self {
        match (codec, layout) {
            (Codec::None, Layout::PerChunk) => Self::None,
            (Codec::Lz4, Layout::PerChunk) => Self::Lz4,
            (Codec::Lz4Hc, Layout::PerChunk) => Self::Lz4Best,
            (Codec::Gzip, Layout::PerChunk) => Self::Gzip,
            (Codec::Zstd, Layout::PerChunk) => Self::ZstdPerChunk,
            (Codec::None, Layout::PerBank) => Self::NonePerBank,
            (Codec::Lz4, Layout::PerBank) => Self::Lz4FastPerBank,
            (Codec::Lz4Hc, Layout::PerBank) => Self::Lz4PerBank,
            (Codec::Gzip, Layout::PerBank) => Self::GzipPerBank,
            (Codec::Zstd, Layout::PerBank) => Self::ZstdPerBank,
            (Codec::None, Layout::PerColumn) => Self::NonePerColumn,
            (Codec::Lz4, Layout::PerColumn) => Self::Lz4FastPerColumn,
            (Codec::Lz4Hc, Layout::PerColumn) => Self::Lz4PerColumn,
            (Codec::Gzip, Layout::PerColumn) => Self::GzipPerColumn,
            (Codec::Zstd, Layout::PerColumn) => Self::ZstdPerColumn,
        }
    }

    /// The codec half of the pair — what `compress`/`decompress` dispatch on.
    pub const fn codec(self) -> Codec {
        match self {
            Self::None | Self::NonePerBank | Self::NonePerColumn => Codec::None,
            Self::Lz4 | Self::Lz4FastPerBank | Self::Lz4FastPerColumn => Codec::Lz4,
            Self::Lz4Best | Self::Lz4PerBank | Self::Lz4PerColumn => Codec::Lz4Hc,
            Self::Gzip | Self::GzipPerBank | Self::GzipPerColumn => Codec::Gzip,
            Self::ZstdPerChunk | Self::ZstdPerBank | Self::ZstdPerColumn => Codec::Zstd,
        }
    }

    /// The layout half of the pair.
    pub const fn layout(self) -> Layout {
        match self {
            Self::None | Self::Lz4 | Self::Lz4Best | Self::Gzip | Self::ZstdPerChunk => {
                Layout::PerChunk
            }
            Self::NonePerBank
            | Self::Lz4FastPerBank
            | Self::Lz4PerBank
            | Self::GzipPerBank
            | Self::ZstdPerBank => Layout::PerBank,
            Self::NonePerColumn
            | Self::Lz4FastPerColumn
            | Self::Lz4PerColumn
            | Self::GzipPerColumn
            | Self::ZstdPerColumn => Layout::PerColumn,
        }
    }

    /// True for every by-bank tag — decoded through `ByBankRecord::parse`
    /// rather than the whole-record path.
    pub const fn is_by_bank(self) -> bool {
        matches!(self.layout(), Layout::PerBank)
    }

    /// True for every per-column tag, decoded through `PerColumnRecord`.
    pub const fn is_per_column(self) -> bool {
        matches!(self.layout(), Layout::PerColumn)
    }
}
