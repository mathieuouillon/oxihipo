//! HIPO writer.
//!
//! Builder-style construction:
//!
//! ```no_run
//! use oxihipo::{Compression, Dict, Writer};
//!
//! # fn run(dict: &Dict) -> oxihipo::Result<()> {
//! let mut w = Writer::create("out.hipo")
//!     .schemas(dict)
//!     .compression(Compression::Lz4)
//!     .build()?;
//! w.event(|ev| {
//!     ev.bank("REC::Particle", |b| {
//!         b.row(|r| {
//!             r.set("pid", 11_i32)?;
//!             r.set("px", 0.5_f32)?;
//!             Ok(())
//!         })?;
//!         Ok(())
//!     })?;
//!     Ok(())
//! })?;
//! w.finish()?;
//! # Ok(()) }
//! ```
//!
//! For raw byte interop, [`Writer::append_raw`] accepts an already-serialised
//! event payload.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::compress::ScratchBuf;
use crate::error::{HipoError, Result};
use crate::event::OwnedEvent;
use crate::schema::{Dict, Schema};
use crate::wire::bytes::{Endianness, write_u32_le};
use crate::wire::constants::*;
use crate::wire::file_header::FileHeader;
use crate::wire::record_header::RecordHeader;
use crate::write::bank::BankWriter;
use crate::write::record::{Compression, RecordBuilder, build_record_bytes};

/// Knobs governing record sizing. Defaults match the C++ writer.
#[derive(Debug, Clone, Copy)]
pub struct WriterOptions {
    pub compression: Compression,
    pub max_record_events: u32,
    pub max_record_bytes: usize,
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self {
            compression: Compression::Lz4,
            max_record_events: 1_000_000,
            max_record_bytes: 8 * 1024 * 1024,
        }
    }
}

impl WriterOptions {
    pub fn with_compression(mut self, c: Compression) -> Self {
        self.compression = c;
        self
    }

    pub fn with_max_record_events(mut self, n: u32) -> Self {
        self.max_record_events = n;
        self
    }

    pub fn with_max_record_bytes(mut self, n: usize) -> Self {
        self.max_record_bytes = n;
        self
    }
}

/// Builder returned by [`Writer::create`].
#[derive(Debug)]
pub struct WriterBuilder {
    path: PathBuf,
    dict: Option<Dict>,
    options: WriterOptions,
}

impl WriterBuilder {
    pub fn schemas(mut self, dict: &Dict) -> Self {
        self.dict = Some(dict.clone());
        self
    }

    pub fn compression(mut self, c: Compression) -> Self {
        self.options.compression = c;
        self
    }

    pub fn max_record_events(mut self, n: u32) -> Self {
        self.options.max_record_events = n;
        self
    }

    pub fn max_record_bytes(mut self, n: usize) -> Self {
        self.options.max_record_bytes = n;
        self
    }

    pub fn options(mut self, o: WriterOptions) -> Self {
        self.options = o;
        self
    }

    pub fn build(self) -> Result<Writer> {
        let Self {
            path,
            dict,
            options,
        } = self;
        let dict = dict.unwrap_or_default();
        Writer::create_inner(path, dict, options)
    }
}

/// What a finished [`Writer`] wrote — returned by [`Writer::finish`].
#[derive(Debug, Clone, Copy)]
pub struct WriteSummary {
    /// Total events written across all records.
    pub events: u64,
    /// Number of data records written (excludes the file header and trailer).
    pub records: u64,
    /// Total bytes on disk, including the file header, dictionary, and
    /// trailer index.
    pub bytes: u64,
}

/// HIPO file writer.
///
/// Call [`Writer::finish`] when done — it consumes the writer, writes the
/// trailer index, and returns a [`WriteSummary`]. Dropping a writer without
/// finishing finalises best-effort but cannot report errors, so it warns
/// instead (loudly if that best-effort finalisation itself fails).
#[derive(Debug)]
pub struct Writer {
    path: PathBuf,
    file: BufWriter<File>,
    dict: Dict,
    opts: WriterOptions,
    /// Current data-record builder.
    builder: RecordBuilder,
    compress_buf: ScratchBuf,
    payload_buf: ScratchBuf,
    index_entries: Vec<IndexEntry>,
    file_header_offset: u64,
    cursor: u64,
    user_header_length: u32,
    finished: bool,
}

