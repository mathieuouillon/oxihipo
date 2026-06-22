//! `FileInner` — shared, immutable state for an open HIPO file.
//!
//! Lives inside an `Arc` so multiple [`Chain`](super::Chain) clones and
//! iterators can share one file handle and one parsed dictionary.
//!
//! # Memory model
//!
//! The file is **never** mapped or read whole. Open parses only the file
//! header, the dictionary record, and the trailer index (all small, via
//! positioned reads). Record payloads are streamed in on demand — one
//! record at a time, into a recycled buffer — so scanning a 10–100 GB file
//! costs O(one record) of resident memory, not O(file). Random access
//! ([`Chain::event`](super::Chain::event)) and the parallel reader read the
//! same way.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{HipoError, Result};
use crate::event::Event;
use crate::schema::Dict;
use crate::wire::constants::*;
use crate::wire::event_index::FileEventIndex;
use crate::wire::file_header::FileHeader;
use crate::wire::record::Record;
use crate::wire::record_header::RecordHeader;

/// A positioned-read file handle shared across the chain.
///
/// On Unix we use `pread`, which is safe to issue concurrently from many
/// threads against one descriptor (it takes the offset as an argument and
/// never touches the shared file cursor), so the parallel reader needs no
/// per-thread handles. Elsewhere we serialise behind a `Mutex` — the
/// non-Unix parallel path trades I/O concurrency for portability.
#[cfg(unix)]
type SharedFile = Arc<File>;
#[cfg(not(unix))]
type SharedFile = Arc<std::sync::Mutex<File>>;

/// Read-only shared file state.
#[derive(Debug)]
pub(crate) struct FileInner {
    pub path: PathBuf,
    /// Positioned-read handle. Records are streamed in on demand via
    /// [`Self::read_exact_at`]; the whole file is never mapped.
    file: SharedFile,
    /// Total file length in bytes, for bounds checks.
    len: u64,
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
        let file = File::open(&path)?;
        let len = file.metadata()?.len();
        #[cfg(unix)]
        let shared: SharedFile = Arc::new(file);
        #[cfg(not(unix))]
        let shared: SharedFile = Arc::new(std::sync::Mutex::new(file));

        if len < FILE_HEADER_SIZE as u64 {
            return Err(HipoError::FileTooSmall {
                actual: len,
                min: FILE_HEADER_SIZE as u64,
            });
        }
        let mut hdr = [0u8; FILE_HEADER_SIZE];
        read_at(&shared, 0, &mut hdr)?;
        let file_header = FileHeader::parse(&hdr)?;

        let dict_record_offset = u64::from(file_header.header_length);
        let first_data_record_offset =
            dict_record_offset + u64::from(file_header.user_header_length);

        let dict = parse_dictionary(&shared, len, dict_record_offset)?;

        // Build the data-record index. The trailer at `trailer_position`
        // (when present) lists every record including the dictionary; we
        // filter the dictionary out below. Fall back to a sequential scan
        // if the trailer can't be decoded.
        let index = if file_header.trailer_position != 0 {
            match build_index_from_trailer(&shared, len, &file_header, first_data_record_offset) {
                Ok(idx) => idx,
                Err(_) => build_index_by_scanning(&shared, len, first_data_record_offset)?,
            }
        } else {
            build_index_by_scanning(&shared, len, first_data_record_offset)?
        };

        Ok(Self {
            path,
            file: shared,
            len,
            file_header,
            dict: Arc::new(dict),
            index,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stream a whole record (header + payload + index + padding) at
    /// `offset` into `buf`, resizing and reusing it across calls. Returns
    /// the parsed record header. Bounds-checks against the file length so a
    /// corrupt span can't read past EOF.
    pub(crate) fn read_record_into(&self, offset: u64, buf: &mut Vec<u8>) -> Result<RecordHeader> {
        read_record_into(&self.file, self.len, offset, buf)
    }

    /// Parse just a record header at `offset` (a small positioned read) —
    /// for cheap header peeks (record-tag pushdown) that must not pull the
    /// whole record into memory.
    pub(crate) fn read_record_header(&self, offset: u64) -> Result<RecordHeader> {
        if offset + RECORD_HEADER_SIZE as u64 > self.len {
            return Err(HipoError::CorruptRecord {
                offset,
                reason: "record header past EOF",
            });
        }
        let mut hdr = [0u8; RECORD_HEADER_SIZE];
        read_at(&self.file, offset, &mut hdr)?;
        RecordHeader::parse(&hdr)
    }
}

#[cfg(unix)]
fn read_at(file: &SharedFile, offset: u64, buf: &mut [u8]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset).map_err(HipoError::Io)
}

#[cfg(not(unix))]
fn read_at(file: &SharedFile, offset: u64, buf: &mut [u8]) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = file.lock().expect("file handle mutex poisoned");
    f.seek(SeekFrom::Start(offset)).map_err(HipoError::Io)?;
    f.read_exact(buf).map_err(HipoError::Io)
}

