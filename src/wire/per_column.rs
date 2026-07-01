//! `Lz4PerColumn` record decoder + lazy per-column decompression cache.
//!
//! The on-disk layout (see `crate::write::record::build_per_column_record_bytes`)
//! stores each `(bank, column)` as its own LZ4-HC stream, laid out
//! cross-event contiguous. The reader parses the directory eagerly and
//! inflates a column stream only when that column is actually read, so a
//! `px`-only analysis never touches the other columns' bytes.
//!
//! Banks that have no schema, are composite, or whose bytes are not a
//! whole number of rows are stored **opaquely** as a single stream — the
//! reader serves those exactly like a by-bank stream.

use std::sync::{Arc, OnceLock};

use crate::compress::decompress;
use crate::error::{HipoError, Result};
use crate::wire::bytes::read_u32_le;
use crate::wire::constants::CompressionType;
use crate::wire::record_header::RecordHeader;

/// Owns a [`PerColumnRecord`]'s compressed payload section (one `pread`'d
/// copy per record; individual column streams inflate lazily).
#[derive(Debug)]
struct Backing(Box<[u8]>);

impl Backing {
    #[inline]
    fn section(&self) -> &[u8] {
        &self.0
    }
}

/// Counts column-stream inflate calls — used by tests to prove the
/// partial-decompression contract (untouched columns stay compressed).
#[cfg(test)]
pub static COLUMN_INFLATE_COUNTER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
struct BankDescriptor {
    group: u16,
    item: u8,
    data_type: u8,
}

/// Precomputed column geometry for one bank, cached per record so
/// whole-event reassembly resolves each bank's schema once per record
/// instead of once per event. An empty `cols` marks an opaque or
/// schema-less bank.
#[derive(Debug, Clone)]
pub struct BankLayout {
    pub row_size: u32,
    /// `(row_offset, col_width)` per column, in schema order.
    pub cols: Vec<(u32, u32)>,
}

/// Shared state attached to every `OwnedEvent` from an `Lz4PerColumn`
/// record. One heap allocation per record, plus one per column stream
/// that is actually decompressed.
#[derive(Debug)]
pub struct PerColumnRecord {
    pub header: RecordHeader,
    descriptors: Vec<BankDescriptor>,
    event_count: u32,
    event_tags: Vec<u32>,
    presence: Vec<u8>,
    bytes_per_row: usize,
    /// Columns per bank. `0` marks an **opaque** bank stored as a single
    /// whole-bank stream (no schema / composite / ragged rows).
    num_cols: Vec<u16>,
    /// Prefix sum of streams-per-bank: bank `b`'s streams occupy
    /// `stream_base[b]..stream_base[b + 1]` of the flat stream arrays.
    /// Streams-per-bank is `max(1, num_cols[b])`.
    stream_base: Vec<usize>,
    /// Per-bank **cumulative byte** offsets into the (reconstructed)
    /// column-major bank data: `bank_offsets[b]` has `event_count + 1`
    /// entries; event `e`'s bank-`b` bytes span
    /// `bank_offsets[b][e]..bank_offsets[b][e + 1]`.
    bank_offsets: Vec<Vec<u32>>,
    /// Lazy decompressed column/opaque streams, one per flat stream index.
    stream_data: Vec<OnceLock<Box<[u8]>>>,
    backing: Backing,
    /// Per stream, its compressed byte range within `backing.section()`.
    compressed_streams: Vec<std::ops::Range<usize>>,
    /// Per stream, its expected decompressed size.
    decompressed_sizes: Vec<u32>,
    /// Lazily-built per-bank column geometry, cached so whole-event
    /// reassembly resolves each bank's schema once per record (not per
    /// event). Populated by the first caller that has the dict — see
    /// [`Self::column_layout`].
    col_layout: OnceLock<Vec<BankLayout>>,
}

