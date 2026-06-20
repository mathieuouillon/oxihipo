//! Record-building primitives + the public `Compression` enum.
//!
//! Most users never touch [`RecordBuilder`] or [`build_record_bytes`]
//! directly — the [`Writer`](super::Writer) handles record building
//! transparently. They're exposed for advanced callers that assemble
//! compressed records themselves.

use crate::compress::{ScratchBuf, compress};
use crate::error::{HipoError, Result};
use crate::event::Event;
use crate::wire::bytes::{Endianness, write_u32_le};
use crate::wire::constants::*;
use crate::wire::record_header::RecordHeader;

/// Default events-per-chunk when [`Compression::lz4_chunked`] is used
/// without an explicit setting. 32 is the documented sweet spot:
/// large enough that LZ4's back-reference window still helps within a
/// chunk, small enough that chunk-level parallelism saturates a typical
/// 8–16 thread machine.
pub const DEFAULT_EVENTS_PER_CHUNK: u32 = 32;

/// Compression mode for HIPO records.
///
/// This enum is **writer-facing**; it may carry parameters
/// (`events_per_chunk`) that aren't part of the on-disk record header.
/// The wire-level compression tag is derived from this enum by the
/// record builder.
///
/// - `None` / `Lz4` / `Lz4Best` / `Gzip` are interchangeable with the
///   wire tag of the same name, and produce files readable by the C++
///   `hipo4` reader.
/// - `Lz4Chunked` is a Rust-only format extension (record payload split
///   into multiple independently-LZ4-compressed chunks) that enables
///   intra-record parallel decompression.
/// - `Lz4ByBank` is a Rust-only format extension (record payload split
///   into one LZ4 stream per bank type) that enables true partial
///   decompression — `ev.bank("name")` inflates only the requested
///   bank's stream, leaving every other bank compressed.
///
/// **Files written with `Lz4Chunked` or `Lz4ByBank` are not readable
/// by the C++ `hipo4` reader.**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Lz4,
    Lz4Best,
    Gzip,
    /// Split each record's events into groups of `events_per_chunk` and
    /// compress every group as an independent LZ4 block. Reader is then
    /// free to decompress chunks in parallel.
    Lz4Chunked {
        events_per_chunk: u32,
    },
    /// Store each bank type as its own LZ4 stream within the record,
    /// plus an event×bank presence table. Readers decompress only the
    /// bank streams they actually touch.
    Lz4ByBank,
    /// Version 2 of [`Self::Lz4ByBank`]: identical bank streams, but the
    /// directory is prefixed with an extension-format-version byte and is
    /// itself LZ4-compressed (the per-event size matrix is highly
    /// redundant), shrinking the on-disk directory on skim-like data.
    /// The reader handles both v1 and v2 transparently.
    Lz4ByBankV2,
}

impl Compression {
    /// Convenience constructor for [`Compression::Lz4Chunked`] using
    /// [`DEFAULT_EVENTS_PER_CHUNK`].
    pub const fn lz4_chunked() -> Self {
        Self::Lz4Chunked {
            events_per_chunk: DEFAULT_EVENTS_PER_CHUNK,
        }
    }

    /// Wire-level compression tag that gets written into the record
    /// header.
    ///
    /// This is an internal mapping used by the record builder; callers
    /// don't normally need it.
    pub(crate) const fn wire_tag(self) -> CompressionType {
        match self {
            Self::None => CompressionType::None,
            Self::Lz4 => CompressionType::Lz4,
            Self::Lz4Best => CompressionType::Lz4Best,
            Self::Gzip => CompressionType::Gzip,
            Self::Lz4Chunked { .. } => CompressionType::Lz4Chunked,
            Self::Lz4ByBank => CompressionType::Lz4ByBank,
            Self::Lz4ByBankV2 => CompressionType::Lz4ByBankV2,
        }
    }
}

