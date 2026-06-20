//! `Lz4ByBank` record decoder + lazy per-bank decompression cache.
//!
//! This is the partial-decompression backend. The on-disk layout stores
//! each bank type as its own LZ4 stream within the record; the reader
//! parses the directory eagerly (cheap) and inflates a stream only when
//! `ev.bank(name)` actually requests one. Streams never read by the
//! analysis stay compressed for the record's lifetime.
//!
//! See `crate::write::record::build_by_bank_record_bytes` for the wire
//! spec.

use std::sync::{Arc, OnceLock};

use crate::compress::decompress;
use crate::error::{HipoError, Result};
use crate::wire::bytes::read_u32_le;
use crate::wire::constants::CompressionType;
use crate::wire::record_header::RecordHeader;

/// Counts bank-stream inflate calls. Active under `cfg(test)` or when
/// the (non-default) `test-instrumentation` feature is enabled —
/// otherwise zero overhead. Used to prove the partial-decompression
/// contract from tests.
#[cfg(test)]
pub static BANK_INFLATE_COUNTER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Shared state attached to every `OwnedEvent` yielded from an
/// `Lz4ByBank` record. Lifetime: as long as any event from this record
/// is live. Heap allocations: one per record (this struct), one per
/// bank-stream that's actually decompressed.
#[derive(Debug)]
pub struct ByBankRecord {
    /// The record header (preserved for tags / metadata).
    pub header: RecordHeader,
    /// Bank descriptors in directory order.
    descriptors: Vec<BankDescriptor>,
    /// Number of events in this record.
    event_count: u32,
    /// Per-event tag (EventHeader.tag).
    event_tags: Vec<u32>,
    /// Packed presence matrix: `bytes_per_row` bytes per event, row-major.
    /// Bit `(e * bytes_per_row + b/8) & (1 << (b%8))` = 1 iff event e has bank b.
    presence: Vec<u8>,
    bytes_per_row: usize,
    /// Per-bank **cumulative** byte offsets into the decompressed stream:
    /// `bank_offsets[b]` has `event_count + 1` entries, and event `e`'s
    /// bank-`b` data occupies `bank_offsets[b][e]..bank_offsets[b][e+1]`.
    /// Precomputing the prefix sum once at parse makes `bank_byte_range`
    /// and `bank_size` O(1) (previously an O(events) re-summation per call,
    /// i.e. O(events²·banks) to walk a record).
    bank_offsets: Vec<Vec<u32>>,
    /// `bank_data[b]` lazily holds the decompressed bank stream once
    /// any caller has touched it.
    bank_data: Vec<OnceLock<Box<[u8]>>>,
    /// Owned copy of the **compressed** section (the slice between the
    /// record header and the trailing pad). Used by lazy decompression.
    raw: Box<[u8]>,
    /// For each bank, the byte range of its compressed stream within
    /// `raw`.
    compressed_streams: Vec<std::ops::Range<usize>>,
    /// For each bank, the expected decompressed size.
    decompressed_sizes: Vec<u32>,
}

#[derive(Debug, Clone, Copy)]
struct BankDescriptor {
    group: u16,
    item: u8,
    data_type: u8,
}

