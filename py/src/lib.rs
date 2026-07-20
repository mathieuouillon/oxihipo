//! `_oxihipo` — the compiled Python extension. Exposes a `Chain` reader whose
//! heavy work (the columnar materializer) runs in Rust with the GIL released;
//! the Pythonic `array`/`arrays`/`numpy` ergonomics and Awkward assembly live
//! in the pure-Python `oxihipo` package layered on top of `read_columns`.

use numpy::{IntoPyArray, PyArray1, PyReadonlyArray1};
use oxihipo::event::{BankBuilder, EventBuilder};
use oxihipo::{Chain, ChainEventIter, ColumnData, Compression, DataType, Dict, Schema, Writer};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::ops::Range;

mod error;
use error::to_pyerr;

/// Per-bank result of [`PyChain::read_columns`]: the bank name, its shared
/// `int64` offsets, and one `(name, values ndarray, inner_len)` per column.
type BankColumns<'py> = (
    String,
    Bound<'py, PyArray1<i64>>,
    Vec<(String, Bound<'py, PyAny>, u32)>,
);

/// One `PyWriter::extend` call's banks: `[(bank, offsets_i64, [(col, values)])]`.
type ExtendBanks<'py> = Vec<(
    String,
    PyReadonlyArray1<'py, i64>,
    Vec<(String, Bound<'py, PyAny>)>,
)>;

/// Move a materialized column into a NumPy array (zero-copy: NumPy takes over
/// the Rust `Vec`'s allocation and frees it through Rust when collected).
fn column_data_to_py<'py>(py: Python<'py>, data: ColumnData) -> Bound<'py, PyAny> {
    match data {
        ColumnData::I8(v) => v.into_pyarray(py).into_any(),
        ColumnData::I16(v) => v.into_pyarray(py).into_any(),
        ColumnData::I32(v) => v.into_pyarray(py).into_any(),
        ColumnData::I64(v) => v.into_pyarray(py).into_any(),
        ColumnData::F32(v) => v.into_pyarray(py).into_any(),
        ColumnData::F64(v) => v.into_pyarray(py).into_any(),
    }
}

/// NumPy dtype name for a schema column (array columns get a `[N]` suffix,
/// e.g. `"float32[3]"`) — used by `typenames()`.
fn dtype_str(ty: DataType, length: u32) -> String {
    let base = match ty {
        DataType::Byte => "int8",
        DataType::Short => "int16",
        DataType::Int => "int32",
        DataType::Long => "int64",
        DataType::Float => "float32",
        DataType::Double => "float64",
    };
    if length > 1 {
        format!("{base}[{length}]")
    } else {
        base.to_string()
    }
}

fn mk_range(start: Option<u64>, stop: Option<u64>) -> Option<Range<u64>> {
    match (start, stop) {
        (None, None) => None,
        (s, e) => Some(s.unwrap_or(0)..e.unwrap_or(u64::MAX)),
    }
}

/// The reader. Immutable after construction (`frozen` ⇒ `Sync`), so `&PyChain`
/// is safely shared across threads while the GIL is released.
#[pyclass(name = "Chain", module = "oxihipo", frozen)]
struct PyChain {
    inner: oxihipo::Chain,
}

#[pymethods]
impl PyChain {
    /// Open a file, directory, glob, or list of paths.
    #[new]
    fn new(py: Python<'_>, source: Bound<'_, PyAny>) -> PyResult<Self> {
        // A SINGLE path auto-detects (file → itself; dir → its *.hipo; else a
        // glob), but an explicit LIST is taken verbatim — so the two must reach
        // different `Chain::open` overloads. `str` is checked before the
        // sequence branches (a `str` is itself an iterable of 1-char strings).
        enum Source {
            One(String),
            Many(Vec<String>),
        }
        let src = if let Ok(s) = source.extract::<String>() {
            Source::One(s)
        } else if let Ok(list) = source.extract::<Vec<String>>() {
            Source::Many(list)
        } else if let Ok(it) = source.try_iter() {
            // A sequence of os.PathLike → fsdecode each via str().
            let mut v = Vec::new();
            for item in it {
                v.push(item?.str()?.extract::<String>()?);
            }
            Source::Many(v)
        } else {
            // A single os.PathLike.
            Source::One(source.str()?.extract::<String>()?)
        };
        // Blocking I/O (open + header + dictionary + trailer) with GIL released.
        let inner = py
            .detach(|| match src {
                Source::One(s) => oxihipo::Chain::open(s),
                Source::Many(v) => oxihipo::Chain::open(v),
            })
            .map_err(to_pyerr)?;
        Ok(Self { inner })
    }

