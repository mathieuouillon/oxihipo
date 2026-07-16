---
id: python-binding
title: Python binding design
sidebar_position: 1
---

# oxihipo → Python binding — final design

:::info This is a design record, not a user guide
This was the design proposal written **before** the binding was built, kept as a
record of the reasoning and the trade-offs. The design has since been
implemented and extended, so some details here describe intent rather than the
shipped code — for example, the binding pins pyo3 0.23 and therefore uses
`Python::allow_threads` (renamed to `detach` in later pyo3), and features like
multi-process reading and the direct-pyarrow path came later.

For how the binding actually behaves today, read
[How it works](../python/how-it-works.md) and the
[Python guide](../python/reading.md).
:::

**Status:** authoritative design, reconciled against 6 adversarial verdicts. Where a proposal and a verdict disagreed, the verdict won. The four load-bearing corrections applied throughout are: (1) `Python::allow_threads` is deprecated → use **`Python::detach`**; (2) the columnar fast path (`for_each_column`) **cannot** feed Awkward because it discards per-event row counts → a **new bulk materializer** that emits offsets *and* honors the chain filter is mandatory; (3) offsets are **`Vec<i64>` / `Index64`**, never u64/int32; (4) panics are handled by PyO3's built-in trampoline (return `PyResult` + `From<HipoError>`), **not** hand-rolled `catch_unwind` — but the binding crate must still compile `panic = "unwind"`.

---

## 1. Design goals & the one core idea

**Core idea: whole columns, not events. All heavy lifting in Rust.**

CLAS12 physicists already read ROOT via `uproot` + `awkward`. This binding makes a HIPO bank *feel like a uproot jagged branch*: `open(...).arrays("REC::Particle")` returns an `ak.Array` with the same `ListOffsetArray` layout uproot produces, built with **zero copies past decompression**, with the entire per-event loop living in Rust behind a **released GIL**. Python never iterates events, never touches a payload byte, never sees a borrowed Rust view.

Non-negotiable commitments (each maps to a verdict):

1. **Nothing borrowed crosses into Python.** No `Bank`, `OwnedEvent`, `EventCtx`, `Cow`, or `&[T]` is ever a `#[pyclass]` field or return. Every Python-visible array is a fully-owned Rust `Vec<T>` moved into NumPy via a capsule that drops it with Rust's own allocator. This one rule kills the entire use-after-free / allocator-mismatch class *and* forces bulk materialization in the same stroke.
2. **The unit of work is a whole column (or a bounded batch of one), never an event.** There is intentionally no `for event in chain` fast path.
3. **Jagged = flat `content` values + `int64` offsets**, wrapped into `ak.contents.ListOffsetArray` with zero copy. Offsets are `int64` unconditionally.
4. **Heavy work runs under `py.detach`** in pure Rust (no `Py`/`Bound` captured), returning plain `Vec`s; the GIL is re-acquired only to wrap finished buffers.
5. **`for_each_column` is not reused as-is** — it drops per-event counts and ignores `self.filter`. A new `read_columns` materializer supplies both.

---

## 2. Architecture / crate layout

Two crates in one workspace. The core `oxihipo` crate stays **100% pyo3-free** — it gains only pure-Rust materializer methods (§6) usable by any Rust consumer. A thin `oxihipo-py` cdylib depends on it plus `pyo3` + `numpy`.

```
oxihipo/                       # repo root = core package AND workspace root
  Cargo.toml                   # [package] oxihipo  +  [workspace] members=["py"]
  src/ …                       # UNCHANGED core + new pure-Rust methods (§6)
  py/
    Cargo.toml                 # oxihipo-py, crate-type=["cdylib"], panic="unwind"
    pyproject.toml             # maturin backend
    src/lib.rs                 # #[pymodule] _oxihipo
    src/error.rs               # From<HipoError> for PyErr + exception tree
    src/chain.rs               # PyChain, PySchema, PyBank
    python/oxihipo/__init__.py # re-exports + Awkward/pandas assembly (lazy imports)
    python/oxihipo/_core.pyi   # hand-written stubs
    python/oxihipo/py.typed
```

**Why a separate crate root, not just a member.** Cargo `[profile]` is workspace-global and the core sets `panic = "abort"` with hard `expect`s on corrupt input. A cdylib inheriting `panic="abort"` turns any such panic into an **interpreter-wide abort** — PyO3's trampoline can only *catch* an unwinding panic. The `py/` crate therefore sets its own `[profile.release] panic = "unwind"`. (Achievable either by making `py/` its own workspace, or by removing `panic="abort"` from the core — the separate root is cleaner.)

**Where Awkward lives: Python, not Rust.** The compiled module produces only typed NumPy buffers + metadata. `__init__.py` holds all `import awkward` assembly. This keeps awkward/uproot version churn out of the `.so`, and lets the pure-`numpy(...)` path work with awkward uninstalled.

`py/Cargo.toml`:
```toml
[package]
name = "oxihipo-py"
edition = "2024"

[lib]
name = "_oxihipo"
crate-type = ["cdylib"]

[dependencies]
oxihipo = { path = "..", features = ["lz4-c"] }
pyo3  = { version = "0.29", features = ["abi3-py39", "extension-module"] }
numpy = "0.29"          # rust-numpy, tracks pyo3; see §10 on the into_pyarray name

[profile.release]
panic = "unwind"        # REQUIRED: PyO3 trampoline must catch, not abort
```