#[derive(Debug, Clone, Copy)]
struct IndexEntry {
    position: u64,
    record_length: u64,
    event_count: u32,
    user_word_1: u64,
    user_word_2: u64,
}

impl Writer {
    /// Begin building a writer for the file at `path`.
    pub fn create(path: impl AsRef<Path>) -> WriterBuilder {
        WriterBuilder {
            path: path.as_ref().to_path_buf(),
            dict: None,
            options: WriterOptions::default(),
        }
    }

    /// Shortcut: build directly from a `(path, dict, options)` triple.
    pub fn create_with(
        path: impl AsRef<Path>,
        dict: &Dict,
        options: WriterOptions,
    ) -> Result<Self> {
        Self::create(path).schemas(dict).options(options).build()
    }

    fn create_inner(path: PathBuf, dict: Dict, opts: WriterOptions) -> Result<Self> {
        let path_for_err = path.clone();
        Self::create_inner_impl(path, dict, opts).map_err(|e| e.with_path(path_for_err))
    }

    fn create_inner_impl(path: PathBuf, dict: Dict, opts: WriterOptions) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        let mut me = Self {
            path,
            file: BufWriter::new(file),
            dict,
            opts,
            builder: RecordBuilder::new(),
            compress_buf: ScratchBuf::new(),
            payload_buf: ScratchBuf::new(),
            index_entries: Vec::new(),
            file_header_offset: 0,
            cursor: 0,
            user_header_length: 0,
            finished: false,
        };
        me.write_file_header_and_dictionary()?;
        Ok(me)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn schemas(&self) -> &Dict {
        &self.dict
    }

    pub fn options(&self) -> &WriterOptions {
        &self.opts
    }

