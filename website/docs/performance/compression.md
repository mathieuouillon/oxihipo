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

Compression is a **(codec × layout) pair**. The codec decides what squeezes the
bytes; the layout decides what gets squeezed separately. They are independent,
and all 15 combinations work.

```rust
use oxihipo::{Codec, Compression, Layout};

// The pair, spelled out:
let c = Compression::new(Codec::Zstd, Layout::PerColumn).with_zstd_level(3);

// The six historical names are the same thing under an old spelling:
assert_eq!(Compression::Lz4PerColumn, Compression::new(Codec::Lz4Hc, Layout::PerColumn));
```

| `Codec` | What it is |
|---|---|
| `None` | Uncompressed. |
| `Lz4` | Stock LZ4. Fastest to write. |
| `Lz4Hc` | LZ4 high-compression: ~10–15% smaller than `Lz4`, ~4× the write cost, identical to decode. Needs the `lz4-c` feature; without it, falls back to standard LZ4. |
| `Gzip` | Smallest of the five, and by far the slowest to inflate. |
| `Zstd` | Levels 1–6 via `with_zstd_level`. Near-gzip ratios at a fraction of the write cost. The level is a writer-side choice and never reaches the wire — one tag decodes every level. |

| `Layout` | What it is |
|---|---|
| `PerChunk` | One stream for the whole record — the stock HIPO shape. Reading any bank inflates every bank. |
| `PerBank` | One stream per bank type, plus a compressed presence directory. `ev.bank(…)` inflates only that bank. |
| `PerColumn` | One stream per `(bank, column)`, cross-event contiguous. Reads at column granularity, and homogeneous columns compress better than a bank's interleaved bytes. |

### Which pair should I use?