---

## 3. Python API surface (uproot-shaped)

```python
import oxihipo as ox

f = ox.open("run5042.hipo")            # str | os.PathLike | dir | glob | list[...]
f = ox.open(["a.hipo", "b.hipo"])      # -> Chain (maps onto Chain::open(IntoSources))

len(f); f.num_entries                  # int  == chain.event_count()
f.file_count; f.files                  # int ; list[str]

# ---- discovery (two-level namespace: banks are branches, columns sub-branches) ----
f.keys()                               # ['REC::Particle', 'REC::Calorimeter', ...]
f.keys(recursive=True)                 # ['REC::Particle/px', 'REC::Particle/pid', ...]
f.typenames()                          # {'REC::Particle/px': 'float32', 'REC::Track/cov': 'float32[16]'}
"REC::Particle" in f                   # membership
f.schema("REC::Particle")              # -> Schema (.name .group .item .columns[Column])

part = f["REC::Particle"]              # -> Bank proxy ("a branch with sub-branches")
part.keys()                            # ['pid','px','py','pz',...,'cov']

# ---- THE columnar hot path -------------------------------------------------
px  = f.array("REC::Particle", "px")           # -> ak.Array,  type: N * var * float32
px  = f["REC::Particle/px"].array()            # identical, path form

p   = f.arrays("REC::Particle",                # whole bank -> jagged record array,
               ["pid","px","py","pz"])         #   ONE shared offsets buffer
p.px, p.pid                                     # each jagged; p[event].px indexes rows

cov = f.array("REC::Track", "cov")             # array col F#16 -> N * var * 16 * float32

ev  = f.arrays(["REC::Particle","REC::Calorimeter"])   # top-level record, one field/bank
ev["REC::Particle"].px                                 # each bank independently jagged

# ---- uproot selection knobs ------------------------------------------------
f.arrays(filter_name="REC::*")                          # glob over bank/col keys
f.arrays("REC::Particle", entry_start=0, entry_stop=1_000_000)
f.arrays("REC::Particle", library="ak")                 # default -> ak.Array
f.arrays("REC::Particle", library="np")                 # dict[str, np.ndarray]
f.arrays("REC::Particle", library="pd")                 # pandas MultiIndex(entry, subentry)

# ---- numpy-only (no awkward import needed) ---------------------------------
content, offsets, inner_len = f.numpy("REC::Particle", "px")  # (ndarray, int64 ndarray, int)

# ---- bounded-memory streaming for 10–100 GB inputs -------------------------
for chunk in f.iterate(["REC::Particle"], step_size="200 MB"):
    hist.fill(ak.flatten(chunk.px))                     # chunk is ak.Array, dropped each loop
for chunk, report in f.iterate("REC::Particle", step_size=500_000, report=True):
    ...                                                 # report.entry_start/entry_stop/file_path
for chunk in ox.iterate("/data/run5042/*.hipo", "REC::Particle", step_size="1 GB"):
    ...                                                 # multi-file, never opens all at once

# ---- filter pushdown (BRANCH-SELECTION / presence, NOT a value-level cut) ---
g = f.filtered(require=["REC::Particle"], record_tag=[0x42])   # -> new Chain
g.skim("electrons.hipo", compression="lz4bybank")             # -> WriteSummary dict

# ---- single-event inspection (debugging only, explicitly NOT the hot path) --
ev = f.event(42)                        # -> Event | None
```

Return-type contract:

| method | returns | dtype rule |
|---|---|---|
| `array(bank, col)` | `ak.Array` `N*var*T` (or `…*var*M*T` for `T#M`) | wire type from schema |
| `arrays(sel, library=)` | `ak.Array` record (ak) / `dict[str,ndarray]` (np) / `DataFrame` (pd) | per-column |
| `numpy(bank, col)` | `(ndarray content, ndarray[int64] offsets, int inner_len)` | native |
| `iterate(...)` | generator of `ak.Array` (or `(array, Report)`) | native |
| `filtered(...)` | new `Chain` | — |
| `schema` | `Schema` / dict | strings |

Design commitments, with verdict corrections baked in:

- **No `file[tree]` layer.** HIPO has no TTree; the chain *is* the tree. `open(...)` returns one object behaving like a uproot `TTree`. This is the deliberate divergence and it simplifies the model.
- **dtype is inferred from the schema, never passed by the user** (uproot-style).
- **`entry_start`/`entry_stop`** are honored exactly by materializing the covering records then slicing the resulting `ak.Array` (a view, no copy). The columnar fast path is otherwise whole-chain; the new materializer (§6) takes a `Range<u64>`.
- **`filtered()` maps to uproot's `filter_name` / branch-presence semantics, NOT to uproot's value-level `cut`.** Value cuts are the user's job in numpy/awkward *after* materialization. No Python predicate is ever invoked per event (that would be slow and, under a released GIL, UB — §7).
- **`library="np"`** returns object-dtype ndarrays for genuinely jagged columns (uproot parity), with a documented `jagged="flat"` opt-in returning `(content, counts)`; **`library="pd"`** delegates to `ak.to_dataframe` (MultiIndex `(entry, subentry)`), one frame per bank.

