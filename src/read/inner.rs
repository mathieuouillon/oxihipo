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
use crate::schema::{Dict, Schema};
use crate::tag::TagRegistry;
use crate::wire::constants::*;
use crate::wire::event_index::FileEventIndex;
use crate::wire::file_header::FileHeader;
use crate::wire::record::Record;
use crate::wire::record_header::RecordHeader;

/// The byte source shared across the chain — see [`ReadAt`].
type SharedFile = Arc<dyn ReadAt>;

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
    /// Name↔bit tag registry parsed from the dictionary record — empty if
    /// the file carries none. Shared by `Arc` like `dict`.
    pub tag_registry: Arc<TagRegistry>,
    /// User key/value configuration from the dictionary record (the
    /// `(32555,…)` "run config" store), in file order. Empty if none.
    pub config: Arc<Vec<(String, String)>>,
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
        Self::from_source(Arc::new(LocalFile::new(file)), len, path)
    }

    /// Parse a HIPO file out of any [`ReadAt`] source: header, dictionary,
    /// record index. Split from [`Self::open_inner`] so obtaining the bytes and
    /// interpreting them are separate concerns — the whole point of the seam.
    fn from_source(shared: SharedFile, len: u64, path: PathBuf) -> Result<Self> {
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

        let (dict, tag_registry, config) = parse_dictionary(&shared, len, dict_record_offset)?;

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
            tag_registry: Arc::new(tag_registry),
            config: Arc::new(config),
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

fn read_at(file: &SharedFile, offset: u64, buf: &mut [u8]) -> Result<()> {
    file.read_exact_at(buf, offset)
}

/// A source of bytes at an offset — everything the read path asks of a file.
///
/// The whole read side goes through this one method, so a source that is not a
/// local file (an in-memory image, eventually HTTP range requests) only has to
/// implement it. Two design points are load-bearing:
///
/// - it fills a **caller-owned** `buf` rather than returning one, because
///   `tests/no_alloc.rs` pins the steady-state scan loop at zero allocations;
/// - there is no `len()`. The file length is captured once at open
///   ([`FileInner::len`]) and every bounds check reads that, so a source is
///   never asked its size on the hot path.
///
/// `Send + Sync` because the parallel readers hold `Arc<FileInner>` across rayon
/// workers, and PyO3's `frozen` pyclass requires `Chain: Sync`. `Debug` because
/// `FileInner` derives it.
pub(crate) trait ReadAt: std::fmt::Debug + Send + Sync {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<()>;
}

/// The local-file source: `pread` where it exists, and a private cursor behind
/// a mutex where it does not.
///
/// `pread` takes the offset as an argument and never touches the shared file
/// cursor, so many threads can read one descriptor at once. The fallback
/// serialises instead — the non-unix parallel path trades I/O concurrency for
/// portability.
#[derive(Debug)]
struct LocalFile {
    #[cfg(unix)]
    file: File,
    #[cfg(not(unix))]
    file: std::sync::Mutex<File>,
}

impl LocalFile {
    fn new(file: File) -> Self {
        #[cfg(unix)]
        return Self { file };
        #[cfg(not(unix))]
        return Self {
            file: std::sync::Mutex::new(file),
        };
    }
}

impl ReadAt for LocalFile {
    #[cfg(unix)]
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.read_exact_at(buf, offset).map_err(HipoError::Io)
    }

    #[cfg(not(unix))]
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = self.file.lock().expect("file handle mutex poisoned");
        f.seek(SeekFrom::Start(offset)).map_err(HipoError::Io)?;
        f.read_exact(buf).map_err(HipoError::Io)
    }
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