    /// Total number of events across the chain.
    #[getter]
    fn num_entries(&self) -> u64 {
        self.inner.event_count()
    }

    fn __len__(&self) -> PyResult<usize> {
        usize::try_from(self.inner.event_count())
            .map_err(|_| pyo3::exceptions::PyOverflowError::new_err("event count exceeds usize"))
    }

    /// Number of files in the chain.
    #[getter]
    fn file_count(&self) -> usize {
        self.inner.file_count()
    }

    /// The chain's file paths, in order.
    #[getter]
    fn files(&self) -> Vec<String> {
        self.inner
            .files()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }

    /// `bank in chain` — is a bank present in the dictionary?
    fn __contains__(&self, bank: &str) -> bool {
        self.inner.schemas().get(bank).is_some()
    }

    /// Bank names (`recursive=False`) or `"bank/column"` keys
    /// (`recursive=True`), like uproot's `keys()`.
    #[pyo3(signature = (recursive=false))]
    fn keys(&self, recursive: bool) -> Vec<String> {
        let dict = self.inner.schemas();
        if recursive {
            dict.iter()
                .flat_map(|s| {
                    let bank = s.name().to_string();
                    s.entries()
                        .iter()
                        .map(move |e| format!("{bank}/{}", e.name))
                })
                .collect()
        } else {
            dict.iter().map(|s| s.name().to_string()).collect()
        }
    }

    /// Column names of one bank. Raises `KeyError` if the bank is unknown.
    fn columns(&self, bank: &str) -> PyResult<Vec<String>> {
        let schema = self.inner.schemas().require(bank).map_err(to_pyerr)?;
        Ok(schema.entries().iter().map(|e| e.name.clone()).collect())
    }