impl ByBankRecord {
    /// Parse a freshly read on-disk record (header at offset 0, then
    /// the compressed payload section) into the in-memory directory.
    /// **Does not decompress any bank stream.**
    pub fn parse(src: &[u8]) -> Result<Arc<Self>> {
        let header = RecordHeader::parse(src)?;
        if !matches!(header.compression, CompressionType::Lz4ByBank) {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "ByBankRecord::parse called on non-Lz4ByBank record",
            });
        }
        if src.len() < header.total_bytes() as usize {
            return Err(HipoError::FileTooSmall {
                actual: src.len() as u64,
                min: header.total_bytes(),
            });
        }
        let header_len = header.header_length as usize;
        let payload_disk_len = header.payload_bytes() as usize;
        let pad = header.compressed_data_padding as usize;
        let payload_disk = &src[header_len..header_len + payload_disk_len];
        let section = &payload_disk[..payload_disk_len.saturating_sub(pad)];

        Self::parse_section(header, section)
    }

    fn parse_section(header: RecordHeader, section: &[u8]) -> Result<Arc<Self>> {
        if section.len() < 8 {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4ByBank: section truncated (header)",
            });
        }
        let num_banks = read_u32_le(section, 0) as usize;
        let event_count = read_u32_le(section, 4);

        // Sanity: event_count in the section must match the record header.
        if event_count != header.event_count {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4ByBank: event_count mismatch with record header",
            });
        }

        // Overflow-safe directory size. `num_banks` and `event_count` are
        // attacker-controlled u32 fields; `4 * num_banks * event_count` (the
        // size matrix) is a product of two u32s that can wrap a 64-bit
        // `usize`. Compute the total in u64 with explicit overflow checks
        // FIRST, so the wrapping multiply can't bypass the length gate and
        // drive the per-element read loop out of bounds.
        let nb = num_banks as u64;
        let ec = event_count as u64;
        let bytes_per_row_u64 = (num_banks.div_ceil(8)) as u64;
        let total_dir = (|| -> Option<u64> {
            let desc = 4u64.checked_mul(nb)?;
            let comp = 4u64.checked_mul(nb)?;
            let decomp = 4u64.checked_mul(nb)?;
            let tags = 4u64.checked_mul(ec)?;
            let presence = ec.checked_mul(bytes_per_row_u64)?;
            let sizes = 4u64.checked_mul(nb)?.checked_mul(ec)?;
            8u64.checked_add(desc)?
                .checked_add(comp)?
                .checked_add(decomp)?
                .checked_add(tags)?
                .checked_add(presence)?
                .checked_add(sizes)
        })()
        .ok_or(HipoError::CorruptRecord {
            offset: 0,
            reason: "Lz4ByBank: directory size overflow",
        })?;
        if (section.len() as u64) < total_dir {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4ByBank: directory truncated",
            });
        }

        // The total fits and is <= section.len() <= isize::MAX, so every
        // sub-offset below is a valid usize that cannot overflow.
        let desc_off = 8;
        let desc_bytes = 4 * num_banks;
        let comp_sizes_off = desc_off + desc_bytes;
        let comp_sizes_bytes = 4 * num_banks;
        let decomp_sizes_off = comp_sizes_off + comp_sizes_bytes;
        let decomp_sizes_bytes = 4 * num_banks;
        let tags_off = decomp_sizes_off + decomp_sizes_bytes;
        let tags_bytes = 4 * event_count as usize;
        let presence_off = tags_off + tags_bytes;
        let bytes_per_row = num_banks.div_ceil(8);
        let presence_bytes = event_count as usize * bytes_per_row;
        let event_bank_sizes_off = presence_off + presence_bytes;
        let event_bank_sizes_bytes = 4 * num_banks * event_count as usize;
        let streams_off = event_bank_sizes_off + event_bank_sizes_bytes;

        if section.len() < streams_off {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4ByBank: directory truncated",
            });
        }

        // Parse descriptors.
        let mut descriptors = Vec::with_capacity(num_banks);
        for b in 0..num_banks {
            let off = desc_off + b * 4;
            let group = u16::from_le_bytes([section[off], section[off + 1]]);
            let item = section[off + 2];
            let data_type = section[off + 3];
            descriptors.push(BankDescriptor {
                group,
                item,
                data_type,
            });
        }

        // Parse compressed/decompressed sizes.
        let mut compressed_sizes: Vec<u32> = Vec::with_capacity(num_banks);
        let mut decompressed_sizes: Vec<u32> = Vec::with_capacity(num_banks);
        for b in 0..num_banks {
            compressed_sizes.push(read_u32_le(section, comp_sizes_off + b * 4));
            decompressed_sizes.push(read_u32_le(section, decomp_sizes_off + b * 4));
        }

        // Parse per-event tags.
        let mut event_tags: Vec<u32> = Vec::with_capacity(event_count as usize);
        for e in 0..event_count as usize {
            event_tags.push(read_u32_le(section, tags_off + e * 4));
        }

        // Copy presence as-is (already packed).
        let presence = section[presence_off..presence_off + presence_bytes].to_vec();

        // Parse the per-event size matrix into per-bank CUMULATIVE offset
        // tables (event_count+1 entries each). Computing the prefix sum here,
        // once, makes `bank_byte_range`/`bank_size` O(1). We also validate
        // that each bank's sizes sum to its decompressed stream length, so a
        // hostile directory can never make `bank_byte_range` index past the
        // inflated stream (which would otherwise panic on `&stream[range]`).
        let mut bank_offsets: Vec<Vec<u32>> = Vec::with_capacity(num_banks);
        for (b, &expected) in decompressed_sizes.iter().enumerate() {
            let mut cum: Vec<u32> = Vec::with_capacity(event_count as usize + 1);
            let row_off = event_bank_sizes_off + b * 4 * event_count as usize;
            let mut acc: u32 = 0;
            cum.push(0);
            for e in 0..event_count as usize {
                let sz = read_u32_le(section, row_off + e * 4);
                acc = acc.saturating_add(sz);
                cum.push(acc);
            }
            if acc != expected {
                return Err(HipoError::CorruptRecord {
                    offset: 0,
                    reason: "Lz4ByBank: per-event sizes do not sum to decompressed bank size",
                });
            }
            bank_offsets.push(cum);
        }

        // Compute compressed-stream byte ranges within the (eventually
        // owned) `raw` buffer. We copy the whole section into an owned
        // buffer so callers can drop the source mmap range and we still
        // have what we need for lazy decompression.
        let mut compressed_streams = Vec::with_capacity(num_banks);
        let mut off = streams_off;
        for &cs in &compressed_sizes {
            let cs = cs as usize;
            compressed_streams.push(off..off + cs);
            off += cs;
        }
        if off > section.len() {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4ByBank: stream bytes run past section end",
            });
        }

        let raw: Box<[u8]> = section.to_vec().into_boxed_slice();
        let bank_data = (0..num_banks).map(|_| OnceLock::new()).collect();

        Ok(Arc::new(Self {
            header,
            descriptors,
            event_count,
            event_tags,
            presence,
            bytes_per_row,
            bank_offsets,
            bank_data,
            raw,
            compressed_streams,
            decompressed_sizes,
        }))
    }

    pub fn header(&self) -> &RecordHeader {
        &self.header
    }

    pub fn event_count(&self) -> u32 {
        self.event_count
    }

    pub fn num_banks(&self) -> usize {
        self.descriptors.len()
    }

    pub fn event_tag(&self, event_idx: u32) -> u32 {
        self.event_tags[event_idx as usize]
    }

    /// Look up a bank by (group, item). `None` if no bank with that ID
    /// appears anywhere in this record.
    pub fn bank_index(&self, group: u16, item: u8) -> Option<u32> {
        // Linear scan — typical B ~30, faster than hashing.
        self.descriptors
            .iter()
            .position(|d| d.group == group && d.item == item)
            .map(|i| i as u32)
    }

    /// True if event `event_idx` carries bank index `bank_idx`.
    pub fn has(&self, event_idx: u32, bank_idx: u32) -> bool {
        let e = event_idx as usize;
        let b = bank_idx as usize;
        let byte = e * self.bytes_per_row + b / 8;
        let bit = (b % 8) as u8;
        (self.presence[byte] >> bit) & 1 == 1
    }

    /// Bank descriptor: (group, item, data_type).
    pub fn descriptor(&self, bank_idx: u32) -> (u16, u8, u8) {
        let d = &self.descriptors[bank_idx as usize];
        (d.group, d.item, d.data_type)
    }

    /// Byte size of event `event_idx`'s instance of bank `bank_idx`.
    /// Zero if the event lacks the bank, *or* if it has an empty bank.
    /// Combine with [`Self::has`] to disambiguate. O(1) — a difference of
    /// two cumulative offsets.
    pub fn bank_size(&self, event_idx: u32, bank_idx: u32) -> u32 {
        let o = &self.bank_offsets[bank_idx as usize];
        let e = event_idx as usize;
        o[e + 1] - o[e]
    }

    /// Borrow the decompressed bank stream for `bank_idx`, inflating it
    /// the first time. Subsequent calls are lock-free. Thread-safe.
    pub fn bank_stream(&self, bank_idx: u32) -> Result<&[u8]> {
        let b = bank_idx as usize;
        // Fast path: already inflated.
        if let Some(data) = self.bank_data[b].get() {
            return Ok(data.as_ref());
        }
        // Slow path: inflate. `get_or_init` is allowed to be called by
        // multiple threads concurrently — exactly one initializer runs;
        // others block on it.
        let expected = self.decompressed_sizes[b] as usize;
        let range = &self.compressed_streams[b];
        let src = &self.raw[range.clone()];

        // `OnceLock::get_or_init` doesn't allow fallible closures, so we
        // do the inflate outside and stash. If two threads race, the
        // second one's work is discarded — acceptable for the rare
        // first-touch case.
        if expected == 0 {
            // Empty stream → empty payload, no decompression needed.
            let _ = self.bank_data[b].set(Box::new([]));
        } else {
            #[cfg(test)]
            BANK_INFLATE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut out: Vec<u8> = Vec::with_capacity(expected);
            decompress(CompressionType::Lz4, src, &mut out, expected)?;
            // Trim to expected size (decompress may write the slack).
            out.truncate(expected);
            let _ = self.bank_data[b].set(out.into_boxed_slice());
        }
        // Safe: we just set it.
        Ok(self.bank_data[b]
            .get()
            .expect("OnceLock::set succeeded above")
            .as_ref())
    }

    /// Byte range within `bank_stream(bank_idx)` of event `event_idx`'s
    /// bank data. Caller is responsible for checking `has(event, bank)`
    /// first. O(1): a slice of the precomputed cumulative-offset table
    /// (`bank_offsets`), validated at parse to stay within the inflated
    /// stream, so the returned range can never index past `bank_stream`.
    pub fn bank_byte_range(&self, event_idx: u32, bank_idx: u32) -> std::ops::Range<usize> {
        let o = &self.bank_offsets[bank_idx as usize];
        let e = event_idx as usize;
        o[e] as usize..o[e + 1] as usize
    }

    /// True if bank `bank_idx`'s stream has already been decompressed
    /// in this record. Used by tests to verify the partial-decompression
    /// contract — banks the analysis never touches must remain
    /// compressed throughout the record's lifetime.
    #[cfg(test)]
    pub fn is_bank_inflated(&self, bank_idx: u32) -> bool {
        self.bank_data[bank_idx as usize].get().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, EventBuilder};
    use crate::wire::bytes::write_u32_le;
    use crate::wire::constants::BANK_STRUCTURE_SIZE;

    /// Build one event with two banks: (300,1) "particle" and (300,30)
    /// "event". The data bytes are arbitrary but distinguishable.
    fn build_event(evno: u32) -> Vec<u8> {
        let mut eb = EventBuilder::new();
        // Particle bank — variable-sized payload.
        let mut p = vec![0u8; BANK_STRUCTURE_SIZE];
        p[0..2].copy_from_slice(&300u16.to_le_bytes());
        p[2] = 1;
        p[3] = 11;
        let particle_payload = vec![(evno & 0xff) as u8; 64];
        write_u32_le(&mut p, 4, particle_payload.len() as u32);
        p.extend_from_slice(&particle_payload);
        eb.add_bank_bytes(&p);

        // Event bank.
        let mut e = vec![0u8; BANK_STRUCTURE_SIZE];
        e[0..2].copy_from_slice(&300u16.to_le_bytes());
        e[2] = 30;
        e[3] = 11;
        let event_payload = (evno as u64).to_le_bytes().to_vec();
        write_u32_le(&mut e, 4, event_payload.len() as u32);
        e.extend_from_slice(&event_payload);
        eb.add_bank_bytes(&e);
        eb.finish()
    }

    #[test]
    fn parses_directory_without_decompressing() {
        let evs: Vec<Vec<u8>> = (0..10).map(build_event).collect();
        let refs: Vec<&[u8]> = evs.iter().map(|v| v.as_slice()).collect();
        let mut payload_buf = Vec::new();
        let mut compress_buf = Vec::new();
        let raw = crate::write::build_record_bytes(
            &refs,
            0,
            0,
            crate::write::Compression::Lz4ByBank,
            1,
            &mut payload_buf,
            &mut compress_buf,
        )
        .unwrap();

        let before = BANK_INFLATE_COUNTER.load(std::sync::atomic::Ordering::Relaxed);
        let rec = ByBankRecord::parse(&raw).unwrap();
        let after_parse = BANK_INFLATE_COUNTER.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            before, after_parse,
            "parsing the directory must not decompress any bank"
        );

        assert_eq!(rec.event_count(), 10);
        assert_eq!(rec.num_banks(), 2);
        for e in 0..10 {
            assert!(rec.has(e, 0));
            assert!(rec.has(e, 1));
        }
    }

    #[test]
    fn touching_one_bank_does_not_inflate_others() {
        let evs: Vec<Vec<u8>> = (0..10).map(build_event).collect();
        let refs: Vec<&[u8]> = evs.iter().map(|v| v.as_slice()).collect();
        let mut payload_buf = Vec::new();
        let mut compress_buf = Vec::new();
        let raw = crate::write::build_record_bytes(
            &refs,
            0,
            0,
            crate::write::Compression::Lz4ByBank,
            1,
            &mut payload_buf,
            &mut compress_buf,
        )
        .unwrap();
        let rec = ByBankRecord::parse(&raw).unwrap();

        // Identify which bank index corresponds to (300, 30) "event"
        // and which is (300, 1) "particle".
        let event_b = rec.bank_index(300, 30).unwrap();
        let particle_b = rec.bank_index(300, 1).unwrap();

        // Touch only the "event" bank stream, for every event.
        for e in 0..rec.event_count() {
            let _ = rec.bank_stream(event_b).unwrap();
            let range = rec.bank_byte_range(e, event_b);
            assert_eq!(range.len(), 8); // u64 evno = 8 bytes
        }
        assert!(rec.is_bank_inflated(event_b));
        assert!(
            !rec.is_bank_inflated(particle_b),
            "particle bank must stay compressed when never requested"
        );

        // Now touch "particle" once — it should inflate exactly once
        // even across multiple events.
        let count_before = BANK_INFLATE_COUNTER.load(std::sync::atomic::Ordering::Relaxed);
        for _ in 0..rec.event_count() {
            let _ = rec.bank_stream(particle_b).unwrap();
        }
        let count_after = BANK_INFLATE_COUNTER.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            count_after - count_before,
            1,
            "OnceLock must inflate exactly once per bank, regardless of access count"
        );
        assert!(rec.is_bank_inflated(particle_b));
    }

    #[test]
    fn round_trip_preserves_event_bytes() {
        let evs: Vec<Vec<u8>> = (0..5).map(build_event).collect();
        let refs: Vec<&[u8]> = evs.iter().map(|v| v.as_slice()).collect();
        let mut payload_buf = Vec::new();
        let mut compress_buf = Vec::new();
        let raw = crate::write::build_record_bytes(
            &refs,
            0,
            0,
            crate::write::Compression::Lz4ByBank,
            1,
            &mut payload_buf,
            &mut compress_buf,
        )
        .unwrap();
        let rec = ByBankRecord::parse(&raw).unwrap();

        let particle_b = rec.bank_index(300, 1).unwrap();
        let event_b = rec.bank_index(300, 30).unwrap();
        let p_stream = rec.bank_stream(particle_b).unwrap();
        let e_stream = rec.bank_stream(event_b).unwrap();

        // For each event, reconstruct & check the per-bank bytes match
        // the original. The original event was [particle_struct,
        // event_struct]; in the on-disk by-bank file the streams hold
        // just the data bytes (no struct header), in event order.
        for evno in 0..5_u32 {
            let original = Event::new(&evs[evno as usize]);
            let (_, orig_particle) = original.find(300, 1).unwrap();
            let (_, orig_event) = original.find(300, 30).unwrap();

            let p_range = rec.bank_byte_range(evno, particle_b);
            let e_range = rec.bank_byte_range(evno, event_b);
            assert_eq!(&p_stream[p_range], orig_particle);
            assert_eq!(&e_stream[e_range], orig_event);
        }
    }
}
