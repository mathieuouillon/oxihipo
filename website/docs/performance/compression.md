---
id: compression
title: Compression formats
sidebar_position: 1
---

# Compression formats

Once page-fault stalls are masked, **LZ4 inflate dominates wall time** on ifarm.
oxihipo reads and writes the four stock HIPO codecs (`hipo4`-compatible) and adds
two opt-in format extensions that attack that decode cost by breaking the record
into smaller independently-compressed units — by bank, or by column — so the
reader stops inflating data it never reads.

## The problem with one block per record

The stock HIPO format stores **one LZ4 block per record**. Two consequences
follow, and both hurt:

1. A record's decompress is one sequential pass on one worker. Idle cores on the
   same record can't help.
2. You cannot decompress *part* of a record without inflating all of it. Reading
   one bank costs you every bank.

## The full menu

Every mode oxihipo can write, in wire-tag order. The Rust name is the
[`Compression`](../rust/writing.md) variant; the Python name is the string
[`skim`](../python/reading.md) accepts.

| Mode (`Compression::`) | Python (`skim=`) | `hipo4`? | What it does |
|---|---|:---:|---|
| `None` | `"none"` | ✅ | Uncompressed. |
| `Lz4` | `"lz4"` | ✅ | Stock LZ4, one block per record. |
| `Lz4Best` | `"lz4best"` | ✅ | LZ4 high-compression. Needs the `lz4-c` feature; without it, falls back to standard LZ4 (identical output to `Lz4`). |
| `Gzip` | `"gzip"` | ✅ | Stock gzip, one block per record. |
| `Lz4PerBank` | `"lz4perbank"` | ❌ | One LZ4-HC stream per bank, plus a compressed directory; inflate only the banks you read. |
| `Lz4PerColumn` | `"lz4percolumn"` | ❌ | One LZ4-HC stream per `(bank, column)`; best ratio and finest-grained selective reads. |

The two extensions (`Lz4PerBank`, `Lz4PerColumn`) carry wire tags 6 and 7 that
the C++ `hipo4` reader doesn't understand — use them for Rust-only, or
oxihipo-Python-only, consumers. The four stock codecs stay byte-compatible with
`hipo4`.

:::note Two older extensions were removed
Earlier versions also shipped `Lz4Chunked` (parallel-inflate-everything, tag 4)
and `Lz4ByBank` v1 (tag 5). Both are superseded — `Lz4PerBank` does everything
v1 did with smaller files, and `Lz4PerColumn` goes finer still — so they were
removed. A file written in either old format is now **rejected on read**.
:::

## Stock formats (`hipo4`-compatible)

`None`, `Lz4`, `Lz4Best`, and `Gzip` are the standard HIPO codecs. Reach for
these when a `hipo4`-based tool has to read the output.

```rust
use oxihipo::{Compression, Writer};

let mut w = Writer::create("out.hipo")
    .schemas(dict)
    .compression(Compression::Lz4)   // or None / Lz4Best / Gzip
    .build()?;
```

- **`Lz4`** is the everyday choice: fast to write, fast to read, universally
  readable.
- **`Lz4Best`** trades write speed for a smaller file at the same read speed. It
  routes to `LZ4_compress_HC` **only when the `lz4-c` feature is enabled**;
  without that feature it silently falls back to standard LZ4 and produces
  exactly the same bytes as `Lz4`.
- **`Gzip`** compresses tighter than stock `Lz4` but is markedly slower to
  inflate — rarely the right trade for an analysis you'll read many times.

None of these solve the one-block-per-record problem; the extensions below do.

## `Lz4PerBank` — decompress only the banks you read

**This is usually the one you want.** Real analyses touch 2–5 banks out of ~30;
the other ~85% is wasted LZ4 work.

`Compression::Lz4PerBank` stores each bank type as its own LZ4-HC stream within
the record, plus an event×bank presence directory (itself LZ4-compressed,
prefixed with an extension-format-version byte). The reader parses the directory
eagerly but inflates a bank's stream only when `ev.bank(name)` actually asks for
it. Banks you never touch stay compressed for the record's lifetime.

```rust
let mut w = Writer::create("out.hipo")
    .schemas(dict)
    .compression(Compression::Lz4PerBank)
    .build()?;
```

**No reader-side API change.** `for ev in chain.events() { ev?.bank("X"); }`
just works, because `OwnedEvent` is polymorphic over its storage backend. A scan
that only ever calls `ev.bank("REC::Event")` will *never* inflate
`REC::Particle`'s stream — a contract asserted in the test suite
(`wire::by_bank::tests::touching_one_bank_does_not_inflate_others`).

Measured on a 1.1 GB CLAS12 file (`rec0.hipo`, 289 k events, 195 records, local
SSD; `bench_par` reads `REC::Particle.rows()` only):

| Format | Sequential | Parallel | Size |
|---|---:|---:|---:|
| `Lz4` baseline | 980 kev/s | 5,073 kev/s | 1,135 MB |
| **by-bank** | **4,025 kev/s (4.1×)** | **15,675 kev/s (3.1×)** | **1,225 MB (+8%)** |

:::note About these numbers
The by-bank rows were measured on the original by-bank variant (fast default-LZ4
streams). `Lz4PerBank` shares that layout with **HC-compressed** streams, so its
selective-read speed is the same and its files are *smaller* — the throughput
figures carry over, and the size is conservative.
:::

### Trade-offs

- **Compression ratio is usually *better*.** Per-bank streams see strong
  cross-event back-reference locality — `REC::Particle` from consecutive events
  has near-identical layout — so files land within a few percent of `Lz4`, and
  on skim files with uniform topology dramatically smaller (see
  [Benchmarks](./benchmarks.md)).