    /// Build one event via a closure. The closure receives an
    /// [`EventWriter`] it uses to attach banks; on return the assembled
    /// event bytes are appended to the writer.
    pub fn event<F>(&mut self, f: F) -> Result<()>
    where
        F: FnOnce(&mut EventWriter<'_>) -> Result<()>,
    {
        let mut ev = EventWriter::new(&self.dict);
        f(&mut ev)?;
        let bytes = ev.finish();
        self.append_raw(&bytes)
    }

    /// Append already-serialised event bytes. Auto-flushes the current
    /// record when it reaches the configured size or event-count limits.
    pub fn append_raw(&mut self, event_bytes: &[u8]) -> Result<()> {
        if self.builder.event_count() >= self.opts.max_record_events
            || (self.builder.event_count() > 0
                && self.builder.estimated_payload_size() + event_bytes.len()
                    > self.opts.max_record_bytes)
        {
            self.flush_record()?;
        }
        self.builder.add_event(event_bytes);
        Ok(())
    }

    /// Append an [`OwnedEvent`]'s bytes. The dict reference is ignored —
    /// the writer's own schema dictionary is the source of truth.
    pub fn append_owned(&mut self, event: &OwnedEvent) -> Result<()> {
        self.append_raw(event.bytes())
    }

    /// Flush the current record to disk and reset the builder.
    pub fn flush_record(&mut self) -> Result<()> {
        if self.builder.is_empty() {
            return Ok(());
        }
        let record_number = self.index_entries.len() as u32 + 1;
        let event_slices: Vec<&[u8]> = self.builder.event_slices().collect();
        let record_bytes = build_record_bytes(
            &event_slices,
            self.builder.user_word_1(),
            self.builder.user_word_2(),
            self.opts.compression,
            record_number,
            self.payload_buf.vec_mut(),
            self.compress_buf.vec_mut(),
        )?;
        self.write_record(record_bytes)?;
        self.builder.reset();
        Ok(())
    }

    /// Append a *prebuilt* HIPO record (header + payload) to the file.
    /// `RH_RECORD_NUMBER` is patched so records are numbered in file order.
    pub fn write_record(&mut self, mut bytes: Vec<u8>) -> Result<()> {
        if bytes.len() < RECORD_HEADER_SIZE {
            return Err(HipoError::CorruptRecord {
                offset: self.cursor,
                reason: "prebuilt record shorter than the record header",
            });
        }
        // Patch RH_RECORD_NUMBER so output records are numbered in file order.
        let record_number = self.index_entries.len() as u32 + 1;
        write_u32_le(&mut bytes, RH_RECORD_NUMBER, record_number);

        let header = RecordHeader::parse(&bytes[..RECORD_HEADER_SIZE])?;
        let position = self.cursor;
        let record_length = bytes.len() as u64;
        let entry = IndexEntry {
            position,
            record_length,
            event_count: header.event_count,
            user_word_1: header.user_word_1,
            user_word_2: header.user_word_2,
        };
        self.file.write_all(&bytes)?;
        self.cursor += record_length;
        self.index_entries.push(entry);
        Ok(())
    }

    pub fn set_user_word_1(&mut self, v: u64) {
        self.builder.set_user_word_1(v);
    }

    pub fn set_user_word_2(&mut self, v: u64) {
        self.builder.set_user_word_2(v);
    }

    /// Flush the last record, write the trailer, patch the file header with
    /// the trailer position, and return a [`WriteSummary`].
    ///
    /// Consumes the writer: appending after `finish` is a compile error, and
    /// the `?` here is the place finalisation errors surface. You **must**
    /// call this to produce a complete file — dropping a writer without
    /// finishing runs a *best-effort* finalisation that cannot report errors
    /// (see the [`Writer`] type docs).
    pub fn finish(mut self) -> Result<WriteSummary> {
        self.finalize()?;
        Ok(WriteSummary {
            events: self
                .index_entries
                .iter()
                .map(|e| e.event_count as u64)
                .sum(),
            records: self.index_entries.len() as u64,
            bytes: self.cursor,
        })
    }

    /// Idempotent finalisation, shared by [`Self::finish`] and `Drop`.
    fn finalize(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.flush_record()?;
        self.write_trailer()?;
        self.file.flush()?;
        self.finished = true;
        Ok(())
    }

    fn write_file_header_and_dictionary(&mut self) -> Result<()> {
        let dict_record = build_dictionary_record(&self.dict)?;
        self.user_header_length = dict_record.len() as u32;

        let mut bit_info: u32 = HIPO_VERSION;
        bit_info |= 1 << BITINFO_HAS_DICTIONARY_BIT;
        bit_info |= 5 << BITINFO_HEADER_TYPE_SHIFT;
        let header = FileHeader {
            file_number: 1,
            header_length: FILE_HEADER_SIZE as u32,
            record_count: 0,
            index_array_length: 0,
            bit_info,
            user_header_length: self.user_header_length,
            user_register: 0,
            trailer_position: 0,
            user_int1: 0,
            user_int2: 0,
            endianness: Endianness::Little,
        };
        let mut hdr_buf = [0u8; FILE_HEADER_SIZE];
        header.write(&mut hdr_buf);
        self.file.write_all(&hdr_buf)?;
        self.cursor = FILE_HEADER_SIZE as u64;

        self.file.write_all(&dict_record)?;
        self.cursor += dict_record.len() as u64;
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        let trailer_pos = self.cursor;
        let trailer_bytes = build_trailer_record(&self.index_entries)?;
        self.file.write_all(&trailer_bytes)?;
        self.cursor += trailer_bytes.len() as u64;

        self.file.flush()?;
        let inner = self.file.get_mut();

        inner.seek(SeekFrom::Start(
            self.file_header_offset + FH_TRAILER_POS as u64,
        ))?;
        inner.write_all(&trailer_pos.to_le_bytes())?;

        inner.seek(SeekFrom::Start(
            self.file_header_offset + FH_BIT_INFO as u64,
        ))?;
        let mut bi = [0u8; 4];
        inner.read_exact(&mut bi)?;
        let bi_v = u32::from_le_bytes(bi) | (1 << BITINFO_TRAILER_WITH_INDEX_BIT);
        inner.seek(SeekFrom::Start(
            self.file_header_offset + FH_BIT_INFO as u64,
        ))?;
        inner.write_all(&bi_v.to_le_bytes())?;

        inner.seek(SeekFrom::End(0))?;
        Ok(())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // Dropped without an explicit `finish()`. Finalise best-effort so the
        // file is still readable, but make noise: a `Drop` can't propagate the
        // error the way `finish()?` does, and a silent failure here yields a
        // truncated / index-less file. Never panic — the crate aborts on
        // panic, and a panic while unwinding would take the process down.
        match self.finalize() {
            Err(e) => eprintln!(
                "error: oxihipo::Writer for {} dropped without finish() and the \
                 best-effort finalize failed: {e}; the file may be truncated or \
                 missing its index.",
                self.path.display(),
            ),
            Ok(()) => {
                #[cfg(debug_assertions)]
                eprintln!(
                    "warning: oxihipo::Writer for {} dropped without finish(); recovered \
                     via best-effort finalize — call finish()? to surface errors.",
                    self.path.display(),
                );
            }
        }
    }
}

/// Build the dictionary record (one event per schema, payload = schema text).
fn build_dictionary_record(dict: &Dict) -> Result<Vec<u8>> {
    let mut events: Vec<Vec<u8>> = Vec::with_capacity(dict.len());
    for schema in dict.iter() {
        events.push(build_dictionary_event(schema));
    }
    let event_refs: Vec<&[u8]> = events.iter().map(|e| e.as_slice()).collect();
    let mut payload_buf = Vec::new();
    let mut compress_buf = Vec::new();
    build_record_bytes(
        &event_refs,
        0,
        0,
        Compression::None,
        0,
        &mut payload_buf,
        &mut compress_buf,
    )
}

fn build_dictionary_event(schema: &Schema) -> Vec<u8> {
    let text = schema.to_text();
    let mut bank = Vec::with_capacity(BANK_STRUCTURE_SIZE + text.len());
    bank.extend_from_slice(&DICT_GROUP.to_le_bytes());
    bank.push(DICT_ITEM);
    bank.push(11);
    bank.extend_from_slice(&(text.len() as u32).to_le_bytes());
    bank.extend_from_slice(text.as_bytes());
    let mut event = vec![0u8; EVENT_HEADER_SIZE];
    event[0..4].copy_from_slice(b"EVNT");
    event.extend_from_slice(&bank);
    let total = event.len() as u32;
    write_u32_le(&mut event, EH_SIZE, total);
    event
}

fn build_trailer_record(entries: &[IndexEntry]) -> Result<Vec<u8>> {
    let rows = entries.len();
    let row_size = 32;
    let mut bank_data = vec![0u8; rows * row_size];
    for (r, e) in entries.iter().enumerate() {
        bank_data[r * 8..r * 8 + 8].copy_from_slice(&(e.position as i64).to_le_bytes());
    }
    let len_off = rows * 8;
    for (r, e) in entries.iter().enumerate() {
        bank_data[len_off + r * 4..len_off + r * 4 + 4]
            .copy_from_slice(&(e.record_length as i32).to_le_bytes());
    }
    let ent_off = rows * 12;
    for (r, e) in entries.iter().enumerate() {
        bank_data[ent_off + r * 4..ent_off + r * 4 + 4]
            .copy_from_slice(&(e.event_count as i32).to_le_bytes());
    }
    let u1_off = rows * 16;
    for (r, e) in entries.iter().enumerate() {
        bank_data[u1_off + r * 8..u1_off + r * 8 + 8]
            .copy_from_slice(&(e.user_word_1 as i64).to_le_bytes());
    }
    let u2_off = rows * 24;
    for (r, e) in entries.iter().enumerate() {
        bank_data[u2_off + r * 8..u2_off + r * 8 + 8]
            .copy_from_slice(&(e.user_word_2 as i64).to_le_bytes());
    }

    let mut bank = Vec::with_capacity(BANK_STRUCTURE_SIZE + bank_data.len());
    bank.extend_from_slice(&FILE_INDEX_GROUP.to_le_bytes());
    bank.push(FILE_INDEX_ITEM);
    bank.push(11);
    bank.extend_from_slice(&(bank_data.len() as u32).to_le_bytes());
    bank.extend_from_slice(&bank_data);

    let mut event = vec![0u8; EVENT_HEADER_SIZE];
    event[0..4].copy_from_slice(b"EVNT");
    event.extend_from_slice(&bank);
    let total = event.len() as u32;
    write_u32_le(&mut event, EH_SIZE, total);

    let mut payload_buf = Vec::new();
    let mut compress_buf = Vec::new();
    build_record_bytes(
        &[event.as_slice()],
        0,
        0,
        Compression::None,
        0,
        &mut payload_buf,
        &mut compress_buf,
    )
}

// ---- EventWriter (closure target for Writer::event) ----------------------

use crate::event::{BankBuilder, EventBuilder};

/// Closure target for [`Writer::event`]. Provides `bank(name, |b| {...})`
/// for attaching banks to the event under construction.
#[derive(Debug)]
pub struct EventWriter<'a> {
    dict: &'a Dict,
    builder: EventBuilder,
}