/// Read the file's user-header record: add every embedded schema to a fresh
/// `Dict`, and parse the tag-name registry if one is present (an extra
/// `(DICT_GROUP, TAG_REGISTRY_ITEM)` text bank). Missing or unreadable
/// records are treated as "empty" — same tolerance as the C++ reader.
#[allow(clippy::type_complexity)]
fn parse_dictionary(
    file: &SharedFile,
    file_len: u64,
    offset: u64,
) -> Result<(Dict, TagRegistry, Vec<(String, String)>)> {
    let mut dict = Dict::new();
    let mut tag_registry = TagRegistry::new();
    let mut config: Vec<(String, String)> = Vec::new();
    let mut buf = Vec::new();
    if read_record_into(file, file_len, offset, &mut buf).is_err() {
        return Ok((dict, tag_registry, config));
    }
    let mut record = Record::new();
    if record.load(&buf).is_err() {
        return Ok((dict, tag_registry, config));
    }
    for ev_idx in 0..record.event_count() {
        let Some(ev_buf) = record.event(ev_idx) else {
            continue;
        };
        let ev = Event::new(ev_buf);
        if let Some(schema) = parse_dict_schema(&ev) {
            // Cap the dictionary at u16::MAX schemas (an id is a u16). A file
            // with more is hostile/corrupt; stop adding rather than letting
            // `Dict::add` panic. Replacing an existing name is always allowed.
            if dict.get(schema.name()).is_some() || dict.len() < u16::MAX as usize {
                dict.add(schema);
            }
        } else if let Some((_, payload)) = ev.find(DICT_GROUP, TAG_REGISTRY_ITEM) {
            let parsed = TagRegistry::parse_text(parse_evio_string(payload).trim());
            if !parsed.is_empty() {
                tag_registry = parsed;
            }
        } else if let Some((_, key)) = ev.find(CONFIG_GROUP, CONFIG_KEY_ITEM) {
            // A user-config entry: key at (32555,1), value at (32555,2). Read
            // type-agnostically (the string payload bytes) and keep the last
            // value for a repeated key, matching the C++/Java readers' map.
            if let Some((_, value)) = ev.find(CONFIG_GROUP, CONFIG_STRING_ITEM) {
                let k = parse_evio_string(key).to_string();
                let v = parse_evio_string(value).to_string();
                if let Some(slot) = config.iter_mut().find(|(ek, _)| *ek == k) {
                    slot.1 = v;
                } else {
                    config.push((k, v));
                }
            }
        }
    }
    Ok((dict, tag_registry, config))
}

