//! In-memory decoded HIPO record.

use crate::compress::{ScratchBuf, decompress};
use crate::error::{HipoError, Result};
use crate::wire::bytes::{Endianness, read_u32_le};
use crate::wire::constants::CompressionType;
use crate::wire::record_header::RecordHeader;

/// Result of [`decode_record_into`] — header plus offset metadata. The
/// decompressed payload and event-offset table live in caller-owned
/// `Vec`s so they can be recycled across records.
#[derive(Debug)]
pub struct DecodedRecord {
    pub header: RecordHeader,
    /// Byte offset of the data section within the decompressed payload.
    /// (= `index_array_length + user_header_length + user_header_padding`)
    pub data_start: u32,
}

/// Parse `src` (which begins with a 56-byte record header), decompress its
/// payload into `payload`, and refill `event_offsets` (cumulative byte
/// offsets within the data section; `event_count + 1` entries, first is
/// zero). Both `Vec`s are owned by the caller and are typically reused
/// across records to amortize allocation.
pub fn decode_record_into(
    src: &[u8],
    payload: &mut Vec<u8>,
    event_offsets: &mut Vec<u32>,
) -> Result<DecodedRecord> {
    let header = RecordHeader::parse(src)?;
    if src.len() < header.total_bytes() as usize {
        return Err(HipoError::FileTooSmall {
            actual: src.len() as u64,
            min: header.total_bytes(),
        });
    }
    let header_len = header.header_length as usize;
    let payload_disk_len = header.payload_bytes() as usize;
    let payload_disk = &src[header_len..header_len + payload_disk_len];

    let decompressed_size = header.decompressed_payload_size();
    let pad = header.compressed_data_padding as usize;
    let compressed_input = if matches!(header.compression, CompressionType::None) {
        payload_disk
    } else {
        let n = payload_disk_len.saturating_sub(pad);
        &payload_disk[..n]
    };

    decompress(
        header.compression,
        compressed_input,
        payload,
        decompressed_size,
    )?;

    // Build event offsets from the index array (first `index_array_length`
    // bytes of the decompressed payload). `event_count` comes straight from
    // the header and is decoupled from the produced payload length: a short
    // LZ4 decode (tolerated by `DECOMPRESS_SLACK`) or an inconsistent header
    // can leave `payload` shorter than `n*4`. Guard before the read loop so a
    // corrupt record surfaces as `CorruptRecord` instead of an out-of-bounds
    // read. (u64 math avoids wrapping `n*4` on a 32-bit usize.)
    let n = header.event_count as usize;
    if (payload.len() as u64) < (n as u64) * 4 {
        return Err(HipoError::CorruptRecord {
            offset: 0,
            reason: "index array shorter than event_count*4",
        });
    }
    event_offsets.clear();
    event_offsets.reserve(n + 1);
    event_offsets.push(0u32);
    let mut acc: u32 = 0;
    for i in 0..n {
        let raw = read_u32_le(payload, i * 4);
        let size = if matches!(header.endianness, Endianness::Big) {
            raw.swap_bytes()
        } else {
            raw
        };
        acc = acc.saturating_add(size);
        event_offsets.push(acc);
    }

    // The largest event ends at `data_start + acc`; validate it fits inside the
    // decompressed payload so the later zero-copy `payload[lo..hi]` slices (in
    // `Chain::event` and the event iterator) cannot go out of bounds. Offsets
    // are monotonic, so one check per record covers every event — the
    // per-event read path stays untouched. u64 math also closes the u32
    // overflow in the `data_start` sum on hostile headers.
    let data_start_u64 = header.index_array_length as u64
        + header.user_header_length as u64
        + header.user_header_padding as u64;
    let end = data_start_u64.checked_add(acc as u64);
    if data_start_u64 > u32::MAX as u64 || end.is_none_or(|e| e > payload.len() as u64) {
        return Err(HipoError::CorruptRecord {
            offset: 0,
            reason: "event offsets extend past record payload",
        });
    }
    let data_start = data_start_u64 as u32;

    Ok(DecodedRecord { header, data_start })
}

/// One decompressed HIPO record.
///
/// The record buffer is held internally in a [`ScratchBuf`] so consecutive
/// records reuse the same allocation. Event slices borrow from this buffer.
#[derive(Debug)]
pub struct Record {
    header: RecordHeader,
    /// Decompressed payload: index || user_header || user_pad || data.
    payload: ScratchBuf,
    /// Cumulative byte offsets within the *data* section: event `i` lives
    /// in `data[offsets[i]..offsets[i+1]]`. There are `event_count + 1`
    /// entries; the first is always 0.
    event_offsets: Vec<u32>,
}

