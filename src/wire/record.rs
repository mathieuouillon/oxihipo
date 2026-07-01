//! In-memory decoded HIPO record.

use crate::compress::{ScratchBuf, decompress, decompress_into_slice};
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

    if matches!(header.compression, CompressionType::Lz4Chunked) {
        // `payload_disk` (minus trailing alignment padding) is the
        // chunked compressed payload section. Inflate in-place into
        // `payload`, build offsets from the inline `event_sizes[]` table.
        let pad = header.compressed_data_padding as usize;
        let section = &payload_disk[..payload_disk_len.saturating_sub(pad)];
        decode_chunked_into(&header, section, payload, event_offsets)?;
        return Ok(DecodedRecord {
            header,
            data_start: 0,
        });
    }

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

    let data_start =
        header.index_array_length + header.user_header_length + header.user_header_padding as u32;

    Ok(DecodedRecord { header, data_start })
}

/// Decode an `Lz4Chunked` compressed payload section into `payload`
/// (resized to the total decompressed bytes) and build `event_offsets`
/// directly from the inline `event_sizes[]` table.
///
/// Layout — see `build_chunked_record_bytes` in `write::record` for the
/// authoritative spec. The section length is
/// `Σ compressed_chunk_sizes + table_bytes` (no trailing padding — the
/// caller has already stripped the 4-byte alignment pad).
fn decode_chunked_into(
    header: &RecordHeader,
    section: &[u8],
    payload: &mut Vec<u8>,
    event_offsets: &mut Vec<u32>,
) -> Result<()> {
    // We don't support big-endian chunked records — the writer always
    // emits little-endian, and there are no historical chunked records
    // in the wild that we'd need to byte-swap. Reject explicitly so the
    // failure mode is loud.
    if matches!(header.endianness, Endianness::Big) {
        return Err(HipoError::CorruptRecord {
            offset: 0,
            reason: "Lz4Chunked: big-endian records not supported",
        });
    }

    let event_count = header.event_count as usize;

    if section.len() < 8 {
        return Err(HipoError::CorruptRecord {
            offset: 0,
            reason: "Lz4Chunked: chunk table truncated (header)",
        });
    }
    let num_chunks = read_u32_le(section, 0) as usize;
    // events_per_chunk (offset 4) is informational — we don't need it
    // for decoding since each chunk's bytes/decompressed-size are
    // explicit in the tables below.

    let event_sizes_off = 8;
    let event_sizes_bytes = event_count * 4;
    let comp_sizes_off = event_sizes_off + event_sizes_bytes;
    let comp_sizes_bytes = num_chunks * 4;
    let decomp_sizes_off = comp_sizes_off + comp_sizes_bytes;
    let decomp_sizes_bytes = num_chunks * 4;
    let payload_off = decomp_sizes_off + decomp_sizes_bytes;

    if section.len() < payload_off {
        return Err(HipoError::CorruptRecord {
            offset: 0,
            reason: "Lz4Chunked: chunk table truncated (sizes)",
        });
    }

    // Build event_offsets from the inline `event_sizes[]` table — no
    // decompression needed for this step (the partial-decompression win).
    event_offsets.clear();
    event_offsets.reserve(event_count + 1);
    event_offsets.push(0u32);
    let mut acc: u32 = 0;
    for i in 0..event_count {
        let size = read_u32_le(section, event_sizes_off + i * 4);
        acc = acc.saturating_add(size);
        event_offsets.push(acc);
    }
    let total_decompressed = acc as usize;

    // Sanity: total_decompressed must equal Σ decompressed_chunk_sizes
    // and must equal header.data_length.
    if total_decompressed != header.data_length as usize {
        return Err(HipoError::CorruptRecord {
            offset: 0,
            reason: "Lz4Chunked: Σ event_sizes != header.data_length",
        });
    }

    // Parse per-chunk sizes and validate the total compressed length.
    let mut comp_sizes: Vec<u32> = Vec::with_capacity(num_chunks);
    let mut decomp_sizes: Vec<u32> = Vec::with_capacity(num_chunks);
    let mut sum_decomp: u64 = 0;
    let mut sum_comp: u64 = 0;
    for c in 0..num_chunks {
        let cs = read_u32_le(section, comp_sizes_off + c * 4);
        let ds = read_u32_le(section, decomp_sizes_off + c * 4);
        comp_sizes.push(cs);
        decomp_sizes.push(ds);
        sum_comp += cs as u64;
        sum_decomp += ds as u64;
    }
    if sum_decomp as usize != total_decompressed {
        return Err(HipoError::CorruptRecord {
            offset: 0,
            reason: "Lz4Chunked: Σ decompressed_chunk_sizes != Σ event_sizes",
        });
    }
    if payload_off + sum_comp as usize > section.len() {
        return Err(HipoError::CorruptRecord {
            offset: 0,
            reason: "Lz4Chunked: chunk payloads run past section end",
        });
    }

    // Resize the destination buffer to the exact total. Existing
    // capacity is preserved on shrink.
    payload.clear();
    if payload.capacity() < total_decompressed {
        payload.reserve_exact(total_decompressed - payload.capacity());
    }
    payload.resize(total_decompressed, 0);

    // Split the destination into N disjoint mutable slices, one per
    // chunk. `split_at_mut` lets us hand out non-aliased slices for
    // parallel use.
    let mut dst_slices: Vec<&mut [u8]> = Vec::with_capacity(num_chunks);
    {
        let mut rest: &mut [u8] = payload.as_mut_slice();
        for &ds in &decomp_sizes {
            let (head, tail) = rest.split_at_mut(ds as usize);
            dst_slices.push(head);
            rest = tail;
        }
        debug_assert!(rest.is_empty());
    }

    // Slice the source side analogously.
    let mut src_slices: Vec<&[u8]> = Vec::with_capacity(num_chunks);
    {
        let mut off = payload_off;
        for &cs in &comp_sizes {
            let cs = cs as usize;
            src_slices.push(&section[off..off + cs]);
            off += cs;
        }
    }

    // Inflate chunks in parallel. `rayon::scope` uses the global pool;
    // when called inside a `for_each` worker (nested rayon), it
    // shares that pool — no deadlock. For sequential callers this is
    // a free win: idle cores on the record get used.
    //
    // We collect per-task results into a Vec<Result<()>> to surface
    // the first error after the scope completes (panicking inside
    // rayon::scope would propagate, which is heavier than we want).
    let work: Vec<(usize, &[u8], &mut [u8])> = src_slices
        .into_iter()
        .zip(dst_slices)
        .enumerate()
        .map(|(i, (s, d))| (i, s, d))
        .collect();

    // One result slot per chunk; written by exactly one thread.
    let mut results: Vec<Option<Result<()>>> = (0..num_chunks).map(|_| None).collect();
    {
        let results_slots: Vec<&mut Option<Result<()>>> = results.iter_mut().collect();
        rayon::scope(|s| {
            for ((idx, src, dst), out_slot) in work.into_iter().zip(results_slots) {
                s.spawn(move |_| {
                    let r = decompress_into_slice(CompressionType::Lz4, src, dst).map(|_| ());
                    *out_slot = Some(r);
                    let _ = idx; // index only used for debugging
                });
            }
        });
    }
    for r in results {
        match r {
            Some(Ok(())) => {}
            Some(Err(e)) => return Err(e),
            None => {
                return Err(HipoError::Compression(
                    "Lz4Chunked: rayon worker dropped without writing result",
                ));
            }
        }
    }

    Ok(())
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
            // By-bank records (v1/v2) can't be loaded into a single payload
            // buffer — they keep banks individually compressed for
            // partial decode. Callers must use `ByBankRecord::parse`
            // instead. Bug to reach here in production code.
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

        if matches!(header.compression, CompressionType::Lz4Chunked) {
            let pad = header.compressed_data_padding as usize;
            let section = &payload_disk[..payload_len_on_disk.saturating_sub(pad)];
            decode_chunked_into(
                &header,
                section,
                self.payload.vec_mut(),
                &mut self.event_offsets,
            )?;
            self.header = header;
            return Ok(());
        }

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
        if matches!(self.header.compression, CompressionType::Lz4Chunked) {
            // Chunked records carry no user header in the decompressed
            // payload (and writers never produce one for data records).
            return &[];
        }
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
        if matches!(self.header.compression, CompressionType::Lz4Chunked) {
            // Chunked records skip the index array entirely — the
            // `event_sizes[]` table lives in the compressed payload
            // section, not inside the decompressed buffer. The
            // decompressed buffer is just concatenated event bytes.
            0
        } else {
            self.header.index_array_length as usize
                + self.header.user_header_length as usize
                + self.header.user_header_padding as usize
        }
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
    fn round_trip_lz4_chunked() {
        // Mix sizes and counts so we exercise: short last chunk, byte
        // events, kilobyte events.
        let big_a = vec![0xAB_u8; 200];
        let big_b = vec![0xCD_u8; 1024];
        let big_c = vec![0x12_u8; 8 * 1024];
        let small = b"the quick brown fox".to_vec();
        let events: Vec<Vec<u8>> = vec![
            big_a.clone(),
            big_b.clone(),
            small.clone(),
            big_c.clone(),
            small.clone(),
            big_a.clone(),
            big_b.clone(),
        ];
        let refs: Vec<&[u8]> = events.iter().map(|e| e.as_slice()).collect();

        // events_per_chunk = 3 → 3 chunks (3 / 3 / 1).
        let mut payload_buf = Vec::new();
        let mut compress_buf = Vec::new();
        let raw = crate::write::record::build_record_bytes(
            &refs,
            &crate::schema::Dict::default(),
            0,
            0,
            crate::write::Compression::Lz4Chunked {
                events_per_chunk: 3,
            },
            1,
            &mut payload_buf,
            &mut compress_buf,
        )
        .unwrap();

        // Load through the public Record API and assert byte equality.
        let mut rec = Record::new();
        rec.load(&raw).unwrap();
        assert_eq!(rec.event_count(), events.len() as u32);
        for (i, expected) in events.iter().enumerate() {
            assert_eq!(rec.event(i as u32).unwrap(), expected.as_slice());
        }

        // Also exercise decode_record_into (the free function used by
        // the chain reader).
        let mut payload = Vec::new();
        let mut offsets = Vec::new();
        let dec = decode_record_into(&raw, &mut payload, &mut offsets).unwrap();
        assert_eq!(dec.header.compression, CompressionType::Lz4Chunked);
        assert_eq!(dec.data_start, 0);
        // For each event, check its byte range in the decompressed
        // payload matches the expected source bytes.
        for (i, expected) in events.iter().enumerate() {
            let lo = offsets[i] as usize;
            let hi = offsets[i + 1] as usize;
            assert_eq!(&payload[lo..hi], expected.as_slice());
        }
    }

    #[test]
    fn round_trip_lz4_chunked_last_partial() {
        // 5 events with events_per_chunk = 2 → 3 chunks (2 / 2 / 1).
        let evs: Vec<Vec<u8>> = (0..5)
            .map(|i| {
                let mut v = vec![0u8; 64 + i as usize * 8];
                for (j, b) in v.iter_mut().enumerate() {
                    *b = ((i * 31 + j as i32) & 0xff) as u8;
                }
                v
            })
            .collect();
        let refs: Vec<&[u8]> = evs.iter().map(|e| e.as_slice()).collect();

        let mut payload_buf = Vec::new();
        let mut compress_buf = Vec::new();
        let raw = crate::write::record::build_record_bytes(
            &refs,
            &crate::schema::Dict::default(),
            0,
            0,
            crate::write::Compression::Lz4Chunked {
                events_per_chunk: 2,
            },
            42,
            &mut payload_buf,
            &mut compress_buf,
        )
        .unwrap();

        let mut rec = Record::new();
        rec.load(&raw).unwrap();
        assert_eq!(rec.event_count(), 5);
        for (i, expected) in evs.iter().enumerate() {
            assert_eq!(rec.event(i as u32).unwrap(), expected.as_slice());
        }
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