/// Overflow-checked byte length of the *fixed* part of the per-column
/// directory (everything except the per-stream size table, whose length
/// depends on the `num_cols` values read from the directory itself).
fn directory_fixed_len(num_banks: usize, event_count: u32) -> Result<usize> {
    let nb = num_banks as u64;
    let ec = event_count as u64;
    let bpr = num_banks.div_ceil(8) as u64;
    let total = (|| -> Option<u64> {
        let desc = 4u64.checked_mul(nb)?;
        let ncols = 2u64.checked_mul(nb)?;
        let tags = 4u64.checked_mul(ec)?;
        let presence = ec.checked_mul(bpr)?;
        let sizes = 4u64.checked_mul(nb)?.checked_mul(ec)?;
        desc.checked_add(ncols)?
            .checked_add(tags)?
            .checked_add(presence)?
            .checked_add(sizes)
    })()
    .ok_or(HipoError::CorruptRecord {
        offset: 0,
        reason: "Lz4PerColumn: directory size overflow",
    })?;
    usize::try_from(total).map_err(|_| HipoError::CorruptRecord {
        offset: 0,
        reason: "Lz4PerColumn: directory size overflow",
    })
}

impl PerColumnRecord {
    /// Parse a record, **owning** a copy of its compressed section. Does
    /// not decompress any column stream — those inflate lazily on read.
    pub fn parse(src: &[u8]) -> Result<Arc<Self>> {
        let header = Self::check_header(src)?;
        let (section_off, section_len) = Self::section_bounds(&header, src.len())?;
        let section = &src[section_off..section_off + section_len];
        Self::parse_section(
            header,
            section,
            Backing(section.to_vec().into_boxed_slice()),
        )
    }

