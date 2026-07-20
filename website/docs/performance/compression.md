---
id: compression
title: Compression formats
sidebar_position: 1
---

# Compression formats

Once page-fault stalls are masked, **LZ4 inflate dominates wall time** on ifarm.
oxihipo reads and writes the four stock HIPO codecs (`hipo4`-compatible) and adds
a family of opt-in format extensions that attack that decode cost. This page
covers all of them, and when each is worth it.

## The problem with one block per record

The stock HIPO format stores **one LZ4 block per record**. Two consequences
follow, and both hurt:

1. A record's decompress is one sequential pass on one worker. Idle cores on the
   same record can't help.
2. You cannot decompress *part* of a record without inflating all of it. Reading
   one bank costs you every bank.

The extensions below all break the record into smaller independently-compressed
units — by chunk, by bank, or by column — so the reader stops doing work it
doesn't need.

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
| `Lz4Chunked { events_per_chunk }` | — *(needs a parameter)* | ❌ | Record split into independently-LZ4 chunks the reader inflates in parallel. |
| `Lz4ByBank` | `"lz4bybank"` | ❌ | One LZ4 stream per bank; inflate only the banks you read. |
| `Lz4ByBankV2` | `"lz4bybankv2"` | ❌ | By-bank with LZ4-HC streams and a compressed directory — smaller, slower to write. |
| `Lz4PerColumn` | `"lz4percolumn"` | ❌ | One LZ4-HC stream per `(bank, column)`; best ratio and finest-grained selective reads. |

The four extensions (`Lz4Chunked`, `Lz4ByBank`, `Lz4ByBankV2`, `Lz4PerColumn`)
carry new wire tags (4–7) that the C++ `hipo4` reader doesn't understand — use
them for Rust-only, or oxihipo-Python-only, consumers. The four stock codecs
stay byte-compatible with `hipo4`.

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

## `Lz4ByBank` — decompress only the banks you read

**This is usually the one you want.** Real analyses touch 2–5 banks out of ~30;
the other ~85% is wasted LZ4 work.

`Compression::Lz4ByBank` stores each bank type as its own LZ4 stream within the
record, plus a small event×bank presence directory. The reader parses the
directory eagerly but inflates a bank's stream only when `ev.bank(name)` actually
asks for it. Banks you never touch stay compressed for the record's lifetime.

```rust
let mut w = Writer::create("out.hipo")
    .schemas(dict)
    .compression(Compression::Lz4ByBank)
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
| `Lz4Chunked` E=32 | 2,628 kev/s (2.7×) | 5,881 kev/s (1.2×) | 1,253 MB (+10%) |
| **`Lz4ByBank`** | **4,025 kev/s (4.1×)** | **15,675 kev/s (3.1×)** | **1,225 MB (+8%)** |

### Trade-offs

- **Compression ratio is usually *better*.** Per-bank streams see strong
  cross-event back-reference locality — `REC::Particle` from consecutive events
  has near-identical layout — so files land smaller than `Lz4Chunked` and within
  5–10% of `Lz4`. On skim files with uniform topology it can be dramatically
  smaller (see [Benchmarks](./benchmarks.md)).
- **No C++ `hipo4` compatibility.** Wire tag 5; `hipo4` won't read it.
- **Memory.** Once a bank is touched anywhere in a record, its decompressed
  bytes stay alive until the record leaves the iterator's window. Touch every
  bank and you're back to the `Lz4` memory profile.
- **Fast to write** — the bank streams use default LZ4. For a smaller file at the
  same read speed, see `Lz4ByBankV2` next.

## `Lz4ByBankV2` — smaller by-bank files

`Compression::Lz4ByBankV2` is the same by-bank layout tuned for ratio:

- each bank stream is **LZ4-HC** compressed (per-bank grouping *plus* HC
  compounds to beat whole-record `Lz4Best`), and
- the directory is prefixed with an extension-format-version byte and is itself
  **LZ4-compressed** (the per-event size matrix is highly redundant), shrinking
  the on-disk directory.

```rust
let mut w = Writer::create("out.hipo")
    .schemas(dict)
    .compression(Compression::Lz4ByBankV2)
    .build()?;
