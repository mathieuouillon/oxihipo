# oxihipo (Python)

[![PyPI](https://img.shields.io/badge/pypi-v0.7.0-006dad)](https://pypi.org/project/oxihipo/)
[![Python](https://img.shields.io/badge/python-3.10%2B-3776ab)](https://pypi.org/project/oxihipo/)
[![Documentation](https://img.shields.io/badge/📖_docs-mathieuouillon.github.io%2Foxihipo-b5410b)](https://mathieuouillon.github.io/oxihipo/docs/python/reading)
[![Tutorial](https://img.shields.io/badge/🎓_tutorial-CLAS12_analysis_in_Python-0d7a6f)](https://mathieuouillon.github.io/oxihipo/docs/tutorial)

> **New to CLAS12?** Start with the
> **[CLAS12 analysis tutorial](https://mathieuouillon.github.io/oxihipo/docs/tutorial)** —
> eight pages from your first `open()` to DIS kinematics, `pindex` detector joins,
> and invariant/missing-mass spectra, with runnable code and sample data.

Fast, **columnar** reading *and writing* of HIPO (CLAS12) files, powered by the
Rust `oxihipo` core. A HIPO bank reads like a
[uproot](https://uproot.readthedocs.io) jagged branch, and columns come back as
[Awkward](https://awkward-array.org) arrays — built *zero-copy* from buffers the
Rust side fills with the GIL released. Writing is columnar too: `create` a new
file, `recreate` to replace one, or `update` to decorate an existing file with a
derived bank.

```python
import oxihipo as ox

f = ox.open("run5042.hipo")                 # file | dir | glob | list of paths
f.num_entries                               # event count
f.keys()                                    # ['REC::Particle', 'REC::Event', ...]

p = f.arrays("REC::Particle", ["pid", "px", "py", "pz"])
p.px                                        # jagged: p[event].px indexes particles
ak.sum(p.px, axis=1)                         # per-event reductions, no Python loop
```

Runnable scripts live in [`examples/`](https://github.com/mathieuouillon/oxihipo/tree/main/py/examples) — every one works against the
bundled sample with no arguments:

| | |
|---|---|
| [`quickstart.py`](https://github.com/mathieuouillon/oxihipo/blob/main/py/examples/quickstart.py) | open a file, inspect it, read columns |
| [`analysis.py`](https://github.com/mathieuouillon/oxihipo/blob/main/py/examples/analysis.py) | a columnar analysis with Awkward (cuts, reductions) |
| [`streaming.py`](https://github.com/mathieuouillon/oxihipo/blob/main/py/examples/streaming.py) | `iterate` a chain bigger than RAM |
| [`parallel.py`](https://github.com/mathieuouillon/oxihipo/blob/main/py/examples/parallel.py) | `workers=N` multi-process reading |
| [`writing.py`](https://github.com/mathieuouillon/oxihipo/blob/main/py/examples/writing.py) | write a file: jagged, `T#N` array, and scalar columns |
| [`decorate.py`](https://github.com/mathieuouillon/oxihipo/blob/main/py/examples/decorate.py) | attach a derived bank to a cooked file |
| [`event_tags.py`](https://github.com/mathieuouillon/oxihipo/blob/main/py/examples/event_tags.py) | tags: filter by name, tag-and-skim, retag in place |
| [`interop.py`](https://github.com/mathieuouillon/oxihipo/blob/main/py/examples/interop.py) | NumPy / pandas / Arrow → polars, duckdb |
| [`rdataframe.py`](https://github.com/mathieuouillon/oxihipo/blob/main/py/examples/rdataframe.py) | feed ROOT's RDataFrame |
| [`tutorial_sample.py`](https://github.com/mathieuouillon/oxihipo/blob/main/py/examples/tutorial_sample.py) | generate the CLAS12-shaped sample for the [tutorial](https://mathieuouillon.github.io/oxihipo/docs/tutorial) |
| `bench_*.py` | read, compression, and RDataFrame benchmarks |

## Reading

| call | returns |
|---|---|
| `f.arrays(bank, [cols])` | `ak.Array` — jagged record `N * var * {col: T}` |
| `f.arrays([bankA, bankB])` / `f.arrays(filter_name="REC::*")` | record with one field per bank |
| `f.array(bank, col)` | one column, `N * var * T` |
| `f.numpy(bank, col)` | `(values, offsets, inner_len)` — plain NumPy, no Awkward import |
| `f.event_tags()` | per-event tag (`EH_TAG`) as `uint32[n_events]` — aligned 1:1 with `arrays()` |
| `f["REC::Particle"]` | a **bank proxy**: `.keys()`, `.typenames()`, `.array(col)`, `["col"]` |
| `f["REC::Particle/px"]` | the `px` column |

Common knobs (on `arrays` / `array` / `numpy` / `iterate`):

- `entry_start=`, `entry_stop=` — restrict to a global event range.
- `filter_name="REC::*"` — glob over `bank` / `bank/column` keys.
- `library=` — `"ak"` (default, `ak.Array`), `"np"` (`dict` of object-dtype
  `ndarray`), `"pd"` (pandas, one frame per bank), `"arrow"` (`pyarrow.Table`,
  one `large_list` column per field — for polars / duckdb). A non-matching
  `filter_name` / empty bank list yields an *empty* result, not an error.
- `threads=` — `0` = all cores (default), `1` = sequential, `n` = `n`-thread pool.
- `workers=` — read with `N` **processes** for I/O-bound filesystems; see
  [Parallel reading](#parallel-reading-multi-process).

## Streaming (bigger than RAM)

`iterate` yields the chain in fully-materialized chunks; each is dropped before
the next is read, so resident memory stays ≈ one chunk.

```python
for chunk in f.iterate("REC::Particle", ["px"], step_size="200 MB"):
    hist.fill(ak.flatten(chunk.px))

for chunk, report in f.iterate("REC::Particle", step_size=1_000_000, report=True):
    ...  # report.entry_start / report.entry_stop / report.file_path

# multi-file, never opens it all at once:
for chunk in ox.iterate("/data/run5042/*.hipo", "REC::Particle", step_size="1 GB"):
    ...
```

`step_size` is an event count (`int`) or a byte budget (`"200 MB"`, `"1 GB"`);
chunks are aligned to record and file boundaries.

## Parallel reading (multi-process)

On a parallel filesystem (JLab ifarm `/volatile`, Lustre) a single process
saturates well below the filesystem's aggregate bandwidth — the limit is
*per-process*, not per-node. `workers=N` splits the chain into `N` disjoint,
record-aligned event ranges, reads them from `N` separate processes, and
stitches the result — turning one I/O stream into `N`.

```python
# whole-array read, N processes, stitched into one ak.Array:
a = ox.arrays("/volatile/run5042/*.hipo", "REC::Particle", ["px", "py", "pz"], workers=8)

# streaming, ~N reads in flight (resident memory ≈ N chunks), yielded in order:
for chunk in ox.iterate("/volatile/run5042/*.hipo", "REC::Particle", step_size="1 GB", workers=8):
    ...
```

- Works with everything else: `filter_name`, `entry_start`/`entry_stop`,
  `library=`, and `.filtered(...)` all carry through to the workers.
- Without an explicit `threads=`, the machine's cores are split across the
  workers (total ≈ all cores); on an I/O-bound farm the surplus decode threads
  simply wait on the read.
- **This helps only when I/O is the bottleneck.** On a local, already-cached
  disk the limit is decode/bandwidth, not I/O, so `workers>1` just adds process
  and IPC overhead — keep the default `workers=1` there.
- Each `arrays(workers=N)` / `iterate(workers=N)` call spins up its own worker
  pool, so pay the spawn cost once: prefer a **single** `iterate(...)` over a
  many-file chain to a loop of small `arrays()` calls.

> **Required:** any script that passes `workers=` must be guarded by
> `if __name__ == "__main__":`. Workers are spawned (not forked — forking after
> Rust's thread pool exists is unsafe), so each re-imports your script; without
> the guard it would re-run at import. See [`examples/parallel.py`](https://github.com/mathieuouillon/oxihipo/blob/main/py/examples/parallel.py).

## Analysis helpers

Reading columns is half of an analysis. These turn them into physics without
hand-written constants or joins.

**PDG masses.** `pid` is a code; kinematics need a mass.

```python
p = f.arrays("REC::Particle", ["pid", "px", "py", "pz"])
m = ox.pdg_mass(p.pid)                 # jagged, same shape as p.pid — GeV
ox.pdg_name(11)                        # 'e-', for labels
```

Two CLAS12 cases that general PDG helpers get wrong are handled: `pid == 0` (a
track the reconstruction couldn't identify — you get `nan`, not an exception) and
`pid == 45`, which is a **Geant3** code, not a PDG one (45/46/47/49 =
deuteron/triton/He4/He3). `ox.PDG_MASS_GEV` is the table and is user-extensible.

**Lorentz vectors** via [vector](https://vector.readthedocs.io):

```python
v = ox.to_vector(p, mass="pdg")
v.E, v.pt, v.eta, v.phi, v.mass
(v[:, 0] + v[:, 1]).mass               # invariant mass
v[:, 0].deltaR(v[:, 1])
```

Omitting `mass` gives a **3-vector**, not a massless 4-vector — an assumed-zero
mass wearing a four-vector's interface is how a wrong invariant mass happens.

**`pindex` joins.** Detector banks point at their particle by row number.
`ox.link` wires both directions so the join is something you follow:

```python
ev = ox.link(f.arrays(["REC::Particle", "REC::Calorimeter"]))

ev["REC::Calorimeter"].particle.px         # the particle each row belongs to
ev["REC::Particle"]["REC::Calorimeter"]    # that particle's rows, grouped
```

`ox.group_by_index(cal, ak.num(part))` is the one-directional form, and turns a
per-particle detector quantity into a column:

```python
part["cal_energy"] = ak.sum(ox.group_by_index(cal, ak.num(part)).energy, axis=-1)
```

An out-of-range `pindex` is never attached to whichever particle happens to be
there: `None` going forward, dropped going back.

**`map_reduce` — analysis in the workers.** `workers=` parallelises only the
read; the physics still runs serially in the parent, which is where a CLAS12
selection spends its time. `map_reduce` runs your function where the chunk
already is and sends back only what it returns:

```python
import hist

def analyze(chunk):                        # module level — it is pickled to workers
    h = hist.Hist(hist.axis.Regular(100, 0, 10, name="Q2"))
    h.fill(q2_of(chunk))
    return h

h = ox.open("/volatile/rga/*.hipo").map_reduce(analyze, "REC::Particle", workers=8)
```

A filled histogram pickles to a few hundred bytes against the hundreds of
megabytes it was filled from. `reduce=` defaults to `operator.add`, which
`hist.Hist`, `boost_histogram`, `np.ndarray` and numbers all implement; results
are folded in **event order**, so a non-commutative `reduce` is safe.

**Dask.** `f.to_dask(...)` is a real `dask-awkward` source: nothing is read to
build the graph, entry boundaries are known (so `len()` and slices work — except
under `cut=`, which may drop events and so forfeits them), and
columns are projected — `dak.sum(p.px)` reads `px`, not the whole bank.

## Filtering and skimming

```python
g = f.filtered(require=["REC::Particle"])           # events carrying a bank
g = f.filtered(record_tag=[0x42])                   # by record tag
g = f.filtered(event_tag=[1, 4])                    # by per-event tag (EH_TAG)
g = f.filtered(event_tag="dvcs")                    # by tag name (if the file has a registry)
summary = g.skim("electrons.hipo", compression="lz4percolumn")   # SkimSummary(events, records, bytes)
```

`filtered()` returns a new chain; the filter reduces what `arrays()` / `skim()`
yield (its `num_entries` stays the pre-filter total, as in uproot).

## Writing

`create` opens a new file (and refuses an existing path); `recreate` *replaces*
one; `update` *decorates* an existing one. All three return a
columnar `Writer` with an uproot-style `new_bank` / `extend` / `close` API —
columns are written **zero-copy** from NumPy or Awkward, with the GIL released.

```python
with ox.create("out.hipo", compression="lz4percolumn") as w:
    w.new_bank("NEW::bank", {"px": "F", "pid": "I", "cov": "F#3"})   # scalars + T#N arrays
    w.extend({"NEW::bank": {                                          # a batch of events
        "px":  ak.Array([[1.0, 2.0], [], [3.0]]),                    # jagged: rows per event
        "pid": ak.Array([[11, -11], [], [211]]),
        "cov": ak.Array([[[1, 2, 3], [4, 5, 6]], [], [[7, 8, 9]]]),  # 3-vector per row
    }})
```

- `new_bank(bank, {col: typechar})` — declare a bank; `typechar` ∈ `B/S/I/L/F/D`,
  optionally `#N` for a fixed-length array column (`"F#3"`). The unique `item`
  auto-assigns.
- `extend({bank: data})` — append a batch. `data` is an `ak.Array` record (what
  `arrays(bank)` returns) or a dict of columns — a jagged `ak.Array` per column,
  or a 1-D NumPy array for a scalar-per-event bank. Call it in a loop to stream
  large outputs in bounded memory.
- `close()` (or leaving the `with`) writes the trailer index and returns a
  `SkimSummary`.

**Decorate — add a bank to a cooked file** without rewriting the physics banks
(an ML score, a computed kinematic):

```python
f = ox.open("dst.hipo")
scores = model.predict(f.arrays("REC::Particle")).astype("float32")   # one per event

w = ox.update("dst.hipo", "decorated.hipo")     # or dst=None to replace in place
w.new_bank("ML::pred", {"score": "F"})
w.extend({"ML::pred": {"score": scores}})        # aligned 1:1 with the source events
w.close()
```

Every source event is copied verbatim (existing banks, array columns included),
with the new banks attached; they must cover all source events (`close` errors
otherwise). Full guide:
[Writing](https://mathieuouillon.github.io/oxihipo/docs/python/writing).

## RDataFrame (ROOT)

`rdataframe` hands a selection to ROOT's
[RDataFrame](https://root.cern/manual/data_frame/) through Awkward's generated
`RDataSource` — a jagged bank column becomes an `RVec<T>`, a `T#N` array column
a nested `RVec`, no copy of the view. Column names are the `bank/column` keys
sanitized to C++ identifiers (`REC::Particle/px` → `REC_Particle_px`).

```python
df = ox.rdataframe("run5042.hipo", "REC::Particle", ["px", "py", "pid"])
h = df.Define("pt", "sqrt(REC_Particle_px*REC_Particle_px"
                   " + REC_Particle_py*REC_Particle_py)").Histo1D("pt")

# bigger than RAM: one RDataFrame per chunk, merge histograms across chunks
total = None
for chunk in ox.iterate_rdataframe("run5042.hipo", "REC::Particle", ["px"], step_size="1 GB"):
    h = chunk.Histo1D(("pt", "", 100, 0, 10), "REC_Particle_px").GetValue()
    total = h.Clone() if total is None else (total.Add(h) or total)
    total.SetDirectory(0)
```

Needs a working ROOT/PyROOT (not on PyPI — conda-forge or system) plus
`awkward`; `pip install oxihipo[root]` covers the awkward side. `filter_name`,
`entry_start`/`entry_stop`, and `.filtered(...)` all carry through. See
[`examples/rdataframe.py`](https://github.com/mathieuouillon/oxihipo/blob/main/py/examples/rdataframe.py) and the
[RDataFrame guide](https://mathieuouillon.github.io/oxihipo/docs/python/rdataframe).

The bridge is a **no-copy view** — `rdataframe` costs ~1 ms over the bare
`arrays` read. But the RDF loop is single-threaded here (implicit MT doesn't work
with the Awkward-generated source), so on a simple kernel it runs slower than the
vectorized Awkward equivalent: use it to reuse RDF/C++ code, not for speed. Numbers
+ reproduction: [`examples/bench_rdataframe.py`](https://github.com/mathieuouillon/oxihipo/blob/main/py/examples/bench_rdataframe.py) and
the [RDataFrame guide's Performance section](https://mathieuouillon.github.io/oxihipo/docs/python/rdataframe#performance).

## Discovery

```python
f.keys()                       # bank names
f.keys(recursive=True)         # 'bank/column' keys
f.keys(filter_name="REC::*")   # globbed
f.typenames()                  # {'REC::Particle/px': 'float32', 'REC::Track/cov': 'float32[3]'}
"REC::Particle" in f
```

## How it works

The whole per-event loop runs in **Rust with the GIL released**. One pass over
the file materializes each requested column into a flat NumPy buffer plus one
shared `int64` offsets buffer per bank — exactly an Awkward
`ListOffsetArray` / `Index64` layout — moved into NumPy zero-copy. The Python
layer only *wraps* those buffers (`NumpyArray` / `RegularArray` for `T#N` array
columns / `ListOffsetArray`), so nothing is copied past decompression and Python
never iterates events. Errors map onto a Python exception tree
(`KeyError` for a missing bank/column, `TypeError` for a dtype mismatch,
`OSError` for I/O, `oxihipo.CorruptFileError` for a malformed record).

## Performance

Reading through the binding runs within **~10% of native Rust** — the per-event
decode is Rust behind a released GIL, and columns move into NumPy zero-copy. On
a 9.1 GB CLAS12 file (598k events, Apple M4 Pro, all cores),
`f.arrays("REC::Particle", ["px","py","pz","pid"])` reads at ~5.6 GB/s vs Rust's
6.3 GB/s. Details + reproduction:
[Python vs Rust benchmark](https://mathieuouillon.github.io/oxihipo/docs/design/python-vs-rust-benchmark)
and [`examples/bench_columns.py`](https://github.com/mathieuouillon/oxihipo/blob/main/py/examples/bench_columns.py).

## Install

```sh
pip install oxihipo          # wheels for Linux / macOS / Windows, CPython >= 3.10
```

That is the whole install: **every backend ships by default**, so `library="ak"`,
`"pd"`, `"np"` and `"arrow"` all work out of the box. The imports stay lazy, so
`import oxihipo` costs nothing for a backend you never call.

The one piece pip cannot supply is ROOT itself — see
[Dependencies](#dependencies).

### Build from source

Requires the Rust toolchain and [maturin](https://www.maturin.rs).

```sh
cd py
maturin develop --release        # build + install into the active venv
# or: maturin build --release     # produce an abi3 wheel under target/wheels
```

The extension is built with **pyo3 0.29** and **rust-numpy 0.29**, with an
`abi3-py310` floor — so one `abi3` wheel per OS/arch works across CPython ≥ 3.10.
pyo3 0.29 supports current CPython natively; only for an interpreter *newer* than
it knows do you need `PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1`.

## Dependencies

`pip install oxihipo` installs all of these:

| package | powers |
|---|---|
| `numpy >= 1.24` | the columnar buffers themselves; `library="np"` |
| `awkward >= 2.6` | `array` / `arrays` (`library="ak"`), and the pandas + ROOT paths |
| `pandas >= 2.0` | `library="pd"` |
| `pyarrow >= 14` | `library="arrow"`, assembled directly — no awkward on the polars / duckdb path |

**ROOT is the exception.** `rdataframe` / `iterate_rdataframe` need a working
ROOT/PyROOT, which is not on PyPI — install it via conda-forge
(`conda install -c conda-forge root`) or your system. The `oxihipo[root]` extra
covers only the awkward side, which you already have.

**Two extras are real**, being genuinely optional and not small:

| extra | powers |
|---|---|
| `oxihipo[dask]` | `to_dask()` — a lazy `dask-awkward` array over the chain |
| `oxihipo[vector]` | `to_vector()` — Lorentz-vector behaviours |

`oxihipo[all]` pulls both. The `[awkward]`, `[pandas]` and `[arrow]` extras still
resolve so old install commands keep working, but they are no-ops now — those
ship by default.

Nothing above is imported at `import oxihipo` time — each backend is imported on
first use, so an unused one costs only disk. If you need the minimal footprint,
`pip install --no-deps oxihipo numpy` still gives you the `numpy()` /
`read_columns()` paths.