---

## 4. The data path: Rust bytes → NumPy → Awkward, no hidden copy

Exactly **one** copy exists beyond decompression, and it is provably minimal. Everything after is pointer-wrapping.

```
LZ4 stream (per-column, in Arc<PerColumnRecord>, re-parsed & dropped per record)
   │  decompress                                   ── the real work
   │  bytemuck::try_cast_slice::<u8,T> -> &[T]      ── 0 copy (aligned 4-byte cols)
   │  or pod_read_unaligned element-wise            ── 1 gather (misaligned 8-byte cols)
   ▼  extend into a PRE-SIZED Vec<T>  ───────── THE ONE COPY ─────────►  Vec<T> (owned)
   ▼  Vec::into_pyarray(py)            ── 0 copy: MOVE, capsule base owns the Vec
numpy.ndarray  (Rust owns the buffer via PySliceContainer; numpy never frees it)
   ▼  ak.contents.NumpyArray(nd)       ── 0 copy (wraps)
   ▼  ak.contents.RegularArray(.., N)  ── 0 copy, only for T#N array columns
   ▼  ak.contents.ListOffsetArray(ak.index.Index64(offsets_nd), inner)  ── 0 copy
   ▼  ak.Array(layout)                 ── 0 copy
```

**Why the one copy is mandatory (verdict 3).** The decompressed `Box<[u8]>` is owned by a `PerColumnRecord` that is re-parsed and dropped inside the scan loop. To present Awkward a single contiguous `content`, the per-record slices must be concatenated into one owned buffer. Keeping every record's box alive would defeat the streaming memory model *and* still not yield a contiguous buffer. So: **one copy, into a pre-sized `Vec`, done off-GIL.** For `Lz4PerColumn` this degenerates to a single `extend_from_slice` (memcpy) of the whole cross-event stream per record — memcpy bandwidth, dwarfed by the LZ4 inflate that produced it.

**Rust→NumPy handoff (the safe primitive, verdict 1).**
```rust
use numpy::{IntoPyArray, PyArray1};
let v: Vec<f32> = build_values_exact();     // pre-sized: len == capacity, no slack
let nd = v.into_pyarray(py);                // MOVE: numpy's `base` is a PySliceContainer
                                            //   holding (ptr,len,cap); its Drop reconstitutes
                                            //   Vec::from_raw_parts and frees via Rust.
```
This is genuinely zero-copy, capacity-correct, single-free: NumPy holds a *reference* to the capsule, never ownership, so `OWNDATA=False` — numpy can neither `resize` nor `free` the buffer. The array is **writable but non-resizable**.

> **Verdict-1 correction on `shrink_to_fit`:** *do not* call `shrink_to_fit()` before `into_pyarray` as a reflex — it is an extra realloc/copy. Instead **pre-size exactly**: for `Lz4PerColumn` the total element count is `Σ bank_size(e,b)/row_size` from the directory *without inflating anything*, so `Vec::with_capacity(exact)` + fill gives `len == capacity` for free. Only fall back to `shrink_to_fit` for the growth-sized fallback formats where the total isn't known up front, and only if trailing capacity RAM matters.

**GIL release (verdict 3).** The scan runs inside `py.detach(|| …)` (the replacement for the deprecated `allow_threads`). The closure and its return must be `Ungil + Send`; the compiler *statically forbids* capturing any `Py`/`Bound`/`Python` handle. `Vec<T>`, `Vec<i64>`, `HipoError` are all `Send`. NumPy/Awkward construction happens only after the GIL is re-acquired.

**NumPy→Awkward handoff (verdict 2), Python side, lazy `import awkward`:**
```python
def _wrap_column(offsets_i64, content_nd, inner_len):
    node = ak.contents.NumpyArray(content_nd)          # 0-copy wrap
    if inner_len > 1:
        node = ak.contents.RegularArray(node, inner_len)   # T#N inner axis
    return ak.contents.ListOffsetArray(ak.index.Index64(offsets_i64), node)  # 0-copy
```
Never `ak.Array([[...],[...]])` and never `ak.from_numpy` on nested Python — those copy and re-introduce per-event overhead.

---

## 5. Jagged representation — the exact Awkward layout

A HIPO bank is already column-major and every column shares one per-event row count. That is exactly a `ListOffsetArray` wrapping a `RecordArray`, with **one shared `Index64` offsets buffer per bank**.

### 5.1 One bank → **list-of-records** (the default, verdict 2b)

For `f.arrays("REC::Particle", ["pid","px","py","cov"])` with `cov: F#16`:
```
ListOffsetArray(offsets=Index64[n_events+1]            # <- jaggedness, stored ONCE per bank
  RecordArray(                                          #    length = n_rows (total)
    contents=[
      NumpyArray(pid : int32   [n_rows]),
      NumpyArray(px  : float32 [n_rows]),
      NumpyArray(py  : float32 [n_rows]),
      RegularArray(NumpyArray(cov : float32 [n_rows*16]), size=16),   # array column
    ],
    fields=["pid","px","py","cov"]))
```
Verdict 2 endorses **list-of-records** (`ListOffsetArray(offsets, RecordArray([...]))`, type `var*{col:T}`) over record-of-lists, because it lets a physicist write `p[event].px` and it is what `ak.zip` produces — while sharing the single offsets buffer just as well. Use it as the default.