    /// `{"bank/column": "dtype"}` for every column (array columns get a
    /// `[N]` suffix).
    fn typenames<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let out = PyDict::new(py);
        for s in self.inner.schemas().iter() {
            for e in s.entries() {
                out.set_item(
                    format!("{}/{}", s.name(), e.name),
                    dtype_str(e.ty, e.length),
                )?;
            }
        }
        Ok(out)
    }

    /// The columnar workhorse. `selection` is a list of `(bank, [columns])`
    /// (empty `columns` = all columns of that bank). Returns, per bank, its
    /// name, `int64` offsets, and `(name, values, inner_len)` per column.
    /// Runs the whole per-event loop in Rust with the GIL released; the
    /// Python layer assembles Awkward from the returned NumPy buffers.
    #[pyo3(signature = (selection, entry_start=None, entry_stop=None, threads=0))]
    fn read_columns<'py>(
        &self,
        py: Python<'py>,
        selection: Vec<(String, Vec<String>)>,
        entry_start: Option<u64>,
        entry_stop: Option<u64>,
        threads: usize,
    ) -> PyResult<Vec<BankColumns<'py>>> {
        // Borrow the owned strings as the &str/&[&str] the core API wants.
        let cols: Vec<Vec<&str>> = selection
            .iter()
            .map(|(_, cs)| cs.iter().map(String::as_str).collect())
            .collect();
        let sel: Vec<(&str, &[&str])> = selection
            .iter()
            .zip(&cols)
            .map(|((b, _), cs)| (b.as_str(), cs.as_slice()))
            .collect();
        let range = mk_range(entry_start, entry_stop);

        // Heavy work off the GIL — the closure captures no Python handle.
        let bufs = py
            .detach(|| self.inner.read_columns(&sel, range, threads))
            .map_err(to_pyerr)?;

        // Re-acquired GIL: move each owned buffer into NumPy (zero-copy).
        Ok(bufs
            .into_iter()
            .map(|b| {
                let offsets = b.offsets.into_pyarray(py);
                let columns = b
                    .columns
                    .into_iter()
                    .map(|c| (c.name, column_data_to_py(py, c.data), c.inner_len))
                    .collect();
                (b.bank, offsets, columns)
            })
            .collect())
    }

    /// Per-event tag column (`EH_TAG`): one `uint32` per surviving event in
    /// global event order, aligned 1:1 with `read_columns` / `arrays` over the
    /// same filter and range. Read from the event header or record directory —
    /// no bank is inflated.
    #[pyo3(signature = (entry_start=None, entry_stop=None, threads=0))]
    fn event_tags<'py>(
        &self,
        py: Python<'py>,
        entry_start: Option<u64>,
        entry_stop: Option<u64>,
        threads: usize,
    ) -> PyResult<Bound<'py, PyArray1<u32>>> {
        let range = mk_range(entry_start, entry_stop);
        let tags = py
            .detach(|| self.inner.event_tags(range, threads))
            .map_err(to_pyerr)?;
        Ok(tags.into_pyarray(py))
    }

    /// The file's persisted tag registry as ordered `(name, bit position)`
    /// pairs — empty if the file carries none. The Python wrapper surfaces it
    /// as the `Chain.tag_names` dict and resolves `filtered(event_tag="dvcs")`
    /// through it.
    fn tag_names(&self) -> Vec<(String, u32)> {
        self.inner
            .tag_registry()
            .iter()
            .map(|(name, bit)| (name.to_string(), bit))
            .collect()
    }

    /// A new `Chain` restricted to events carrying every bank in `require`,
    /// whose record tag is in `record_tag`, and whose per-event tag is in
    /// `event_tag` (or overlaps the `event_tag_any` bitmask). Cheap — clones
    /// the shared file handles, does not reopen. `KeyError` if a required
    /// bank isn't in the dictionary.
    #[pyo3(signature = (require=None, record_tag=None, event_tag=None, event_tag_any=None))]
    fn filtered(
        &self,
        require: Option<Vec<String>>,
        record_tag: Option<Vec<u64>>,
        event_tag: Option<Vec<u32>>,
        event_tag_any: Option<u32>,
    ) -> PyResult<PyChain> {
        let mut filter = oxihipo::Filter::new();
        if let Some(names) = require {
            filter = filter.and_require(names);
        }
        if let Some(tags) = record_tag {
            filter = filter.record_tag(tags);
        }
        if let Some(tags) = event_tag {
            filter = filter.event_tag(tags);
        }
        if let Some(mask) = event_tag_any {
            filter = filter.event_tag_any(mask);
        }
        let inner = self.inner.clone().with_filter(filter).map_err(to_pyerr)?;
        Ok(PyChain { inner })
    }

    /// Copy the (filtered) chain to `dst`, re-compressing with `compression`
    /// (`"none"`, `"lz4"`, `"lz4best"`, `"gzip"`, `"lz4perbank"`,
    /// `"lz4percolumn"`). Returns `{"events", "records", "bytes"}`.
    ///
    /// With `tags` (a `uint32` array aligned 1:1 with the events this chain
    /// yields — same order/length as `event_tags()` / `arrays()`), each event's
    /// per-event tag is **overwritten** with the corresponding value, producing
    /// a tagged DST; `tag_names` (a `[(name, bit)]` list) records the output's
    /// tag registry so the DST is self-describing. A length mismatch raises
    /// `ValueError`.
    #[pyo3(signature = (dst, compression="lz4percolumn", tags=None, tag_names=None))]
    fn skim<'py>(
        &self,
        py: Python<'py>,
        dst: String,
        compression: &str,
        tags: Option<PyReadonlyArray1<'py, u32>>,
        tag_names: Option<Vec<(String, u32)>>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let comp = parse_compression(compression)?;
        let summary = match tags {
            None => py
                .detach(|| self.inner.skim(&dst, comp))
                .map_err(to_pyerr)?,
            Some(arr) => {
                // Copy the tags out so the heavy skim runs with the GIL released
                // and no borrow into the NumPy buffer crosses the boundary.
                let tags_vec: Vec<u32> = arr.as_slice()?.to_vec();
                let tags_len = tags_vec.len();
                let names_owned: Vec<(String, u32)> = tag_names.unwrap_or_default();
                let summary = py
                    .detach(|| {
                        let names: Vec<(&str, u32)> =
                            names_owned.iter().map(|(n, b)| (n.as_str(), *b)).collect();
                        let mut i = 0usize;
                        self.inner.skim_tagged(&dst, comp, &names, |_ev| {
                            let t = tags_vec.get(i).copied().unwrap_or(0);
                            i += 1;
                            t
                        })
                    })
                    .map_err(to_pyerr)?;
                if summary.events as usize != tags_len {
                    return Err(pyo3::exceptions::PyValueError::new_err(format!(
                        "tags has {tags_len} entries but the (filtered) chain yields {} events; \
                         compute tags from the same chain/filter (e.g. f.event_tags() or f.arrays())",
                        summary.events
                    )));
                }
                summary
            }
        };
        let out = PyDict::new(py);
        out.set_item("events", summary.events)?;
        out.set_item("records", summary.records)?;
        out.set_item("bytes", summary.bytes)?;
        Ok(out)
    }

    /// Overwrite one event's per-event tag (`EH_TAG`) **in place** on disk,
    /// without rewriting the file (a single 4-byte write). Requires write
    /// permission on the file. Only uncompressed (`Compression::None`) files
    /// can be patched — a compressed record raises `ValueError`; an
    /// out-of-range `entry` raises `IndexError`.
    #[pyo3(signature = (entry, tag))]
    fn set_event_tag(&self, py: Python<'_>, entry: u64, tag: u32) -> PyResult<()> {
        py.detach(|| self.inner.set_event_tag(entry, tag))
            .map_err(to_pyerr)
    }

    /// Batch `set_event_tag`: `updates` is a list of `(entry, tag)` pairs.
    /// Every update is validated (index in range, record uncompressed) before
    /// any write, so a bad update fails the whole batch without a partial
    /// change. Returns the number patched.
    #[pyo3(signature = (updates))]
    fn set_event_tags(&self, py: Python<'_>, updates: Vec<(u64, u32)>) -> PyResult<usize> {
        py.detach(|| self.inner.set_event_tags(updates))
            .map_err(to_pyerr)
    }

    /// Per-record positions (no decompression):
    /// `(file_index, record_index, global_event_start, event_count)`.
    fn record_spans(&self) -> Vec<(usize, usize, u64, u32)> {
        self.inner
            .record_spans()
            .into_iter()
            .map(|s| {
                (
                    s.file_index,
                    s.record_index,
                    s.global_event_start,
                    s.event_count,
                )
            })
            .collect()
    }

    /// Decompressed payload bytes per record (same order as `record_spans`) —
    /// for sizing byte-based streaming batches.
    fn record_decompressed_sizes(&self, py: Python<'_>) -> PyResult<Vec<u64>> {
        py.detach(|| self.inner.record_decompressed_sizes())
            .map_err(to_pyerr)
    }
}

