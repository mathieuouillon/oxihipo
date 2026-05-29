//! HIPO file header — first 56 bytes of every HIPO file.

use crate::error::{HipoError, Result};
use crate::wire::bytes::{Endianness, read_u32_le, read_u64_le, write_u32_le, write_u64_le};
use crate::wire::constants::*;

/// Decoded file header.
///
/// All fields are stored in their *native* (decoded) representation, not
/// raw words: lengths are byte counts, not word counts.
#[derive(Debug, Clone)]
pub struct FileHeader {
    pub file_number: u32,
    /// Total header length **in bytes** (the on-disk word count, * 4).
    pub header_length: u32,
    pub record_count: u32,
    /// Length of the index array in bytes.
    pub index_array_length: u32,
    pub bit_info: u32,
    pub user_header_length: u32,
    pub user_register: u64,
    pub trailer_position: u64,
    pub user_int1: u32,
    pub user_int2: u32,
    pub endianness: Endianness,
}

impl FileHeader {
    pub fn version(&self) -> u32 {
        self.bit_info & BITINFO_VERSION_MASK
    }

    pub fn has_trailer_with_index(&self) -> bool {
        (self.bit_info >> BITINFO_TRAILER_WITH_INDEX_BIT) & 1 == 1
    }

    pub fn has_dictionary(&self) -> bool {
        (self.bit_info >> BITINFO_HAS_DICTIONARY_BIT) & 1 == 1
    }

    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < FILE_HEADER_SIZE {
            return Err(HipoError::FileTooSmall {
                actual: buf.len() as u64,
                min: FILE_HEADER_SIZE as u64,
            });
        }

        let unique = read_u32_le(buf, FH_UNIQUE_WORD);
        if unique != HIPO_FILE_UNIQUE_WORD {
            return Err(HipoError::BadMagic {
                offset: 0,
                found: unique,
                expected: HIPO_FILE_UNIQUE_WORD,
            });
        }

        let magic = read_u32_le(buf, FH_MAGIC_NUMBER);
        let endianness = Endianness::from_magic(magic).ok_or(HipoError::BadMagic {
            offset: FH_MAGIC_NUMBER as u64,
            found: magic,
            expected: HEADER_MAGIC,
        })?;

        let swap = matches!(endianness, Endianness::Big);
        let r32 = |off| {
            let v = read_u32_le(buf, off);
            if swap { v.swap_bytes() } else { v }
        };
        let r64 = |off| {
            let v = read_u64_le(buf, off);
            if swap { v.swap_bytes() } else { v }
        };

        let header = Self {
            file_number: r32(FH_FILE_NUMBER),
            header_length: r32(FH_HEADER_LENGTH).saturating_mul(4),
            record_count: r32(FH_RECORD_COUNT),
            index_array_length: r32(FH_INDEX_ARRAY_LEN),
            bit_info: r32(FH_BIT_INFO),
            user_header_length: r32(FH_USER_HEADER_LEN),
            user_register: r64(FH_USER_REGISTER),
            trailer_position: r64(FH_TRAILER_POS),
            user_int1: r32(FH_USER_INT1),
            user_int2: r32(FH_USER_INT2),
            endianness,
        };

        let v = header.version();
        if v == 0 || v > HIPO_VERSION {
            return Err(HipoError::UnsupportedVersion { version: v });
        }
        Ok(header)
    }

    pub fn write(&self, out: &mut [u8; FILE_HEADER_SIZE]) {
        write_u32_le(out, FH_UNIQUE_WORD, HIPO_FILE_UNIQUE_WORD);
        write_u32_le(out, FH_FILE_NUMBER, self.file_number);
        write_u32_le(out, FH_HEADER_LENGTH, self.header_length / 4);
        write_u32_le(out, FH_RECORD_COUNT, self.record_count);
        write_u32_le(out, FH_INDEX_ARRAY_LEN, self.index_array_length);
        write_u32_le(out, FH_BIT_INFO, self.bit_info);
        write_u32_le(out, FH_USER_HEADER_LEN, self.user_header_length);
        write_u32_le(out, FH_MAGIC_NUMBER, HEADER_MAGIC);
        write_u64_le(out, FH_USER_REGISTER, self.user_register);
        write_u64_le(out, FH_TRAILER_POS, self.trailer_position);
        write_u32_le(out, FH_USER_INT1, self.user_int1);
        write_u32_le(out, FH_USER_INT2, self.user_int2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> FileHeader {
        FileHeader {
            file_number: 1,
            header_length: 56,
            record_count: 42,
            index_array_length: 168,
            bit_info: 0x5000_0006 | 0x400, // version 6, trailer-with-index bit
            user_header_length: 0,
            user_register: 0xDEAD_BEEF_DEAD_BEEF,
            trailer_position: 0x1234_5678,
            user_int1: 1,
            user_int2: 2,
            endianness: Endianness::Little,
        }
    }

    #[test]
    fn round_trip() {
        let mut buf = [0u8; FILE_HEADER_SIZE];
        sample().write(&mut buf);
        let parsed = FileHeader::parse(&buf).unwrap();
        assert_eq!(parsed.file_number, 1);
        assert_eq!(parsed.record_count, 42);
        assert_eq!(parsed.version(), 6);
        assert!(parsed.has_trailer_with_index());
        assert!(!parsed.has_dictionary());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = [0u8; FILE_HEADER_SIZE];
        sample().write(&mut buf);
        buf[0] = 0xAA;
        let err = FileHeader::parse(&buf).unwrap_err();
        assert!(matches!(err, HipoError::BadMagic { .. }));
    }

    #[test]
    fn rejects_bad_endian_magic() {
        let mut buf = [0u8; FILE_HEADER_SIZE];
        sample().write(&mut buf);
        write_u32_le(&mut buf, FH_MAGIC_NUMBER, 0xDEAD_BEEF);
        let err = FileHeader::parse(&buf).unwrap_err();
        assert!(matches!(err, HipoError::BadMagic { .. }));
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut buf = [0u8; FILE_HEADER_SIZE];
        let mut h = sample();
        h.bit_info = 0x5000_00FF;
        h.write(&mut buf);
        let err = FileHeader::parse(&buf).unwrap_err();
        assert!(matches!(err, HipoError::UnsupportedVersion { .. }));
    }

    #[test]
    fn too_small() {
        let buf = [0u8; 16];
        let err = FileHeader::parse(&buf).unwrap_err();
        assert!(matches!(err, HipoError::FileTooSmall { .. }));
    }
}
