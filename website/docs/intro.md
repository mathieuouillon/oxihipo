---
id: intro
title: Introduction
sidebar_position: 1
slug: /intro
---

# Introduction

**oxihipo** is a pure-Rust reader and writer for the **HIPO v6** binary container
used at Jefferson Lab CLAS12. It is built so that read throughput meaningfully
exceeds the C++ `hipo4` reader on the same hardware, with an API that fits Rust
idioms — and it ships a columnar, [uproot](https://uproot.readthedocs.io)-shaped
Python binding on top.

It reads and writes HIPO version 6 files. Physics, FFI, ROOT, and XRootD layers
are intentionally out of scope.

## Which language?

Both front ends read the same files through the same Rust core.

| | Use it when | Start here |
|---|---|---|
| **Rust** | You want the fastest possible event loop, are writing files, or are building an analysis binary. | [Getting started → Rust](./getting-started/rust.md) |
| **Python** | You want banks as [Awkward](https://awkward-array.org) arrays for interactive analysis, histogramming, or a notebook. | [Getting started → Python](./getting-started/python.md) |

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
   [`Lz4ByBank`](./performance/compression.md) format stores each bank as its
   own stream and inflates it only when `ev.bank(name)` asks for it — a real
   analysis touches maybe 5 of ~30 banks, so the other ~85% of LZ4 work simply
   never happens.

That third point is why the headline number on this site is a 25× throughput
improvement rather than a few percent. See
[Benchmarks](./performance/benchmarks.md) for the full tables and the hardware
they were measured on.

## Scope and status

- A single `oxihipo` library crate. No bundled binary — downstream consumers
  build whatever frontend they need on top.
- `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` are all clean, and every PR runs them.
- Not yet published to crates.io or PyPI; install
  [from git](./getting-started/rust.md) or build the Python wheel
  [with maturin](./getting-started/python.md).

### Known gaps

- `SortedWriter` and `StreamWriter` (per-tag bin writers, auto-flush) —
  deferred.
- A bench-vs-`hipo4` comparator — deferred.
- **Sub-chunked `Lz4ByBank`**: combining `Lz4Chunked`-style intra-stream
  parallelism with per-bank streams, for very large records where a single
  bank's stream is multi-MB. Per-bank streams already parallelise *across* banks
  in `for_each`; this is the next step if profiles say one bank dominates.

## License

[MIT](https://github.com/mathieuouillon/oxihipo/blob/main/LICENSE).