// ---- Writer ---------------------------------------------------------------

/// Map a hipo type char to a [`DataType`]. Array columns (`T#N`) are not yet
/// supported by the Python writer.
fn dtype_from_char(c: &str) -> PyResult<DataType> {
    Ok(match c {
        "B" => DataType::Byte,
        "S" => DataType::Short,
        "I" => DataType::Int,
        "L" => DataType::Long,
        "F" => DataType::Float,
        "D" => DataType::Double,
        other => {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "unknown column type {other:?} (expected one of B/S/I/L/F/D); \
                 array columns are not yet supported by the Python writer"
            )));
        }
    })
}

/// A column's values copied out of a NumPy array into an owned, typed `Vec`,
/// so the actual write runs with the GIL released.
enum ColData {
    I8(Vec<i8>),
    I16(Vec<i16>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
}

impl ColData {
    /// Copy `arr` (a 1-D NumPy array) into the typed `Vec` matching `dt`.
    fn from_py(dt: DataType, col: &str, arr: &Bound<'_, PyAny>) -> PyResult<Self> {
        fn take<T: numpy::Element + Copy>(
            arr: &Bound<'_, PyAny>,
            col: &str,
            want: &str,
        ) -> PyResult<Vec<T>> {
            let a = arr.extract::<PyReadonlyArray1<'_, T>>().map_err(|_| {
                pyo3::exceptions::PyValueError::new_err(format!(
                    "column {col:?}: expected a contiguous 1-D {want} array"
                ))
            })?;
            Ok(a.as_slice()?.to_vec())
        }
        Ok(match dt {
            DataType::Byte => ColData::I8(take(arr, col, "int8")?),
            DataType::Short => ColData::I16(take(arr, col, "int16")?),
            DataType::Int => ColData::I32(take(arr, col, "int32")?),
            DataType::Long => ColData::I64(take(arr, col, "int64")?),
            DataType::Float => ColData::F32(take(arr, col, "float32")?),
            DataType::Double => ColData::F64(take(arr, col, "float64")?),
        })
    }

    fn len(&self) -> usize {
        match self {
            ColData::I8(v) => v.len(),
            ColData::I16(v) => v.len(),
            ColData::I32(v) => v.len(),
            ColData::I64(v) => v.len(),
            ColData::F32(v) => v.len(),
            ColData::F64(v) => v.len(),
        }
    }

    /// Set flat value `i` into bank-builder row `row` of column `name`.
    fn set_at(&self, bb: &mut BankBuilder, name: &str, row: u32, i: usize) -> oxihipo::Result<()> {
        match self {
            ColData::I8(v) => bb.set_i8_at(name, row, v[i])?,
            ColData::I16(v) => bb.set_i16_at(name, row, v[i])?,
            ColData::I32(v) => bb.set_i32_at(name, row, v[i])?,
            ColData::I64(v) => bb.set_i64_at(name, row, v[i])?,
            ColData::F32(v) => bb.set_f32_at(name, row, v[i])?,
            ColData::F64(v) => bb.set_f64_at(name, row, v[i])?,
        };
        Ok(())
    }
}