- Outer `ListOffsetArray`: `offsets[e]..offsets[e+1]` = rows of event `e`. `len = n_events+1`, `offsets[0]==0`, `offsets[-1]==n_rows`.
- **Array column `T#N` → `RegularArray(NumpyArray, N)`.** Its length is `n_rows` so the *same* outer offsets index it correctly. HIPO stores an array cell as N contiguous elements per row, rows back-to-back — exactly `RegularArray` content order, no reshuffle. **Offsets are row counts, not element counts; `RegularArray` absorbs the `×N`.** This is why one shared offsets buffer serves scalar and array columns alike.

### 5.2 One column → the simpler layout
```
ListOffsetArray(offsets=Index64[n_events+1], NumpyArray(content))
```

### 5.3 Multiple banks → top-level record (length `n_events`)
```
RecordArray(
  [ ListOffsetArray(part_offsets, RecordArray([...])),   # REC::Particle
    ListOffsetArray(calo_offsets, RecordArray([...])) ], # REC::Calorimeter (different counts!)
  fields=["REC::Particle","REC::Calorimeter"])
```
Each bank field carries its **own** shared offsets (banks have different per-event row counts). Nests cleanly: `ev["REC::Particle","px"]`.

### 5.4 offsets dtype — **int64 (`Index64`), unconditionally** (verdicts 2 & 4)

- A full-chain `.arrays()` concatenates all events; `n_rows` can exceed 2³¹ on a multi-file CLAS12 chain → int32 offsets **silently overflow**. int64 is uproot's default (`AsJagged` `index_format="i64"`) precisely for this.
- **Build offsets as `Vec<i64>` in Rust, never u64.** Awkward has no unsigned-64 Index; a u64 numpy array forces a cast-copy at `Index64` construction. Offsets are a freshly-computed buffer anyway, so this is just a Rust-side dtype choice — make it `i64`.
- Cost is negligible (length `n_events+1`).
- Per-chunk `.iterate()` arrays *may* use int32 offsets internally (bounded chunk), but any `ak.concatenate` to a chain-wide array must promote to int64. Default everywhere: int64.

### 5.5 Missing banks / columns / cross-bank alignment

- **Bank absent in an event** → row count 0 → offsets delta 0 → empty sublist `[]`. Uniform across all storage layouts.
- Because every bank's offsets count *every surviving event* (including the 0s), arrays from different banks are all length-`n` at the outer level and can be `ak.zip`'d / masked together. **This is why `for_each_column` cannot be reused** — its fallback emits nothing for absent events, so offsets become unreconstructable and banks silently misalign (verdict 5, R8).
- **Bank absent from the dict entirely** → `KeyError` (a typo, fail-fast).
- **Column absent from the schema** → `KeyError`, resolved up front before any pass (the byte-level path has no element type to synthesize a default). Documented; revisit if users want silent-empty.
- **Composite/opaque banks** (schema-less) → excluded from `keys()` by default; error clearly if requested columnar, plus a raw `f.raw(bank, entry) -> bytes` escape hatch.

Invariants the materializer asserts before handoff (violation raises, never returns a misaligned array):
```
offsets.len() == n_surviving_events + 1
offsets[0] == 0  &&  monotonic non-decreasing
content.len() == (*offsets.last() as usize) * inner_len
```

---

## 6. New Rust/pyo3 methods (signatures) + reuse of the 3 storage layouts

All core additions are **pure Rust**, pyo3-free, unit-testable, and useful to any Rust caller — this is what keeps pyo3 out of the core crate. They reuse the existing record-dispatch skeleton (`for_each_column` at `src/read/chain.rs:325`, `build_tasks` tag pushdown at `:586`, `Bank::col_bytes` at `src/event/bank.rs:177`, directory-only sizes at `src/wire/per_column.rs:430`).

### 6.1 The one heavy materializer (`impl Chain`)

Byte-typed so a single pass can carry columns of *different* element types (`pid:i32`, `px:f32`) — one monomorphized `T` can't. `ColumnData` reinterprets on the Python side from `DataType`.

```rust
pub enum ColumnData { I8(Vec<i8>), I16(Vec<i16>), I32(Vec<i32>),
                      I64(Vec<i64>), F32(Vec<f32>), F64(Vec<f64>) }

pub struct ColumnBuffers {
    pub bank: String,
    pub offsets: Vec<i64>,                                  // len n+1, SHARED by all columns
    pub columns: Vec<(String, DataType, u32 /*inner_len N*/, ColumnData)>,
}

impl Chain {
    /// Bulk columnar extraction over the FILTERED chain, ONE pass. For every
    /// requested bank present in a covering record, gather each requested
    /// column's flat values and build that bank's shared per-event row offsets
    /// exactly once. Honors self.filter + record-tag pushdown. Errors propagate
    /// (corrupt record => Err, never a short/misaligned result).
    pub fn read_columns(
        &self,
        selection: &[(&str, &[&str])],        // (bank, cols); empty cols => all
        range: Option<Range<u64>>,            // global event indices; None = whole chain
        threads: usize,                       // 0 = rayon cores, 1 = sequential, n = n workers
    ) -> Result<Vec<ColumnBuffers>>;
}
```

