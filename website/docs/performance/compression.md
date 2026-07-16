---
id: compression
title: Compression formats
sidebar_position: 1
---

# Compression formats

Once page-fault stalls are masked, **LZ4 inflate dominates wall time** on ifarm.
This page is about the two opt-in format extensions that attack that, and when
each is worth it.

## The problem with one block per record

The stock HIPO format stores **one LZ4 block per record**. Two consequences
follow, and both hurt:

1. A record's decompress is one sequential pass on one worker. Idle cores on the
   same record can't help.
2. You cannot decompress *part* of a record without inflating all of it. Reading
   one bank costs you every bank.

## `Lz4ByBank` — decompress only the banks you read

**This is usually the one you want.** Real analyses touch 2–5 banks out of ~30;
the other ~85% is wasted LZ4 work.

`Compression::Lz4ByBank` stores each bank type as its own LZ4 stream within the
record. The reader parses a small directory eagerly but inflates a bank's stream
only when `ev.bank(name)` actually asks for it. Banks you never touch stay
compressed for the record's lifetime.

```rust
use oxihipo::{Compression, Writer};

let mut w = Writer::create("out.hipo")
    .schemas(dict)
    .compression(Compression::Lz4ByBank)
    .build()?;
w.finish()?;
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
- **No C++ `hipo4` compatibility.** New compression tag (5); `hipo4` won't read
  it. Use it for Rust-only (or oxihipo-Python-only) consumers.
- **Memory.** Once a bank is touched anywhere in a record, its decompressed
  bytes stay alive until the record leaves the iterator's window. Touch every
  bank and you're back to the `Lz4` memory profile.

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
- **No C++ `hipo4` compatibility.** New compression tag (4), same caveat as
  above.

It parallelises decompression of *every* bank for *every* event — which is why
`Lz4ByBank` generally wins: not doing the work beats doing it faster.

## Converting existing files

Both come with a `recook` example that re-emits an existing file, for A/B
benchmarking or a one-time conversion:

```sh
# Lz4Chunked
cargo run --release --example recook -- \
    /volatile/.../in.hipo /scratch/$USER/out_chunked.hipo 32

# Lz4ByBank, single file
cargo run --release --example recook_by_bank -- \
    /volatile/.../in.hipo /scratch/$USER/out_by_bank.hipo

# Lz4ByBank, whole directory in parallel (one file per rayon worker)
cargo run --release --example recook_by_bank -- --batch \
    /volatile/.../skim_slices/hipo /scratch/$USER/skim_by_bank/

# then measure
cargo run --release --example bench_par -- /scratch/$USER/out_by_bank.hipo 0
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
decompression automatically, with no `Lz4ByBank`-aware code anywhere.

## Which should I use?

| Situation | Use |
|---|---|
| C++ `hipo4` has to read the file | `Lz4` (or `Gzip`) |
| Rust/Python-only, analysis touches a few banks | **`Lz4ByBank`** |
| Rust/Python-only, analysis touches nearly every bank | `Lz4Chunked`, E=32–64 |
| Archival, size matters most | `Lz4Best` (needs the `lz4-c` feature) |

`Chain::skim` defaults to `Lz4ByBank` for exactly this reason.
