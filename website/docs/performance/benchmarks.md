---
id: benchmarks
title: Benchmarks
sidebar_position: 3
---

# Benchmarks

Every number here was measured, and each table says on what. Throughput depends
heavily on the file, the filesystem, and how many banks the analysis touches —
so treat these as evidence that the approach works, not as a promise about your
workload. Reproduce with `bench_par` on your own data.

## Headline: by-bank partial decompression on JLab ifarm

A 29.7 GB CLAS12 skim file (`pi0_skim_CxC_Outbending_slice000.hipo`, 1.85 M
events, 29,430 records) on `ifarm2401` (64 logical cores). `bench_par` reads
`REC::Particle.rows()` only — exactly the partial-decompression case the by-bank
format is designed for.

| Location | Format | Sequential | par=10 | par=32 | par=64 | Size |
|---|---|---:|---:|---:|---:|---:|
| `/volatile` (Lustre, hot) | `Lz4` baseline | 72 kev/s | 1,437 | — | — | 29.7 GB |
| `/volatile` (Lustre, hot) | **by-bank** | 437 | 14,724 | 27,643 | **36,578** | **6.66 GB** (−77.6%) |
| `/scratch` (local SSD) | `Lz4` baseline | 159 | 1,112 | — | — | 29.7 GB |
| `/scratch` (local SSD) | by-bank | 1,695 | 7,558 | — | — | 6.66 GB |

*(rates in kev/s)*

`par=64` on `/volatile` reaches **36.6 Mev/s** — 25× the `Lz4` baseline
throughput at par=10.

:::note Measured on the original by-bank variant
These were measured on the first by-bank format (fast default-LZ4 streams).
`Lz4ByBankV2` — what you write today — shares the layout with HC-compressed
streams: selective-read speed is the same (the figures carry over) and the file
is *smaller* than the 6.66 GB shown. `Lz4PerColumn` inflates at column
granularity and is smaller still.
:::

:::caution The −77.6% is not typical
That compression ratio is exceptional **for skim files**: near-identical
per-event topology gives the per-bank streams enormous cross-event redundancy to
dedup. On generic reco files expect closer to ±5%.
:::

Reading the matrix:

- **`/volatile` beats `/scratch` in parallel** here because the by-bank file was
  ifarm-page-cache-hot from a just-completed recook. Cold-read Lustre numbers
  (after the cache evicts) land closer to the `/scratch` row.
- **Sequential is permanently Lustre-bound on `/volatile`** — single-stream RPCs
  cap around 400–500 kev/s regardless of format. Stage to `/scratch` for
  sequential work.
- **Thread scaling is linear well past `num_cpus`** for the by-bank format on
  Lustre.

## Format comparison on a local SSD

1.1 GB CLAS12 file (`rec0.hipo`, 289 k events, 195 records, local SSD).
`bench_par` reads `REC::Particle.rows()` only.

| Format | Sequential | Parallel | Size |
|---|---:|---:|---:|
| `Lz4` baseline | 980 kev/s | 5,073 kev/s | 1,135 MB |
| **by-bank** | **4,025 kev/s (4.1×)** | **15,675 kev/s (3.1×)** | **1,225 MB (+8%)** |

That 4×/3× gap over whole-record `Lz4` is the whole thesis of
[the compression page](./compression.md): not decompressing the ~85% of banks
you never read beats decompressing them faster. (Same-variant caveat as above —
`Lz4ByBankV2` reads at this speed and writes smaller.)

## Python vs Rust

Reading `REC::Particle` `px,py,pz,pid` from a 9.1 GB CLAS12 file (598,738
events, 4.7 M particles; Apple M4 Pro, all cores, warm cache):

| | Throughput | vs Rust |
|---|--:|--:|
| Rust `read_columns` | 6.3 GB/s | 1.00× |
| Python `read_columns` (NumPy) | 5.8 GB/s | 0.91× |
| Python `arrays` (Awkward) | 5.6 GB/s | 0.89× |

The per-event decode runs in Rust behind a released GIL and columns move into
NumPy zero-copy, so the binding costs about 10%. Full method and reproduction:
[Python vs Rust benchmark](../design/python-vs-rust-benchmark.md).

## Multi-process reading

`workers=N` targets the per-process I/O ceiling on a parallel filesystem. On a
**page-cached local file** it is *slower* — the bottleneck there is decode, not
I/O, so extra processes only add spawn and IPC cost. Measured on the same 9.1 GB
file (Apple M4 Pro, page-cached), `workers=1` at 1.41 s beats `workers=2` at
1.71 s.

This is the design working as intended, not a regression: see
[Parallel reading](../python/parallel.md). Benchmark `workers=` on the farm, not
on your laptop.

## Baseline sequential scan

Validated on a 1.7 GB CLAS12 file (`rec_clas_022050.evio.00000.hipo`): a
sequential `Chain::events()` scan reads all 187,941 events at ~257 kev/s.
`Chain::for_each` fans the same scan across cores.

## Reproducing

```sh
# Rust throughput, threads = 0 → all cores
cargo run --release --example bench_par -- /path/to/file.hipo 0

# Convert a file first if you want the by-bank (Lz4ByBankV2) numbers
cargo run --release --example recook_by_bank -- in.hipo out_by_bank.hipo
cargo run --release --example bench_par -- out_by_bank.hipo 0

# Python vs Rust columnar read
cargo run --release --example bench_columns -- /path/to/file.hipo
python py/examples/bench_columns.py /path/to/file.hipo
```