**Two-pass internal design (memory-bounded, verdict-5–safe):**
- **Pass 1 (metadata only, no column inflation):** walk records covering `range`; per surviving event, `rows_e = PerColumnRecord::bank_size(e,b)/row_size` (a subtraction in the precomputed directory table — same for `ByBankRecord`). Build each bank's `offsets: Vec<i64>` prefix-sum and total `n_rows`. Gives exact output sizes (→ exact `Vec::with_capacity`, no shrink) and each record's disjoint write-offset.
- **Pass 2 (per-column value gather):** pre-allocate each `ColumnData` variant to `n_rows×N`; fill. Dispatch on `header.compression`:

| Layout | Offsets source | Value gather | Cost |
|---|---|---|---|
| **Lz4PerColumn (columnar)** | directory, **no inflate** | `content.extend_from_slice(column_stream(b,c))` — one memcpy of the whole cross-event stream | ≈ memcpy |
| **Lz4PerColumn (opaque)** | directory | `column_stream(b,0)` once; per event `bank_byte_range` → `Bank` → `col_bytes` → memcpy | 1 inflate + gather |
| **Lz4ByBank** | directory, **no inflate** | `bank_stream(b)` once; per event `Bank::new(schema, &stream[range])` → `col_bytes` → memcpy | 1 inflate/bank + gather |
| **Bytes / Lz4 / Gzip / Lz4Chunked** | `Bank::rows()` from decode | `decode_record_into` once; per event `Event::find` → `col_bytes` → memcpy | full decode + gather |

The **offsets-once-per-bank** rule is structural: locate the bank in a record, push the per-event delta once, then loop its columns copying each — columns never re-walk events or re-touch the directory. `Bank::col_bytes` already returns the correct column bytes for both contiguous and per-column backings, so no new slicing logic.

> **8-byte misalignment (R10):** the gather copies bytes into a *fresh contiguous* buffer, so `Bank::col`'s `Cow::Owned`/`read_unaligned` wire path is irrelevant to correctness — misaligned `i64`/`f64` become a per-element gather instead of a memcpy. Only the *output* buffer alignment matters (§9).

### 6.2 Streaming cursor (bigger-than-RAM)

```rust
pub enum BatchStep { Record, Events(u32) }
pub struct ColumnBatch { pub banks: Vec<ColumnBuffers> }
impl Chain {
    pub fn column_batches<'a>(&'a self, selection: &[(&str,&[&str])], step: BatchStep,
                              range: Option<Range<u64>>)
        -> Result<impl Iterator<Item = Result<ColumnBatch>> + 'a>;
    /// Entry↔record map so iterate() can turn "200 MB" into an event count.
    pub fn record_spans(&self) -> impl Iterator<Item = RecordSpan> + '_;
}
pub struct RecordSpan { pub file_idx: usize, pub record_idx: usize,
    pub global_event_start: u64, pub event_count: u32,
    pub compressed_bytes: u32, pub decompressed_bytes: u32 }
```
Each `ColumnBatch` is materialized, wrapped in Python, and **dropped before the next** → resident memory ≈ one batch. Chunk edges snap to record boundaries; the outer `entry_start/entry_stop` is trimmed with an `ak` slice.

### 6.3 Typed single-column fast path (truly zero boundary work)
```rust
impl Chain {
    /// Single column; content moves straight into numpy as Vec<T>, no byte->view.
    pub fn read_column_typed<T: BankColumnType>(&self, bank: &str, column: &str,
        range: Option<Range<u64>>) -> Result<(Vec<i64>, Vec<T>)>;   // (offsets, content)
    /// Flat values only (no offsets) — accumulating for_each_column into an owned Vec.
    pub fn column_values<T: BankColumnType>(&self, bank: &str, column: &str,
        range: Option<Range<u64>>) -> Result<Vec<T>>;
}
```

### 6.4 Cheap filtered clone
```rust
#[derive(Clone)] pub struct Chain { … }  // Arc/Vec-of-Arc/small fields; clone is cheap
// binding: chain.clone().with_filter(f)  — doesn't reopen files or consume the Python handle
```

