---
id: intro
title: Introduction
sidebar_position: 1
slug: /intro
---

# Introduction

[![PyPI](https://img.shields.io/badge/pypi-v0.7.1-006dad)](https://pypi.org/project/oxihipo/)
[![Python](https://img.shields.io/badge/python-3.10%2B-3776ab)](https://pypi.org/project/oxihipo/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/mathieuouillon/oxihipo/blob/main/LICENSE)

That is the current release — see the [release notes](./release-notes.md) for
what changed in it.

**oxihipo** is a pure-Rust reader and writer for the **HIPO v6** binary container
used at Jefferson Lab CLAS12. It is built so that read throughput meaningfully
exceeds the C++ `hipo4` reader on the same hardware, with an API that fits Rust
idioms — and it ships a columnar, [uproot](https://uproot.readthedocs.io)-shaped
Python binding on top.

It reads and writes HIPO version 6 files. FFI and XRootD layers are intentionally
out of scope; ROOT is reachable through [RDataFrame](./python/rdataframe.md), and
the Python side carries a thin *analysis* layer — PDG masses, Lorentz vectors and
`pindex` joins — described below. The Rust crate itself stays physics-free.

## Which language?

Both front ends read the same files through the same Rust core.

| | Use it when | Start here |
|---|---|---|
| **Rust** | You want the fastest possible event loop, are writing files, or are building an analysis binary. | [Getting started → Rust](./getting-started/rust.md) |
| **Python** | You want banks as [Awkward](https://awkward-array.org) arrays for interactive analysis, histogramming, or a notebook — plus PDG masses, Lorentz vectors, `pindex` joins and parallel `map_reduce` on top. | [Getting started → Python](./getting-started/python.md) |

## What makes it fast

Three things do most of the work, and they're worth understanding before you
tune anything:

1. **Nothing is copied that doesn't have to be.** `bank.col::<T>("name")`
   borrows straight from the decompressed record buffer when the bytes are
   aligned to `T` — always for 4-byte types. In Python, those same buffers move
   into NumPy zero-copy.
2. **Nothing is resident that doesn't have to be.** Records stream one at a time
   via `pread` into a recycled buffer. The file is never mapped or read whole,
   so a 100 GB scan holds about one record in memory (one per worker in
   parallel mode).
3. **Nothing is decompressed that you don't read.** This is the big one on
   ifarm. The stock HIPO format stores one LZ4 block per record, so reading any
   bank inflates *every* bank. The opt-in
   [`Lz4PerBank`](./performance/compression.md) (and `Lz4PerColumn`) formats
   store each bank (or column) as its own stream and inflate it only when
   `ev.bank(name)` asks for it — a real analysis touches maybe 5 of ~30 banks,
   so the other ~85% of LZ4 work simply never happens.

That third point is why the headline number on this site is a 25× throughput
improvement rather than a few percent. See
[Benchmarks](./performance/benchmarks.md) for the full tables and the hardware
they were measured on.

## Past reading: the analysis layer

Reading columns quickly is only useful if the physics on top is not the new
bottleneck. On the Python side that means:

- **`pdg_mass`** — masses from `pid`, vectorized, including the two CLAS12 codes
  that break general PDG helpers (`0`, and the Geant3 nuclei `45`–`49`).
- **`to_vector`** — Lorentz-vector behaviours, so `E`, `pt`, `eta` and invariant
  mass come from [vector](https://vector.readthedocs.io) rather than by hand.
- **`group_by_index` / `link`** — the `pindex` join between detector banks and
  particles, for every particle at once instead of one hardcoded row.
- **`map_reduce`** — runs *your* function in the worker processes, so the
  analysis is parallel too, not just the read.
- **`to_dask`** — a real `dask-awkward` source: lazy, with known entry
  boundaries and column projection.

See [Reading](./python/reading.md) and
[Parallel reading](./python/parallel.md).

## Scope and status

- A single `oxihipo` library crate. No bundled binary — downstream consumers
  build whatever frontend they need on top.
- `cargo test`, `cargo clippy --all-targets -- -D warnings` and
  `cargo fmt --check` are all clean, and every PR runs them on Linux, macOS and
  Windows — alongside the declared MSRV, every feature combination, `cargo-deny`,
  the doctests, a build of the fuzz targets, and a smoke-run of the examples.
- The Python package is on PyPI (`pip install oxihipo`). The Rust crate is not
  yet on crates.io — depend on it [from git](./getting-started/rust.md).

### Known gaps

- `SortedWriter` and `StreamWriter` (per-tag bin writers, auto-flush) —
  deferred.
Intra-stream parallel inflate used to be listed here. It is **closed as
not-worth-doing**, on measurement: the parallel work unit is a whole record, not
a stream, so record *count* — already tunable with `max_record_bytes` — is what
limits a scan. See
[Record size and parallel scaling](./performance/compression.md#record-size-and-parallel-scaling).

## License

[MIT](https://github.com/mathieuouillon/oxihipo/blob/main/LICENSE).