impl<'a> EventWriter<'a> {
    fn new(dict: &'a Dict) -> Self {
        Self {
            dict,
            builder: EventBuilder::new(),
        }
    }

    pub fn with_tag(&mut self, tag: u32) -> &mut Self {
        self.builder.set_tag(tag);
        self
    }

    pub fn tag(&self) -> u32 {
        self.builder.tag()
    }

    /// Attach a bank built by `f`. Looks up the schema in the writer's
    /// dictionary; returns [`HipoError::UnknownSchema`] if missing.
    pub fn bank<F>(&mut self, name: &str, f: F) -> Result<&mut Self>
    where
        F: for<'b> FnOnce(&mut BankWriter<'b, 'a>) -> Result<()>,
    {
        let schema = self.dict.require(name)?;
        let mut bw = BankWriter::new(BankBuilder::new(schema));
        f(&mut bw)?;
        let bytes = bw.into_inner().finish();
        self.builder.add_bank_bytes(&bytes);
        Ok(self)
    }

    /// Append a pre-serialised structure (e.g. forwarded from another
    /// event).
    pub fn add_bank_bytes(&mut self, bytes: &[u8]) -> &mut Self {
        self.builder.add_bank_bytes(bytes);
        self
    }

    fn finish(self) -> Vec<u8> {
        self.builder.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{DataType, Schema};

    fn sample_dict() -> Dict {
        let mut d = Dict::new();
        d.add(Schema::from_columns(
            "REC::Particle",
            300,
            1,
            [
                ("pid".into(), DataType::Int),
                ("px".into(), DataType::Float),
            ],
        ));
        d
    }

    #[test]
    fn builder_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.hipo");
        {
            let mut w = Writer::create(&path)
                .schemas(&sample_dict())
                .compression(Compression::None)
                .build()
                .unwrap();
            for pid in 1..=5 {
                w.event(|ev| {
                    ev.bank("REC::Particle", |b| {
                        b.row(|r| {
                            r.set("pid", pid)?;
                            r.set("px", pid as f32 * 0.1)?;
                            Ok(())
                        })?;
                        Ok(())
                    })?;
                    Ok(())
                })
                .unwrap();
            }
            w.finish().unwrap();
        }

        let f = crate::read::Chain::open(&path).unwrap();
        assert_eq!(f.event_count(), 5);
        assert!(f.schemas().get("REC::Particle").is_some());
    }

