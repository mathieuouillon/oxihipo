---
id: how-it-works
title: How it works
sidebar_position: 4
---

# How it works

The whole per-event loop runs in **Rust with the GIL released**.

One pass over the file materializes each requested column into a flat NumPy
buffer, plus one shared `int64` offsets buffer per bank — which is exactly an
Awkward `ListOffsetArray` / `Index64` layout. Those buffers are *moved* into
NumPy (NumPy takes ownership of the Rust `Vec`'s allocation and frees it through
Rust when collected), so nothing is copied past decompression.

The Python layer only **wraps** those buffers:

| Column shape | Awkward layout |
|---|---|
| scalar column | `ListOffsetArray(Index64, NumpyArray)` |
| `T#N` fixed-size array column | `ListOffsetArray(Index64, RegularArray(NumpyArray))` |
| a whole bank | `ListOffsetArray(Index64, RecordArray([...]))` |

Python never iterates events. That's the entire performance story: the loop is
Rust, the buffers are moved not copied, and the wrapping is O(number of
columns).

## Architecture

```
oxihipo (pure Python)        ← ergonomics: arrays/array/numpy/iterate, library=
      │                        assembly into ak / np / pd / arrow
      ▼
oxihipo._oxihipo (compiled)  ← PyO3 + rust-numpy; exposes Chain.read_columns
      │                        GIL released around the read
      ▼
oxihipo (Rust core)          ← Chain, records, LZ4, columnar materializer
```

`read_columns` is the whole compiled surface that matters. Everything Pythonic —
bank proxies, globs, `library=`, streaming — is pure Python layered on top, which
is why most of the binding can change without a Rust rebuild.

## What it costs

About 10%. Reading `REC::Particle` `px,py,pz,pid` from a 9.1 GB CLAS12 file
(598,738 events, 4.7 M particles; Apple M4 Pro, all cores, warm cache):

| | Throughput | vs Rust |
|---|--:|--:|
| Rust `read_columns` | 6.3 GB/s | 1.00× |
| Python `read_columns` (NumPy) | 5.8 GB/s | 0.91× |
| Python `arrays` (Awkward) | 5.6 GB/s | 0.89× |

The gap is one PyO3 call plus the zero-copy `into_pyarray`; Awkward wrapping
adds roughly 2% on top. Method and reproduction:
[Python vs Rust benchmark](../design/python-vs-rust-benchmark.md).

Note the overhead is already present in the pure-NumPy path, which does no
Awkward or Arrow assembly at all — so it isn't the array library's fault, and a
native Arrow exporter wouldn't reclaim it.

## The arrow path

`library="arrow"` builds the `pyarrow.Table` **directly** from the returned
NumPy buffers — `pa.array(offsets)` and `pa.array(values)` wrap them zero-copy,
`LargeListArray.from_arrays` gives the jagged column, and `FixedSizeListArray`
handles `T#N` array columns. No Awkward round-trip, so the polars/duckdb path
doesn't need awkward installed at all.

The emitted schema is plain `large_list<...>` rather than Awkward's
extension types. That's better for polars and duckdb, but it means an
arrow → awkward round-trip won't carry Awkward-specific metadata.

## Errors

`HipoError` maps onto a Python exception tree, so you catch what you'd expect
to catch:

| Situation | Exception |
|---|---|
| missing bank or column | `KeyError` |
| dtype mismatch | `TypeError` |
| I/O failure | `OSError` |
| malformed record | `oxihipo.CorruptFileError` |

`CorruptFileError` derives from `oxihipo.OxihipoError`, so `except OxihipoError`
catches everything library-specific.

## Typing

The package ships a `py.typed` marker (PEP 561) and a stub for the compiled
extension, so `mypy` sees the real surface rather than `Any`. Both `mypy` and
`mypy.stubtest` run in CI against a freshly built wheel, along with a check that
the `py.typed` marker actually ships.