    fn check_header(src: &[u8]) -> Result<RecordHeader> {
        let header = RecordHeader::parse(src)?;
        if !header.compression.is_per_column() {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "PerColumnRecord::parse called on a non-per-column record",
            });
        }
        if src.len() < header.total_bytes() as usize {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4PerColumn: record extends past buffer",
            });
        }
        Ok(header)
    }

    fn section_bounds(header: &RecordHeader, src_len: usize) -> Result<(usize, usize)> {
        let header_len = header.header_length as usize;
        let payload_disk_len = header.payload_bytes() as usize;
        let pad = header.compressed_data_padding as usize;
        if header_len + payload_disk_len > src_len {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4PerColumn: payload extends past record",
            });
        }
        Ok((header_len, payload_disk_len.saturating_sub(pad)))
    }

    /// Section layout: `[u8 ver=1, u8×3 reserved, u32 B, u32 E, u32
    /// dir_comp, u32 dir_decomp, LZ4(directory), column streams]`.
    fn parse_section(header: RecordHeader, section: &[u8], backing: Backing) -> Result<Arc<Self>> {
        if section.len() < 20 {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4PerColumn: section truncated (header)",
            });
        }
        if section[0] != 1 {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4PerColumn: unsupported extension-format version",
            });
        }
        let num_banks = read_u32_le(section, 4) as usize;
        let event_count = read_u32_le(section, 8);
        if event_count != header.event_count {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4PerColumn: event_count mismatch with record header",
            });
        }
        let dir_comp_len = read_u32_le(section, 12) as usize;
        let dir_decomp_len = read_u32_le(section, 16) as usize;
        let streams_off = 20usize
            .checked_add(dir_comp_len)
            .ok_or(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4PerColumn: directory size overflow",
            })?;
        if streams_off > section.len() {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4PerColumn: directory truncated",
            });
        }
        // The directory's fixed part must at least fit; the per-stream size
        // table is validated after we know `num_cols`.
        if dir_decomp_len < directory_fixed_len(num_banks, event_count)? {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4PerColumn: directory size inconsistent with bank/event counts",
            });
        }
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

    #[allow(clippy::too_many_arguments)]
    fn from_directory(
        header: RecordHeader,
        section: &[u8],
        backing: Backing,
        num_banks: usize,
        event_count: u32,
        dir: &[u8],
        streams_off: usize,
    ) -> Result<Arc<Self>> {
        let fixed_len = directory_fixed_len(num_banks, event_count)?;
        if dir.len() < fixed_len {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4PerColumn: directory body truncated",
            });
        }
        let ec = event_count as usize;
        let bytes_per_row = num_banks.div_ceil(8);
        let num_cols_off = 4 * num_banks;
        let tags_off = num_cols_off + 2 * num_banks;
        let presence_off = tags_off + 4 * ec;
        let presence_bytes = ec * bytes_per_row;
        let sizes_off = presence_off + presence_bytes; // event_bank_byte_sizes
        let stream_sizes_off = sizes_off + 4 * num_banks * ec;

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
        debug_assert!(
            descriptors
                .windows(2)
                .all(|w| (w[0].group, w[0].item) < (w[1].group, w[1].item)),
            "PerColumn descriptors must be sorted ascending by (group, item)",
        );

        // num_cols + streams-per-bank prefix sum
        let mut num_cols: Vec<u16> = Vec::with_capacity(num_banks);
        let mut stream_base: Vec<usize> = Vec::with_capacity(num_banks + 1);
        let mut total_streams = 0usize;
        stream_base.push(0);
        for b in 0..num_banks {
            let nc = u16::from_le_bytes([dir[num_cols_off + b * 2], dir[num_cols_off + b * 2 + 1]]);
            num_cols.push(nc);
            let per_bank = if nc == 0 { 1 } else { nc as usize };
            total_streams =
                total_streams
                    .checked_add(per_bank)
                    .ok_or(HipoError::CorruptRecord {
                        offset: 0,
                        reason: "Lz4PerColumn: stream count overflow",
                    })?;
            stream_base.push(total_streams);
        }

        // Now that we know the stream count, the per-stream size table must fit.
        let stream_table_len =
            8usize
                .checked_mul(total_streams)
                .ok_or(HipoError::CorruptRecord {
                    offset: 0,
                    reason: "Lz4PerColumn: stream table overflow",
                })?;
        if dir.len() < stream_sizes_off + stream_table_len {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4PerColumn: per-stream size table truncated",
            });
        }

        // event tags
        let mut event_tags: Vec<u32> = Vec::with_capacity(ec);
        for e in 0..ec {
            event_tags.push(read_u32_le(dir, tags_off + e * 4));
        }
        let presence = dir[presence_off..presence_off + presence_bytes].to_vec();

        // per-bank cumulative byte offsets (from event_bank_byte_sizes)
        let mut bank_offsets: Vec<Vec<u32>> = Vec::with_capacity(num_banks);
        for b in 0..num_banks {
            let mut cum: Vec<u32> = Vec::with_capacity(ec + 1);
            let row_off = sizes_off + b * 4 * ec;
            let mut acc: u32 = 0;
            cum.push(0);
            for e in 0..ec {
                acc = acc.saturating_add(read_u32_le(dir, row_off + e * 4));
                cum.push(acc);
            }
            bank_offsets.push(cum);
        }

        // per-stream compressed + decompressed sizes
        let mut compressed_sizes: Vec<u32> = Vec::with_capacity(total_streams);
        let mut decompressed_sizes: Vec<u32> = Vec::with_capacity(total_streams);
        for s in 0..total_streams {
            let off = stream_sizes_off + s * 8;
            compressed_sizes.push(read_u32_le(dir, off));
            decompressed_sizes.push(read_u32_le(dir, off + 4));
        }

        // Validate: a bank's cumulative decompressed bytes must equal the
        // sum of its streams' decompressed sizes (columns partition the
        // bank; the opaque stream is the bank). Guards against a directory
        // whose per-event sizes disagree with the stored streams.
        for b in 0..num_banks {
            let bank_bytes = *bank_offsets[b].last().unwrap_or(&0) as u64;
            let stream_bytes: u64 = (stream_base[b]..stream_base[b + 1])
                .map(|s| decompressed_sizes[s] as u64)
                .sum();
            if bank_bytes != stream_bytes {
                return Err(HipoError::CorruptRecord {
                    offset: 0,
                    reason: "Lz4PerColumn: bank byte size disagrees with its column streams",
                });
            }
        }

        // compressed-stream ranges within `section`
        let mut compressed_streams = Vec::with_capacity(total_streams);
        let mut off = streams_off;
        for &cs in &compressed_sizes {
            let cs = cs as usize;
            let end = off.checked_add(cs).ok_or(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4PerColumn: stream offset overflow",
            })?;
            compressed_streams.push(off..end);
            off = end;
        }
        if off > section.len() {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4PerColumn: stream bytes run past section end",
            });
        }

        let stream_data = (0..total_streams).map(|_| OnceLock::new()).collect();
        Ok(Arc::new(Self {
            header,
            descriptors,
            event_count,
            event_tags,
            presence,
            bytes_per_row,
            num_cols,
            stream_base,
            bank_offsets,
            stream_data,
            backing,
            compressed_streams,
            decompressed_sizes,
            col_layout: OnceLock::new(),
        }))
    }

    // ---- Metadata accessors (mirror ByBankRecord) ----------------------

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

    pub fn bank_index(&self, group: u16, item: u8) -> Option<u32> {
        self.descriptors
            .binary_search_by(|d| (d.group, d.item).cmp(&(group, item)))
            .ok()
            .map(|i| i as u32)
    }

    pub fn has(&self, event_idx: u32, bank_idx: u32) -> bool {
        let e = event_idx as usize;
        let b = bank_idx as usize;
        let byte = e * self.bytes_per_row + b / 8;
        (self.presence[byte] >> (b % 8) as u8) & 1 == 1
    }

    pub fn descriptor(&self, bank_idx: u32) -> (u16, u8, u8) {
        let d = &self.descriptors[bank_idx as usize];
        (d.group, d.item, d.data_type)
    }

    /// Decompressed byte size of event `event_idx`'s instance of bank
    /// `bank_idx` (the column-major bank data). O(1).
    pub fn bank_size(&self, event_idx: u32, bank_idx: u32) -> u32 {
        let o = &self.bank_offsets[bank_idx as usize];
        let e = event_idx as usize;
        o[e + 1] - o[e]
    }

    /// `true` if bank `bank_idx` is stored as a single opaque stream
    /// (no schema / composite / ragged rows) rather than split by column.
    pub fn is_opaque(&self, bank_idx: u32) -> bool {
        self.num_cols[bank_idx as usize] == 0
    }

    /// Number of column streams the writer stored for this bank (0 when
    /// opaque).
    pub fn num_columns(&self, bank_idx: u32) -> u16 {
        self.num_cols[bank_idx as usize]
    }

    /// Cumulative decompressed bytes of bank `bank_idx` before event
    /// `event_idx` (i.e. `sum` of earlier events' bank sizes). Combined
    /// with the schema's `row_size` this yields the row offset a columnar
    /// slice starts at.
    pub fn bank_byte_offset(&self, event_idx: u32, bank_idx: u32) -> u32 {
        self.bank_offsets[bank_idx as usize][event_idx as usize]
    }

    /// Byte range within the opaque stream of event `event_idx`'s bank
    /// data. Only meaningful for opaque banks.
    pub fn bank_byte_range(&self, event_idx: u32, bank_idx: u32) -> std::ops::Range<usize> {
        let o = &self.bank_offsets[bank_idx as usize];
        let e = event_idx as usize;
        o[e] as usize..o[e + 1] as usize
    }

    /// Borrow the decompressed stream for `(bank_idx, col_idx)`, inflating
    /// it on first touch. For opaque banks pass `col_idx == 0`.
    /// Lock-free after the first call; thread-safe.
    pub fn column_stream(&self, bank_idx: u32, col_idx: u16) -> Result<&[u8]> {
        let base = self.stream_base[bank_idx as usize];
        let per_bank = self.stream_base[bank_idx as usize + 1] - base;
        let c = col_idx as usize;
        if c >= per_bank {
            return Err(HipoError::CorruptRecord {
                offset: 0,
                reason: "Lz4PerColumn: column index out of range",
            });
        }
        let s = base + c;
        if let Some(data) = self.stream_data[s].get() {
            return Ok(data.as_ref());
        }
        let expected = self.decompressed_sizes[s] as usize;
        if expected == 0 {
            let _ = self.stream_data[s].set(Box::new([]));
        } else {
            #[cfg(test)]
            COLUMN_INFLATE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let src = &self.backing.section()[self.compressed_streams[s].clone()];
            let mut out: Vec<u8> = Vec::with_capacity(expected);
            decompress(CompressionType::Lz4, src, &mut out, expected)?;
            out.truncate(expected);
            let _ = self.stream_data[s].set(out.into_boxed_slice());
        }
        Ok(self.stream_data[s]
            .get()
            .expect("OnceLock::set succeeded above")
            .as_ref())
    }

    /// Per-bank column geometry, computed once via `build` and cached for
    /// the record's lifetime. `build` runs only on the first call — the
    /// caller supplies it (it needs the schema dictionary, which the wire
    /// layer doesn't hold). Lets whole-event reassembly resolve schemas
    /// once per record rather than once per event.
    pub fn column_layout<F: FnOnce() -> Vec<BankLayout>>(&self, build: F) -> &[BankLayout] {
        self.col_layout.get_or_init(build)
    }

    /// True if the stream for `(bank_idx, col_idx)` has already inflated.
    /// Used by tests to verify the partial-decompression contract.
    #[cfg(test)]
    pub fn is_column_inflated(&self, bank_idx: u32, col_idx: u16) -> bool {
        let s = self.stream_base[bank_idx as usize] + col_idx as usize;
        self.stream_data[s].get().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering::Relaxed;

    const ROW_SIZE: usize = 4 + 4 + 12 + 2; // pid/I + px/F + cov/F#3 + status/S = 22

    fn dict() -> crate::schema::Dict {
        use crate::schema::{DataType, Schema};
        let mut d = crate::schema::Dict::new();
        d.add(Schema::from_columns(
            "REC::Particle",
            300,
            1,
            [
                ("pid".into(), DataType::Int, 1),
                ("px".into(), DataType::Float, 1),
                ("cov".into(), DataType::Float, 3),
                ("status".into(), DataType::Short, 1),
            ],
        ));
        d
    }

    /// One event with a column-major `REC::Particle` bank of `rows` rows.
    fn build_particle_event(rows: u32, seed: u32) -> Vec<u8> {
        use crate::event::EventBuilder;
        use crate::wire::bytes::write_u32_le;
        use crate::wire::constants::BANK_STRUCTURE_SIZE;
        let data_len = rows as usize * ROW_SIZE;
        let mut s = vec![0u8; BANK_STRUCTURE_SIZE + data_len];
        s[0..2].copy_from_slice(&300u16.to_le_bytes());
        s[2] = 1; // item
        s[3] = 4; // wire type byte (reader interprets columns via the schema)
        write_u32_le(&mut s, 4, data_len as u32);
        let base = BANK_STRUCTURE_SIZE;
        let (pid_off, px_off) = (base, base + rows as usize * 4);
        let (cov_off, st_off) = (px_off + rows as usize * 4, px_off + rows as usize * 16);
        for r in 0..rows as usize {
            let pid = (seed * 100 + r as u32) as i32;
            s[pid_off + r * 4..pid_off + r * 4 + 4].copy_from_slice(&pid.to_le_bytes());
            let px = seed as f32 + r as f32;
            s[px_off + r * 4..px_off + r * 4 + 4].copy_from_slice(&px.to_le_bytes());
            for k in 0..3 {
                let v = (r * 3 + k) as f32;
                let o = cov_off + r * 12 + k * 4;
                s[o..o + 4].copy_from_slice(&v.to_le_bytes());
            }
            let st = (r as i16) - 1;
            s[st_off + r * 2..st_off + r * 2 + 2].copy_from_slice(&st.to_le_bytes());
        }
        let mut eb = EventBuilder::new();
        eb.add_bank_bytes(&s);
        eb.finish()
    }

    fn build(events: &[Vec<u8>]) -> Vec<u8> {
        let refs: Vec<&[u8]> = events.iter().map(|v| v.as_slice()).collect();
        let mut pb = Vec::new();
        let mut cb = Vec::new();
        crate::write::record::build_record_bytes(
            &refs,
            &dict(),
            0,
            0,
            crate::write::Compression::Lz4PerColumn,
            1,
            &mut pb,
            &mut cb,
        )
        .unwrap()
    }

    #[test]
    fn parses_without_decompressing_and_splits_columns() {
        let evs: Vec<Vec<u8>> = (0..10).map(|s| build_particle_event(3, s)).collect();
        let raw = build(&evs);
        let before = COLUMN_INFLATE_COUNTER.load(Relaxed);
        let rec = PerColumnRecord::parse(&raw).unwrap();
        assert_eq!(
            COLUMN_INFLATE_COUNTER.load(Relaxed),
            before,
            "parsing the directory must not inflate any column"
        );
        assert_eq!(rec.event_count(), 10);
        assert_eq!(rec.num_banks(), 1);
        let b = rec.bank_index(300, 1).unwrap();
        assert!(!rec.is_opaque(b));
        assert_eq!(rec.num_columns(b), 4);
    }

    #[test]
    fn touching_one_column_does_not_inflate_others() {
        let evs: Vec<Vec<u8>> = (0..8).map(|s| build_particle_event(3, s)).collect();
        let raw = build(&evs);
        let rec = PerColumnRecord::parse(&raw).unwrap();
        let b = rec.bank_index(300, 1).unwrap();

        let before = COLUMN_INFLATE_COUNTER.load(Relaxed);
        let px = rec.column_stream(b, 1).unwrap(); // read only the px column
        assert_eq!(
            COLUMN_INFLATE_COUNTER.load(Relaxed) - before,
            1,
            "reading one column must inflate exactly one stream"
        );
        assert!(rec.is_column_inflated(b, 1));
        assert!(!rec.is_column_inflated(b, 0), "pid must stay compressed");
        assert!(!rec.is_column_inflated(b, 2), "cov must stay compressed");
        assert!(!rec.is_column_inflated(b, 3), "status must stay compressed");

        // px column is cross-event contiguous: event s, row r -> s + r.
        let vals: &[f32] = bytemuck::cast_slice(px);
        assert_eq!(vals.len(), 8 * 3);
        assert_eq!(vals[0], 0.0); // event 0, row 0
        assert_eq!(vals[3], 1.0); // event 1, row 0
        assert_eq!(vals[3 * 5 + 2], 5.0 + 2.0); // event 5, row 2
    }

    #[test]
    fn empty_and_opaque_banks_round_trip() {
        // A bank with no schema is stored opaquely (num_cols == 0).
        use crate::event::EventBuilder;
        use crate::wire::bytes::write_u32_le;
        use crate::wire::constants::BANK_STRUCTURE_SIZE;
        let mut s = vec![0u8; BANK_STRUCTURE_SIZE + 5];
        s[0..2].copy_from_slice(&999u16.to_le_bytes()); // unknown (group, item)
        s[2] = 7;
        s[3] = 11;
        write_u32_le(&mut s, 4, 5);
        s[BANK_STRUCTURE_SIZE..].copy_from_slice(b"hello");
        let mut eb = EventBuilder::new();
        eb.add_bank_bytes(&s);
        let ev = eb.finish();
        let raw = build(&[ev]);
        let rec = PerColumnRecord::parse(&raw).unwrap();
        let b = rec.bank_index(999, 7).unwrap();
        assert!(rec.is_opaque(b), "schema-less bank must be opaque");
        let stream = rec.column_stream(b, 0).unwrap();
        assert_eq!(&stream[rec.bank_byte_range(0, b)], b"hello");
    }
}
