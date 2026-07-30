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

    /// Open over a caller-supplied [`ReadAt`]. `label` is carried purely for
    /// diagnostics — it names the source in errors and in `Chain::files`, and
    /// is never opened.
    pub fn from_reader(src: Arc<dyn ReadAt>, len: u64, label: PathBuf) -> Result<Self> {
        Self::from_source(src, len, label.clone()).map_err(|e| e.with_path(label))
    }

    /// Open a file whose 56-byte header cannot be trusted, by finding the
    /// records themselves.
    ///
    /// The file header is pure bookkeeping — magic, version, counts, where the
    /// dictionary starts, where the trailer is — and every one of those can be
    /// re-derived from what follows it, because each record carries its own
    /// header and magic. So a file whose first 56 bytes are gone is not
    /// unreadable, it is only unopenable by the normal path, which parses that
    /// header before it does anything else.
    ///
    /// What this cannot recover is the **dictionary**, which lives in the record
    /// immediately after the header. If the damage reached it, the events are
    /// still there and still copyable, but their banks have no names or column
    /// types — those exist nowhere else in the file. Callers get an empty
    /// dictionary and should expect to supply one from a sibling file.
    pub fn open_salvage(path: PathBuf) -> Result<Self> {
        let file = File::open(&path)?;
        let len = file.metadata()?.len();
        Self::salvage_from_source(Arc::new(LocalFile::new(file)), len, path.clone())
            .map_err(|e| e.with_path(path))
    }

    fn salvage_from_source(shared: SharedFile, len: u64, path: PathBuf) -> Result<Self> {
        // Start the scan *after* an intact file header. The two header kinds
        // carry the same endian magic at the same offset (both
        // `..._MAGIC_NUMBER == 28`) and the file header's compression word
        // reads as a valid `None`, so a file header does parse as a record
        // header. What rejects it in practice is the length check in
        // `find_first_record`: word 0 of a file header is the ASCII `HIPO`
        // magic, which as a record length is far past EOF.
        //
        // So this is belt and braces, not the thing that makes salvage correct
        // — a mutation removing it changes no test. It is kept because relying
        // on `HIPO` being an implausible length is an accident of the layout,
        // and because starting at the dictionary is what the code means.
        let mut fh = [0u8; FILE_HEADER_SIZE];
        let scan_from = match read_at(&shared, 0, &mut fh) {
            Ok(()) => match FileHeader::parse(&fh) {
                Ok(h) => u64::from(h.header_length).max(FILE_HEADER_SIZE as u64),
                Err(_) => 0,
            },
            Err(_) => 0,
        };

        let first = find_first_record(&shared, len, scan_from).ok_or(HipoError::BadMagic {
            offset: 0,
            found: 0,
            expected: HEADER_MAGIC,
        })?;

        // The record at the front is the dictionary in a well-formed file. Try
        // it as one: if it yields schemas, the data starts after it; if not,
        // this file's dictionary is gone (or it never had one) and that first
        // record is data, so indexing must start there rather than skip it.
        let (dict, tag_registry, config) = parse_dictionary(&shared, len, first)?;
        let first_data = if dict.is_empty() {
            first
        } else {
            let mut hdr = [0u8; RECORD_HEADER_SIZE];
            read_at(&shared, first, &mut hdr)?;
            first + RecordHeader::parse(&hdr)?.total_bytes()
        };

        // No trailer position to skip: salvage does not trust the file header
        // (it may be the thing that is damaged), so it finds the trailer by what
        // it holds instead — see the `holds_file_index` check below.
        let mut index = build_index_by_scanning(&shared, len, first_data, 0, true)?;

        // Drop the trailer if the scan walked into it.
        //
        // The normal path never meets this: with a trailer present it indexes
        // *from* the trailer, and it only scans when there is none. Salvage
        // scans a file that usually still has one, and a trailer looks like an
        // ordinary one-event record — no header bit distinguishes it (measured:
        // every record header this crate writes has bits 8-11 of `bit_info`
        // clear, trailer and data record alike). So it is
        // identified by what it holds, the `file::index` bank, which is what
        // `build_index_from_trailer` looks for too. Left in, it added a
        // thirteenth "record" and one phantom event to a 120-event file.
        if index
            .records()
            .last()
            .is_some_and(|last| holds_file_index(&shared, len, last.file_offset))
        {
            index.pop_last();
        }

        // A header describing what was actually found. `trailer_position` is 0:
        // the trailer is not looked for here, and claiming one that may not
        // exist would send a later reader to a bogus offset.
        let file_header = FileHeader {
            file_number: 1,
            header_length: FILE_HEADER_SIZE as u32,
            record_count: index.records().len() as u32,
            index_array_length: 0,
            bit_info: 0x5000_0006,
            user_header_length: (first_data - first) as u32,
            user_register: 0,
            trailer_position: 0,
            user_int1: 0,
            user_int2: 0,
            endianness: crate::wire::bytes::Endianness::Little,
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
                Err(_) => build_index_by_scanning(
                    &shared,
                    len,
                    first_data_record_offset,
                    file_header.trailer_position,
                    false,
                )?,
            }
        } else {
            build_index_by_scanning(&shared, len, first_data_record_offset, 0, false)?
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
/// - there is no `len()`. The length is captured once at open and every bounds
///   check reads that, so a source is never asked its size on the hot path —
///   which is why [`Chain::open_with`](crate::Chain::open_with) takes it as an
///   argument.
///
/// `Send + Sync` because the parallel readers hold the source across rayon
/// workers, and PyO3's `frozen` pyclass requires `Chain: Sync`. `Debug` because
/// the reader state derives it.
///
/// # Implementing one
///
/// Everything above the seam — header parsing, the dictionary, the record
/// index, lazy per-record streaming, every rayon path — works unchanged over
/// whatever this returns. `read_exact_at` takes `&self`, so N workers already
/// issue N concurrent positioned reads against one source: an XRootD, S3 or
/// HTTP-range implementation gets that concurrency without oxihipo owning an
/// async runtime, a network stack or TLS.
///
/// ```no_run
/// use std::sync::Arc;
/// use oxihipo::{Chain, HipoError, ReadAt, Result};
///
/// #[derive(Debug)]
/// struct InMemory(Vec<u8>);
///
/// impl ReadAt for InMemory {
///     fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
///         let start = offset as usize;
///         let end = start + buf.len();
///         if end > self.0.len() {
///             return Err(HipoError::Io(std::io::Error::new(
///                 std::io::ErrorKind::UnexpectedEof,
///                 "read past end of in-memory source",
///             )));
///         }
///         buf.copy_from_slice(&self.0[start..end]);
///         Ok(())
///     }
/// }
///
/// # fn main() -> Result<()> {
/// let bytes = std::fs::read("run.hipo")?;
/// let len = bytes.len() as u64;
/// let chain = Chain::open_with(Arc::new(InMemory(bytes)), len, "memory://run")?;
/// println!("{} events", chain.event_count());
/// # Ok(()) }
/// ```
pub trait ReadAt: std::fmt::Debug + Send + Sync {
    /// Fill `buf` completely, starting at `offset`.
    ///
    /// A short read is an error, not a partial success: every caller above
    /// this treats the buffer as fully populated on `Ok`.
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

/// Walk the file record by record, building the event index.
///
/// `trailer_pos` is the file header's `trailer_position`, or 0 when there is
/// none to skip. `salvage` selects the recovery policy: a salvage scan works
/// around damage, a normal scan raises on it. The two are deliberately
/// different — the library's contract on the normal path is to report
/// corruption, not to quietly hand back a shorter file.
fn build_index_by_scanning(
    file: &SharedFile,
    file_len: u64,
    first_data_record_offset: u64,
    trailer_pos: u64,
    salvage: bool,
) -> Result<FileEventIndex> {
    let mut idx = FileEventIndex::new();
    let mut off = first_data_record_offset;
    let mut hdr = [0u8; RECORD_HEADER_SIZE];
    while off + RECORD_HEADER_SIZE as u64 <= file_len {
        read_at(file, off, &mut hdr)?;
        // A header that doesn't parse ends a normal scan with an error. Salvage
        // instead resynchronises to the next thing that looks like a record and
        // keeps going: one damaged header used to cost the *whole file*, this
        // path included — which defeats the point of having a salvage path at
        // all. Measured on a 12-event file with one corrupted magic word:
        // `open` and `open_salvage` both failed outright; salvage now recovers
        // the records on either side of the damage.
        let h = match RecordHeader::parse(&hdr) {
            Ok(h) => h,
            Err(e) => {
                if !salvage {
                    return Err(e);
                }
                // Strictly forward (`off + 4`), so this cannot spin: the search
                // walks the 4-byte grid and either lands past `off` or reports
                // that nothing else in the file parses.
                match find_first_record(file, file_len, off + 4) {
                    Some(next) => {
                        off = next;
                        continue;
                    }
                    None => break,
                }
            }
        };
        let len = h.total_bytes();
        // A zero-length record can't be advanced past — treat it as the end
        // rather than looping forever on a corrupt header.
        if len == 0 {
            break;
        }
        // A record claiming more bytes than the file has is where a killed
        // writer stopped.
        //
        // Only *salvage* stops here. On the normal path the entry is indexed
        // and the read fails loudly later, which is deliberate: truncation is
        // genuine corruption and the library's contract is to raise on it, not
        // to hand back a shorter file. Making this unconditional turned a
        // truncated file into one that opened quietly with fewer events — the
        // binding's `test_truncated_file_raises` caught it, which is exactly
        // the silent loss that test exists to prevent.
        if salvage && off + len > file_len {
            break;
        }
        // `event_count` is attacker-controlled and was taken on trust, so a
        // corrupt header propagated straight into `Chain::event_count()`:
        // flipping one record's count to 1,000,000 made a 12-event file report
        // 1,000,009 events, and the *first* `events()` item was an error. The
        // index array bounds the real count at four bytes per event, and that
        // is a header field — no decompression needed. Verified against real
        // data before relying on it: across 1,951 records of an 8.5 GB CLAS12
        // DST and a simulation file, `index_array_length` is exactly
        // `event_count * 4`, and C++ hipo4's own golden file matches.
        if u64::from(h.event_count) * 4 > u64::from(h.index_array_length) {
            if !salvage {
                return Err(HipoError::CorruptRecord {
                    offset: off,
                    reason: "record event_count exceeds what its index array can hold",
                });
            }
            // Salvage drops the record rather than the file. `len` is sane
            // (non-zero and within the file, checked above), so the walk can
            // still step over it to whatever follows.
            off = off.checked_add(len).ok_or(HipoError::CorruptRecord {
                offset: off,
                reason: "record length overflows file offset",
            })?;
            continue;
        }
        // An empty record is legal (e.g. a skim that kept nothing from a
        // batch); skip it and keep scanning. Breaking here silently truncated
        // every later record in a trailer-less file.
        //
        // The trailer is skipped by position. A trailer is an ordinary
        // one-event record carrying the `file::index` bank — no header bit
        // distinguishes it — so a scan that meets one indexes it as data and
        // invents an event. The normal path reaches this whenever the trailer
        // *exists but does not parse*, which the fallback below is for: on a
        // 12-event file with a corrupted trailer index, `event_count()` came
        // back as 13.
        if h.event_count > 0 && off != trailer_pos {
            idx.push(off, len, h.event_count);
        }
        off = off.checked_add(len).ok_or(HipoError::CorruptRecord {
            offset: off,
            reason: "record length overflows file offset",
        })?;
        // No early break on a "last record" flag. The accessor that provided
        // one read bit 8, which this crate's own constants name
        // `BITINFO_HAS_DICTIONARY_BIT` — so on a file whose writer sets that bit
        // on its dictionary record, the scan would have stopped at the
        // dictionary and reported an empty file. Nothing this crate writes sets
        // it (verified: every record header it emits has bits 8-11 clear, while
        // the *file* header sets 8 and 10), which is the only reason the bug was
        // inert rather than catastrophic.
        //
        // Termination does not need the flag: the loop advances by each
        // record's own length and is bounded by the file, which is what the
        // trailer-less scan has always actually relied on.
    }
    Ok(idx)
}

/// Whether the record at `offset` carries the trailer's `file::index` bank.
fn holds_file_index(file: &SharedFile, file_len: u64, offset: u64) -> bool {
    let mut buf = Vec::new();
    if read_record_into(file, file_len, offset, &mut buf).is_err() {
        return false;
    }
    let mut record = Record::new();
    if record.load(&buf).is_err() {
        return false;
    }
    record.event(0).is_some_and(|ev| {
        Event::new(ev)
            .find(FILE_INDEX_GROUP, FILE_INDEX_ITEM)
            .is_some()
    })
}

/// The offset of the first thing that parses as a record header.
///
/// Scans on the 4-byte word grid every HIPO structure sits on, from `from`.
/// `RecordHeader::parse` already rejects a bad magic and an unknown compression
/// code, so the remaining check is that the record claims a length that fits in
/// the file — which is what keeps a stray `0xc0da0100` inside compressed
/// payload from being mistaken for the start of a record.
///
/// `from` exists because a *file* header carries the same endian magic at the
/// same offset as a record header, so a scan starting at 0 on an intact file
/// matches the file header itself.
fn find_first_record(file: &SharedFile, file_len: u64, from: u64) -> Option<u64> {
    let mut hdr = [0u8; RECORD_HEADER_SIZE];
    let mut off = from;
    while off + RECORD_HEADER_SIZE as u64 <= file_len {
        if read_at(file, off, &mut hdr).is_ok()
            && let Ok(h) = RecordHeader::parse(&hdr)
        {
            let total = h.total_bytes();
            if total >= RECORD_HEADER_SIZE as u64 && off + total <= file_len {
                return Some(off);
            }
        }
        off += 4;
    }
    None
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