/// Accumulates events into a single record. Flushed via
/// [`Writer::flush_record`](super::Writer::flush_record).
#[derive(Debug, Default)]
pub struct RecordBuilder {
    event_lengths: Vec<u32>,
    events: ScratchBuf,
    user_word_1: u64,
    user_word_2: u64,
}

impl RecordBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn event_count(&self) -> u32 {
        self.event_lengths.len() as u32
    }

    pub fn data_size(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.event_lengths.is_empty()
    }

    pub fn user_word_1(&self) -> u64 {
        self.user_word_1
    }

    pub fn user_word_2(&self) -> u64 {
        self.user_word_2
    }

    pub fn set_user_word_1(&mut self, v: u64) {
        self.user_word_1 = v;
    }

    pub fn set_user_word_2(&mut self, v: u64) {
        self.user_word_2 = v;
    }

    /// Append one event's bytes. Returns the new event count.
    pub fn add_event(&mut self, event_bytes: &[u8]) -> u32 {
        self.event_lengths.push(event_bytes.len() as u32);
        self.events.vec_mut().extend_from_slice(event_bytes);
        self.event_count()
    }

    pub fn reset(&mut self) {
        self.event_lengths.clear();
        self.events.reset_with_capacity(0);
    }

    pub fn estimated_payload_size(&self) -> usize {
        self.event_lengths.len() * 4 + self.events.len()
    }

    pub fn event_slices(&self) -> impl Iterator<Item = &[u8]> {
        let buf = self.events.as_slice();
        self.event_lengths.iter().scan(0usize, move |off, &len| {
            let start = *off;
            *off += len as usize;
            Some(&buf[start..*off])
        })
    }
}

/// Build a complete HIPO record (header + compressed payload) from event
/// byte slices.
///
/// The two scratch `Vec<u8>` arguments are reused across calls to avoid
/// per-record allocations: callers should keep them in long-lived storage
/// (a [`Writer`](super::Writer) instance) and pass them in.
///
/// `record_number` is written into the record header — the 1-based
/// position of the record in the output.
pub fn build_record_bytes(
    events: &[&[u8]],
    user_word_1: u64,
    user_word_2: u64,
    compression: Compression,
    record_number: u32,
    payload_buf: &mut Vec<u8>,
    compress_buf: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    match compression {
        Compression::Lz4Chunked { events_per_chunk } => build_chunked_record_bytes(
            events,
            user_word_1,
            user_word_2,
            events_per_chunk,
            record_number,
            payload_buf,
            compress_buf,
        ),
        Compression::Lz4ByBank => build_by_bank_record_bytes(
            events,
            user_word_1,
            user_word_2,
            record_number,
            payload_buf,
            compress_buf,
        ),
        Compression::Lz4ByBankV2 => build_by_bank_v2_record_bytes(
            events,
            user_word_1,
            user_word_2,
            record_number,
            payload_buf,
            compress_buf,
        ),
        Compression::None | Compression::Lz4 | Compression::Lz4Best | Compression::Gzip => {
            build_single_block_record_bytes(
                events,
                user_word_1,
                user_word_2,
                compression,
                record_number,
                payload_buf,
                compress_buf,
            )
        }
    }
}

