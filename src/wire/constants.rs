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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CompressionType {
    None = 0,
    Lz4 = 1,
    Lz4Best = 2,
    Gzip = 3,
    /// Records whose payload is split into multiple independently-LZ4-
    /// compressed chunks. Layout described in `wire/record.rs`. Enables
    /// intra-record parallel decompression and (future) partial decode.
    /// **Not readable by the C++ `hipo4` reader** — new files written
    /// with this tag are a Rust-only format extension.
    Lz4Chunked = 4,
    /// Records whose payload is split into one LZ4 stream per bank
    /// type, plus a directory of which events have which banks. Layout
    /// described in `wire/by_bank.rs`. Enables true partial
    /// decompression — `ev.bank("name")` inflates only the requested
    /// bank's stream. **Not readable by the C++ `hipo4` reader.**
    Lz4ByBank = 5,
    /// Version 2 of the by-bank format: the directory carries an explicit
    /// extension-format-version byte and is itself LZ4-compressed (the
    /// per-event size matrix is highly redundant), shrinking the on-disk
    /// directory. Bank streams are unchanged. Layout in `wire/by_bank.rs`.
    /// **Not readable by the C++ `hipo4` reader.**
    Lz4ByBankV2 = 6,
}

impl CompressionType {
    pub const fn from_word(comp_word: u32) -> Option<Self> {
        match (comp_word >> COMP_TYPE_SHIFT) & COMP_TYPE_BYTE {
            0 => Some(Self::None),
            1 => Some(Self::Lz4),
            2 => Some(Self::Lz4Best),
            3 => Some(Self::Gzip),
            4 => Some(Self::Lz4Chunked),
            5 => Some(Self::Lz4ByBank),
            6 => Some(Self::Lz4ByBankV2),
            _ => None,
        }
    }

    /// True for both by-bank formats (v1 tag 5 and v2 tag 6) — the
    /// reader treats them identically except for directory decoding.
    pub const fn is_by_bank(self) -> bool {
        matches!(self, Self::Lz4ByBank | Self::Lz4ByBankV2)
    }
}
