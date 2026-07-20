//! Record-building primitives + the public `Compression` enum.
//!
//! Most users never touch [`RecordBuilder`] or [`build_record_bytes`]
//! directly — the [`Writer`](super::Writer) handles record building
//! transparently. They're exposed for advanced callers that assemble
//! compressed records themselves.

use crate::compress::{ScratchBuf, compress};
use crate::error::{HipoError, Result};
use crate::event::Event;
use crate::schema::Dict;
use crate::wire::bytes::{Endianness, write_u32_le};
use crate::wire::constants::*;
use crate::wire::record_header::RecordHeader;

/// Compression mode for HIPO records.
///
/// The wire-level compression tag is derived from this enum by the record
/// builder.
///
/// - `None` / `Lz4` / `Lz4Best` / `Gzip` are interchangeable with the wire
///   tag of the same name, and produce files readable by the C++ `hipo4`
///   reader.
/// - `Lz4ByBankV2` and `Lz4PerColumn` are Rust-only format extensions that
///   enable true partial decompression — `ev.bank("name")` (or one column)
///   inflates only the stream it needs, leaving the rest compressed.
///
/// **Files written with `Lz4ByBankV2` or `Lz4PerColumn` are not readable by
/// the C++ `hipo4` reader.**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Lz4,
    Lz4Best,
    Gzip,
    /// Store each bank type as its own LZ4-HC stream within the record, plus
    /// an event×bank presence directory (itself LZ4-compressed, prefixed with
    /// an extension-format-version byte). Readers decompress only the bank
    /// streams they actually touch; per-bank grouping plus HC beats
    /// whole-record [`Self::Lz4Best`]. Writes are slower (HC).
    Lz4ByBankV2,
    /// Per-*column* layout: within each bank, every column is stored as its
    /// own LZ4-HC stream laid out cross-event contiguous (all events' `px`,
    /// then all `py`, …). Reading one column inflates only that column, and
    /// homogeneous columns compress better than a bank's interleaved bytes,
    /// so this beats [`Self::Lz4ByBankV2`] on both size and selective reads.
    /// Banks without a schema (or composite banks) are stored opaquely as a
    /// single stream. Writes are slower (HC). Layout in `wire/per_column.rs`.
    Lz4PerColumn,
}

impl Compression {
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
            Self::Lz4ByBankV2 => CompressionType::Lz4ByBankV2,
            Self::Lz4PerColumn => CompressionType::Lz4PerColumn,
        }
    }
}