/// Original single-LZ4-block (or None / Gzip) record path. Layout:
/// `index_array || data` → compress → record header.
fn build_single_block_record_bytes(
    events: &[&[u8]],
    user_word_1: u64,
    user_word_2: u64,
    compression: Compression,
    record_number: u32,
    payload_buf: &mut Vec<u8>,
    compress_buf: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    let event_count = events.len() as u32;
    let index_array_length = event_count * 4;

    // Build payload = index || data into the caller's scratch.
    payload_buf.clear();
    let total_data: usize = events.iter().map(|e| e.len()).sum();
    payload_buf.reserve(index_array_length as usize + total_data);
    payload_buf.resize(index_array_length as usize, 0);
    for (i, ev) in events.iter().enumerate() {
        write_u32_le(payload_buf, i * 4, ev.len() as u32);
    }
    for ev in events {
        payload_buf.extend_from_slice(ev);
    }
    let data_length = total_data as u32;

    // Compress into the caller's scratch.
    compress_buf.clear();
    let compressed_len = compress(compression.wire_tag(), payload_buf, compress_buf)?;
    let pad = (4 - (compressed_len % 4)) % 4;
    compress_buf.extend(std::iter::repeat_n(0u8, pad));
    let compressed_with_pad = compress_buf.len();

    let header_length = RECORD_HEADER_SIZE as u32;
    let total_bytes = header_length as u64 + compressed_with_pad as u64;
    let mut bit_info: u32 = HIPO_VERSION;
    bit_info |= ((pad as u32) & BITINFO_PAD_MASK) << BITINFO_PAD3_SHIFT;
    bit_info |= 4 << BITINFO_HEADER_TYPE_SHIFT;

    let header = RecordHeader {
        record_length: total_bytes,
        record_number,
        header_length,
        event_count,
        index_array_length,
        bit_info,
        user_header_length: 0,
        data_length,
        compressed_data_length: compressed_with_pad as u32,
        compression: compression.wire_tag(),
        user_word_1,
        user_word_2,
        endianness: Endianness::Little,
        user_header_padding: 0,
        data_padding: 0,
        compressed_data_padding: pad as u8,
    };

    let mut out = vec![0u8; total_bytes as usize];
    let hdr: &mut [u8; RECORD_HEADER_SIZE] = (&mut out[..RECORD_HEADER_SIZE])
        .try_into()
        .map_err(|_| HipoError::Compression("record header slice"))?;
    header.write(hdr);
    out[RECORD_HEADER_SIZE..].copy_from_slice(&compress_buf[..compressed_with_pad]);
    Ok(out)
}

