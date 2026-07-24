//! Big-endian → little-endian normalization of a decompressed record payload.
//!
//! HIPO records carry an endianness flag in their header magic. Rather than
//! making all ~66 read sites endian-aware — which would cost the little-endian
//! hot path a branch per value and defeat the zero-copy `bytemuck` column casts
//! — a big-endian record's decompressed payload is byte-swapped **once, in
//! place, right after decompression**. Every downstream path (zero-copy column
//! slices, the columnar materializer, event/structure iteration, random access)
//! then sees native little-endian layout and needs no endianness logic at all.
//!
//! A little-endian file — every file any current HIPO writer produces — pays
//! exactly one `if` per record and never enters this module.
//!
//! What gets swapped, in payload order:
//!
//! - the index array (`event_count` × `u32` event lengths);
//! - each event header's `size` / `tag` / `reserved` words (the 4-byte `EVNT`
//!   magic is character data, but a writer that stored it as a `u32` constant
//!   leaves it reversed, so a reversed magic is corrected too);
//! - each structure header's `group` (`u16`) and `length` (`u32`) words;
//! - each structure's payload, per element, using the column layout from the
//!   dictionary (column-major, honouring `T#N` array columns) or, for a
//!   composite bank, its inline row-major format string.
//!
//! A structure whose `(group, item)` is not in the dictionary and which is not
//! composite has no known element layout, so its bytes are left untouched (the
//! same "opaque bank" treatment the per-column writer gives them).

use crate::schema::Dict;
use crate::wire::constants::*;
use crate::wire::record_header::RecordHeader;

/// Reverse each `elem`-byte element in `buf`. `elem` is 1/2/4/8; 1 is a no-op.
#[inline]
fn swap_elements(buf: &mut [u8], elem: usize) {
    match elem {
        2 => {
            for c in buf.chunks_exact_mut(2) {
                c.swap(0, 1);
            }
        }
        4 => {
            for c in buf.chunks_exact_mut(4) {
                c.swap(0, 3);
                c.swap(1, 2);
            }
        }
        8 => {
            for c in buf.chunks_exact_mut(8) {
                c.swap(0, 7);
                c.swap(1, 6);
                c.swap(2, 5);
                c.swap(3, 4);
            }
        }
        _ => {}
    }
}

#[inline]
fn swap_at(buf: &mut [u8], off: usize, width: usize) {
    if let Some(s) = buf.get_mut(off..off + width) {
        s.reverse();
    }
}

#[inline]
fn read_u32_native(buf: &[u8], off: usize) -> u32 {
    match buf.get(off..off + 4) {
        Some(s) => u32::from_le_bytes([s[0], s[1], s[2], s[3]]),
        None => 0,
    }
}

/// Normalize a big-endian record payload to little-endian, in place.
///
/// `payload` is the decompressed record payload (index array, then the user
/// header, then the data section). Returns nothing: a structure that can't be
/// interpreted is skipped, leaving its bytes as they were — the reader's
/// existing bounds checks still apply downstream.
pub(crate) fn normalize_be_payload(payload: &mut [u8], header: &RecordHeader, dict: Option<&Dict>) {
    // 1. Index array: one u32 event length per event.
    let index_len = (header.index_array_length as usize).min(payload.len());
    swap_elements(&mut payload[..index_len], 4);

    // 2. Walk the data section event by event, using the (now little-endian)
    //    index array for event boundaries.
    let data_start = header.index_array_length as usize
        + header.user_header_length as usize
        + header.user_header_padding as usize;
    if data_start >= payload.len() {
        return;
    }
    let n = header.event_count as usize;
    let mut off = data_start;
    for i in 0..n {
        let size = read_u32_native(payload, i * 4) as usize;
        let end = match off.checked_add(size) {
            Some(e) if e <= payload.len() => e,
            _ => break,
        };
        normalize_be_event(&mut payload[off..end], dict);
        off = end;
    }
}