/// One bank of a single `extend` call, resolved against the schema and copied
/// into owned typed buffers.
struct ResolvedBank {
    schema: Schema,
    /// `n_events + 1` cumulative row offsets (event `e`'s rows = `[e]..[e+1]`).
    offsets: Vec<i64>,
    columns: Vec<(String, ColData)>,
}

/// Columnar HIPO writer. Create a fresh file (`ox.create`) or decorate an
/// existing one with extra banks (`ox.recreate`). Not thread-shared.
#[pyclass(name = "Writer", module = "oxihipo", unsendable)]
struct PyWriter {
    dst: String,
    compression: Compression,
    /// Accumulated schemas: the new banks (fresh) or source + new (decorate).
    dict: Dict,
    /// Next auto-assigned (unique) item number for `new_bank` without an explicit item.
    next_item: u8,
    writer: Option<Writer>,
    /// Decorate mode: the source event stream, merged event-by-event.
    source: Option<ChainEventIter>,
    /// Decorate mode: source event count, to reject under/over-provisioning.
    source_total: Option<u64>,
    events_written: u64,
    finished: bool,
}

#[pymethods]
impl PyWriter {
    /// `source=None` → fresh file; `source=path` → decorate that file (copy its
    /// events, attaching the banks you declare + `extend`).
    #[new]
    #[pyo3(signature = (dst, compression="lz4percolumn", source=None))]
    fn new(
        py: Python<'_>,
        dst: String,
        compression: &str,
        source: Option<String>,
    ) -> PyResult<Self> {
        let comp = parse_compression(compression)?;
        let (dict, next_item, source_iter, source_total) = match source {
            None => (Dict::new(), 1u8, None, None),
            Some(src) => {
                let chain = py.detach(|| Chain::open(&src)).map_err(to_pyerr)?;
                let dict = chain.schemas().clone();
                let next_item = dict
                    .iter()
                    .map(|s| s.item())
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                let total = chain.event_count();
                (dict, next_item, Some(chain.events()), Some(total))
            }
        };
        Ok(Self {
            dst,
            compression: comp,
            dict,
            next_item,
            writer: None,
            source: source_iter,
            source_total,
            events_written: 0,
            finished: false,
        })
    }