    #[test]
    fn unknown_schema_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.hipo");
        let mut w = Writer::create(&path)
            .schemas(&sample_dict())
            .build()
            .unwrap();
        let err = w
            .event(|ev| {
                ev.bank("NOPE", |_b| Ok(()))?;
                Ok(())
            })
            .unwrap_err();
        assert!(matches!(err, HipoError::UnknownSchema { .. }));
    }

    #[test]
    fn finish_returns_summary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.hipo");
        let mut w = Writer::create(&path)
            .schemas(&sample_dict())
            .compression(Compression::None)
            .build()
            .unwrap();
        for pid in 1..=5 {
            w.event(|ev| {
                ev.bank("REC::Particle", |b| {
                    b.row(|r| {
                        r.set("pid", pid)?;
                        r.set("px", pid as f32 * 0.1)?;
                        Ok(())
                    })?;
                    Ok(())
                })?;
                Ok(())
            })
            .unwrap();
        }
        let summary = w.finish().unwrap();
        assert_eq!(summary.events, 5);
        assert_eq!(summary.records, 1);
        assert!(summary.bytes > 0);
    }

    #[test]
    fn drop_without_finish_is_still_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.hipo");
        {
            let mut w = Writer::create(&path)
                .schemas(&sample_dict())
                .compression(Compression::None)
                .build()
                .unwrap();
            w.event(|ev| {
                ev.bank("REC::Particle", |b| {
                    b.row(|r| {
                        r.set("pid", 7)?;
                        r.set("px", 0.7_f32)?;
                        Ok(())
                    })?;
                    Ok(())
                })?;
                Ok(())
            })
            .unwrap();
            // Intentionally no `finish()` — the best-effort `Drop` must still
            // produce a readable file (it also prints a warning to stderr).
        }
        let f = crate::read::Chain::open(&path).unwrap();
        assert_eq!(f.event_count(), 1);
    }
}