### 6.5 The pyo3 layer
```rust
#[pyclass(name="Chain", module="oxihipo", frozen)]
struct PyChain { inner: oxihipo::Chain }

// compile-time guard (R7): fails the build if Chain ever loses Send+Sync
const _: () = { fn a<T: Send + Sync>() {} let _ = a::<oxihipo::Chain>; };

#[pymethods]
impl PyChain {
    #[new] fn new(py: Python<'_>, src: Bound<'_, PyAny>) -> PyResult<Self> {
        let sources = extract_sources(&src)?;              // str | os.PathLike | Sequence
        let inner = py.detach(|| oxihipo::Chain::open(sources))?;   // GIL released for blocking I/O
        Ok(Self { inner })
    }
    #[getter] fn num_entries(&self) -> u64 { self.inner.event_count() }
    fn __len__(&self) -> PyResult<usize> { usize::try_from(self.inner.event_count())
        .map_err(|_| PyOverflowError::new_err("event_count exceeds isize")) }   // R (saturate, not wrap)

    #[pyo3(signature=(selection, *, entry_start=None, entry_stop=None, threads=0))]
    fn read_columns<'py>(&self, py: Python<'py>, selection: Vec<(String, Vec<String>)>,
        entry_start: Option<u64>, entry_stop: Option<u64>, threads: usize)
        -> PyResult<Bound<'py, PyList>>
    {
        let sel = borrow(&selection);
        let range = mk_range(entry_start, entry_stop);
        // HEAVY WORK, GIL RELEASED — closure captures no Py handle (Ungil enforced):
        let bufs = py.detach(|| self.inner.read_columns(&sel, range, threads))
            .map_err(err::to_py)?;
        // RE-ACQUIRED GIL: move each Vec into numpy (0-copy), tag dtype + inner_len
        Ok(into_py_buffers(py, bufs))   // __init__.py assembles ak.Array from these
    }
}
```
`#[pyclass(frozen)]`: every method is `&self`, no `RefCell`/`PyRef` borrow panics, and the pyclass is `Sync` so `&PyChain` is legitimately shareable while the GIL is released (R7). `Chain` is immutable after construction (its `OnceLock` caches are per-parsed-record, local to each call). If `FileInner` ever proves `!Sync` on some target, the guard fails the build and we fall back to `Mutex<Chain>` (serializes calls; see §11 Q1).

---

## 7. Parallelism & GIL rules

1. **`py.detach` (not the deprecated `allow_threads`)** wraps every `read_columns`/`column_batches`/`column_values`/`open` call. The closure is pure Rust, returns `(Vec<i64>, Vec<ColumnData>, …)`, and — by the `Ungil` bound — **cannot** capture or return a `Py`/`Bound`/`Python`. NumPy/Awkward construction happens only after re-acquire.
2. **Parallelize across records inside `read_columns`** via rayon, using an **indexed / ordered collect** (like `from_paths`) so record order — hence `px[i]`/`pid[i]` alignment — is preserved. Never append into a shared `Vec` from workers (R16). `threads==1` runs the sequential arm (correct order for free, no stitch). Resident memory stays ≈ `workers × one record` + the output buffers, matching the crate's pread/no-mmap streaming model.
3. **Do not parallelize *within* a single `for_each_column` call** — its visitor is `FnMut`, inherently sequential per column (verdict 3). Independent parallelism is across records/columns/banks/files.
4. **No Python callback ever runs in the hot path.** Filtering is Rust-side `Filter` (bank presence + record tag); value cuts are post-materialization numpy/awkward. A Python lambda invoked under a released GIL is UB (R6) — structurally prevented because `read_columns` has no callback parameter.
5. **`threads=` knob** on `arrays`/`iterate` maps to `for_each`'s model (0 = all cores default, 1 = sequential, n = n). Default all cores.

---

## 8. Type mapping (DataType → NumPy dtype)

| HIPO `DataType` | Rust element | NumPy dtype | note |
|---|---|---|---|
| `Byte` (I8) | `i8` | `int8` | **signed** — never `uint8` (R15) |
| `Short` (I16) | `i16` | `int16` | |
| `Int` (I32) | `i32` | `int32` | 4-byte: `Cow::Borrowed`, zero-copy wire cast |
| `Long` (I64) | `i64` | `int64` | 8-byte: may be misaligned → per-element gather |
| `Float` (F32) | `f32` | `float32` | 4-byte, zero-copy wire cast |
| `Double` (F64) | `f64` | `float64` | 8-byte, as above |
| **offsets** | `i64` | `int64` (`Index64`) | always; never u64/int32 |

dtype is discovered from the schema, never passed by the user. All six are `Pod`/`bytemuck`-castable and satisfy `numpy::Element` (incl. the `Sync` bound tightened in recent rust-numpy). Content is native little-endian on all supported targets, so Awkward wraps without upcast.

---

## 9. Correctness / soundness rules

Distilled from the critic's risk register and the verdicts. Each is a design rule, not an aspiration.