/// Chunked-LZ4 record path.
///
/// Compressed payload layout (all `u32` little-endian):
///
/// ```text
/// +-- compressed payload section ----------------+
/// | u32 num_chunks (N)                            |
/// | u32 events_per_chunk (E)        last < E ok   |
/// | E_total × u32 event_sizes[]     E_total =     |
/// |                                  event_count  |
/// |                                  uncompressed |
/// | N × u32 compressed_chunk_sizes                |
/// | N × u32 decompressed_chunk_sizes              |
/// | concatenated LZ4 chunk payloads               |
/// +-----------------------------------------------+
/// ```
///
/// The `event_sizes[]` array sits *outside* any LZ4 stream, so a reader
/// can compute every event's byte range without inflating anything —
/// that's the property that enables future partial decompression.
fn build_chunked_record_bytes(
    events: &[&[u8]],
    user_word_1: u64,
    user_word_2: u64,
    events_per_chunk: u32,
    record_number: u32,
    payload_buf: &mut Vec<u8>,
    compress_buf: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    if events_per_chunk == 0 {
        return Err(HipoError::Compression(
            "Lz4Chunked: events_per_chunk must be >= 1",
        ));
    }

    let event_count = events.len() as u32;
    let e_per = events_per_chunk as usize;
    let num_chunks = events.len().div_ceil(e_per);

    // Per-event uncompressed sizes — the canonical index array, written
    // *outside* any LZ4 stream so a reader can compute event byte ranges
    // without inflating anything.
    let event_sizes: Vec<u32> = events.iter().map(|e| e.len() as u32).collect();

    // Compress each chunk independently; record the table entries.
    let mut compressed_chunks: Vec<Vec<u8>> = Vec::with_capacity(num_chunks);
    let mut decompressed_sizes: Vec<u32> = Vec::with_capacity(num_chunks);
    let mut compressed_sizes: Vec<u32> = Vec::with_capacity(num_chunks);

    for group in events.chunks(e_per) {
        // Concatenate the group's events into payload_buf (reused).
        payload_buf.clear();
        let group_bytes: usize = group.iter().map(|e| e.len()).sum();
        payload_buf.reserve(group_bytes);
        for ev in group {
            payload_buf.extend_from_slice(ev);
        }

        // Compress this chunk as a single LZ4 block.
        compress_buf.clear();
        let n = compress(CompressionType::Lz4, payload_buf, compress_buf)?;
        compressed_sizes.push(n as u32);
        decompressed_sizes.push(group_bytes as u32);
        compressed_chunks.push(compress_buf[..n].to_vec());
    }

    // Assemble the compressed payload section into payload_buf (reused
    // again — we no longer need the per-chunk uncompressed concat).
    let table_bytes = 8 // num_chunks + events_per_chunk
        + 4 * event_sizes.len()
        + 4 * compressed_sizes.len()
        + 4 * decompressed_sizes.len();
    let chunk_bytes: usize = compressed_chunks.iter().map(|c| c.len()).sum();
    let section_len = table_bytes + chunk_bytes;

    payload_buf.clear();
    payload_buf.reserve(section_len);
    payload_buf.extend_from_slice(&(num_chunks as u32).to_le_bytes());
    payload_buf.extend_from_slice(&events_per_chunk.to_le_bytes());
    for s in &event_sizes {
        payload_buf.extend_from_slice(&s.to_le_bytes());
    }
    for s in &compressed_sizes {
        payload_buf.extend_from_slice(&s.to_le_bytes());
    }
    for s in &decompressed_sizes {
        payload_buf.extend_from_slice(&s.to_le_bytes());
    }
    for chunk in &compressed_chunks {
        payload_buf.extend_from_slice(chunk);
    }
    debug_assert_eq!(payload_buf.len(), section_len);

    // 4-byte align the section length (compressed-data padding).
    let pad = (4 - (section_len % 4)) % 4;
    let compressed_with_pad = section_len + pad;
    payload_buf.extend(std::iter::repeat_n(0u8, pad));

    let event_count_real = event_count;
    let data_length: u32 = decompressed_sizes.iter().sum();
    let index_array_length = event_count_real * 4;

    let header_length = RECORD_HEADER_SIZE as u32;
    let total_bytes = header_length as u64 + compressed_with_pad as u64;
    let mut bit_info: u32 = HIPO_VERSION;
    bit_info |= ((pad as u32) & BITINFO_PAD_MASK) << BITINFO_PAD3_SHIFT;
    bit_info |= 4 << BITINFO_HEADER_TYPE_SHIFT;

    let header = RecordHeader {
        record_length: total_bytes,
        record_number,
        header_length,
        event_count: event_count_real,
        index_array_length,
        bit_info,
        user_header_length: 0,
        data_length,
        compressed_data_length: compressed_with_pad as u32,
        compression: CompressionType::Lz4Chunked,
        user_word_1,
        user_word_2,
        endianness: Endianness::Little,
        user_header_padding: 0,
        data_padding: 0,
        compressed_data_padding: pad as u8,
    };

    let mut out = vec![0u8; total_bytes as usize];
    let hdr: &mut [u8; RECORD_HEADER_SIZE] = (&mut out[..RECORD_HEADER_SIZE])
        .try_into()
        .map_err(|_| HipoError::Compression("record header slice"))?;
    header.write(hdr);
    out[RECORD_HEADER_SIZE..].copy_from_slice(&payload_buf[..compressed_with_pad]);
    Ok(out)
}