- **No C++ `hipo4` compatibility.** Wire tag 6; `hipo4` won't read it.
- **Memory.** Once a bank is touched anywhere in a record, its decompressed
  bytes stay alive until the record leaves the iterator's window. Touch every
  bank and you're back to the `Lz4` memory profile.
- **Writes are slower** (HC). If read latency of a few banks is all you need,
  this is the pick; if you read at *column* granularity, `Lz4PerColumn` is
  smaller still.

## `Lz4PerColumn` — per-column streams, best ratio and finest reads

`Compression::Lz4PerColumn` goes one level finer than by-bank: within each bank,
**every column is its own LZ4-HC stream**, laid out cross-event contiguous (all
events' `px`, then all `py`, …). Two wins compound:

- **Reading one column inflates only that column** — finer than by-bank, which
  inflates a whole bank to reach one field.
- **Homogeneous columns compress better** than a bank's interleaved bytes (a
  column of `float32` next to a column of `float32` from the next event dedups
  far better than `px,py,pz,…` interleaved).

So it beats `Lz4PerBank` on **both** size and selective-read speed. Banks
without a schema (and composite banks) are stored opaquely as a single stream.
Wire tag 7. It's the default for [`skim`](../python/reading.md).

```rust
let mut w = Writer::create("out.hipo")
    .schemas(dict)
    .compression(Compression::Lz4PerColumn)
    .build()?;
```

:::note Record size matters more here
`Lz4PerColumn` (and `Lz4PerBank`) default to a **32 MB** uncompressed-payload
record-flush target, versus 8 MB for the stock codecs. A record-size sweep on
CLAS12 data showed the trade-off for per-column: the compression **ratio rises
monotonically** with record size (≈2.04× at 8 MB → 2.18× at 128 MB), but
selective reads **degrade past ~32 MB** (a larger stream must inflate to reach
one column) and 128 MB regresses everything. 32 MB sits at the ratio/read knee.
Drop to 16 MB for marginally faster reads, or raise it for maximum ratio, via
`WriterBuilder::max_record_bytes`.
:::

## Head-to-head — all formats

The same 50,000 events of a real CLAS12 file (`rec_clas_022083`, 274 banks)
re-encoded into every format (Apple M4 Pro, single thread, warm cache,
best-of-3). `Ratio` is file size versus `None` (smaller is better); `sel` / `all`
are the ms to read every column of one bank / all 274:

| Format | Size MB | Ratio | sel (1 bk) | all (274) |
|---|---:|---:|---:|---:|
| `None` | 1734 | 1.00× | 158 | 1589 |
| `Lz4` | 1081 | 0.62× | 396 | 1817 |
| `Lz4Best` | 922 | 0.53× | 395 | 1826 |
| `Gzip` | 852 | 0.49× | 2878 | 4348 |
| **`Lz4PerBank`** | 872 | 0.50× | **86** | 1529 |
| **`Lz4PerColumn`** | **813** | **0.47×** | **75** | **1280** |

*(read columns in ms)*

`Lz4PerColumn` is the **smallest file** — beating even `Gzip` — *and* the
**fastest read at every scope**: one bank is ~5× faster than whole-record `Lz4`
(75 ms vs 396 ms) because it inflates only that bank's columns. `Gzip` packs
tightly but inflates an order of magnitude slower. The full breakdown — an extra
read scope plus the matching Python numbers — is on the
[Benchmarks](./benchmarks.md) page.

## Converting existing files

The `recook_by_bank` example re-emits an existing file as `Lz4PerBank`, for A/B
benchmarking or a one-time conversion:

```sh
# single file
cargo run --release --example recook_by_bank -- \
    /volatile/.../in.hipo /scratch/$USER/out_by_bank.hipo

# whole directory in parallel (one file per rayon worker)
cargo run --release --example recook_by_bank -- --batch \
    /volatile/.../skim_slices/hipo /scratch/$USER/skim_by_bank/

# then measure
cargo run --release --example bench_par -- /scratch/$USER/out_by_bank.hipo 0
```

For `Lz4PerColumn` — or any other target — write it directly with the
[`Writer`](../rust/writing.md), or from Python re-compress with
[`skim`](../python/reading.md) (which defaults to per-column):

```python
import oxihipo as ox

ox.open("/volatile/.../in.hipo").skim("out.hipo")  # -> Lz4PerColumn
```

### End-to-end recipe for a real analysis

```sh
# 1. One-time conversion (per slice, in parallel over the directory)
cargo run --release --example recook_by_bank -- --batch \
    /volatile/.../pi0_CxC_skim_slices/hipo \
    /volatile/clas12/$USER/pi0_by_bank/

# 2. Point your reader at the new directory — no code change.
```

Step 2 is the point: every `ev.bank(name)` call benefits from partial
decompression automatically, with no format-aware code anywhere.

## Which should I use?

| Situation | Use |
|---|---|
| C++ `hipo4` has to read the file | `Lz4` (or `Gzip` for a tighter, slower file) |
| Archival with `hipo4` compatibility, size matters | `Lz4Best` (needs the `lz4-c` feature) |
| Rust/Python-only, analysis touches a few banks | **`Lz4PerBank`** |
| Rust/Python-only, you read a few *columns* — or want the best ratio | **`Lz4PerColumn`** |

`Chain::skim` (and Python `skim`) default to `Lz4PerColumn`: the best ratio and
the finest selective reads, at the cost of a slower one-time write. Drop to
`Lz4PerBank` if you'd rather write faster and still only inflate the banks you
read.