/// Normalize one event: its header words, then every structure it holds.
fn normalize_be_event(ev: &mut [u8], dict: Option<&Dict>) {
    if ev.len() < EVENT_HEADER_SIZE {
        return;
    }
    // The magic is 4 characters ("EVNT"). A writer that stored it as a u32
    // constant leaves it reversed on a big-endian host; correct that so the
    // downstream magic check passes.
    if &ev[0..4] == b"TNVE" {
        ev[0..4].reverse();
    }
    swap_at(ev, EH_SIZE, 4);
    swap_at(ev, EH_TAG, 4);
    swap_at(ev, EH_RESERVED, 4);

    let ev_size = read_u32_native(ev, EH_SIZE) as usize;
    let end = ev_size.min(ev.len());
    let mut pos = EVENT_HEADER_SIZE;
    while pos + BANK_STRUCTURE_SIZE <= end {
        // Structure header: group(u16) item(u8) type(u8) length(u32).
        swap_at(ev, pos, 2);
        swap_at(ev, pos + 4, 4);
        let group = u16::from_le_bytes([ev[pos], ev[pos + 1]]);
        let item = ev[pos + 2];
        let length_word = read_u32_native(ev, pos + 4);
        let data_size = (length_word & STRUCT_SIZE_MASK) as usize;
        let format_size = ((length_word >> STRUCT_FORMAT_SHIFT) & STRUCT_FORMAT_BYTE) as usize;
        let data_start = pos + BANK_STRUCTURE_SIZE;
        let data_end = match data_start.checked_add(data_size) {
            Some(e) if e <= end => e,
            _ => break, // truncated: leave the rest untouched
        };
        normalize_be_structure(
            &mut ev[data_start..data_end],
            group,
            item,
            format_size,
            dict,
        );
        pos = data_end;
    }
}

/// Normalize one structure's payload. `format_size > 0` marks a composite bank
/// (row-major, inline format string); otherwise the dictionary supplies the
/// column-major layout. Unknown, non-composite banks are left as-is.
fn normalize_be_structure(
    data: &mut [u8],
    group: u16,
    item: u8,
    format_size: usize,
    dict: Option<&Dict>,
) {
    if data.is_empty() {
        return;
    }
    if format_size > 0 {
        normalize_be_composite(data, format_size);
        return;
    }
    // The trailer's file-index bank has a fixed layout and never appears in the
    // dictionary, so it needs its own rule: column-major position/L, length/I,
    // entries/I, userWordOne/L, userWordTwo/L (32 bytes per row).
    if group == FILE_INDEX_GROUP && item == FILE_INDEX_ITEM {
        let rows = data.len() / 32;
        if rows > 0 {
            let mut off = 0usize;
            for w in [8usize, 4, 4, 8, 8] {
                let span = rows * w;
                let Some(col) = data.get_mut(off..off + span) else {
                    return;
                };
                swap_elements(col, w);
                off += span;
            }
        }
        return;
    }
    let Some(schema) = dict.and_then(|d| d.get_by_id(group, item)) else {
        return; // opaque bank: no layout information
    };
    let row_size = schema.row_size() as usize;
    if row_size == 0 {
        return;
    }
    let rows = data.len() / row_size;
    if rows == 0 {
        return;
    }
    // Column-major: each column occupies `rows * size * length` contiguous
    // bytes, in schema order.
    let mut off = 0usize;
    for e in schema.entries() {
        let elem = e.ty.size();
        let span = rows * elem * e.length as usize;
        let Some(col) = data.get_mut(off..off + span) else {
            return;
        };
        swap_elements(col, elem);
        off += span;
    }
}

/// Composite bank: `format_size` bytes of format string, then row-major rows.
fn normalize_be_composite(data: &mut [u8], format_size: usize) {
    if format_size > data.len() {
        return;
    }
    // The format string is character data — no swap. Parse it to learn the
    // per-row field widths.
    let Ok(format_str) = std::str::from_utf8(&data[..format_size]) else {
        return;
    };
    let Ok(format) = crate::event::CompositeFormat::parse(format_str.trim_end_matches('\0')) else {
        return;
    };
    let row_size = format.row_size() as usize;
    if row_size == 0 {
        return;
    }
    let body = &mut data[format_size..];
    let rows = body.len() / row_size;
    for r in 0..rows {
        let base = r * row_size;
        for f in format.fields() {
            let elem = f.ty.size();
            let off = base + f.row_offset as usize;
            if let Some(s) = body.get_mut(off..off + elem) {
                s.reverse();
            }
        }
    }
}
