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

use memmap2::Mmap;

use crate::compress::decompress;
use crate::error::{HipoError, Result};
use crate::wire::bytes::read_u32_le;
use crate::wire::constants::CompressionType;
use crate::wire::record_header::RecordHeader;

/// Where a [`ByBankRecord`]'s compressed payload section lives.
///
/// The reader path borrows it straight from the memory-mapped file
/// (`Mmap`) — the `Arc<Mmap>` keeps the mapping alive for the record's
/// lifetime, so there is **no per-record copy** of the (potentially
/// multi-MB) compressed section. Test / in-memory construction owns a
/// copy instead.
#[derive(Debug)]
enum Backing {
    /// Borrowed from the mmap at `[section_off, section_off + len)`.
    Mmap { mmap: Arc<Mmap>, section_off: usize },
    /// Owned copy of the section (tests, in-memory `parse`).
    Owned(Box<[u8]>),
}

impl Backing {
    #[inline]
    fn section(&self, section_len: usize) -> &[u8] {
        match self {
            Backing::Mmap { mmap, section_off } => &mmap[*section_off..*section_off + section_len],
            Backing::Owned(b) => b,
        }
    }
}

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
    /// The compressed payload section — borrowed from the mmap on the
    /// reader path (no copy), owned for in-memory construction.
    backing: Backing,
    /// Length in bytes of the compressed section (`backing` resolves to a
    /// slice of exactly this length).
    section_len: usize,
    /// For each bank, the byte range of its compressed stream **within the
    /// section** (i.e. relative to `backing.section(..)`).
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

/// Overflow-checked byte length of the by-bank directory *body*
/// (descriptors through the per-event size matrix, excluding any outer
/// header). `num_banks` and `event_count` are attacker-controlled, so the
/// `4 * num_banks * event_count` size matrix is computed with checked
/// arithmetic to avoid a `usize` wrap that could bypass a length gate.
fn directory_body_len(num_banks: usize, event_count: u32) -> Result<usize> {
    let nb = num_banks as u64;
    let ec = event_count as u64;
    let bpr = (num_banks.div_ceil(8)) as u64;
    let total = (|| -> Option<u64> {
        let desc = 4u64.checked_mul(nb)?;
        let comp = 4u64.checked_mul(nb)?;
        let decomp = 4u64.checked_mul(nb)?;
        let tags = 4u64.checked_mul(ec)?;
        let presence = ec.checked_mul(bpr)?;
        let sizes = 4u64.checked_mul(nb)?.checked_mul(ec)?;
        desc.checked_add(comp)?
            .checked_add(decomp)?
            .checked_add(tags)?
            .checked_add(presence)?
            .checked_add(sizes)
    })()
    .ok_or(HipoError::CorruptRecord {
        offset: 0,
        reason: "Lz4ByBank: directory size overflow",
    })?;
    usize::try_from(total).map_err(|_| HipoError::CorruptRecord {
        offset: 0,
        reason: "Lz4ByBank: directory size overflow",
    })
}

impl ByBankRecord {
    /// Parse a record from an in-memory byte buffer, **owning** a copy of
    /// its compressed section. Used by tests and any caller that doesn't
    /// have the bytes in an mmap. **Does not decompress any bank stream.**
    pub fn parse(src: &[u8]) -> Result<Arc<Self>> {
        let header = Self::check_header(src)?;
        let (section_off, section_len) = Self::section_bounds(&header, src.len(), 0)?;
        let section = &src[section_off..section_off + section_len];
        Self::parse_section(
            header,
            section,
            Backing::Owned(section.to_vec().into_boxed_slice()),
        )
    }

    /// Parse a record that lives in a memory-mapped file at
    /// `[lo, hi)`, **borrowing** its compressed section from the mmap —
    /// no per-record copy. The `Arc<Mmap>` is cloned into the record so
    /// the mapping outlives it. **Does not decompress any bank stream.**
    pub fn parse_mmap(mmap: Arc<Mmap>, lo: usize, hi: usize) -> Result<Arc<Self>> {
        let src = &mmap[lo..hi];
        let header = Self::check_header(src)?;
        // Section bounds are relative to `src` (offset `lo` in the mmap).
        let (rel_off, section_len) = Self::section_bounds(&header, src.len(), lo)?;
        let section_off = rel_off; // already absolute (section_bounds adds `lo`)
        let section = &mmap[section_off..section_off + section_len];
        Self::parse_section(
            header,
            section,
            Backing::Mmap {
                mmap: Arc::clone(&mmap),
                section_off,
            },
        )
    }