/// Accumulates events into a single record. Flushed via
/// [`Writer::flush_record`](super::Writer::flush_record). Crate-internal.
#[derive(Debug, Default)]
pub(crate) struct RecordBuilder {
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
/// position of the record in the output. Crate-internal.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_record_bytes(
    events: &[&[u8]],
    dict: &Dict,
    user_word_1: u64,
    user_word_2: u64,
    compression: Compression,
    record_number: u32,
    payload_buf: &mut Vec<u8>,
    compress_buf: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    match compression {
        Compression::Lz4ByBankV2 => build_by_bank_v2_record_bytes(
            events,
            user_word_1,
            user_word_2,
            record_number,
            payload_buf,
            compress_buf,
        ),
        Compression::Lz4PerColumn => build_per_column_record_bytes(
            events,
            dict,
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

/// Per-bank directory pieces produced by the by-bank builder.
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

/// Steps 1–4 of the by-bank builder: walk events into per-bank tables, sort
/// banks for determinism, LZ4-HC-compress each bank stream, pack the presence
/// matrix.
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
        // HC-compress each bank stream for a better ratio: per-bank grouping
        // plus LZ4-HC compounds to beat whole-record `Lz4Best`. The output is
        // still a plain LZ4 block, so the reader is unchanged.
        let n = compress(CompressionType::Lz4Best, stream, compress_buf)?;
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

/// Per-*column* record path (`Lz4PerColumn`, tag 7).
///
/// Within each bank, every column is stored as its own LZ4-HC stream laid
/// out **cross-event contiguous** (all events' `px`, then all `py`, …).
/// Reading one column inflates only that column; homogeneous columns also
/// compress better than a bank's interleaved column-major bytes. Banks
/// without a schema, composite banks, or banks whose bytes are not a whole
/// number of rows are stored **opaquely** as one stream (like a by-bank
/// stream), so the reader can still reconstruct them.
///
/// Compressed payload layout:
///
/// ```text
/// +-- compressed payload section -------------------+
/// | u8  ext_format_version (= 1)                    |
/// | u8  reserved[3]                                 |   pad to 4-byte align
/// | u32 num_banks (B)                               |
/// | u32 event_count (E)                             |
/// | u32 directory_compressed_len                    |
/// | u32 directory_decompressed_len                  |
/// | LZ4(directory)                                  |
/// | concat S × LZ4-HC column/opaque streams         |   S = total streams
/// +-------------------------------------------------+
///
/// directory (decompressed) =
///   B × { u16 group, u8 item, u8 data_type }        descriptors
///   B × u16 num_cols                                (0 = opaque, 1 stream)
///   E × u32 event_tags
///   E × ceil(B/8) presence
///   B × E × u32 event_bank_byte_sizes               (decompressed bank size)
///   S × { u32 compressed_size, u32 decompressed_size }
/// ```
///
/// Streams appear bank-major: bank 0's columns (or its single opaque
/// stream), then bank 1's, and so on — matching the `num_cols` order.
fn build_per_column_record_bytes(
    events: &[&[u8]],
    dict: &Dict,
    user_word_1: u64,
    user_word_2: u64,
    record_number: u32,
    payload_buf: &mut Vec<u8>,
    compress_buf: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    const EXT_FORMAT_VERSION: u8 = 1;
    let event_count = events.len() as u32;
    let e_count = events.len();

    // ---- 1. Walk events into per-bank tables. --------------------------
    let mut event_tags: Vec<u32> = Vec::with_capacity(e_count);
    let mut descriptors: Vec<(u16, u8, u8)> = Vec::new();
    let mut lookup: std::collections::HashMap<(u16, u8), usize> = std::collections::HashMap::new();
    let mut present: Vec<Vec<bool>> = Vec::new(); // [b][e]
    let mut byte_sizes: Vec<Vec<u32>> = Vec::new(); // [b][e] decompressed bank size
    let mut bank_bytes: Vec<Vec<u8>> = Vec::new(); // [b] = concat of bank data across events
    let mut composite: Vec<bool> = Vec::new(); // [b] any structure carries an inline header

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
                    byte_sizes.push(vec![0u32; e_count]);
                    bank_bytes.push(Vec::new());
                    composite.push(false);
                    b
                }
            };
            present[b][e_idx] = true;
            byte_sizes[b][e_idx] = data.len() as u32;
            bank_bytes[b].extend_from_slice(data);
            if hdr.header_size > 0 {
                composite[b] = true;
            }
        }
    }
    let num_banks = descriptors.len();

    // ---- 2. Sort banks by (group, item) for a reproducible layout. ------
    let mut sort_idx: Vec<usize> = (0..num_banks).collect();
    sort_idx.sort_by_key(|&b| (descriptors[b].0, descriptors[b].1));
    let descriptors: Vec<(u16, u8, u8)> = sort_idx.iter().map(|&b| descriptors[b]).collect();
    let present: Vec<Vec<bool>> = sort_idx.iter().map(|&b| present[b].clone()).collect();
    let byte_sizes: Vec<Vec<u32>> = sort_idx.iter().map(|&b| byte_sizes[b].clone()).collect();
    let composite: Vec<bool> = sort_idx.iter().map(|&b| composite[b]).collect();
    let mut bank_bytes: Vec<Vec<u8>> = sort_idx
        .iter()
        .map(|&b| std::mem::take(&mut bank_bytes[b]))
        .collect();

    // ---- 3. Split each bank into per-column streams (or keep opaque). ---
    let mut num_cols: Vec<u16> = Vec::with_capacity(num_banks); // 0 = opaque
    let mut streams: Vec<Vec<u8>> = Vec::new(); // flat, bank-major

    for b in 0..num_banks {
        let (group, item, _t) = descriptors[b];
        // Columnar iff there is a schema, it has a positive row size, the
        // bank isn't composite, and every present event is a whole number
        // of rows. Otherwise store the bank as one opaque stream.
        let schema = dict.get_by_id(group, item);
        let columnar = match schema {
            Some(s) if s.row_size() > 0 && !composite[b] => {
                let rs = s.row_size();
                (0..e_count).all(|e| !present[b][e] || byte_sizes[b][e].is_multiple_of(rs))
            }
            _ => false,
        };
        let bb = std::mem::take(&mut bank_bytes[b]);
        if !columnar {
            num_cols.push(0);
            streams.push(bb);
            continue;
        }
        let s = schema.expect("columnar implies schema");
        let rs = s.row_size() as usize;
        let ncols = s.num_columns();
        num_cols.push(ncols as u16);
        let mut col_streams: Vec<Vec<u8>> = vec![Vec::new(); ncols];
        let mut off = 0usize; // running offset into the concatenated bank bytes
        for e in 0..e_count {
            if !present[b][e] {
                continue;
            }
            let sz = byte_sizes[b][e] as usize;
            let rows_e = sz / rs;
            let block = &bb[off..off + sz];
            off += sz;
            for (c, cs) in col_streams.iter_mut().enumerate() {
                let entry = &s.entries()[c];
                let col_width = entry.ty.size() * entry.length as usize;
                let col_start = rows_e * entry.row_offset as usize;
                cs.extend_from_slice(&block[col_start..col_start + rows_e * col_width]);
            }
        }
        streams.extend(col_streams);
    }
    let num_streams = streams.len();

    // ---- 4. Compress each stream (LZ4-HC). ------------------------------
    let mut compressed_streams: Vec<Vec<u8>> = Vec::with_capacity(num_streams);
    let mut compressed_sizes: Vec<u32> = Vec::with_capacity(num_streams);
    let mut decompressed_sizes: Vec<u32> = Vec::with_capacity(num_streams);
    for stream in &streams {
        let decompressed = stream.len() as u32;
        decompressed_sizes.push(decompressed);
        if decompressed == 0 {
            compressed_sizes.push(0);
            compressed_streams.push(Vec::new());
            continue;
        }
        compress_buf.clear();
        let n = compress(CompressionType::Lz4Best, stream, compress_buf)?;
        compressed_sizes.push(n as u32);
        compressed_streams.push(compress_buf[..n].to_vec());
    }

    // ---- 5. Pack the presence matrix (row-major, ceil(B/8) bytes/row). --
    let bytes_per_row = num_banks.div_ceil(8);
    let mut presence: Vec<u8> = vec![0u8; e_count * bytes_per_row];
    for (b, row) in present.iter().enumerate() {
        for (e, &p) in row.iter().enumerate() {
            if p {
                presence[e * bytes_per_row + b / 8] |= 1u8 << (b % 8) as u8;
            }
        }
    }

    // ---- 6. Build the directory body (uncompressed). -------------------
    let dir_len = 4 * num_banks            // descriptors
        + 2 * num_banks                     // num_cols
        + 4 * e_count                       // event tags
        + e_count * bytes_per_row           // presence
        + 4 * num_banks * e_count           // event_bank_byte_sizes
        + 8 * num_streams; // per-stream compressed+decompressed sizes
    let mut dir = Vec::with_capacity(dir_len);
    for &(g, i, t) in &descriptors {
        dir.extend_from_slice(&g.to_le_bytes());
        dir.push(i);
        dir.push(t);
    }
    for &nc in &num_cols {
        dir.extend_from_slice(&nc.to_le_bytes());
    }
    for t in &event_tags {
        dir.extend_from_slice(&t.to_le_bytes());
    }
    dir.extend_from_slice(&presence);
    for row in &byte_sizes {
        for &s in row {
            dir.extend_from_slice(&s.to_le_bytes());
        }
    }
    for i in 0..num_streams {
        dir.extend_from_slice(&compressed_sizes[i].to_le_bytes());
        dir.extend_from_slice(&decompressed_sizes[i].to_le_bytes());
    }
    debug_assert_eq!(dir.len(), dir_len);

    // ---- 7. LZ4-compress the directory. --------------------------------
    let mut dir_compressed = Vec::new();
    let dir_comp_len = if dir.is_empty() {
        0
    } else {
        compress(CompressionType::Lz4, &dir, &mut dir_compressed)?
    };

    // ---- 8. Assemble the payload section. ------------------------------
    let pc_header = 4 + 4 + 4 + 4 + 4; // version+reserved | B | E | dir_comp | dir_decomp
    let stream_bytes: usize = compressed_streams.iter().map(|s| s.len()).sum();
    let section_len = pc_header + dir_comp_len + stream_bytes;

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

    // ---- 9. 4-byte align + record header. ------------------------------
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
        compression: CompressionType::Lz4PerColumn,
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