impl Record {
    pub fn new() -> Self {
        Self {
            header: RecordHeader {
                record_length: 0,
                record_number: 0,
                header_length: 0,
                event_count: 0,
                index_array_length: 0,
                bit_info: 0,
                user_header_length: 0,
                data_length: 0,
                compressed_data_length: 0,
                compression: CompressionType::None,
                user_word_1: 0,
                user_word_2: 0,
                endianness: Endianness::Little,
                user_header_padding: 0,
                data_padding: 0,
                compressed_data_padding: 0,
            },
            payload: ScratchBuf::new(),
            event_offsets: Vec::new(),
        }
    }

    pub fn header(&self) -> &RecordHeader {
        &self.header
    }

    pub fn event_count(&self) -> u32 {
        self.header.event_count
    }

    /// Capacity of the underlying decompression buffer (testing aid).
    pub fn payload_capacity(&self) -> usize {
        self.payload.capacity()
    }

    /// Decode an entire record from a buffer containing the header at
    /// offset 0 followed by the payload.
    pub fn load(&mut self, compressed_record: &[u8]) -> Result<()> {
        let header = RecordHeader::parse(compressed_record)?;
        self.load_with_header(compressed_record, header)
    }

    /// Same as [`Self::load`] but the caller has already parsed the header.
    pub fn load_with_header(
        &mut self,
        compressed_record: &[u8],
        header: RecordHeader,
    ) -> Result<()> {
        if header.compression.is_by_bank() {
            // By-bank records can't be loaded into a single payload buffer —
            // they keep banks individually compressed for partial decode.
            // Callers must use `ByBankRecord::parse` instead. Bug to reach
            // here in production code.
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Record::load on by-bank record; use ByBankRecord::parse",
            });
        }
        if compressed_record.len() < header.total_bytes() as usize {
            return Err(HipoError::FileTooSmall {
                actual: compressed_record.len() as u64,
                min: header.total_bytes(),
            });
        }
        let header_len = header.header_length as usize;
        let payload_len_on_disk = header.payload_bytes() as usize;
        let payload_disk = &compressed_record[header_len..header_len + payload_len_on_disk];

        let decompressed_size = header.decompressed_payload_size();
        let pad = header.compressed_data_padding as usize;
        let compressed_input = if matches!(header.compression, CompressionType::None) {
            payload_disk
        } else {
            // Strip trailing compressed-data padding before decoding.
            let n = payload_len_on_disk.saturating_sub(pad);
            &payload_disk[..n]
        };

        decompress(
            header.compression,
            compressed_input,
            self.payload.vec_mut(),
            decompressed_size,
        )?;

        self.build_event_offsets(&header)?;
        self.header = header;
        Ok(())
    }

    fn build_event_offsets(&mut self, header: &RecordHeader) -> Result<()> {
        let n = header.event_count as usize;
        let payload = self.payload.as_slice();
        // Same guard as `decode_record_into`: the index array must actually
        // be present in the decompressed payload before we read it (the
        // read helpers are bounds-checked, but this turns a would-be panic
        // into a clean `CorruptRecord`).
        if (payload.len() as u64) < (n as u64) * 4 {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "index array shorter than event_count*4",
            });
        }
        self.event_offsets.clear();
        self.event_offsets.reserve(n + 1);
        self.event_offsets.push(0);

        let mut acc: u32 = 0;
        for i in 0..n {
            let raw = read_u32_le(payload, i * 4);
            let size = if matches!(header.endianness, Endianness::Big) {
                raw.swap_bytes()
            } else {
                raw
            };
            acc = acc.saturating_add(size);
            self.event_offsets.push(acc);
        }
        // Validate the largest event stays inside the payload, so `event()`'s
        // zero-copy `payload[lo..hi]` slice can never go out of bounds. One
        // check per record (offsets are monotonic); `event()` is untouched.
        let data_start = header.index_array_length as usize
            + header.user_header_length as usize
            + header.user_header_padding as usize;
        if data_start
            .checked_add(acc as usize)
            .is_none_or(|end| end > payload.len())
        {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "event offsets extend past record payload",
            });
        }
        Ok(())
    }

    /// Borrow the raw bytes of event `i`. Zero-copy; the slice points into
    /// the record's decompressed payload buffer.
    pub fn event(&self, i: u32) -> Option<&[u8]> {
        let i = i as usize;
        if i + 1 >= self.event_offsets.len() {
            return None;
        }
        let data_start = self.data_section_offset();
        let lo = data_start + self.event_offsets[i] as usize;
        let hi = data_start + self.event_offsets[i + 1] as usize;
        Some(&self.payload.as_slice()[lo..hi])
    }

    pub fn user_header(&self) -> &[u8] {
        let lo = self.header.index_array_length as usize;
        let hi = lo + self.header.user_header_length as usize;
        &self.payload.as_slice()[lo..hi]
    }

    pub fn iter(&self) -> RecordIter<'_> {
        RecordIter {
            record: self,
            next: 0,
        }
    }

    fn data_section_offset(&self) -> usize {
        self.header.index_array_length as usize
            + self.header.user_header_length as usize
            + self.header.user_header_padding as usize
    }
}