/// Extract a schema from one dictionary event, in either on-disk form.
///
/// A schema is carried as the compact text `{name/group/item}{cols}` at
/// `(120, 2)` or as JSON at `(120, 1)`. The Rust and C++ writers emit the
/// compact form (C++ and the Java writer additionally emit JSON); some
/// producers — notably the Java `hipo4` writer path — may store *only* the
/// JSON structure. Prefer the compact text when present and fall back to JSON,
/// so a JSON-only dictionary is no longer read as schema-less.
fn parse_dict_schema(ev: &Event) -> Option<Schema> {
    if let Some((_, payload)) = ev.find(DICT_GROUP, DICT_ITEM) {
        return Schema::parse_text(parse_evio_string(payload).trim()).ok();
    }
    if let Some((_, payload)) = ev.find(DICT_GROUP, DICT_JSON_ITEM) {
        return Schema::parse_json(parse_evio_string(payload).trim()).ok();
    }
    None
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
        // A zero-length record can't be advanced past — treat it as the end
        // rather than looping forever on a corrupt header.
        if len == 0 {
            break;
        }
        // An empty record is legal (e.g. a skim that kept nothing from a
        // batch); skip it and keep scanning. Breaking here silently truncated
        // every later record in a trailer-less file.
        if h.event_count > 0 {
            idx.push(off, len, h.event_count);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A whole file held in memory. Test-only: nothing ships an alternative
    /// source yet, and this exists to prove the seam is real — that the read
    /// path goes through [`ReadAt`] and not through `File` behind its back.
    #[derive(Debug)]
    struct InMemory(Vec<u8>);

    impl ReadAt for InMemory {
        fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
            let start = offset as usize;
            let end = start + buf.len();
            if end > self.0.len() {
                return Err(HipoError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "read past end of in-memory source",
                )));
            }
            buf.copy_from_slice(&self.0[start..end]);
            Ok(())
        }
    }

    /// Everything — file header, dictionary, record index, record payloads —
    /// must come back identically when the bytes arrive from something that is
    /// not a file. If any part of the read path still reached for `File`, this
    /// would not compile or would not match.
    #[test]
    fn the_read_path_works_over_a_non_file_source() {
        use crate::{Chain, DataType, Schema, Writer};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem.hipo");
        let mut d = Dict::new();
        d.add(Schema::from_columns(
            "REC::Particle",
            300,
            31,
            [("pid".into(), DataType::Int, 1)],
        ));
        let mut w = Writer::create(&path)
            .schemas(&d)
            .max_record_events(4)
            .build()
            .unwrap();
        for e in 0..20i32 {
            w.event(|ev| {
                ev.bank("REC::Particle", |b| {
                    b.row(|r| {
                        r.set("pid", e)?;
                        Ok(())
                    })?;
                    Ok(())
                })?;
                Ok(())
            })
            .unwrap();
        }
        w.finish().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let len = bytes.len() as u64;
        let from_memory =
            FileInner::from_source(Arc::new(InMemory(bytes)), len, PathBuf::from("mem")).unwrap();
        let from_disk = FileInner::open(path.clone()).unwrap();

        assert_eq!(from_memory.dict, from_disk.dict, "dictionary");
        assert_eq!(
            from_memory.index.total_events(),
            from_disk.index.total_events(),
            "event count"
        );
        assert_eq!(
            from_memory.index.records().len(),
            from_disk.index.records().len(),
            "record count"
        );

        // ...and record payloads decode byte-for-byte the same.
        let mut a = Vec::new();
        let mut b = Vec::new();
        for (ra, rb) in from_memory
            .index
            .records()
            .iter()
            .zip(from_disk.index.records())
        {
            from_memory
                .read_record_into(ra.file_offset, &mut a)
                .unwrap();
            from_disk.read_record_into(rb.file_offset, &mut b).unwrap();
            assert_eq!(a, b, "record at {}", ra.file_offset);
        }

        // Sanity: the on-disk chain really does hold what we wrote.
        let chain = Chain::open(&path).unwrap();
        assert_eq!(chain.event_count(), 20);
    }

    /// Build a one-structure dictionary event (same on-disk layout the writer
    /// emits): a 16-byte `EVNT` header, then group(2 LE) item(1) type(1)
    /// size(4 LE) and the text. `find` matches on group/item, so the type byte
    /// is irrelevant here.
    fn dict_event(item: u8, text: &str) -> Vec<u8> {
        let mut bank = Vec::new();
        bank.extend_from_slice(&DICT_GROUP.to_le_bytes());
        bank.push(item);
        bank.push(6);
        bank.extend_from_slice(&(text.len() as u32).to_le_bytes());
        bank.extend_from_slice(text.as_bytes());
        let mut event = vec![0u8; EVENT_HEADER_SIZE];
        event[0..4].copy_from_slice(b"EVNT");
        event.extend_from_slice(&bank);
        let total = event.len() as u32;
        event[EH_SIZE..EH_SIZE + 4].copy_from_slice(&total.to_le_bytes());
        event
    }

    // A schema stored ONLY as JSON at (120, 1) — no compact (120, 2) structure,
    // as some Java-written files carry it — must still be parsed. Before the
    // JSON fallback, parse_dict_schema saw no (120, 2) and returned None, so the
    // dictionary came back empty and every bank read back with zero rows.
    #[test]
    fn reads_json_only_dictionary_event() {
        let json = r#"{ "name": "REC::Particle", "group": 400, "item": 1,
            "entries": [ { "name": "pid", "type": "I" }, { "name": "px", "type": "F" },
                         { "name": "cov", "type": "F#5" } ] }"#;
        let ev_buf = dict_event(DICT_JSON_ITEM, json);
        let ev = Event::new(&ev_buf);
        let schema = parse_dict_schema(&ev).expect("JSON-only dictionary event must parse");
        assert_eq!(schema.name(), "REC::Particle");
        assert_eq!(schema.group(), 400);
        assert_eq!(schema.item(), 1);
        assert_eq!(schema.entries().len(), 3);
        assert_eq!(schema.entries()[2].length, 5); // the F#5 array column
    }

    // The compact text form at (120, 2) still parses, and wins when both are
    // present (it is checked first).
    #[test]
    fn reads_compact_text_dictionary_event() {
        let text = "{REC::Traj/100/1}{tid/I,cov/F#5,hit/S#3}";
        let ev_buf = dict_event(DICT_ITEM, text);
        let ev = Event::new(&ev_buf);
        let schema = parse_dict_schema(&ev).expect("compact dictionary event must parse");
        assert_eq!(schema.name(), "REC::Traj");
        assert_eq!(schema.entries().len(), 3);
    }
}
