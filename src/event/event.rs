//! `Event<'a>` — bare borrowed-bytes wrapper over a HIPO event buffer.
//!
//! Holds *only* the byte slice (no dict reference) so the same low-level
//! lifetime can travel anywhere — composites and raw writes.
//! Most user code touches [`EventCtx`](crate::event::EventCtx) instead, which
//! pairs an `Event` with the schema dictionary.

use crate::wire::bytes::read_u32_le;
use crate::wire::constants::*;

/// Decoded 8-byte structure header.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct StructureHeader {
    pub group: u16,
    pub item: u8,
    pub ty: u8,
    /// Data size in bytes, excluding the 8-byte header.
    pub data_size: u32,
    /// Inline header size (composite banks). Zero for plain banks.
    pub header_size: u8,
}

impl StructureHeader {
    pub const fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < BANK_STRUCTURE_SIZE {
            return None;
        }
        // Manual little-endian decode so this stays `const`.
        let group = (buf[0] as u16) | ((buf[1] as u16) << 8);
        let item = buf[2];
        let ty = buf[3];
        let length = (buf[4] as u32)
            | ((buf[5] as u32) << 8)
            | ((buf[6] as u32) << 16)
            | ((buf[7] as u32) << 24);
        Some(Self {
            group,
            item,
            ty,
            data_size: length & STRUCT_SIZE_MASK,
            header_size: ((length >> STRUCT_FORMAT_SHIFT) & STRUCT_FORMAT_BYTE) as u8,
        })
    }
}

/// Borrowed view over a HIPO event buffer.
///
/// Cheap to construct, cheap to copy. Find a structure by `(group, item)`
/// in O(structures) — typical events have 10–30 banks so a linear scan is
/// faster than building a hash map.
#[derive(Debug, Copy, Clone)]
pub struct Event<'a> {
    buf: &'a [u8],
}

impl<'a> Event<'a> {
    /// Wrap a byte slice as an event. The buffer is *not* validated until
    /// the first [`Self::find`] call — use [`Self::tag`] / [`Self::size`]
    /// for header inspection.
    #[inline]
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    pub fn raw(&self) -> &'a [u8] {
        self.buf
    }

    #[inline]
    pub fn size(&self) -> u32 {
        if self.buf.len() < EVENT_HEADER_SIZE {
            return 0;
        }
        read_u32_le(self.buf, EH_SIZE)
    }

    #[inline]
    pub fn tag(&self) -> u32 {
        if self.buf.len() < EVENT_HEADER_SIZE {
            return 0;
        }
        read_u32_le(self.buf, EH_TAG)
    }

    /// Find the structure with `(group, item)`. Returns `(header, data)`,
    /// where `data` is a borrowed slice of length `header.data_size`.
    pub fn find(&self, group: u16, item: u8) -> Option<(StructureHeader, &'a [u8])> {
        for (hdr, data) in self.iter_structures() {
            if hdr.group == group && hdr.item == item {
                return Some((hdr, data));
            }
        }
        None
    }

    /// True if a structure with the given identifier exists in the event.
    pub fn has(&self, group: u16, item: u8) -> bool {
        self.find(group, item).is_some()
    }

    pub fn iter_structures(&self) -> StructureIter<'a> {
        let size = self.size() as usize;
        let cap = std::cmp::min(size, self.buf.len());
        StructureIter {
            buf: self.buf,
            pos: EVENT_HEADER_SIZE,
            end: cap,
        }
    }

    /// Yields `(structure_start_offset, header, data)` triples. Used by
    /// composite decoding so we can reconstruct the structure's full byte
    /// slice without a second linear scan.
    pub(crate) fn iter_structures_with_offset(
        &self,
    ) -> impl Iterator<Item = (usize, StructureHeader, &'a [u8])> {
        let size = self.size() as usize;
        let cap = std::cmp::min(size, self.buf.len());
        let buf = self.buf;
        let mut pos = EVENT_HEADER_SIZE;
        std::iter::from_fn(move || {
            if pos + BANK_STRUCTURE_SIZE > cap {
                return None;
            }
            let hdr = StructureHeader::parse(&buf[pos..])?;
            let data_start = pos + BANK_STRUCTURE_SIZE;
            let data_end = data_start + hdr.data_size as usize;
            if data_end > cap || data_end > buf.len() {
                return None;
            }
            let start = pos;
            let data = &buf[data_start..data_end];
            pos = data_end;
            Some((start, hdr, data))
        })
    }
}

/// Iterator over structures in an event.
#[derive(Debug, Clone)]
pub struct StructureIter<'a> {
    buf: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> Iterator for StructureIter<'a> {
    type Item = (StructureHeader, &'a [u8]);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos + BANK_STRUCTURE_SIZE > self.end {
            return None;
        }
        let hdr = StructureHeader::parse(&self.buf[self.pos..])?;
        let data_start = self.pos + BANK_STRUCTURE_SIZE;
        let data_end = data_start + hdr.data_size as usize;
        if data_end > self.end || data_end > self.buf.len() {
            return None;
        }
        let data = &self.buf[data_start..data_end];
        self.pos = data_end;
        Some((hdr, data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::bytes::write_u32_le;

    fn build_event(structures: &[(u16, u8, &[u8])]) -> Vec<u8> {
        let mut buf = vec![0u8; EVENT_HEADER_SIZE];
        buf[0..4].copy_from_slice(b"EVNT");
        for (g, i, data) in structures {
            buf.extend_from_slice(&g.to_le_bytes());
            buf.push(*i);
            buf.push(11);
            buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
            buf.extend_from_slice(data);
        }
        let total = buf.len() as u32;
        write_u32_le(&mut buf, EH_SIZE, total);
        buf
    }

    #[test]
    fn finds_structures_in_order() {
        let buf = build_event(&[(300, 1, b"first"), (332, 11, b"second")]);
        let e = Event::new(&buf);
        let (h1, d1) = e.find(300, 1).unwrap();
        assert_eq!(d1, b"first");
        assert_eq!(h1.data_size, 5);
        let (_h2, d2) = e.find(332, 11).unwrap();
        assert_eq!(d2, b"second");
        assert!(e.has(332, 11));
        assert!(!e.has(0, 0));
    }

    #[test]
    fn iterates_all_structures() {
        let buf = build_event(&[(1, 1, b"a"), (2, 2, b"bb"), (3, 3, b"ccc")]);
        let e = Event::new(&buf);
        let groups: Vec<_> = e.iter_structures().map(|(h, _)| h.group).collect();
        assert_eq!(groups, vec![1, 2, 3]);
    }

    #[test]
    fn truncated_event_stops_iteration_safely() {
        let mut buf = build_event(&[(1, 1, b"hello"), (2, 2, b"world")]);
        buf.pop();
        let e = Event::new(&buf);
        let count = e.iter_structures().count();
        assert!(count <= 2);
    }

    #[test]
    fn empty_event() {
        let buf = build_event(&[]);
        let e = Event::new(&buf);
        assert_eq!(e.iter_structures().count(), 0);
        assert!(e.find(0, 0).is_none());
    }
}