impl Default for Record {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct RecordIter<'a> {
    record: &'a Record,
    next: u32,
}

impl<'a> Iterator for RecordIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        let ev = self.record.event(self.next)?;
        self.next += 1;
        Some(ev)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.record.event_count().saturating_sub(self.next) as usize;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for RecordIter<'_> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress::compress;
    use crate::wire::bytes::write_u32_le;
    use crate::wire::constants::*;

    fn build_test_record(events: &[&[u8]], compression: CompressionType) -> Vec<u8> {
        let event_count = events.len() as u32;
        let index_array_length = event_count * 4;
        let mut payload = vec![0u8; index_array_length as usize];
        for (i, ev) in events.iter().enumerate() {
            write_u32_le(&mut payload, i * 4, ev.len() as u32);
        }
        let data_start = payload.len();
        for ev in events {
            payload.extend_from_slice(ev);
        }
        let data_length = (payload.len() - data_start) as u32;

        let mut compressed = Vec::new();
        compress(compression, &payload, &mut compressed).unwrap();
        let compressed_len = compressed.len() as u32;

        let pad = (4 - (compressed_len % 4)) % 4;
        compressed.extend(std::iter::repeat_n(0u8, pad as usize));

        let header_length = RECORD_HEADER_SIZE as u32;
        let total_bytes = header_length + compressed.len() as u32;

        let mut bit_info: u32 = 6;
        bit_info |= (pad & BITINFO_PAD_MASK) << BITINFO_PAD3_SHIFT;

        let header = RecordHeader {
            record_length: total_bytes as u64,
            record_number: 1,
            header_length,
            event_count,
            index_array_length,
            bit_info,
            user_header_length: 0,
            data_length,
            compressed_data_length: compressed_len,
            compression,
            user_word_1: 0,
            user_word_2: 0,
            endianness: Endianness::Little,
            user_header_padding: 0,
            data_padding: 0,
            compressed_data_padding: pad as u8,
        };

        let mut out = vec![0u8; total_bytes as usize];
        let header_buf: &mut [u8; RECORD_HEADER_SIZE] =
            (&mut out[..RECORD_HEADER_SIZE]).try_into().unwrap();
        header.write(header_buf);
        out[RECORD_HEADER_SIZE..].copy_from_slice(&compressed);
        out
    }

    #[test]
    fn round_trip_uncompressed() {
        let evs: &[&[u8]] = &[b"hello", b"world!!", b"x"];
        let raw = build_test_record(evs, CompressionType::None);
        let mut rec = Record::new();
        rec.load(&raw).unwrap();
        assert_eq!(rec.event_count(), 3);
        assert_eq!(rec.event(0).unwrap(), b"hello");
        assert_eq!(rec.event(1).unwrap(), b"world!!");
        assert_eq!(rec.event(2).unwrap(), b"x");
        assert!(rec.event(3).is_none());

        let collected: Vec<&[u8]> = rec.iter().collect();
        assert_eq!(collected, evs);
    }

    #[test]
    fn round_trip_lz4() {
        let evs: &[&[u8]] = &[
            &[0xAB; 200],
            &[0xCD; 1024],
            b"the quick brown fox jumps over the lazy dog",
        ];
        let raw = build_test_record(evs, CompressionType::Lz4);
        let mut rec = Record::new();
        rec.load(&raw).unwrap();
        assert_eq!(rec.event_count(), 3);
        assert_eq!(rec.event(1).unwrap().len(), 1024);
        assert!(rec.event(1).unwrap().iter().all(|&b| b == 0xCD));
    }

    #[test]
    fn empty_record() {
        let raw = build_test_record(&[], CompressionType::None);
        let mut rec = Record::new();
        rec.load(&raw).unwrap();
        assert_eq!(rec.event_count(), 0);
        assert_eq!(rec.iter().count(), 0);
    }

    #[test]
    fn buffer_reuse() {
        let mut rec = Record::new();
        let raw1 = build_test_record(&[&[1; 100]], CompressionType::Lz4);
        let raw2 = build_test_record(&[&[2; 100]], CompressionType::Lz4);
        rec.load(&raw1).unwrap();
        let cap_after_first = rec.payload_capacity();
        rec.load(&raw2).unwrap();
        assert!(rec.payload_capacity() >= cap_after_first);
    }
}
