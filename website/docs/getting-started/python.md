---
id: python
title: Python
sidebar_position: 2
---

# Getting started with Python

A HIPO bank reads like a [uproot](https://uproot.readthedocs.io) jagged branch,
and columns come back as [Awkward](https://awkward-array.org) arrays — built
zero-copy from buffers the Rust side fills with the GIL released.

## Install

```sh
pip install oxihipo
```

Wheels ship for Linux, macOS, and Windows (CPython ≥ 3.10). That one command is
the whole install — **every `library=` backend comes with it**, so `"ak"` / `"np"`
/ `"pd"` / `"arrow"` all work out of the box. Two things are opt-in, and they are
listed below: `[dask]` and `[vector]`.

Or build from source with [maturin](https://www.maturin.rs) and the Rust
toolchain:

```sh
git clone https://github.com/mathieuouillon/oxihipo
cd oxihipo/py
maturin develop --release        # build + install into the active venv
# or: maturin build --release    # produce an abi3 wheel under target/wheels
```

The extension is `abi3` — one wheel per OS/arch works across CPython ≥ 3.10. If
your interpreter is newer than the pinned pyo3 knows about, build with
`PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1`.

### What comes with it

| Package | Powers |
|---|---|
| `numpy >= 1.24` | the columnar buffers themselves; `numpy()` and `library="np"` |
| `awkward >= 2.6` | `array` / `arrays` (`library="ak"`, the default), and the pandas + ROOT paths |
| `pandas >= 2.0` | `library="pd"` |
| `pyarrow >= 14` | `library="arrow"` — assembled directly with pyarrow, no awkward on the polars/duckdb path |

None of these is imported when you `import oxihipo`; each loads on first use, so
a backend you never call costs only disk.

:::warning ROOT is not on PyPI
`rdataframe` / `iterate_rdataframe` additionally need a working ROOT with
PyROOT, which pip cannot install. Get it from conda-forge
(`conda install -c conda-forge root`) or your system package manager. The
`oxihipo[root]` extra only covers the awkward side, which you already have.
:::

### Two extras that are not no-ops

Everything in the table above ships by default. These two do not, being
genuinely optional and not small:

| extra | powers |
|---|---|
| `pip install oxihipo[dask]` | [`to_dask()`](../python/reading.md#dask) — a lazy `dask-awkward` array over the chain |
| `pip install oxihipo[vector]` | [`to_vector()`](../python/reading.md#lorentz-vectors) — Lorentz-vector behaviours |

`oxihipo[all]` pulls both.

:::note
The `[awkward]`, `[pandas]` and `[arrow]` extras still resolve so older install
commands keep working — those *are* no-ops now, since they ship by default. For a
minimal footprint, `pip install --no-deps oxihipo numpy` still gives the
`numpy()` / `read_columns()` paths.
:::

## Read your first file

```python
import oxihipo as ox

f = ox.open("run5042.hipo")     # file | dir | glob | list of paths
f.num_entries                   # event count
f.keys()                        # ['REC::Particle', 'REC::Event', ...]

p = f.arrays("REC::Particle", ["pid", "px", "py", "pz"])
p.px                            # jagged: p[event].px indexes particles
ak.sum(p.px, axis=1)            # per-event reductions, no Python loop
```

Bigger than RAM? Stream it — resident memory stays at about one chunk:

```python
for chunk in f.iterate("REC::Particle", ["px"], step_size="200 MB"):
    hist.fill(ak.flatten(chunk.px))
```

Write a file, or *decorate* an existing one with a derived bank:

```python
with ox.create("out.hipo") as w:
    w.new_bank("NEW::bank", {"px": "F", "pid": "I"})
    w.extend({"NEW::bank": {"px": p.px, "pid": p.pid}})   # columnar, zero-copy

# add an ML score to a cooked file without rewriting the physics banks:
w = ox.update("dst.hipo", "decorated.hipo")
w.new_bank("ML::pred", {"score": "F"})
w.extend({"ML::pred": {"score": scores}})                 # one per source event
w.close()
```

Turn columns into physics without hardcoding constants or writing joins:

```python
p  = f.arrays("REC::Particle", ["pid", "px", "py", "pz"])
v  = ox.to_vector(p, mass="pdg")     # masses from pid; E, pt, eta, invariant mass
ev = ox.link(f.arrays(["REC::Particle", "REC::Calorimeter"]))
ev["REC::Calorimeter"].particle.px   # follow pindex instead of masking by hand
```

And run the analysis itself across processes, not just the read:

```python
h = f.map_reduce(analyze, "REC::Particle", workers=8)   # analyze runs IN the worker
```

Runnable scripts live in
[`py/examples/`](https://github.com/mathieuouillon/oxihipo/tree/main/py/examples)
— each runs against the bundled sample with no arguments:
`quickstart.py`, `analysis.py` (columnar cuts + reductions), `streaming.py`,
`parallel.py`, `writing.py`, `decorate.py`, `event_tags.py`, `interop.py`
(pandas / Arrow / polars / duckdb), `rdataframe.py`, plus the `bench_*.py`
benchmarks.

## Where to go next

- [Reading](../python/reading.md) — `arrays`, `array`, `numpy`, bank proxies, `library=`
- [PDG masses](../python/reading.md#pdg-masses) — `pdg_mass` / `pdg_name`, and the two CLAS12 codes that break other helpers
- [Lorentz vectors](../python/reading.md#lorentz-vectors) — `to_vector`: `E`, `pt`, `eta`, invariant mass
- [`pindex` joins](../python/reading.md#joining-detector-banks-by-pindex) — `group_by_index` / `link`, detector banks ↔ particles
- [Writing](../python/writing.md) — `create` / `recreate` / `update`, `new_bank` / `extend`
- [RDataFrame](../python/rdataframe.md) — `rdataframe` / `iterate_rdataframe`, ROOT's declarative dataframe over HIPO
- [Streaming](../python/streaming.md) — `iterate` and `step_size`
- [Parallel reading](../python/parallel.md) — `workers=N` for I/O-bound filesystems
- [How it works](../python/how-it-works.md) — the zero-copy path, and what it costs