/// By-bank record path.
///
/// Compressed payload layout (all `u32` little-endian unless noted):
///
/// ```text
/// +-- compressed payload section -------------------+
/// | u32 num_banks (B)                               |
/// | u32 event_count (E)                             |   redundant w/ header; self-describing
/// | B × { u16 group, u8 item, u8 data_type }        |   bank descriptors (4 B each)
/// | B × u32 compressed_bank_sizes                   |
/// | B × u32 decompressed_bank_sizes                 |
/// | E × u32 event_tags                              |   EventHeader.tag for each event
/// | E × ceil(B/8) bytes presence_matrix             |   bit[e,b]=1 iff event e has bank b
/// | B × E × u32 event_bank_sizes                    |   per-event byte size of each bank
/// |                                                  |   (0 if event lacks the bank)
/// | concat B × LZ4 bank streams                     |   bank b stream = LZ4(concat of bank-b
/// |                                                  |   data bytes across all events that have
/// |                                                  |   it, in event order, no padding)
/// +--------------------------------------------------+
/// ```
///
/// On read, the directory is parsed eagerly but bank streams stay
/// compressed until `ev.bank(name)` requests one — that's the partial-
/// decompression hook.
fn build_by_bank_record_bytes(
    events: &[&[u8]],
    user_word_1: u64,
    user_word_2: u64,
    record_number: u32,
    payload_buf: &mut Vec<u8>,
    compress_buf: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    let ByBankParts {
        num_banks,
        event_count,
        e_count,
        descriptors,
        compressed_streams,
        compressed_sizes,
        decompressed_sizes,
        event_tags,
        presence,
        bytes_per_row,
        sizes,
    } = build_by_bank_parts(events, compress_buf)?;

    // ---- 5. Assemble the payload section. -----------------------------
    let directory_bytes = 8                       // num_banks + event_count
        + 4 * num_banks                            // descriptors
        + 4 * num_banks                            // compressed sizes
        + 4 * num_banks; // decompressed sizes
    let per_event_bytes = 4 * e_count              // tags
        + e_count * bytes_per_row; // presence
    let event_bank_sizes_bytes = 4 * num_banks * e_count;
    let stream_bytes: usize = compressed_streams.iter().map(|s| s.len()).sum();
    let section_len = directory_bytes + per_event_bytes + event_bank_sizes_bytes + stream_bytes;

    payload_buf.clear();
    payload_buf.reserve(section_len);
    payload_buf.extend_from_slice(&(num_banks as u32).to_le_bytes());
    payload_buf.extend_from_slice(&event_count.to_le_bytes());
    for &(g, i, t) in &descriptors {
        payload_buf.extend_from_slice(&g.to_le_bytes());
        payload_buf.push(i);
        payload_buf.push(t);
    }
    for s in &compressed_sizes {
        payload_buf.extend_from_slice(&s.to_le_bytes());
    }
    for s in &decompressed_sizes {
        payload_buf.extend_from_slice(&s.to_le_bytes());
    }
    for t in &event_tags {
        payload_buf.extend_from_slice(&t.to_le_bytes());
    }
    payload_buf.extend_from_slice(&presence);
    for row in &sizes {
        for &s in row {
            payload_buf.extend_from_slice(&s.to_le_bytes());
        }
    }
    for stream in &compressed_streams {
        payload_buf.extend_from_slice(stream);
    }
    debug_assert_eq!(payload_buf.len(), section_len);

    // ---- 6. 4-byte align + record header. -----------------------------
    let pad = (4 - (section_len % 4)) % 4;
    let compressed_with_pad = section_len + pad;
    payload_buf.extend(std::iter::repeat_n(0u8, pad));

    // `data_length` reports the sum of decompressed bank-stream bytes.
    // The Lz4ByBank decoder uses the per-bank fields directly; this is
    // kept for header consistency (informational).
    let data_length: u32 = decompressed_sizes.iter().sum();
    let index_array_length = event_count * 4;

    let header_length = RECORD_HEADER_SIZE as u32;
    let total_bytes = header_length as u64 + compressed_with_pad as u64;
    let mut bit_info: u32 = HIPO_VERSION;
    bit_info |= ((pad as u32) & BITINFO_PAD_MASK) << BITINFO_PAD3_SHIFT;
    bit_info |= 4 << BITINFO_HEADER_TYPE_SHIFT;

    let header = RecordHeader {
        record_length: total_bytes,
        record_number,
        header_length,
        event_count,
        index_array_length,
        bit_info,
        user_header_length: 0,
        data_length,
        compressed_data_length: compressed_with_pad as u32,
        compression: CompressionType::Lz4ByBank,
        user_word_1,
        user_word_2,
        endianness: Endianness::Little,
        user_header_padding: 0,
        data_padding: 0,
        compressed_data_padding: pad as u8,
    };

    let mut out = vec![0u8; total_bytes as usize];
    let hdr: &mut [u8; RECORD_HEADER_SIZE] = (&mut out[..RECORD_HEADER_SIZE])
        .try_into()
        .map_err(|_| HipoError::Compression("record header slice"))?;
    header.write(hdr);
    out[RECORD_HEADER_SIZE..].copy_from_slice(&payload_buf[..compressed_with_pad]);
    Ok(out)
}