    /// Validate that `src` begins with an `Lz4ByBank` record header that
    /// fits within `src`.
    fn check_header(src: &[u8]) -> Result<RecordHeader> {
        let header = RecordHeader::parse(src)?;
        if !header.compression.is_by_bank() {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "ByBankRecord::parse called on non-by-bank record",
            });
        }
        if src.len() < header.total_bytes() as usize {
            return Err(HipoError::FileTooSmall {
                actual: src.len() as u64,
                min: header.total_bytes(),
            });
        }
        Ok(header)
    }

    /// Compute the compressed section's `(offset, len)`. `base` is added
    /// to the offset so callers reading from an mmap get an absolute
    /// offset; pass `0` for a `src`-relative offset.
    fn section_bounds(
        header: &RecordHeader,
        src_len: usize,
        base: usize,
    ) -> Result<(usize, usize)> {
        let header_len = header.header_length as usize;
        let payload_disk_len = header.payload_bytes() as usize;
        let pad = header.compressed_data_padding as usize;
        if header_len + payload_disk_len > src_len {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4ByBank: payload extends past record",
            });
        }
        let section_len = payload_disk_len.saturating_sub(pad);
        Ok((base + header_len, section_len))
    }

    fn parse_section(header: RecordHeader, section: &[u8], backing: Backing) -> Result<Arc<Self>> {
        match header.compression {
            CompressionType::Lz4ByBankV2 => Self::parse_section_v2(header, section, backing),
            // v1 (`Lz4ByBank`) — `check_header` guarantees a by-bank tag.
            _ => Self::parse_section_v1(header, section, backing),
        }
    }

    /// v1 layout: `[u32 B, u32 E, <directory body>, bank streams]` — the
    /// directory is uncompressed and contiguous with the streams.
    fn parse_section_v1(
        header: RecordHeader,
        section: &[u8],
        backing: Backing,
    ) -> Result<Arc<Self>> {
        if section.len() < 8 {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4ByBank: section truncated (header)",
            });
        }
        let num_banks = read_u32_le(section, 0) as usize;
        let event_count = read_u32_le(section, 4);
        if event_count != header.event_count {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4ByBank: event_count mismatch with record header",
            });
        }
        let dir_body_len = directory_body_len(num_banks, event_count)?;
        if section.len() - 8 < dir_body_len {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4ByBank: directory truncated",
            });
        }
        let streams_off = 8 + dir_body_len;
        let dir = &section[8..];
        Self::from_directory(
            header,
            section,
            backing,
            num_banks,
            event_count,
            dir,
            streams_off,
        )
    }

    /// v2 layout: `[u8 ver=2, u8×3 reserved, u32 B, u32 E, u32 dir_comp,
    /// u32 dir_decomp, LZ4(directory), bank streams]`.
    fn parse_section_v2(
        header: RecordHeader,
        section: &[u8],
        backing: Backing,
    ) -> Result<Arc<Self>> {
        if section.len() < 20 {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4ByBankV2: section truncated (header)",
            });
        }
        if section[0] != 2 {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4ByBankV2: unsupported extension-format version",
            });
        }
        let num_banks = read_u32_le(section, 4) as usize;
        let event_count = read_u32_le(section, 8);
        if event_count != header.event_count {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4ByBankV2: event_count mismatch with record header",
            });
        }
        let dir_comp_len = read_u32_le(section, 12) as usize;
        let dir_decomp_len = read_u32_le(section, 16) as usize;
        let streams_off = 20usize
            .checked_add(dir_comp_len)
            .ok_or(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4ByBankV2: directory size overflow",
            })?;
        if streams_off > section.len() {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4ByBankV2: directory truncated",
            });
        }
        // The declared decompressed directory length must match the layout
        // implied by the bank/event counts — reject before inflating into a
        // buffer of that size.
        if dir_decomp_len != directory_body_len(num_banks, event_count)? {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4ByBankV2: directory size inconsistent with bank/event counts",
            });
        }
        // Inflate the (small) directory into an owned buffer.
        let dir: Vec<u8> = if dir_decomp_len == 0 {
            Vec::new()
        } else {
            let mut buf = Vec::with_capacity(dir_decomp_len);
            decompress(
                CompressionType::Lz4,
                &section[20..streams_off],
                &mut buf,
                dir_decomp_len,
            )?;
            buf.truncate(dir_decomp_len);
            buf
        };
        Self::from_directory(
            header,
            section,
            backing,
            num_banks,
            event_count,
            &dir,
            streams_off,
        )
    }

    /// Parse the directory *body* (descriptors at offset 0 of `dir`) and
    /// assemble the record. `streams_off` is the byte offset of the first
    /// bank stream within `section`. Shared by v1 (where `dir` borrows the
    /// section) and v2 (where `dir` is the inflated directory).
    fn from_directory(
        header: RecordHeader,
        section: &[u8],
        backing: Backing,
        num_banks: usize,
        event_count: u32,
        dir: &[u8],
        streams_off: usize,
    ) -> Result<Arc<Self>> {
        let dir_body_len = directory_body_len(num_banks, event_count)?;
        if dir.len() < dir_body_len {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4ByBank: directory body truncated",
            });
        }
        let bytes_per_row = num_banks.div_ceil(8);
        let comp_sizes_off = 4 * num_banks;
        let decomp_sizes_off = comp_sizes_off + 4 * num_banks;
        let tags_off = decomp_sizes_off + 4 * num_banks;
        let presence_off = tags_off + 4 * event_count as usize;
        let presence_bytes = event_count as usize * bytes_per_row;
        let event_bank_sizes_off = presence_off + presence_bytes;

        // descriptors
        let mut descriptors = Vec::with_capacity(num_banks);
        for b in 0..num_banks {
            let off = b * 4;
            descriptors.push(BankDescriptor {
                group: u16::from_le_bytes([dir[off], dir[off + 1]]),
                item: dir[off + 2],
                data_type: dir[off + 3],
            });
        }
        // compressed / decompressed sizes
        let mut compressed_sizes: Vec<u32> = Vec::with_capacity(num_banks);
        let mut decompressed_sizes: Vec<u32> = Vec::with_capacity(num_banks);
        for b in 0..num_banks {
            compressed_sizes.push(read_u32_le(dir, comp_sizes_off + b * 4));
            decompressed_sizes.push(read_u32_le(dir, decomp_sizes_off + b * 4));
        }
        // per-event tags
        let mut event_tags: Vec<u32> = Vec::with_capacity(event_count as usize);
        for e in 0..event_count as usize {
            event_tags.push(read_u32_le(dir, tags_off + e * 4));
        }
        // presence (already packed)
        let presence = dir[presence_off..presence_off + presence_bytes].to_vec();
        // per-bank CUMULATIVE offset tables (event_count+1 entries) — O(1)
        // random access; validate Σ sizes == decompressed stream length so a
        // hostile directory can't index past the inflated stream.
        let mut bank_offsets: Vec<Vec<u32>> = Vec::with_capacity(num_banks);
        for (b, &expected) in decompressed_sizes.iter().enumerate() {
            let mut cum: Vec<u32> = Vec::with_capacity(event_count as usize + 1);
            let row_off = event_bank_sizes_off + b * 4 * event_count as usize;
            let mut acc: u32 = 0;
            cum.push(0);
            for e in 0..event_count as usize {
                let sz = read_u32_le(dir, row_off + e * 4);
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
        // compressed-stream ranges within `section` (absolute section offsets)
        let mut compressed_streams = Vec::with_capacity(num_banks);
        let mut off = streams_off;
        for &cs in &compressed_sizes {
            let cs = cs as usize;
            let end = off.checked_add(cs).ok_or(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4ByBank: stream offset overflow",
            })?;
            compressed_streams.push(off..end);
            off = end;
        }
        if off > section.len() {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4ByBank: stream bytes run past section end",
            });
        }

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
            backing,
            section_len: section.len(),
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
        let section = self.backing.section(self.section_len);
        let src = &section[self.compressed_streams[b].clone()];

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

    fn build_raw(events: &[&[u8]], compression: crate::write::Compression) -> Vec<u8> {
        let mut payload_buf = Vec::new();
        let mut compress_buf = Vec::new();
        crate::write::build_record_bytes(
            events,
            0,
            0,
            compression,
            1,
            &mut payload_buf,
            &mut compress_buf,
        )
        .unwrap()
    }

    #[test]
    fn v2_round_trip_preserves_event_bytes() {
        let evs: Vec<Vec<u8>> = (0..5).map(build_event).collect();
        let refs: Vec<&[u8]> = evs.iter().map(|v| v.as_slice()).collect();
        let raw = build_raw(&refs, crate::write::Compression::Lz4ByBankV2);

        // The header must carry the v2 tag.
        let header = RecordHeader::parse(&raw).unwrap();
        assert_eq!(header.compression, CompressionType::Lz4ByBankV2);

        let rec = ByBankRecord::parse(&raw).unwrap();
        let particle_b = rec.bank_index(300, 1).unwrap();
        let event_b = rec.bank_index(300, 30).unwrap();
        let p_stream = rec.bank_stream(particle_b).unwrap();
        let e_stream = rec.bank_stream(event_b).unwrap();
        for evno in 0..5_u32 {
            let original = Event::new(&evs[evno as usize]);
            let (_, orig_particle) = original.find(300, 1).unwrap();
            let (_, orig_event) = original.find(300, 30).unwrap();
            assert_eq!(
                &p_stream[rec.bank_byte_range(evno, particle_b)],
                orig_particle
            );
            assert_eq!(&e_stream[rec.bank_byte_range(evno, event_b)], orig_event);
        }
    }

    #[test]
    fn v1_and_v2_decode_identically() {
        let evs: Vec<Vec<u8>> = (0..8).map(build_event).collect();
        let refs: Vec<&[u8]> = evs.iter().map(|v| v.as_slice()).collect();
        let v1 =
            ByBankRecord::parse(&build_raw(&refs, crate::write::Compression::Lz4ByBank)).unwrap();
        let v2 =
            ByBankRecord::parse(&build_raw(&refs, crate::write::Compression::Lz4ByBankV2)).unwrap();

        assert_eq!(v1.event_count(), v2.event_count());
        assert_eq!(v1.num_banks(), v2.num_banks());
        for b in 0..v1.num_banks() as u32 {
            assert_eq!(v1.descriptor(b), v2.descriptor(b));
            let s1 = v1.bank_stream(b).unwrap();
            let s2 = v2.bank_stream(b).unwrap();
            assert_eq!(s1, s2, "bank {b} streams differ between v1 and v2");
            for e in 0..v1.event_count() {
                assert_eq!(v1.has(e, b), v2.has(e, b));
                assert_eq!(v1.bank_byte_range(e, b), v2.bank_byte_range(e, b));
            }
        }
    }

    #[test]
    fn v2_directory_is_smaller_than_v1() {
        // Many events × repetitive sizes → the v2 compressed directory
        // should be strictly smaller than v1's raw directory.
        let evs: Vec<Vec<u8>> = (0..200).map(build_event).collect();
        let refs: Vec<&[u8]> = evs.iter().map(|v| v.as_slice()).collect();
        let v1 = build_raw(&refs, crate::write::Compression::Lz4ByBank);
        let v2 = build_raw(&refs, crate::write::Compression::Lz4ByBankV2);
        assert!(
            v2.len() < v1.len(),
            "v2 ({}) should be smaller than v1 ({}) thanks to the compressed directory",
            v2.len(),
            v1.len()
        );
    }

    #[test]
    fn v2_touching_one_bank_does_not_inflate_others() {
        let evs: Vec<Vec<u8>> = (0..10).map(build_event).collect();
        let refs: Vec<&[u8]> = evs.iter().map(|v| v.as_slice()).collect();
        let rec =
            ByBankRecord::parse(&build_raw(&refs, crate::write::Compression::Lz4ByBankV2)).unwrap();

        let event_b = rec.bank_index(300, 30).unwrap();
        let particle_b = rec.bank_index(300, 1).unwrap();
        let _ = rec.bank_stream(event_b).unwrap();
        assert!(rec.is_bank_inflated(event_b));
        assert!(
            !rec.is_bank_inflated(particle_b),
            "v2: untouched bank must stay compressed"
        );
    }
}
