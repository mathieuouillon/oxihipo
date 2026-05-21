//! `FileInner` — shared, immutable state for an open HIPO file.
//!
//! Lives inside an `Arc` so multiple [`File`](super::File) clones and
//! iterators can share one mmap and one parsed dictionary.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use memmap2::{Advice, Mmap, MmapOptions};

use crate::error::{HipoError, Result};
use crate::event::Event;
use crate::schema::Dict;
use crate::wire::constants::*;
use crate::wire::event_index::FileEventIndex;
use crate::wire::file_header::FileHeader;
use crate::wire::record::Record;
use crate::wire::record_header::RecordHeader;

/// Read-only shared file state.
#[derive(Debug)]
pub(crate) struct FileInner {
    pub path: PathBuf,
    pub mmap: Mmap,
    pub file_header: FileHeader,
    /// Wrapped in `Arc` so iterators and `OwnedEvent`s share the dict
    /// without cloning it (which would clone each schema's name →
    /// index `HashMap`).
    pub dict: Arc<Dict>,
    /// Regular records only (no dictionary, no trailer).
    pub index: FileEventIndex,
}

impl FileInner {
    pub fn open(path: PathBuf) -> Result<Self> {
        Self::open_inner(path.clone()).map_err(|e| e.with_path(path))
    }

