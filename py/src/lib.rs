//! `_oxihipo` — the compiled Python extension. Exposes a `Chain` reader whose
//! heavy work (the columnar materializer) runs in Rust with the GIL released;
//! the Pythonic `array`/`arrays`/`numpy` ergonomics and Awkward assembly live
//! in the pure-Python `oxihipo` package layered on top of `read_columns`.

use numpy::{IntoPyArray, PyArray1};
use oxihipo::{ColumnData, DataType};
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

    /// A new `Chain` restricted to events carrying every bank in `require`
    /// (and, if given, whose record tag is in `record_tag`). Cheap — clones
    /// the shared file handles, does not reopen. `KeyError` if a required
    /// bank isn't in the dictionary.
    #[pyo3(signature = (require=None, record_tag=None))]
    fn filtered(
        &self,
        require: Option<Vec<String>>,
        record_tag: Option<Vec<u64>>,
    ) -> PyResult<PyChain> {
        let mut filter = oxihipo::Filter::new();
        if let Some(names) = require {
            filter = filter.and_require(names);
        }
        if let Some(tags) = record_tag {
            filter = filter.record_tag(tags);
        }
        let inner = self.inner.clone().with_filter(filter).map_err(to_pyerr)?;
        Ok(PyChain { inner })
    }

    /// Copy the (filtered) chain to `dst`, re-compressing with `compression`
    /// (`"none"`, `"lz4"`, `"lz4best"`, `"gzip"`, `"lz4perbank"`,
    /// `"lz4percolumn"`). Returns `{"events", "records", "bytes"}`.
    #[pyo3(signature = (dst, compression="lz4percolumn"))]
    fn skim<'py>(
        &self,
        py: Python<'py>,
        dst: String,
        compression: &str,
    ) -> PyResult<Bound<'py, PyDict>> {
        let comp = parse_compression(compression)?;
        let summary = py
            .detach(|| self.inner.skim(&dst, comp))
            .map_err(to_pyerr)?;
        let out = PyDict::new(py);
        out.set_item("events", summary.events)?;
        out.set_item("records", summary.records)?;
        out.set_item("bytes", summary.bytes)?;
        Ok(out)
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
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    error::register(m)?;
    Ok(())
}
