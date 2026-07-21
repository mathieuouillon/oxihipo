---
id: rdataframe
title: RDataFrame
sidebar_position: 6
---

# ROOT RDataFrame

`oxihipo.rdataframe` hands a HIPO selection to ROOT's
[RDataFrame](https://root.cern/manual/data_frame/) — so you can write ROOT's
declarative C++ analysis (`Define` / `Filter` / `Histo1D`) over CLAS12 banks from
Python. oxihipo reads the columns with the GIL released, then presents them
through [Awkward](https://awkward-array.org)'s generated `RDataSource`: a jagged
bank column becomes an `RVec<T>` column, a `T#N` array column a nested `RVec`,
**without copying the view**.

:::note Requires ROOT + awkward
This path needs `awkward` (its ROOT interop generates the data source) and a
working **ROOT / PyROOT** — i.e. `import ROOT` must succeed in the same
interpreter. Install with `pip install oxihipo[root]` for the awkward side;
ROOT itself is not on PyPI, so get it from conda-forge
(`conda install -c conda-forge root`) or your system. Without it, `rdataframe`
raises a clear `ModuleNotFoundError`.
:::

## Whole file → one RDataFrame

```python
import oxihipo as ox

df = ox.rdataframe("run5042.hipo", "REC::Particle", ["px", "py", "pz", "pid"])
```

The whole selection is read into oxihipo's columnar buffers first; RDF then runs
its implicitly-multithreaded loop over that in-memory view. That's the right
shape for a **per-run analysis that fits in RAM**. For bigger inputs, stream it
(see [below](#bigger-than-ram-stream-per-chunk)).

### Column names

RDataFrame JIT-compiles column names as C++ identifiers, so `::` and `/` (and
any other non-word character) collapse to `_`:

| HIPO key | RDataFrame column |
|---|---|
| `REC::Particle/px` | `REC_Particle_px` |
| `REC::Event/evno` | `REC_Event_evno` |

The bank name is always kept as a prefix (single- and multi-bank selections
behave the same), so a per-event `Define` reads naturally:

```python
h = (df.Define("pt", "sqrt(REC_Particle_px*REC_Particle_px"
                     " + REC_Particle_py*REC_Particle_py)")   # per-particle RVec
       .Histo1D(("pt", "p_{T};p_{T};particles", 100, 0, 10), "pt"))
h.Draw()
```

Because each column is an `RVec`, ROOT's vector operations (`Sum`, `Filter`,
element access, `.size()`) work per event with no Python loop:

```python
df.Define("mult", "(int) REC_Particle_pid.size()")   # particles / event
  .Define("sum_px", "Sum(REC_Particle_px)")           # Σ px over the event
```

Two source columns that would sanitize to the same name (rare) raise a
`ValueError` rather than silently colliding.

## The knobs

`rdataframe` takes the same selection knobs as [`arrays`](./reading.md):

- **`banks`** — a bank name, a list of banks, or `None` for all. A list gives one
  set of columns per bank (all prefixed by their bank name).
- **`columns=`** — restrict to some columns of a single named bank.
- **`filter_name="REC::Particle/p*"`** — a glob over `bank` / `bank/column` keys.
- **`entry_start=` / `entry_stop=`** — a global event range.
- **`threads=`** — rayon threads *within* oxihipo's read (`0` = all cores).

A [`filtered`](./reading.md#selecting-and-writing) chain carries through — filter
first, then materialize only what survives:

```python
f = ox.open("run5042.hipo").filtered(require=["REC::Particle"], event_tag="dvcs")
df = f.rdataframe("REC::Particle")           # only DVCS-tagged events reach RDF
```

Unlike `arrays` (which yields an *empty* array for a non-matching selection), an
RDataFrame with no columns is useless — so a selection that matches nothing
raises a `ValueError`.

## Bigger than RAM: stream per chunk

`iterate_rdataframe` composes [`iterate`](./streaming.md) with the bridge: it
yields one small `RDataFrame` per bounded-memory chunk, so resident memory stays
≈ one chunk. Each chunk is an **independent** RDF, so you book a result per chunk
and merge across chunks yourself — histograms with `Add`, counts by summing.
Clone the first result and detach it so it outlives its chunk:

```python
total = None
for df in ox.iterate_rdataframe("/data/run5042/*.hipo", "REC::Particle", ["px"],
                                step_size="1 GB"):
    h = df.Histo1D(("pt", "", 100, 0, 10), "REC_Particle_px").GetValue()
    if total is None:
        total = h.Clone()
        total.SetDirectory(0)          # detach: outlive this chunk's RDF
    else:
        total.Add(h)
```

`step_size` is an event count or a byte budget (`"1 GB"`), exactly as in
`iterate`; chunks are record- and file-aligned. `report=True` yields
`(df, Report)` pairs. The trade-off vs. the whole-file call is that RDF loses its
single-graph global optimization across chunks — for a single histogram or count
that's immaterial.

## Performance

Two things are worth knowing before you reach for this. Numbers below are a
1 M-event synthetic `REC::Particle` file (3 M particles, LZ4-per-column),
per-particle `pt = √(px²+py²)` filled into a 100-bin histogram — Apple M4 Pro,
single thread, best of 7, warm cache
([`py/examples/bench_rdataframe.py`](https://github.com/mathieuouillon/oxihipo/tree/main/py/examples/bench_rdataframe.py)):

| step | time | throughput |
|---|--:|--:|
| `arrays` read (baseline) | 18.5 ms | 54 Mevt/s |
| `rdataframe` build (read + wrap) | 19.6 ms | **+1.1 ms** over the read |
| `pt` histogram — Awkward / NumPy | 39 ms | 26 Mevt/s |
| `pt` histogram — RDataFrame | 188 ms | 5.3 Mevt/s |

Plus a **one-time cling warmup** of ≈ 0.7 s to generate the `RDataSource`
template and ≈ 0.7 s to JIT the first `Define` — ~1.4 s per process, amortized
over the whole job.

**The bridge itself is free.** `rdataframe` sits ~1 ms on top of the bare
`arrays` read: `ak.to_rdataframe` builds a *no-copy view*, no second
materialization.

**But the RDataFrame loop is not a speed win here.** On this simple kernel,
single-threaded RDF is ~5× slower end-to-end than the vectorized Awkward/NumPy
equivalent — RDF walks the generated source event by event, materializing an
`RVec` per event, where Awkward runs one flat SIMD kernel over the whole column.
And ROOT's usual answer to that — implicit multithreading (`EnableImplicitMT`) —
**does not work with the Awkward-generated source** (the loop hangs), so the RDF
side stays single-threaded.

So the bridge earns its keep by giving you RDF's **declarative API and ecosystem**
— reusing an existing `Define`/`Filter`/`Histo` graph or C++ analysis code on
HIPO data — not by going faster than staying in Awkward. The performant,
multi-threaded route is a native C++ `RHipoDS` (it would own the threads and skip
the per-event source overhead), which is deliberately out of the pure-Python
core's scope.

## When *not* to use this

- **A pure NumPy/Awkward analysis** — you already have the columns from
  `arrays()`; RDataFrame adds a ROOT dependency and (per the numbers above) runs
  slower single-threaded, for no gain. Use the bridge to reuse RDF/C++ code, not
  to speed up an analysis you'd otherwise write in Awkward.
- **`ROOT.RDF.FromNumpy`** handles only flat, equal-length scalar columns — it
  cannot represent a jagged bank, which is most of CLAS12. This bridge exists
  precisely to carry the jaggedness.
- **`ROOT.RDF.FromArrow`** is an alternative: oxihipo also emits a
  `pyarrow.Table` (`arrays(..., library="arrow")`) with `large_list` columns.
  It works, but Awkward's path is more mature for deeply-jagged / `T#N` data and
  needs no Arrow on the analysis side.

A runnable script is in
[`py/examples/rdataframe.py`](https://github.com/mathieuouillon/oxihipo/tree/main/py/examples/rdataframe.py).