    fn open_inner(path: PathBuf) -> Result<Self> {
        let file = std::fs::File::open(&path)?;
        // SAFETY: memmap2's documented contract — the underlying file must
        // not be mutated by another process while mapped. HIPO files in
        // production are write-once; this is a deliberate trade-off.
        let mmap = unsafe { MmapOptions::new().map(&file)? };
        let _ = mmap.advise(Advice::Sequential);

        if mmap.len() < FILE_HEADER_SIZE {
            return Err(HipoError::FileTooSmall {
                actual: mmap.len() as u64,
                min: FILE_HEADER_SIZE as u64,
            });
        }
        let file_header = FileHeader::parse(&mmap[..FILE_HEADER_SIZE])?;

        let dict_record_offset = u64::from(file_header.header_length);
        let first_data_record_offset =
            dict_record_offset + u64::from(file_header.user_header_length);

        let dict = parse_dictionary(&mmap, dict_record_offset)?;

        // Build the data-record index. The trailer at `trailer_position`
        // (when present) lists every record including the dictionary; we
        // filter the dictionary out below. Fall back to a sequential scan
        // if the trailer can't be decoded.
        let index = if file_header.trailer_position != 0 {
            match build_index_from_trailer(&mmap, &file_header, first_data_record_offset) {
                Ok(idx) => idx,
                Err(_) => build_index_by_scanning(&mmap, first_data_record_offset)?,
            }
        } else {
            build_index_by_scanning(&mmap, first_data_record_offset)?
        };

        Ok(Self {
            path,
            mmap,
            file_header,
            dict: Arc::new(dict),
            index,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-advise the mmap for parallel, out-of-order record access.
    ///
    /// [`open`](Self::open) sets `MADV_SEQUENTIAL`, which suits the
    /// front-to-back `events()` walk but is wrong for
    /// `Chain::par_for_each` / `par_reduce`: under concurrent out-of-order
    /// record faults its evict-behind behavior makes workers drop pages
    /// other workers still need. This resets to `MADV_NORMAL` — default
    /// per-fault readahead, no evict-behind; each record is a contiguous
    /// range, so within-record readahead still helps.
    pub fn advise_parallel(&self) {
        let _ = self.mmap.advise(Advice::Normal);
    }
}

/// Read every dictionary event in the file's user-header record and add the
/// embedded schemas to a fresh `Dict`. Missing or unreadable dictionary
/// records are treated as "empty dict" — same tolerance as the C++ reader.
fn parse_dictionary(mmap: &Mmap, offset: u64) -> Result<Dict> {
    let mut dict = Dict::new();
    let off = offset as usize;
    if off + RECORD_HEADER_SIZE > mmap.len() {
        return Ok(dict);
    }
    let mut record = Record::new();
    if record.load(&mmap[off..]).is_err() {
        return Ok(dict);
    }
    for ev_idx in 0..record.event_count() {
        let Some(ev_buf) = record.event(ev_idx) else {
            continue;
        };
        let ev = Event::new(ev_buf);
        let Some((_, payload)) = ev.find(DICT_GROUP, DICT_ITEM) else {
            continue;
        };
        let text = parse_evio_string(payload);
        if let Ok(schema) = crate::schema::Schema::parse_text(text.trim()) {
            dict.add(schema);
        }
    }
    Ok(dict)
}

/// Decode the schema text out of a `(120, 2)` dictionary structure payload.
fn parse_evio_string(payload: &[u8]) -> &str {
    let end = payload
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(payload.len());
    std::str::from_utf8(&payload[..end]).unwrap_or("")
}

fn build_index_from_trailer(
    mmap: &Mmap,
    header: &FileHeader,
    first_data_record_offset: u64,
) -> Result<FileEventIndex> {
    let trailer_off = header.trailer_position as usize;
    if trailer_off + RECORD_HEADER_SIZE > mmap.len() {
        return Err(HipoError::CorruptRecord {
            offset: header.trailer_position,
            reason: "trailer offset past EOF",
        });
    }
    let mut trailer = Record::new();
    trailer.load(&mmap[trailer_off..])?;
    let Some(idx_event_buf) = trailer.event(0) else {
        return Err(HipoError::CorruptRecord {
            offset: header.trailer_position,
            reason: "trailer record has no event",
        });
    };
    let idx_event = Event::new(idx_event_buf);
    let Some((_, bank_data)) = idx_event.find(FILE_INDEX_GROUP, FILE_INDEX_ITEM) else {
        return Err(HipoError::CorruptRecord {
            offset: header.trailer_position,
            reason: "trailer event missing file::index bank",
        });
    };

    // file::index schema is fixed:
    // position/L, length/I, entries/I, userWordOne/L, userWordTwo/L (32 B/row).
    let row_size = 32;
    if bank_data.is_empty() || !bank_data.len().is_multiple_of(row_size) {
        return Err(HipoError::CorruptRecord {
            offset: header.trailer_position,
            reason: "trailer bank size is not a multiple of 32",
        });
    }
    let rows = (bank_data.len() / row_size) as u32;
    let pos_off = 0;
    let len_off = rows as usize * 8;
    let ent_off = rows as usize * 12;

    let mut idx = FileEventIndex::new();
    let trailer_pos = header.trailer_position;
    for r in 0..rows as usize {
        let pos = i64::from_le_bytes(
            bank_data[pos_off + r * 8..pos_off + r * 8 + 8]
                .try_into()
                .expect("8 bytes for i64"),
        ) as u64;
        let len = i32::from_le_bytes(
            bank_data[len_off + r * 4..len_off + r * 4 + 4]
                .try_into()
                .expect("4 bytes for i32"),
        ) as u64;
        let ent = i32::from_le_bytes(
            bank_data[ent_off + r * 4..ent_off + r * 4 + 4]
                .try_into()
                .expect("4 bytes for i32"),
        ) as u32;
        // Skip the dictionary record (lives in the file user header) and
        // the trailer itself (writer included it in its own index).
        if pos < first_data_record_offset || pos == trailer_pos {
            continue;
        }
        idx.push(pos, len, ent);
    }
    Ok(idx)
}

fn build_index_by_scanning(mmap: &Mmap, first_data_record_offset: u64) -> Result<FileEventIndex> {
    let mut idx = FileEventIndex::new();
    let mut off = first_data_record_offset;
    while (off as usize) + RECORD_HEADER_SIZE <= mmap.len() {
        let h = RecordHeader::parse(&mmap[off as usize..])?;
        let len = h.total_bytes();
        if h.event_count == 0 {
            break;
        }
        idx.push(off, len, h.event_count);
        off = off.checked_add(len).ok_or(HipoError::CorruptRecord {
            offset: off,
            reason: "record length overflows file offset",
        })?;
        if h.is_last_record() {
            break;
        }
    }
    Ok(idx)
}