- **Panic barrier (verdict 6).** Return `PyResult<T>` from every `#[pymethods]`/`#[pyfunction]` and implement `From<HipoError> for PyErr`. **Do not hand-roll `catch_unwind`** at ordinary entry points — PyO3's trampoline already wraps them and converts a panic to `pyo3_runtime.PanicException` (a `BaseException` subclass). Manually guard *only* the surfaces PyO3 does not wrap: raw `pyo3-ffi`/`extern "C"` shims and `Drop`/`__traverse__`/`__clear__`. A rayon worker panic propagates to the calling thread, still inside the wrapped frame — caught correctly. **Keep two disjoint channels:** `Err(HipoError)` → the exception tree; panics → `PanicException`, never remapped into `HipoError` (a swallowed bug is worse than a crash). This all requires the binding crate's `panic = "unwind"` (§2) — under `panic="abort"` the trampoline can't catch.
- **Exception tree.** `create_exception!(oxihipo, HipoError, PyException)` base; subclasses mirror variants. Map `HipoError::Io` → an **`OSError`-derived** leaf so `except OSError` works; `Unknown{Schema,Column}` → `KeyError`; `TypeMismatch`/`ColumnLengthMismatch` → `TypeError`; `CorruptRecord`/`Decompress*`/`BadMagic` → `oxihipo.CorruptFile`; `SchemaParse`/`InvalidGlob` → `ValueError`; `E::Path` attaches path context as `__cause__`.
- **Only owned buffers cross the boundary (R1).** No borrowed view is a pyclass field; the only long-lived pyclass is `Chain` (holds `Arc`s, effectively `'static`).
- **Allocator safety (R2).** Only `IntoPyArray::into_pyarray` (capsule base). Never `from_owned_ptr` / `PyArray_SimpleNewFromData` with our pointer. NumPy holds a reference, never ownership; `OWNDATA=False` ⇒ it cannot resize/free. Do **not** enable a custom global allocator (e.g. mimalloc) in the wheel — the capsule makes allocator choice irrelevant to safety and mixing adds risk for no benefit.
- **Exact-size, no reflex shrink (R3, verdict 1).** Pre-size with `Vec::with_capacity(exact)` (directory-derived for PerColumn/ByBank) and fill exactly so `len==capacity`; skip `shrink_to_fit`. Only shrink the growth-sized fallback formats, and only if trailing RAM matters. `debug_assert!(len == capacity)`.
- **Offsets int64 (R4, verdict 4).** Accumulate as `Vec<i64>`, expose as `int64` PyArray, wrap with `ak.index.Index64`. Never expose an int32-offset option on whole-chain arrays.
- **Offsets emitted for every surviving event (R8).** 0 where absent/empty. Assert `offsets.len()==n+1` before handoff.
- **Filter honored on the column path (R9).** `read_columns`/`column_batches` route through the same filter + tag pushdown as `events()`/`for_each` — never the filter-blind `for_each_column`.
- **Output-buffer alignment (§verdict-agnostic).** NumPy `.view(float64)` on a moved buffer wants 8-byte alignment. `read_column_typed::<T>` is `T`-aligned by construction. For the byte-typed `ColumnData`, back each buffer with an element-aligned allocation (build as `Vec<u64>`/aligned-vec sized to `ceil(bytes/8)`, expose as the target dtype) so `.view()` is *provably* zero-copy and numba-friendly, not merely aligned-in-practice.
- **Awkward must not copy (R11).** Construct via `ak.contents.{NumpyArray,RegularArray,ListOffsetArray}` + `ak.index.Index64` on the exact numpy arrays produced; match dtypes so Awkward never upcasts. Never build from Python nested lists / `ak.from_numpy` on nested input.
- **No per-event Python yield (R12).** Primary API is whole-column; the only Python loop is over requested columns/banks or coarse `iterate` batches (each an `ak.Array` of many events).
- **`Byte` is signed (R15).** `i8` → `int8`, never `uint8`.
- **Array-column reshape (R18).** `T#N` is element-major within a row, row-major across rows → `RegularArray(content, N)` C-order; `N = Schema::column_length`. Cover with a round-trip CI test.

---

## 10. Packaging & distribution

- **Build tool:** maturin; separate `oxihipo-py` cdylib.
- **pyo3 features:** `["extension-module", "abi3-py39"]`. abi3 → **one wheel per (OS, arch)** across CPython ≥ 3.9; `extension-module` drops the libpython link (manylinux-clean). rust-numpy is orthogonal to the CPython limited API — it resolves NumPy's C-API at import time through a capsule, so abi3 + numpy coexist.
- **rust-numpy / `into_pyarray` name (verdict 1):** target a release where `into_pyarray(self, py) -> Bound<'py, PyArray>` is the stable name (rust-numpy **≥ 0.23**; the current paired `0.29` retains this signature). Do **not** straddle the old `0.22` API, where the Bound-returning entry point was `into_pyarray_bound` and plain `into_pyarray` was the deprecated GIL-ref path. Require `T: Element` (all six scalars qualify, incl. the `Sync` bound).
- **NumPy 2.x ABI (R17):** pin a numpy-2-aware rust-numpy; CI-matrix numpy 1.26 **and** ≥2.0. abi3 covers CPython, not numpy — that compatibility is separate and explicit.
- **Runtime deps:** `numpy>=1.24` hard; `awkward>=2.6` imported **lazily** (only in `array`/`arrays`/`iterate`) so `numpy(...)` works without it; `pandas>=2`, `pyarrow` as extras (`oxihipo[pandas,arrow]`). `.array*()` raises a clear `ImportError` if awkward is missing.
- **C deps:** build wheels with `lz4-c` on (`manylinux2014` x86_64+aarch64 — JLab ifarm is AlmaLinux/RHEL; macOS universal2 with `lz4-apple` on arm64; Windows x86_64) for the decompress win. Offer a pure-Rust `lz4_flex` sdist fallback (`--no-default-features`).
- **`panic = "unwind"`** in `py/Cargo.toml` release profile (§2) — the single most important packaging line.
- **Stubs:** hand-written `_core.pyi` (annotated with `numpy.typing`, `@overload` on `counts`/`library`) + `py.typed`. pyo3-stub-gen is optional; hand-write to control ndarray dtype annotations.
- **Module init** (`_oxihipo`): register `PyChain`, `PySchema`, `PyBank`, the exception types, `__version__`. `python/oxihipo/__init__.py` re-exports them and defines the Awkward/pandas assembly.

