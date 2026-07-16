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

Not yet on PyPI. Build the wheel from the repo with
[maturin](https://www.maturin.rs) and the Rust toolchain:

```sh
git clone https://github.com/mathieuouillon/oxihipo
cd oxihipo/py
maturin develop --release        # build + install into the active venv
# or: maturin build --release    # produce an abi3 wheel under target/wheels
```

The extension is `abi3` — one wheel per OS/arch works across CPython ≥ 3.9. If
your interpreter is newer than the pinned pyo3 knows about, build with
`PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1`.

### Optional dependencies

NumPy is required. Each extra pulls in everything its backend actually imports,
so one `pip install oxihipo[<extra>]` gives you a working backend:

| Extra | Pulls in | For |
|---|---|---|
| *(base)* | `numpy >= 1.24` | `numpy()` — raw buffers, no Awkward import |
| `oxihipo[awkward]` | `awkward >= 2.6` | `array` / `arrays` (`library="ak"`, the default) |
| `oxihipo[pandas]` | awkward + pandas | `library="pd"` |
| `oxihipo[arrow]` | `pyarrow >= 14` | `library="arrow"` — assembled directly with pyarrow, no awkward needed on the polars/duckdb path |
| `oxihipo[all]` | awkward + pandas + pyarrow | everything |

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

Runnable scripts live in
[`py/examples/`](https://github.com/mathieuouillon/oxihipo/tree/main/py/examples)
(`quickstart.py`, `analysis.py`, `streaming.py`, `parallel.py`).

## Where to go next

- [Reading](../python/reading.md) — `arrays`, `array`, `numpy`, bank proxies, `library=`
- [Streaming](../python/streaming.md) — `iterate` and `step_size`
- [Parallel reading](../python/parallel.md) — `workers=N` for I/O-bound filesystems
- [How it works](../python/how-it-works.md) — the zero-copy path, and what it costs