/// Per-bank directory pieces shared by the v1 and v2 by-bank builders.
/// `compressed_streams[b]` is the LZ4 block for bank `b`'s concatenated
/// data; `sizes[b][e]` is the (uncompressed) byte size of bank `b` in
/// event `e`.
struct ByBankParts {
    num_banks: usize,
    event_count: u32,
    e_count: usize,
    descriptors: Vec<(u16, u8, u8)>,
    compressed_streams: Vec<Vec<u8>>,
    compressed_sizes: Vec<u32>,
    decompressed_sizes: Vec<u32>,
    event_tags: Vec<u32>,
    presence: Vec<u8>,
    bytes_per_row: usize,
    sizes: Vec<Vec<u32>>,
}

/// Steps 1–4 shared by both by-bank builders: walk events into per-bank
/// tables, sort banks for determinism, LZ4 each bank stream, pack the
/// presence matrix.
fn build_by_bank_parts(events: &[&[u8]], compress_buf: &mut Vec<u8>) -> Result<ByBankParts> {
    let event_count = events.len() as u32;
    let e_count = events.len();

    let mut event_tags: Vec<u32> = Vec::with_capacity(e_count);
    let mut descriptors: Vec<(u16, u8, u8)> = Vec::new();
    let mut lookup: std::collections::HashMap<(u16, u8), usize> = std::collections::HashMap::new();
    let mut present: Vec<Vec<bool>> = Vec::new(); // [b][e]
    let mut sizes: Vec<Vec<u32>> = Vec::new(); // [b][e]
    let mut streams: Vec<Vec<u8>> = Vec::new(); // [b] = concat

    for (e_idx, ev_bytes) in events.iter().enumerate() {
        let ev = Event::new(ev_bytes);
        event_tags.push(ev.tag());
        for (hdr, data) in ev.iter_structures() {
            let key = (hdr.group, hdr.item);
            let b = match lookup.get(&key) {
                Some(&b) => b,
                None => {
                    let b = descriptors.len();
                    descriptors.push((hdr.group, hdr.item, hdr.ty));
                    lookup.insert(key, b);
                    present.push(vec![false; e_count]);
                    sizes.push(vec![0u32; e_count]);
                    streams.push(Vec::new());
                    b
                }
            };
            present[b][e_idx] = true;
            sizes[b][e_idx] = data.len() as u32;
            streams[b].extend_from_slice(data);
        }
    }
    let num_banks = descriptors.len();

    // Sort banks by (group, item) for a reproducible on-disk layout.
    let mut sort_idx: Vec<usize> = (0..num_banks).collect();
    sort_idx.sort_by_key(|&b| (descriptors[b].0, descriptors[b].1));
    let descriptors: Vec<(u16, u8, u8)> = sort_idx.iter().map(|&b| descriptors[b]).collect();
    let present: Vec<Vec<bool>> = sort_idx.iter().map(|&b| present[b].clone()).collect();
    let sizes: Vec<Vec<u32>> = sort_idx.iter().map(|&b| sizes[b].clone()).collect();
    let mut streams: Vec<Vec<u8>> = sort_idx
        .iter()
        .map(|&b| std::mem::take(&mut streams[b]))
        .collect();

    // Compress each bank's concatenated bytes (one LZ4 block).
    let mut compressed_streams: Vec<Vec<u8>> = Vec::with_capacity(num_banks);
    let mut compressed_sizes: Vec<u32> = Vec::with_capacity(num_banks);
    let mut decompressed_sizes: Vec<u32> = Vec::with_capacity(num_banks);
    for stream in &mut streams {
        let decompressed = stream.len() as u32;
        decompressed_sizes.push(decompressed);
        if decompressed == 0 {
            compressed_sizes.push(0);
            compressed_streams.push(Vec::new());
            continue;
        }
        compress_buf.clear();
        let n = compress(CompressionType::Lz4, stream, compress_buf)?;
        compressed_sizes.push(n as u32);
        compressed_streams.push(compress_buf[..n].to_vec());
    }

    // Pack the presence matrix (row-major, ceil(B/8) bytes/row).
    let bytes_per_row = num_banks.div_ceil(8);
    let mut presence: Vec<u8> = vec![0u8; e_count * bytes_per_row];
    for (b, row) in present.iter().enumerate() {
        for (e, &p) in row.iter().enumerate() {
            if p {
                let byte = e * bytes_per_row + b / 8;
                let bit = (b % 8) as u8;
                presence[byte] |= 1u8 << bit;
            }
        }
    }

    Ok(ByBankParts {
        num_banks,
        event_count,
        e_count,
        descriptors,
        compressed_streams,
        compressed_sizes,
        decompressed_sizes,
        event_tags,
        presence,
        bytes_per_row,
        sizes,
    })
}