---

## 11. Phased roadmap & open decisions

### Roadmap

**Phase 0 — core plumbing (Rust only, no pyo3).** Land `Chain::read_columns` (byte-typed, filter-aware, offsets-emitting, PerColumn fast path + fallbacks), `read_column_typed::<T>`, `bank_row_counts`/`record_spans`, and `#[derive(Clone)]` on `Chain`. Full Rust unit tests incl. the array-column round trip and the absent-bank-zero-offset invariant. **No Python yet.**

**Phase 1 — MVP binding.** `oxihipo-py` crate; `open`, `num_entries`/`__len__`, `files`, `keys`, `schema`, `numpy(bank,col)` (offsets+content+inner_len, no awkward dep), `array`/`arrays` (`library="ak"`), `From<HipoError>` tree, `py.detach` + `into_pyarray`, `panic="unwind"`, abi3 wheel on one platform. This delivers the headline zero-copy jagged path.

**Phase 2 — uproot ergonomics.** `f["bank"]` Bank proxy, `keys(recursive)`, `typenames`, `filter_name`, `entry_start/entry_stop`, `library="np"/"pd"`, `filtered`/`skim`. Full CI wheel matrix (manylinux + macOS universal2 + Windows; numpy 1.26 & 2.x).

**Phase 3 — streaming & scale.** `iterate(step_size)` (int + "200 MB") over `column_batches`, module-level `oxihipo.iterate(glob, …)`, `report=True`. Fused multi-column single-pass (`arrays` shares one directory parse per record). Optional `decompress_column_into` to erase the last post-inflate copy.

**Phase 4 — reach.** Native Arrow C-Data-Interface export (`to_arrow`, bypassing awkward for polars/duckdb); `threads=` tuning; composite-bank raw accessor.

### Open decisions for the user

1. **`FileInner: Sync` on all targets?** The `frozen` lock-free design (R7) depends on it. `chain.rs` implies pread is concurrency-safe (Unix) / mutex-serialized (elsewhere), but this must be confirmed in `src/read/inner.rs` before committing. If not `Sync` on some target, the compile-time guard fails the build and we fall back to `Mutex<Chain>` (serializes Python-thread calls, loses cross-thread parallelism there). **Verify first.**
2. **`ColumnData` public enum vs a sealed `dyn ColumnSink`.** The enum is exhaustive over 6 wire types, no vtable, trivially testable — recommended. Confirm the crate owner is comfortable exposing it.
3. **`library="np"` jagged policy.** Object-dtype ndarray (uproot parity) vs `(content, counts)` flat tuple (what physicists usually want, fully zero-copy). Recommend `jagged="object"` default with a `jagged="flat"` opt-in.
4. **`iterate` default step.** `Record` (natural boundaries, non-uniform sizes) vs `Events(n)` (uniform, accumulates across records). Recommend `Events(1_000_000)` for predictability; confirm with a CLAS12 user whether per-chunk *exactness* (vs record-aligned interiors, uproot's cluster behavior) is ever required.
5. **`skim` compression as a string.** `"lz4bybank"` etc. map to the `Compression` enum; `Lz4Chunked{events_per_chunk}` needs a `(str,int)` form or a small `Compression` pyclass. Minor surface decision.
6. **Composite banks.** Which CLAS12 banks (if any) users actually want columnar — likely none. Until confirmed, exclude from `keys()` and error clearly on request.
7. **Arrow native export (Phase 4).** Worth native C-Data-Interface, or is `ak.to_arrow_table` enough? Depends on how many users leave the awkward ecosystem for polars/duckdb.

---

**Key source anchors for implementers (absolute):** `/Users/mathieuouillon/Documents/tmp/hipo-rs/src/read/chain.rs` (`for_each_column` :325, `build_tasks` :586 — generalize into the private record loop `read_columns` reuses); `/Users/mathieuouillon/Documents/tmp/hipo-rs/src/event/bank.rs` (`col_bytes` :177 — per-column byte slice to `extend_from_slice`); `/Users/mathieuouillon/Documents/tmp/hipo-rs/src/wire/per_column.rs` (`bank_size`/`bank_byte_offset` :430 — directory-only offsets, no inflate) and its `ByBankRecord` twins in `src/wire/by_bank.rs`; `/Users/mathieuouillon/Documents/tmp/hipo-rs/src/schema/types.rs` (`DataType`, `column_length`) and `src/schema/handle.rs` (`BankColumnType`); `/Users/mathieuouillon/Documents/tmp/hipo-rs/src/error.rs` (`HipoError` variants). New binding crate: `/Users/mathieuouillon/Documents/tmp/hipo-rs/py/` (does not yet exist).