---
id: cross-implementation
title: vs C++ and Java
sidebar_position: 4
---

# oxihipo vs the C++ and Java readers

The other pages compare oxihipo against itself — formats, filesystems, core
counts. This one compares it against the **other two HIPO implementations**,
the C++ `hipo4` reader and the Java `jnp-hipo4` reader, plus oxihipo's own
Python bindings.

It exists because the front page claims read throughput that "meaningfully
exceeds the C++ reader", and a claim like that should be a measurement rather
than folklore. The measurement turns out to be more interesting — and less
uniformly flattering — than the claim.

:::info The short version
Four implementations, five read scenarios, two API shapes. **Java is quick once
its JIT is warm and wins several rows; oxihipo's decisive advantage is cold
start.** **C++ wins outright when you read many columns per event** — and the
answer to that is oxihipo's columnar API, which is also what the Python binding
drives.
:::

## How a run is proved valid

Every program prints a **checksum** of everything it read. A run only counts if
all four agree. That is what rules out the failure mode that quietly ruins
cross-implementation benchmarks: one of them skipping a bank, or stopping early,
and looking fast for it.

| scenario | checksum |
| --- | --- |
| iterate only | `0.000` |
| one small bank | `19999900000.000` |
| one column | `8400000.000` |
| two columns | `69200102.851` |
| all eight columns | `2170341596.656` |

The gate is not decorative. While tuning the Rust reduction, accumulating float
columns in `f32` instead of `f64` was measurably faster — and produced
`69199880` against the expected `69200102.851`. It was a precision change
disguised as a speedup, and the gate caught it.

## The fixture

200 000 events, three banks — `REC::Particle` (8 columns), `REC::Event` (2), and
`REC::Calorimeter` (3, present on a third of events) — with 1–5 particles per
event, which is what makes the per-column results below behave the way they do.

| format | size | vs uncompressed |
| --- | --- | --- |
| `none` | 27.1 MB | 1.00× |
| `lz4` | 16.3 MB | 1.66× |
| `lz4best` | 13.2 MB | 2.05× |
| `lz4percolumn` | 7.7 MB | **3.51×** |

## The two API shapes

Most of the spread below is API shape, not language. Read this before the
tables.

| | how you read | implementations |
| --- | --- | --- |
| **per-event** | loop events, ask a bank for values | Rust, C++, Java |
| **columnar** | one bulk pass materialises whole columns | Rust `read_columns`, Python |

Within the per-event style the accessors still differ: oxihipo's
`bank.read(handle)` hands back a **typed column slice**, while C++ and Java
expose **per-element** getters. That difference cuts both ways, as the
all-columns row shows.

## Per-event reads

Warm (best of 10 in-process passes) with cold (first pass in a fresh process) in
parentheses, milliseconds, LZ4, Apple Silicon:

| scenario | Rust | C++ | Java |
| --- | --: | --: | --: |
| iterate only, no bank | **6.0** (8.8) | 16.3 (17.0) | 6.7 (25.2) |
| one small bank past a big one | 11.8 (14.1) | 16.9 (17.9) | **8.6** (33.7) |
| one column of eight | 13.0 (13.8) | 16.9 (18.0) | **10.1** (43.6) |
| two columns | 15.8 (17.2) | 17.3 (18.3) | **9.7** (40.0) |
| all eight columns | 33.0 (35.2) | **19.9** (20.7) | 16.4 (63.5) |

Three things are worth pulling out.

**Java is fast warm and slow cold.** Once the JIT has settled it wins three of
five rows. But the cold column is 2.6–4.3× worse — 40.0 ms against 17.2 ms on
the two-column read. Cold is not a corner case: it is exactly what a short
analysis job, a per-file batch worker, or a CI step pays, because each one is a
fresh JVM.

**C++ is the flattest.** It is slowest where there is little to do — `next(banks)`
materialises the banks you asked for whether or not you read them, so
"iterate only" costs it 16.3 ms — but it barely moves as columns are added,
17.3 → 19.9 ms from two columns to eight.

**oxihipo's weak spot is many columns per event.** `bank.read(handle)` sets up a
column view per call; with 1–5 rows per event that setup never amortises, and
eight of them per event dominate. Hence 33.0 ms, 1.7× slower than C++.

:::tip If you read many columns, don't loop events
That row is the per-event API being used against its grain. The columnar API
below reads the same eight columns in 16.9 ms serial and 7.1 ms across cores.
:::