```

Selective reads stay as fast as v1; **writes are slower** (HC). The reader
handles v1 and v2 transparently — you don't choose a decoder, and a chain may mix
both. Wire tag 6.

## `Lz4PerColumn` — per-column streams, best ratio and finest reads

`Compression::Lz4PerColumn` goes one level finer than by-bank: within each bank,
**every column is its own LZ4-HC stream**, laid out cross-event contiguous (all
events' `px`, then all `py`, …). Two wins compound:

- **Reading one column inflates only that column** — finer than by-bank, which
  inflates a whole bank to reach one field.
- **Homogeneous columns compress better** than a bank's interleaved bytes (a
  column of `float32` next to a column of `float32` from the next event dedups
  far better than `px,py,pz,…` interleaved).

So it beats `Lz4ByBankV2` on **both** size and selective-read speed. Banks
without a schema (and composite banks) are stored opaquely as a single stream.
Wire tag 7.

```rust
let mut w = Writer::create("out.hipo")
    .schemas(dict)
    .compression(Compression::Lz4PerColumn)
    .build()?;
```

:::note Record size matters more here
`Lz4PerColumn` (and `Lz4ByBankV2`) default to a **32 MB** uncompressed-payload
record-flush target, versus 8 MB for the other modes. A record-size sweep on
CLAS12 data showed the trade-off for per-column: the compression **ratio rises
monotonically** with record size (≈2.04× at 8 MB → 2.18× at 128 MB), but
selective reads **degrade past ~32 MB** (a larger stream must inflate to reach
one column) and 128 MB regresses everything. 32 MB sits at the ratio/read knee.
Drop to 16 MB for marginally faster reads, or raise it for maximum ratio, via
`WriterBuilder::max_record_bytes`.
:::

## `Lz4Chunked` — parallel inflate within a record

`Compression::Lz4Chunked { events_per_chunk }` splits each record's events into
independently-compressed LZ4 chunks with an offset table.

```rust
let mut w = Writer::create("out.hipo")
    .schemas(dict)
    .compression(Compression::Lz4Chunked { events_per_chunk: 32 })
    .build()?;
```

What it buys:

- **Intra-record parallel decompression.** The reader inflates chunks in
  parallel via `rayon::scope`, so even a sequential `chain.events()` loop uses
  idle cores; `for_each` workers get finer-grained units.
- **Groundwork for partial decompression.** The inline `event_sizes[]` table
  sits outside any LZ4 stream, so a future filter-pushdown API could decompress
  only the chunks holding wanted events.

Trade-offs:

- **Compression ratio.** Per-chunk LZ4 has less back-reference context than
  per-record. At `events_per_chunk = 32` output is typically 5–15% larger; the
  sweet spot is 32–64.
- **No C++ `hipo4` compatibility.** Wire tag 4.

It parallelises decompression of *every* bank for *every* event — which is why
the by-bank and per-column formats generally win: not doing the work beats doing
it faster. `Lz4Chunked` is the right pick only when your analysis genuinely reads
nearly every bank of every event.

## Converting existing files

The `recook` examples re-emit an existing file in a new format, for A/B
benchmarking or a one-time conversion:

```sh
# Lz4Chunked (events_per_chunk = 32)
cargo run --release --example recook -- \
    /volatile/.../in.hipo /scratch/$USER/out_chunked.hipo 32

# Lz4ByBank, single file
cargo run --release --example recook_by_bank -- \
    /volatile/.../in.hipo /scratch/$USER/out_by_bank.hipo

# Lz4ByBankV2 — add --v2
cargo run --release --example recook_by_bank -- --v2 \
    /volatile/.../in.hipo /scratch/$USER/out_v2.hipo

# Lz4ByBank, whole directory in parallel (one file per rayon worker)
cargo run --release --example recook_by_bank -- --batch \
    /volatile/.../skim_slices/hipo /scratch/$USER/skim_by_bank/

# then measure
cargo run --release --example bench_par -- /scratch/$USER/out_by_bank.hipo 0
```

The `recook_by_bank` example emits `Lz4ByBank` (or `Lz4ByBankV2` with `--v2`).
For any other target — including `Lz4PerColumn` — write it directly with the
[`Writer`](../rust/writing.md), or from Python re-compress with
[`skim`](../python/reading.md):

```python
import oxihipo as ox

ox.open("/volatile/.../in.hipo").skim("out.hipo", compression="lz4percolumn")
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
| Rust/Python-only, analysis touches a few banks | **`Lz4ByBank`** |
| Same, and you want a smaller file at equal read speed | `Lz4ByBankV2` (slower to write) |
| Same, and you read a few *columns* — or want the best ratio | `Lz4PerColumn` (slower to write) |
| Rust/Python-only, analysis touches nearly every bank | `Lz4Chunked`, E=32–64 |

`Chain::skim` (and Python `skim`) default to `Lz4ByBank`: fast to write, and it
covers the common "touch a handful of banks" case. Move up to `Lz4ByBankV2` or
`Lz4PerColumn` when the one-time write cost is worth a smaller file or
column-level reads.
