//! HIPO record header — first 56 bytes of every record.

use crate::error::{HipoError, Result};
use crate::wire::bytes::{Endianness, read_u32_le, read_u64_le, write_u32_le, write_u64_le};
use crate::wire::constants::*;

/// Decoded record header.
///
/// Lengths are byte counts, not word counts. The compression word is split
/// into the type and the compressed-data length.
#[derive(Debug, Clone)]
pub struct RecordHeader {
    /// Total record length in bytes (header + payload).
    pub record_length: u64,
    pub record_number: u32,
    /// Header length in bytes.
    pub header_length: u32,
    pub event_count: u32,
    /// Index array length in bytes (always 4 * event_count).
    pub index_array_length: u32,
    pub bit_info: u32,
    pub user_header_length: u32,
    /// Decompressed data length (bytes) of the record payload.
    pub data_length: u32,
    /// Compressed data length **in bytes** (decoded from the comp word).
    pub compressed_data_length: u32,
    pub compression: CompressionType,
    pub user_word_1: u64,
    pub user_word_2: u64,
    pub endianness: Endianness,
    pub user_header_padding: u8,
    pub data_padding: u8,
    pub compressed_data_padding: u8,
}

impl RecordHeader {
    pub fn version(&self) -> u32 {
        self.bit_info & BITINFO_VERSION_MASK
    }

    pub fn is_last_record(&self) -> bool {
        (self.bit_info >> 8) & 1 == 1
    }

    pub fn has_dictionary(&self) -> bool {
        (self.bit_info >> 10) & 1 == 1
    }

    pub fn has_first_event(&self) -> bool {
        (self.bit_info >> 11) & 1 == 1
    }

    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < RECORD_HEADER_SIZE {
            return Err(HipoError::FileTooSmall {
                actual: buf.len() as u64,
                min: RECORD_HEADER_SIZE as u64,
            });
        }

        let magic = read_u32_le(buf, RH_MAGIC_NUMBER);
        let endianness = Endianness::from_magic(magic).ok_or(HipoError::BadMagic {
            offset: RH_MAGIC_NUMBER as u64,
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

        let record_length_words = r32(RH_RECORD_LENGTH);
        let record_number = r32(RH_RECORD_NUMBER);
        let header_length_words = r32(RH_HEADER_LENGTH);
        let event_count = r32(RH_EVENT_COUNT);
        let index_array_length = r32(RH_INDEX_ARRAY_LEN);
        let bit_info = r32(RH_BIT_INFO);
        let user_header_length = r32(RH_USER_HEADER_LEN);
        let data_length = r32(RH_DATA_LENGTH);
        let comp_word = r32(RH_COMP_WORD);
        let user_word_1 = r64(RH_USER_WORD1);
        let user_word_2 = r64(RH_USER_WORD2);

        let compression = CompressionType::from_word(comp_word).ok_or(
            HipoError::UnknownCompression((comp_word >> COMP_TYPE_SHIFT) & COMP_TYPE_BYTE),
        )?;
        let compressed_words = comp_word & COMP_LENGTH_MASK;

        let user_header_padding = ((bit_info >> BITINFO_PAD1_SHIFT) & BITINFO_PAD_MASK) as u8;
        let data_padding = ((bit_info >> BITINFO_PAD2_SHIFT) & BITINFO_PAD_MASK) as u8;
        let compressed_data_padding = ((bit_info >> BITINFO_PAD3_SHIFT) & BITINFO_PAD_MASK) as u8;

        Ok(Self {
            record_length: u64::from(record_length_words) * 4,
            record_number,
            header_length: header_length_words.saturating_mul(4),
            event_count,
            index_array_length,
            bit_info,
            user_header_length,
            data_length,
            compressed_data_length: compressed_words.saturating_mul(4),
            compression,
            user_word_1,
            user_word_2,
            endianness,
            user_header_padding,
            data_padding,
            compressed_data_padding,
        })
    }

    pub fn total_bytes(&self) -> u64 {
        self.record_length
    }

    pub fn payload_bytes(&self) -> u64 {
        self.record_length - u64::from(self.header_length)
    }

    /// Decompressed payload size: index_array + user_header + pad + data.
    pub fn decompressed_payload_size(&self) -> usize {
        self.index_array_length as usize
            + self.user_header_length as usize
            + self.user_header_padding as usize
            + self.data_length as usize
    }

    pub fn write(&self, out: &mut [u8; RECORD_HEADER_SIZE]) {
        let comp_word = ((self.compression as u32) << COMP_TYPE_SHIFT)
            | ((self.compressed_data_length / 4) & COMP_LENGTH_MASK);

        write_u32_le(out, RH_RECORD_LENGTH, (self.record_length / 4) as u32);
        write_u32_le(out, RH_RECORD_NUMBER, self.record_number);
        write_u32_le(out, RH_HEADER_LENGTH, self.header_length / 4);
        write_u32_le(out, RH_EVENT_COUNT, self.event_count);
        write_u32_le(out, RH_INDEX_ARRAY_LEN, self.index_array_length);
        write_u32_le(out, RH_BIT_INFO, self.bit_info);
        write_u32_le(out, RH_USER_HEADER_LEN, self.user_header_length);
        write_u32_le(out, RH_MAGIC_NUMBER, HEADER_MAGIC);
        write_u32_le(out, RH_DATA_LENGTH, self.data_length);
        write_u32_le(out, RH_COMP_WORD, comp_word);
        write_u64_le(out, RH_USER_WORD1, self.user_word_1);
        write_u64_le(out, RH_USER_WORD2, self.user_word_2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RecordHeader {
        RecordHeader {
            record_length: 4096,
            record_number: 7,
            header_length: 56,
            event_count: 100,
            index_array_length: 400,
            bit_info: 0x4000_0006,
            user_header_length: 0,
            data_length: 8000,
            compressed_data_length: 4000,
            compression: CompressionType::Lz4,
            user_word_1: 0,
            user_word_2: 0,
            endianness: Endianness::Little,
            user_header_padding: 0,
            data_padding: 0,
            compressed_data_padding: 0,
        }
    }

    #[test]
    fn round_trip() {
        let mut buf = [0u8; RECORD_HEADER_SIZE];
        sample().write(&mut buf);
        let parsed = RecordHeader::parse(&buf).unwrap();
        assert_eq!(parsed.record_number, 7);
        assert_eq!(parsed.event_count, 100);
        assert_eq!(parsed.compression, CompressionType::Lz4);
        assert_eq!(parsed.compressed_data_length, 4000);
        assert_eq!(parsed.data_length, 8000);
    }

    #[test]
    fn decompressed_size_includes_padding() {
        let h = sample();
        assert_eq!(h.decompressed_payload_size(), 8400);
    }
}