**`Zstd × PerColumn`**, unless you have a specific reason otherwise. It is
smaller than the LZ4-HC equivalent, reads a column just as fast, and writes
**10.7× faster**. See [the matrix](#the-matrix-measured).

- Need `hipo4` compatibility → a `PerChunk` codec, or `Lz4Hc × PerBank` /
  `Lz4Hc × PerColumn` (see [Format versions](#format-versions-and-cross-version-compatibility)).
- Every byte matters → `Gzip × PerColumn` (2.32×), at 3.6× Zstd's write cost.
- Writing is the bottleneck → `Lz4 × PerColumn`: the fastest compressing cell
  to write, and still beats every whole-record codec on selective reads.

:::note Tags 4 and 5 were reused
Earlier versions shipped `Lz4Chunked` (tag 4) and `Lz4ByBank` v1 (tag 5). Both
were removed and their tags left rejected. Those two slots now carry Zstd.

That reuse is safe for one specific reason: a zstd frame begins with the magic
`0xFD2FB528`, so a stale file carrying an old tag fails the frame check rather
than decoding into something plausible. No other codec in those slots would
have that property. Tag 15 is the only one still unassigned.
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

## Record size and parallel scaling

That knee is a **single-thread** result. If you read in parallel there is a
second, larger effect, and it points the other way.

`for_each` parallelises over **records** — the work unit is one whole record,
and within it a worker inflates streams lazily and serially. Nothing inflates
two banks at once. So a parallel scan cannot use more cores than the file has
records, and a 32 MB flush target on a mid-sized file leaves cores idle.

Re-encoding a 134 MB CLAS12 file (100k events, 19 banks) as `Lz4PerBank` at
several flush targets, then reading **every bank of every event**; 12 logical
cores, best-of-3:

| record target | records | size MB | vs 32 MB | 1 thread | all cores | speed-up |
|---|--:|--:|--:|--:|--:|--:|
| 32 MB (default) | 9 | 106.8 | — | 513 ms | 73 ms | 6.8× |
| 16 MB | 17 | 106.9 | +0.14% | 507 ms | 76 ms | 6.7× |
| 8 MB | 33 | 107.3 | +0.43% | 492 ms | 64 ms | 7.7× |
| **4 MB** | **66** | **107.9** | **+1.00%** | **484 ms** | **59 ms** | **8.2×** |
| 2 MB | 132 | 109.4 | +2.47% | 482 ms | 58 ms | 8.3× |
| 1 MB | 264 | 110.8 | +3.79% | 485 ms | 58 ms | 8.5× |

Most of the parallel headroom is bought by 4 MB records for **1%** of file size,
and it flattens after that while the size cost keeps climbing. Single-thread
time does not regress — it improves slightly, a smaller record being kinder to
cache.

:::tip
Rule of thumb: you want **at least a few records per core**. Divide the
uncompressed payload by (cores × 4) and use that as the flush target, floored at
a few MB. The default 32 MB is tuned for ratio and single-thread selective
reads; it is the wrong default for a parallel scan of anything under ~1 GB.
:::

:::info Why not split a single stream instead?
This sat on the roadmap as "intra-stream parallel inflate" and is now closed.
Each stream is one LZ4 block — a chain of back-references that cannot be
inflated in parallel — so splitting one means a **writer-side format change**
emitting independent sub-blocks, each restarting LZ4's match window. That buys
what the table above already gets from `max_record_bytes` at 1% size. And for
`Lz4PerColumn` it could never have helped: profiling three real files puts the
largest *column* stream at 2–8% of its record (199 streams on the ALERT file),
so no single stream is the bottleneck. The largest *bank* stream is 26–38%,
which is why the idea looked plausible for `Lz4PerBank` — but record-level
parallelism reaches it more cheaply and needs no format change.

Reproduce with `cargo run --release --example profile_streams -- file.hipo`
and `cargo run --release --example record_size_scaling -- file.hipo`.
:::

## The matrix, measured

All 15 pairs on 248 MB of a real CLAS12 DST (599k events), Apple M4 Pro, single
thread, warm cache. Best-of-3 with the reps **interleaved across cells**, so
drift over the run cannot land on whichever cell went last. `scan all` reads a
value from every bank of every event; `scan 1 col` reads one column across the
whole file through `read_columns`.

Every cell checksums identically — a codec that read faster by reading less
would otherwise look like a win.

| codec | layout | size | ratio | write | scan all | **scan 1 col** |
|---|---|---:|---:|---:|---:|---:|
| `None` | `PerChunk` | 248 MB | 1.00× | 0.14 s | 77 ms | 36 ms |
| `None` | `PerBank` | 228 MB | 1.09× | 0.18 s | 95 ms | 38 ms |
| `None` | `PerColumn` | 228 MB | 1.09× | 0.24 s | 113 ms | 32 ms |
| `Lz4` | `PerChunk` | 146 MB | 1.70× | 0.33 s | 120 ms | 78 ms |
| `Lz4` | `PerBank` | 142 MB | 1.75× | 0.38 s | 125 ms | 69 ms |
| `Lz4` | `PerColumn` | 136 MB | 1.83× | **0.43 s** | 110 ms | **28 ms** |
| `Lz4Hc` | `PerChunk` | 126 MB | 1.96× | 6.07 s | 109 ms | 64 ms |
| `Lz4Hc` | `PerBank` | 126 MB | 1.97× | 5.67 s | 125 ms | 66 ms |
| `Lz4Hc` | `PerColumn` | 122 MB | 2.03× | 7.73 s | 116 ms | 32 ms |
| `Gzip` | `PerChunk` | 117 MB | 2.12× | 2.37 s | 416 ms | 370 ms |
| `Gzip` | `PerBank` | 116 MB | 2.14× | 2.35 s | 421 ms | 349 ms |
| `Gzip` | `PerColumn` | **107 MB** | **2.32×** | 2.58 s | 112 ms | 30 ms |
| `Zstd` | `PerChunk` | 124 MB | 2.00× | 0.75 s | 228 ms | 184 ms |
| `Zstd` | `PerBank` | 121 MB | 2.05× | 0.80 s | 231 ms | 168 ms |
| **`Zstd`** | **`PerColumn`** | **112 MB** | **2.22×** | **0.72 s** | 118 ms | **35 ms** |

Reproduce with:

```bash
cargo run --release --example bench_compression -- file.hipo 3
```

### What the matrix shows

**The layout decides the selective read, not the codec.** Every `PerColumn`
cell reads one column in 28–35 ms *whatever the codec* — including `Gzip`,
which takes 349–370 ms in the same codec on the whole-record layouts. A
`PerChunk` codec must inflate everything to reach anything. That difference is
an order of magnitude and it dwarfs the codec choice; if you take one thing
from this page, take that.

**`Zstd × PerColumn` is the best default.** Smaller than `Lz4Hc × PerColumn`
(2.22× against 2.03×), reads a column just as fast, and writes in 0.72 s where
LZ4-HC takes 7.73 s — **10.7× faster to write, for a smaller file**. Anywhere
you were reaching for `Lz4PerColumn`, reach for this.

**`Gzip × PerColumn` still wins on size** at 2.32×, for 3.6× Zstd's write cost.
And plain **`Lz4 × PerColumn`** is the fastest compressing cell to write
(0.43 s) while still beating every whole-record codec on selective reads.

**Compression is not the whole story on the `None` row.** `None × PerColumn`
reads a column faster than `None × PerChunk` (32 ms against 36) despite doing
no decompression at all, because the column's bytes are contiguous rather than
strided across every row.

The older whole-file numbers — 1734 MB of the same DST across the six original
formats, plus the matching Python figures — are on the
[Benchmarks](./benchmarks.md) page.

## Through the Python API on ifarm

The table above is a laptop, single-threaded. The same comparison through
`oxihipo.Chain.read_columns` on ifarm2402, reading `REC::Particle` from 150,000
events of `rec_clas_022083` at `threads=16`, best-of-3 and repeated three times:

| Format | Size | warm read | cold read |
|---|---:|---:|---:|
| `Lz4` (as cooked) | 3.24 GB | 0.169–0.193 s | 0.83–1.23 s |
| `Lz4PerBank` | 2.61 GB | 0.057–0.072 s | 0.61–0.96 s |
| `Lz4PerColumn` | 2.44 GB | **0.053–0.060 s** | 0.74–0.96 s |

Values were checked identical across all three files first — a read that is
faster and wrong is not faster.

**Warm reads are ~3× faster**, and that is the largest single win available to a
farm analysis: it beats every thread and process knob measured. The gain comes
from not inflating the other ~46 banks, so it grows the *less* of the file you
want and disappears if you read everything.

**Cold reads gain much less** (~1.3×), and only in proportion to the smaller
file — once the read is waiting on Lustre, the codec cannot help. See
[Parallel reading](../python/parallel.md#when-this-actually-helps) for why extra
processes do not help there either.

Two costs to weigh before converting a production sample:

- **Writing is 19–23× slower.** Re-encoding those 150,000 events took 224 s
  (`Lz4PerBank`) and 274 s (`Lz4PerColumn`) against 12 s for `Lz4`. It is a
  one-time cost paid once per file and repaid on every read, but it is not free.
- **`hipo4` cannot read these files.** Wire tags 6 and 7 are oxihipo
  extensions, so a converted sample is unreadable by C++ `hipo4` and anything
  built on it (coatjava, `clas12root`). Convert a *working copy*, keep the
  original, or stay on `Lz4` if colleagues need it.

## Format versions and cross-version compatibility

Both split codecs carry an **extension-format-version byte** at the front of the
record payload: `Lz4PerBank` is version **2**, `Lz4PerColumn` version **1**.

These numbers are a **compatibility contract, not a private detail.** The C++
(`hipo-cpp`) and Java (`hipo-java`) implementations of these codecs, on their
`feature/bybank-bycolumn-compression` branches, document and implement exactly
these two versions. Verified on a JLab farm node: oxihipo, C++ and Java read the
same by-bank and by-column files and produce byte-identical checksums.

:::warning 0.7.0 bumped these and broke that
0.7.0 raised the versions to 3 and 2 when it added the composite `header_size`
table. Neither other implementation could then read the files — `hipo-cpp`
**segfaulted** and `hipo-java` threw `failed to decode ByBank record section`.
**0.7.1 puts the versions back.** If you wrote split-codec files with 0.7.0 and
need to share them, rewrite them with 0.7.1; oxihipo reads both.
:::

The bump was never necessary. The `header_size` table is appended *after* every
other directory table, so a reader that predates it simply never looks that far.
oxihipo therefore detects the table by **directory length** rather than by the
version byte — which is what lets the version stay put while the data grows.

### Composite banks

A [composite bank](../python/reading.md#composite-banks) carries an inline format
string, and the only thing marking it as composite is the top byte of its
structure length word. The split codecs take a record apart and store bank
payloads separately, discarding those structure headers — and before 0.7.0 they
had nowhere to put that byte, so it was rebuilt as zero. A composite bank came
back looking like an ordinary one and `composite()` returned `None`.

That is what the `header_size` table fixes, and 0.7.1 keeps the fix. It applies
to **newly written files only**:

:::warning Composite banks in existing split-codec files cannot be recovered
If you converted a file to `Lz4PerBank` or `Lz4PerColumn` with oxihipo 0.6.0 or
earlier and it contained composite banks, the marker was never written to disk.
Reading it with a current version still reports `header_size = 0` and
`composite()` still returns `None` — measured, not assumed. The payload bytes
are intact but nothing identifies them as composite.

Re-convert from the original file. This is one more reason to keep it.
:::

The C++ and Java implementations do not carry the table, so a composite bank in
a split-codec file is invisible to them either way — that is a gap in the shared
format, not something a reader can work around.

Neither of the two real CLAS12 files this project tests against (an 8.5 GB DST
and a simulation file, 71 and 106 distinct banks over 2,000 events) carries a
composite bank, so ordinary reconstruction output was never affected.

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
| **Default** — Rust/Python-only | **`Zstd × PerColumn`** |
| Every byte matters, write cost does not | `Gzip × PerColumn` (2.32×) |
| Writing is the bottleneck | `Lz4 × PerColumn` (0.43 s, still 1.83×) |
| C++ `hipo4` has to read the file | `Lz4 × PerChunk`, or `Gzip × PerChunk` for a tighter, slower file |
| `hipo4`-compatible archival, size matters | `Lz4Hc × PerChunk` (needs the `lz4-c` feature) |
| `hipo-cpp`/`hipo-java` split-codec branches | `Lz4Hc × PerBank` or `Lz4Hc × PerColumn` |

```rust
use oxihipo::{Chain, Codec, Compression, Layout};

# fn main() -> oxihipo::Result<()> {
Chain::open("run.hipo")?
    .skim("small.hipo", Compression::new(Codec::Zstd, Layout::PerColumn))?;
# Ok(()) }
```

**Pick the layout first.** It is worth an order of magnitude on selective reads
— 28–35 ms against 349–370 ms — where the codec is worth tens of percent on
size. `PerColumn` unless you have a reason.

Then pick the codec on write cost, since all three `PerColumn` codecs read a
column at about the same speed: `Zstd` (0.72 s, 2.22×) is the balance, `Lz4`
(0.43 s, 1.83×) if writes dominate, `Gzip` (2.58 s, 2.32×) if bytes do.
`Lz4Hc × PerColumn` is now dominated by `Zstd × PerColumn` on every axis —
bigger file, 10.7× slower write, same read — and is worth choosing only for
compatibility with the `hipo-cpp`/`hipo-java` split-codec branches.

`Chain::skim` (and Python `skim`) still default to `Lz4Hc × PerColumn`
(`Compression::Lz4PerColumn`) so that a skimmed file stays readable by those
branches. Pass `Zstd × PerColumn` explicitly when you do not need that.