/// Read a whole record at `offset` into `buf` (resized + reused). Returns
/// the parsed header. Errors on a span that runs past EOF.
fn read_record_into(
    file: &SharedFile,
    file_len: u64,
    offset: u64,
    buf: &mut Vec<u8>,
) -> Result<RecordHeader> {
    if offset + RECORD_HEADER_SIZE as u64 > file_len {
        return Err(HipoError::CorruptRecord {
            offset,
            reason: "record header past EOF",
        });
    }
    let mut hdr = [0u8; RECORD_HEADER_SIZE];
    read_at(file, offset, &mut hdr)?;
    let header = RecordHeader::parse(&hdr)?;
    let total = header.total_bytes();
    if offset.checked_add(total).is_none_or(|end| end > file_len) {
        return Err(HipoError::CorruptRecord {
            offset,
            reason: "record extends past EOF",
        });
    }
    buf.resize(total as usize, 0);
    read_at(file, offset, buf)?;
    Ok(header)
}

/// Read every dictionary event in the file's user-header record and add the
/// embedded schemas to a fresh `Dict`. Missing or unreadable dictionary
/// records are treated as "empty dict" — same tolerance as the C++ reader.
fn parse_dictionary(file: &SharedFile, file_len: u64, offset: u64) -> Result<Dict> {
    let mut dict = Dict::new();
    let mut buf = Vec::new();
    if read_record_into(file, file_len, offset, &mut buf).is_err() {
        return Ok(dict);
    }
    let mut record = Record::new();
    if record.load(&buf).is_err() {
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
    file: &SharedFile,
    file_len: u64,
    header: &FileHeader,
    first_data_record_offset: u64,
) -> Result<FileEventIndex> {
    let mut buf = Vec::new();
    read_record_into(file, file_len, header.trailer_position, &mut buf)?;
    let mut trailer = Record::new();
    trailer.load(&buf)?;
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
    // An empty index bank is valid — it means zero data records (e.g. a skim
    // that kept nothing). Decoding it yields an empty index rather than
    // falling back to a scan that would misread the trailer as a data record.
    // Only a non-multiple-of-32 size is genuine corruption.
    if !bank_data.len().is_multiple_of(row_size) {
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
        );
        let len = i32::from_le_bytes(
            bank_data[len_off + r * 4..len_off + r * 4 + 4]
                .try_into()
                .expect("4 bytes for i32"),
        );
        let ent = i32::from_le_bytes(
            bank_data[ent_off + r * 4..ent_off + r * 4 + 4]
                .try_into()
                .expect("4 bytes for i32"),
        );
        // Reject negative position/length/entries — these are file-controlled
        // and a negative value would wrap to a huge `u64`/`u32` offset used to
        // index later. On any bad row, bail so the caller falls back to the
        // trustworthy sequential scan.
        if pos < 0 || len < 0 || ent < 0 {
            return Err(HipoError::CorruptRecord {
                offset: trailer_pos,
                reason: "trailer index row has a negative field",
            });
        }
        let pos = pos as u64;
        let len = len as u64;
        let ent = ent as u32;
        // Skip the dictionary record (lives in the file user header) and
        // the trailer itself (writer included it in its own index).
        if pos < first_data_record_offset || pos == trailer_pos {
            continue;
        }
        // A record that starts past EOF or extends past it is corruption;
        // fall back to scanning rather than indexing out of bounds later.
        if pos > file_len || pos.checked_add(len).is_none_or(|end| end > file_len) {
            return Err(HipoError::CorruptRecord {
                offset: trailer_pos,
                reason: "trailer index row position/length out of file bounds",
            });
        }
        idx.push(pos, len, ent);
    }
    Ok(idx)
}

fn build_index_by_scanning(
    file: &SharedFile,
    file_len: u64,
    first_data_record_offset: u64,
) -> Result<FileEventIndex> {
    let mut idx = FileEventIndex::new();
    let mut off = first_data_record_offset;
    let mut hdr = [0u8; RECORD_HEADER_SIZE];
    while off + RECORD_HEADER_SIZE as u64 <= file_len {
        read_at(file, off, &mut hdr)?;
        let h = RecordHeader::parse(&hdr)?;
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