/// By-bank **version 2** record path.
///
/// Same bank streams as v1, but the directory is prefixed with an
/// extension-format-version byte and LZ4-compressed (it is dominated by
/// the redundant per-event size matrix). Compressed payload layout:
///
/// ```text
/// +-- compressed payload section -------------------+
/// | u8  ext_format_version (= 2)                    |
/// | u8  reserved[3]                                 |   pad to 4-byte align
/// | u32 num_banks (B)                               |
/// | u32 event_count (E)                             |
/// | u32 directory_compressed_len                    |
/// | u32 directory_decompressed_len                  |
/// | LZ4(directory)                                  |   the v1 directory body
/// | concat B × LZ4 bank streams                     |   (identical to v1)
/// +-------------------------------------------------+
///
/// directory (decompressed) =
///   B × { u16 group, u8 item, u8 data_type }
///   B × u32 compressed_bank_sizes
///   B × u32 decompressed_bank_sizes
///   E × u32 event_tags
///   E × ceil(B/8) presence
///   B × E × u32 event_bank_sizes
/// ```
fn build_by_bank_v2_record_bytes(
    events: &[&[u8]],
    user_word_1: u64,
    user_word_2: u64,
    record_number: u32,
    payload_buf: &mut Vec<u8>,
    compress_buf: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    const EXT_FORMAT_VERSION: u8 = 2;

    let ByBankParts {
        num_banks,
        event_count,
        e_count,
        descriptors,
        compressed_streams,
        compressed_sizes,
        decompressed_sizes,
        event_tags,
        presence,
        bytes_per_row,
        sizes,
    } = build_by_bank_parts(events, compress_buf)?;

    // ---- Build the directory body (uncompressed). ---------------------
    let dir_len = 4 * num_banks            // descriptors
        + 4 * num_banks                     // compressed sizes
        + 4 * num_banks                     // decompressed sizes
        + 4 * e_count                       // tags
        + e_count * bytes_per_row           // presence
        + 4 * num_banks * e_count; // event_bank_sizes
    let mut dir = Vec::with_capacity(dir_len);
    for &(g, i, t) in &descriptors {
        dir.extend_from_slice(&g.to_le_bytes());
        dir.push(i);
        dir.push(t);
    }
    for s in &compressed_sizes {
        dir.extend_from_slice(&s.to_le_bytes());
    }
    for s in &decompressed_sizes {
        dir.extend_from_slice(&s.to_le_bytes());
    }
    for t in &event_tags {
        dir.extend_from_slice(&t.to_le_bytes());
    }
    dir.extend_from_slice(&presence);
    for row in &sizes {
        for &s in row {
            dir.extend_from_slice(&s.to_le_bytes());
        }
    }
    debug_assert_eq!(dir.len(), dir_len);

    // ---- LZ4-compress the directory. ----------------------------------
    let mut dir_compressed = Vec::new();
    let dir_comp_len = if dir.is_empty() {
        0
    } else {
        compress(CompressionType::Lz4, &dir, &mut dir_compressed)?
    };

    // ---- Assemble the v2 payload section. -----------------------------
    let v2_header = 4 + 4 + 4 + 4 + 4; // version+reserved | B | E | dir_comp | dir_decomp
    let stream_bytes: usize = compressed_streams.iter().map(|s| s.len()).sum();
    let section_len = v2_header + dir_comp_len + stream_bytes;

    payload_buf.clear();
    payload_buf.reserve(section_len);
    payload_buf.push(EXT_FORMAT_VERSION);
    payload_buf.extend_from_slice(&[0u8, 0, 0]); // reserved
    payload_buf.extend_from_slice(&(num_banks as u32).to_le_bytes());
    payload_buf.extend_from_slice(&event_count.to_le_bytes());
    payload_buf.extend_from_slice(&(dir_comp_len as u32).to_le_bytes());
    payload_buf.extend_from_slice(&(dir_len as u32).to_le_bytes());
    payload_buf.extend_from_slice(&dir_compressed[..dir_comp_len]);
    for stream in &compressed_streams {
        payload_buf.extend_from_slice(stream);
    }
    debug_assert_eq!(payload_buf.len(), section_len);

    // ---- 4-byte align + record header. --------------------------------
    let pad = (4 - (section_len % 4)) % 4;
    let compressed_with_pad = section_len + pad;
    payload_buf.extend(std::iter::repeat_n(0u8, pad));

    let data_length: u32 = decompressed_sizes.iter().sum();
    let index_array_length = event_count * 4;
    let header_length = RECORD_HEADER_SIZE as u32;
    let total_bytes = header_length as u64 + compressed_with_pad as u64;
    let mut bit_info: u32 = HIPO_VERSION;
    bit_info |= ((pad as u32) & BITINFO_PAD_MASK) << BITINFO_PAD3_SHIFT;
    bit_info |= 4 << BITINFO_HEADER_TYPE_SHIFT;

    let header = RecordHeader {
        record_length: total_bytes,
        record_number,
        header_length,
        event_count,
        index_array_length,
        bit_info,
        user_header_length: 0,
        data_length,
        compressed_data_length: compressed_with_pad as u32,
        compression: CompressionType::Lz4ByBankV2,
        user_word_1,
        user_word_2,
        endianness: Endianness::Little,
        user_header_padding: 0,
        data_padding: 0,
        compressed_data_padding: pad as u8,
    };

    let mut out = vec![0u8; total_bytes as usize];
    let hdr: &mut [u8; RECORD_HEADER_SIZE] = (&mut out[..RECORD_HEADER_SIZE])
        .try_into()
        .map_err(|_| HipoError::Compression("record header slice"))?;
    header.write(hdr);
    out[RECORD_HEADER_SIZE..].copy_from_slice(&payload_buf[..compressed_with_pad]);
    Ok(out)
}
