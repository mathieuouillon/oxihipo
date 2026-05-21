//! Record-building primitives + the public `Compression` enum.
//!
//! Most users never touch [`RecordBuilder`] or [`build_record_bytes`]
//! directly — the [`Writer`](super::Writer) handles record building
//! transparently. They're exposed for advanced callers that assemble
//! compressed records themselves.

use crate::compress::{ScratchBuf, compress};
use crate::error::{HipoError, Result};
use crate::wire::bytes::{Endianness, write_u32_le};
use crate::wire::constants::*;
use crate::wire::record_header::RecordHeader;

/// Public re-export of the wire-level compression enum.
pub use crate::wire::constants::CompressionType as Compression;

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
    let compressed_len = compress(compression, payload_buf, compress_buf)?;
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
        compression,
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