## Columnar reads

The same work through `read_columns`, which is also the path the Python binding
drives. Warm milliseconds:

| scenario | Rust, 1 thread | Python, 1 thread | Rust, all cores | Python, all cores |
| --- | --: | --: | --: | --: |
| one small bank | 7.1 | 6.8 | **2.9** | 3.5 |
| one column | 6.7 | 8.3 | 6.6 | **3.6** |
| two columns | 9.5 | 9.2 | **4.4** | 4.6 |
| all eight columns | 16.9 | 14.8 | **7.1** | 7.3 |

**The Python binding costs roughly nothing.** Serial columnar Python lands
within 0.87–1.10× of native Rust, because the decode runs in Rust with the GIL
released and the columns move into NumPy zero-copy. On the eight-column read
Python is actually *faster* (14.8 against 16.9 ms) — NumPy's vectorised reduce
beats the plain `f64` accumulation loop the Rust program uses. That is a
statement about the reduction, not the reader.

## Compression

Two-column per-event read, warm milliseconds:

| format | Rust | C++ | Java |
| --- | --: | --: | --: |
| `none` | 13.1 | 16.5 | **8.3** |
| `lz4` | 16.4 | 17.6 | **11.2** |
| `lz4best` | 15.6 | 16.5 | **10.2** |
| `lz4percolumn` | **12.8** | n/a | n/a |

`lz4percolumn` is the smallest on disk *and* the fastest to read — it inflates
at column granularity, so a two-column read never touches the other six. The
released C++ and Java readers reject it, correctly: they have no decoder for
wire tag 7. See [Compression](./compression.md).

## Two traps that produced wrong numbers

Both of these were caught only because the results looked implausible. If you
re-run this, check them first.

**Name-taking accessors.** An early version had the C++ loop call
`getInt("pid", row)`, which hashes the column name *per value*. That alone made
C++ look 3.5× slower (38.8 ms against 11.2 ms). Every implementation now
resolves its column indices once, outside the loop.

**Implicit threading.** The Python binding defaults to `threads=0`, meaning
every core, while the Rust call was passing `1`. That made Python appear 2×
faster than the Rust code it calls. Both now take an explicit thread count, and
serial and parallel results are reported separately.

A third, smaller one: oxihipo originally reused a single open file across
iterations while C++ and Java reopened, so it started each pass with warm
scratch buffers the others did not have. All four now reopen per iteration, with
open and dictionary parsing outside the timed region.

## Reproducing it

The programs, the fixture generator, the raw results, and the full command list
live in
[`benches/cross-impl/`](https://github.com/mathieuouillon/oxihipo/tree/main/benches/cross-impl).
In outline:

```sh
# fixtures, one per format
cargo run --release --bin xfixture -- /tmp/xb_lz4.hipo lz4 200000

# Rust — prefix a scenario with c_ for the columnar API; threads 0 = all cores
cargo run --release --bin xbench_rs -- /tmp/xb_lz4.hipo scan2 10 1
cargo run --release --bin xbench_rs -- /tmp/xb_lz4.hipo c_wide 10 0

# C++ (against a built hipo-cpp)
clang++ -std=c++17 -O3 -DNDEBUG -I$HIPO_CPP xbench_cpp.cc -o xbench_cpp \
    -L$HIPO_CPP/build/hipo4 -lhipo4 -Wl,-rpath,$HIPO_CPP/build/hipo4
./xbench_cpp /tmp/xb_lz4.hipo scan2 10

# Java (against the jnp-hipo4 jar)
javac -cp $JAR -d . XBenchJava.java && java -cp "$JAR:." XBenchJava /tmp/xb_lz4.hipo scan2 10

# Python
python xbench_py.py /tmp/xb_lz4.hipo scan2 10 1
```

Each prints `impl · scenario · cold · warm · events · checksum`.

:::warning Run them serially
Concurrent runs contend for memory bandwidth and the numbers stop meaning
anything. And before believing any of it, check that every implementation
printed the same checksum for a given scenario.
:::

## Scope

One machine, one file shape, LZ4-family compression, read-only. It does not
cover writing, multi-file chains, or filtered and tagged reads. Fixed-length
array columns (`T#N`) are absent because the released C++ reader has no support
for them, so that comparison cannot be made against it today.

Nothing here is a promise about your workload — CLAS12 files vary enormously in
bank count, multiplicity, and record size. It is evidence that the approach
works, and a harness you can point at your own data.