    /// Declare a bank schema (the Python `Writer.new_bank`). `cols` is
    /// `[(name, typechar)]` with typechar in B/S/I/L/F/D; `item` auto-assigns
    /// (unique) if omitted.
    #[pyo3(signature = (name, cols, group=1, item=None))]
    fn add_schema(
        &mut self,
        name: String,
        cols: Vec<(String, String)>,
        group: u16,
        item: Option<u8>,
    ) -> PyResult<()> {
        if self.writer.is_some() {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "cannot declare a schema after the first extend()/write",
            ));
        }
        let item = item.unwrap_or_else(|| {
            let i = self.next_item;
            self.next_item = self.next_item.saturating_add(1);
            i
        });
        if item >= self.next_item {
            self.next_item = item.saturating_add(1);
        }
        let entries: Vec<(String, DataType, u32)> = cols
            .into_iter()
            .map(|(n, t)| dtype_from_char(&t).map(|dt| (n, dt, 1u32)))
            .collect::<PyResult<_>>()?;
        self.dict
            .add(Schema::from_columns(name, group, item, entries));
        Ok(())
    }

    /// Append a batch of events. `banks` is `[(bank, offsets_i64, [(col, values)])]`;
    /// every bank in one call must cover the same number of events.
    #[pyo3(signature = (banks))]
    fn extend(&mut self, py: Python<'_>, banks: ExtendBanks<'_>) -> PyResult<()> {
        if self.finished {
            return Err(pyo3::exceptions::PyValueError::new_err("writer is closed"));
        }
        // Resolve + copy every bank's columns; validate shapes.
        let mut resolved: Vec<ResolvedBank> = Vec::with_capacity(banks.len());
        let mut n_events: Option<usize> = None;
        for (name, offsets, cols) in &banks {
            let schema = self
                .dict
                .get(name)
                .ok_or_else(|| {
                    pyo3::exceptions::PyValueError::new_err(format!(
                        "unknown bank {name:?}; declare it with new_bank() first"
                    ))
                })?
                .clone();
            let offs: Vec<i64> = offsets.as_slice()?.to_vec();
            let ne = offs.len().checked_sub(1).ok_or_else(|| {
                pyo3::exceptions::PyValueError::new_err(format!(
                    "bank {name:?}: offsets must be non-empty"
                ))
            })?;
            match n_events {
                None => n_events = Some(ne),
                Some(x) if x != ne => {
                    return Err(pyo3::exceptions::PyValueError::new_err(
                        "all banks in one extend() must cover the same number of events",
                    ));
                }
                _ => {}
            }
            let total_rows = *offs.last().unwrap_or(&0) as usize;
            let mut columns = Vec::with_capacity(cols.len());
            for (cname, arr) in cols {
                let dt = schema
                    .entries()
                    .iter()
                    .find(|e| &e.name == cname)
                    .ok_or_else(|| {
                        pyo3::exceptions::PyValueError::new_err(format!(
                            "bank {name:?} has no column {cname:?}"
                        ))
                    })?
                    .ty;
                let cd = ColData::from_py(dt, cname, arr)?;
                if cd.len() != total_rows {
                    return Err(pyo3::exceptions::PyValueError::new_err(format!(
                        "bank {name:?} column {cname:?}: {} values but offsets imply {total_rows}",
                        cd.len()
                    )));
                }
                columns.push((cname.clone(), cd));
            }
            resolved.push(ResolvedBank {
                schema,
                offsets: offs,
                columns,
            });
        }
        let n_events = n_events.unwrap_or(0);
        if let Some(total) = self.source_total {
            if self.events_written + n_events as u64 > total {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "extending {} events past the source file's {total} events",
                    self.events_written + n_events as u64
                )));
            }
        }

        self.ensure_writer()?;
        let writer = self.writer.as_mut().expect("writer built");
        let source = self.source.as_mut();
        py.detach(move || -> oxihipo::Result<()> {
            let mut source = source;
            for e in 0..n_events {
                let mut eb = EventBuilder::new();
                if let Some(src_it) = source.as_deref_mut() {
                    match src_it.next() {
                        Some(Ok(src_ev)) => {
                            eb.set_tag(src_ev.tag());
                            eb.add_bank_bytes(src_ev.structures_bytes());
                        }
                        Some(Err(err)) => return Err(err),
                        None => {
                            return Err(oxihipo::HipoError::CorruptRecord {
                                offset: 0,
                                reason: "source exhausted before all decorate events were written",
                            });
                        }
                    }
                }
                for rb in &resolved {
                    let lo = rb.offsets[e] as usize;
                    let hi = rb.offsets[e + 1] as usize;
                    let mut bb = BankBuilder::with_row_capacity(&rb.schema, (hi - lo) as u32);
                    bb.push_rows((hi - lo) as u32);
                    for (cname, cd) in &rb.columns {
                        for (row, i) in (lo..hi).enumerate() {
                            cd.set_at(&mut bb, cname, row as u32, i)?;
                        }
                    }
                    eb.add(bb);
                }
                writer.append_raw(&eb.finish())?;
            }
            Ok(())
        })
        .map_err(to_pyerr)?;
        self.events_written += n_events as u64;
        Ok(())
    }

    /// Finish the file (writes the trailer index). Returns
    /// `{"events", "records", "bytes"}`. Idempotent.
    fn close<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        if !self.finished {
            if let Some(total) = self.source_total {
                if self.events_written != total {
                    return Err(pyo3::exceptions::PyValueError::new_err(format!(
                        "decorate covered {} of the source's {total} events; \
                         provide data for all events",
                        self.events_written
                    )));
                }
            }
        }
        self.ensure_writer()?;
        let summary = if let Some(writer) = self.writer.take() {
            py.detach(|| writer.finish()).map_err(to_pyerr)?
        } else {
            oxihipo::WriteSummary {
                events: self.events_written,
                records: 0,
                bytes: 0,
            }
        };
        self.finished = true;
        let out = PyDict::new(py);
        out.set_item("events", summary.events)?;
        out.set_item("records", summary.records)?;
        out.set_item("bytes", summary.bytes)?;
        Ok(out)
    }
}

impl PyWriter {
    fn ensure_writer(&mut self) -> PyResult<()> {
        if self.writer.is_none() {
            let w = Writer::create(&self.dst)
                .schemas(&self.dict)
                .compression(self.compression)
                .build()
                .map_err(to_pyerr)?;
            self.writer = Some(w);
        }
        Ok(())
    }
}

/// Map a compression name to the core enum.
fn parse_compression(name: &str) -> PyResult<oxihipo::Compression> {
    use oxihipo::Compression;
    Ok(match name.to_ascii_lowercase().as_str() {
        "none" => Compression::None,
        "lz4" => Compression::Lz4,
        "lz4best" => Compression::Lz4Best,
        "gzip" => Compression::Gzip,
        "lz4perbank" => Compression::Lz4PerBank,
        "lz4percolumn" => Compression::Lz4PerColumn,
        other => {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "unknown compression {other:?}"
            )));
        }
    })
}

#[pymodule]
fn _oxihipo(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyChain>()?;
    m.add_class::<PyWriter>()?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    error::register(m)?;
    Ok(())
}
