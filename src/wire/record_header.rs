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

    /// Does this record carry the dictionary?
    ///
    /// Uses the same named bit as [`FileHeader::has_dictionary`], which reads
    /// the identical `bit_info` word. These had drifted: this accessor read bit
    /// **10** — the constant for *trailer-with-index* — while `FileHeader` read
    /// bit 8. Both cannot be right about one word's layout.
    ///
    /// [`FileHeader::has_dictionary`]: crate::wire::file_header::FileHeader::has_dictionary
    pub fn has_dictionary(&self) -> bool {
        (self.bit_info >> BITINFO_HAS_DICTIONARY_BIT) & 1 == 1
    }

    /// Does this record carry the "first event" that HIPO lets a file pin?
    ///
    /// Read bit **11** before, which no constant names; the first-event flag is
    /// bit 9.
    pub fn has_first_event(&self) -> bool {
        (self.bit_info >> BITINFO_HAS_FIRST_EVENT_BIT) & 1 == 1
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
        // Big-endian records are supported: the header words are swapped here,
        // and the decompressed payload is normalized to little-endian once per
        // record (see `wire::endian`).
        let big = matches!(endianness, Endianness::Big);
        let r32 = |off| {
            let v = read_u32_le(buf, off);
            if big { v.swap_bytes() } else { v }
        };
        let r64 = |off| {
            let v = read_u64_le(buf, off);
            if big { v.swap_bytes() } else { v }
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

        let record_length = u64::from(record_length_words) * 4;
        let header_length = header_length_words.saturating_mul(4);
        // `payload_bytes()` is `record_length - header_length`; both are
        // attacker-controlled. Enforce the invariant here so that subtraction
        // (and the slice indexing that uses it) can never underflow on a
        // corrupt header — a hostile `header_length > record_length` is
        // rejected as corruption rather than panicking the reader.
        if u64::from(header_length) > record_length {
            return Err(HipoError::CorruptRecord {
                offset: RH_RECORD_LENGTH as u64,
                reason: "record header_length exceeds record_length",
            });
        }

        Ok(Self {
            record_length,
            record_number,
            header_length,
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

    #[test]
    fn parses_big_endian() {
        // Swap every multi-byte field and stamp the big-endian magic; the
        // parsed values must match the little-endian original exactly.
        let mut le = [0u8; RECORD_HEADER_SIZE];
        sample().write(&mut le);
        let expect = RecordHeader::parse(&le).unwrap();

        let mut be = le;
        for off in [
            RH_RECORD_LENGTH,
            RH_RECORD_NUMBER,
            RH_HEADER_LENGTH,
            RH_EVENT_COUNT,
            RH_INDEX_ARRAY_LEN,
            RH_BIT_INFO,
            RH_USER_HEADER_LEN,
            RH_DATA_LENGTH,
            RH_COMP_WORD,
        ] {
            be[off..off + 4].reverse();
        }
        be[RH_USER_WORD1..RH_USER_WORD1 + 8].reverse();
        be[RH_USER_WORD2..RH_USER_WORD2 + 8].reverse();
        write_u32_le(&mut be, RH_MAGIC_NUMBER, HEADER_MAGIC_BE);

        let got = RecordHeader::parse(&be).unwrap();
        assert!(matches!(got.endianness, Endianness::Big));
        assert_eq!(got.record_length, expect.record_length);
        assert_eq!(got.event_count, expect.event_count);
        assert_eq!(got.index_array_length, expect.index_array_length);
        assert_eq!(got.data_length, expect.data_length);
        assert_eq!(got.compression, expect.compression);
        assert_eq!(got.user_word_1, expect.user_word_1);
        assert_eq!(got.user_word_2, expect.user_word_2);
    }

    #[test]
    fn reclaimed_tags_4_and_5_are_zstd_and_only_15_is_unassigned() {
        use crate::wire::constants::{Codec, Layout};

        // Tags 4 and 5 carried `Lz4Chunked` and `Lz4ByBank` v1, both removed
        // during 0.x and rejected at parse ever since. They now carry Zstd.
        //
        // Reusing a tag is only safe if a stale file fails *loudly*, and this
        // is why Zstd went into these two slots specifically: a zstd frame
        // begins with the magic 0xFD2FB528, so an old Lz4Chunked payload does
        // not decode as anything — it fails the frame check. No other codec in
        // these slots would have that property, which is what makes the reuse
        // defensible rather than merely convenient.
        for (tag, want) in [
            (4u32, (Codec::Zstd, Layout::PerChunk)),
            (5, (Codec::Zstd, Layout::PerBank)),
        ] {
            let mut buf = [0u8; RECORD_HEADER_SIZE];
            sample().write(&mut buf);
            write_u32_le(&mut buf, RH_COMP_WORD, tag << COMP_TYPE_SHIFT);
            let h = RecordHeader::parse(&buf)
                .unwrap_or_else(|e| panic!("tag {tag} should now parse: {e:?}"));
            assert_eq!((h.compression.codec(), h.compression.layout()), want);
        }

        // 15 is the only tag left unassigned, and must still be rejected.
        let mut buf = [0u8; RECORD_HEADER_SIZE];
        sample().write(&mut buf);
        write_u32_le(&mut buf, RH_COMP_WORD, 15u32 << COMP_TYPE_SHIFT);
        let err = RecordHeader::parse(&buf).unwrap_err();
        assert!(
            matches!(err, HipoError::UnknownCompression(15)),
            "tag 15 should be rejected, got {err:?}"
        );

        // Every one of the 15 assigned tags round-trips through the pair.
        for tag in 0u8..15 {
            let ct = crate::wire::constants::CompressionType::from_tag(tag)
                .unwrap_or_else(|| panic!("tag {tag} should be assigned"));
            assert_eq!(
                crate::wire::constants::CompressionType::for_pair(ct.codec(), ct.layout()),
                ct,
                "tag {tag} does not round-trip through (codec, layout)"
            );
        }
    }

    #[test]
    fn rejects_header_longer_than_record() {
        let mut buf = [0u8; RECORD_HEADER_SIZE];
        let mut h = sample();
        h.record_length = 4096; // 1024 words
        h.header_length = 8192; // 2048 words > record_length
        h.write(&mut buf);
        let err = RecordHeader::parse(&buf).unwrap_err();
        assert!(matches!(err, HipoError::CorruptRecord { .. }));
    }
}
