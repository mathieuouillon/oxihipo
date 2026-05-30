# oxihipo

[![CI](https://github.com/mathieuouillon/oxihipo/actions/workflows/ci.yml/badge.svg)](https://github.com/mathieuouillon/oxihipo/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.87+](https://img.shields.io/badge/rust-1.87%2B-orange.svg)](https://www.rust-lang.org)

Pure-Rust reader and writer for the **HIPO v6** binary container used at
Jefferson Lab CLAS12. Built so that **read throughput meaningfully exceeds
the C++ `hipo4` reader** on the same hardware, with an API that fits Rust
idioms.

Reads and writes HIPO version 6 files. Physics, FFI, ROOT, and XRootD
layers are intentionally out of scope.

## Features

- **Zero-copy columnar reads.** `bank.col::<T>("name")` returns a
  `Cow<[T]>` that borrows straight from the mmap when the bytes are
  aligned (always for 4-byte types), with a one-shot copy fallback
  otherwise. Fixed-length array columns (`name/T#N`) read as `[T; N]`.
- **One reader: `Chain`.** Single file, directory, or glob; multi-file
  chains share one mmap per file and a single parsed dictionary.
- **Data-parallel scans.** `Chain::par_reduce` / `par_for_each` fan the
  work across cores, with automatic `MADV_WILLNEED` prefetch to hide
  page-fault latency on networked filesystems (Lustre / NFS).
- **Compression beyond stock LZ4 / Gzip.** Two opt-in format extensions:
  `Lz4Chunked` (intra-record parallel inflate) and `Lz4ByBank`
  (decompress only the banks an analysis actually reads — see the
  benchmarks below).
- **Pure-Rust by default**, with optional features: `lz4-c` (C LZ4
  bindings — faster decode + `Lz4Best` HC), `lz4-apple` (Apple
  `libcompression` decode), and `mimalloc-allocator`.

## Add to your project

Not yet published to crates.io — depend on it via git:

```toml
[dependencies]
oxihipo = { git = "https://github.com/mathieuouillon/oxihipo" }
```

## Quick start

```rust
use oxihipo::{Chain, Filter};

# fn main() -> oxihipo::Result<()> {
// Single file or many — `Chain` is the sole reader entry point.
let chain = Chain::open("rec.hipo")?
    .with_filter(Filter::require(["REC::Particle"]))?;

// Plain `for` loop. Each `OwnedEvent` is a slice into a shared,
// ref-counted record buffer — no per-event allocation.
for ev in chain.events() {
    let p = oxihipo::or_continue!(ev.bank("REC::Particle"));
    for r in 0..p.rows() {
        let pid: i32 = p.get("pid", r);
        let px:  f32 = p.get("px",  r);
        let _ = (pid, px);
    }
}
# Ok(()) }
```

Multi-file chains are first-class:

```rust
use oxihipo::Chain;

# fn main() -> oxihipo::Result<()> {
// `Chain::open_dir` takes a directory; `Chain::open` also accepts a
// single file or a glob (e.g. "data/*.hipo").
let chain = Chain::open_dir("/data/cooked/run5042")?;

// Iterate every event of every file, in input order.
let mut total_rows: u64 = 0;
for ev in chain.events() {
    total_rows += ev.bank("REC::Particle").map_or(0, |b| b.rows() as u64);
}
println!("{total_rows} REC::Particle rows across the chain");
# Ok(()) }
```

Saturate every core with `par_reduce` — the same scan, fanned across the
records of every file (`threads = 0` ⇒ one worker per logical CPU):

```rust
use oxihipo::Chain;

# fn main() -> oxihipo::Result<()> {
let chain = Chain::open_dir("/data/cooked/run5042")?;

let total_rows: u64 = chain.par_reduce(
    0,
    || 0u64,
    |acc, ev| acc + ev.bank("REC::Particle").map_or(0, |b| b.rows() as u64),
    |a, b| a + b,
)?;
println!("{total_rows} REC::Particle rows across the chain");
# Ok(()) }
```

Writing is closure-driven:

```rust
use oxihipo::{Compression, Writer};

# fn run(dict: &oxihipo::Dict) -> oxihipo::Result<()> {
let mut w = Writer::create("out.hipo")
    .schemas(dict)
    .compression(Compression::Lz4)
    .build()?;
w.event(|ev| {
    ev.bank("REC::Particle", |b| {
        b.row(|r| { r.set("pid", 11_i32)?; r.set("px", 0.5_f32) })?;
        Ok(())
    })?;
    Ok(())
})?;
w.finish()?;
# Ok(()) }
```

## Status

- Single `oxihipo` library crate. No bundled binary; downstream consumers
  build whatever frontend they need on top.
- `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` all clean.
- Validated on a 1.7 GB CLAS12 file (`rec_clas_022050.evio.00000.hipo`):
  a sequential `Chain::events()` scan reads all 187,941 events at
  ~257 kev/s. `Chain::par_reduce` fans the same scan across cores; measure
  throughput on your hardware with the `bench_par` example.

## Layout

Single-crate repo (`oxihipo` — error, wire, compress, schema, event, read,
write). Inside `src/`:

- `error.rs`, `prelude.rs`
- `wire/` (private) — constants, bytes, headers, record decompression
- `compress.rs` (private) — LZ4/gzip + reusable `ScratchBuf`
- `schema/` — `Schema`, `Dict`, `DataType`, typed `ColumnHandle<T>`
- `event/` — `Event` (raw), `EventCtx` (with `&Dict`), `Bank`, `RowView`,
  `Composite`, `OwnedEvent`, internal `BankBuilder` / `EventBuilder`
- `read/` — `Chain` (the sole reader, `Arc<FileInner>`-backed),
  `ChainEventIter`, `Filter`, parallel `par_for_each` / `par_reduce`
  (`ChainStats`)
- `write/` — `Writer` builder, `BankWriter`, `RowWriter`, `Compression`

## Build

```sh
cargo build --release
cargo test

# Examples
cargo run --release --example write     -- /tmp/demo.hipo
cargo run --release --example read      -- /tmp/demo.hipo
cargo run --release --example parallel  -- /path/to/file.hipo 0
cargo run --release --example bench_par -- /path/to/file.hipo 0
```

## Notable design decisions

- **`Chain` is the only reader.** A chain of one file is the common case;
  multi-file chains share a single mmap per file and one parsed dictionary.
  `Chain::open_all` validates that every file in the chain has the same
  `Dict` — catches mismatched cooking versions at construction time.
- **`Event<'a>` carries only borrowed bytes;** the typical handle is
  `EventCtx<'a> = (Event<'a>, &Dict)`, which lets `ev.bank("REC::Particle")`
  resolve the schema without separate juggling.
- **`Bank::col::<T>("name")` is the single typed getter** (generic; replaces
  six per-type methods). Returns `Cow<[T]>`: zero-copy when the bank's
  bytes are aligned to `T` (always for 4-byte types; usually for 8-byte
  types), and a one-shot `read_unaligned` copy otherwise — matching the
  C++ reader's memcpy semantics.
- **`Bank::get::<T>("name", row)`** is the infallible scalar accessor for
  hot loops. Type is inferred from the binding (`let pid: i32 = b.get("pid",
  r);`) and missing/wrong-type columns return `T::default()`.
- **`ColumnHandle<T>`** lives on `Schema`, resolved once via
  `schema.handle::<T>("name")`. Inside hot loops, `bank.read(h)` is a
  constant-time cast — no per-event name lookup.
- **mimalloc** is gated behind the `mimalloc-allocator` feature (off by
  default), for allocation-heavy workloads where the system allocator
  underperforms on macOS.

## Performance on shared filesystems

When the input lives on a network filesystem — JLab ifarm's `/volatile` and
`/cache` (Lustre), NFS, etc. — `mmap` page-faults become many small RPCs
and dominate wall time. `oxihipo` mitigates this in two places:

- **Parallel runs** (`Chain::par_for_each` / `Chain::par_reduce`) auto-issue
  `MADV_WILLNEED` over each selected record before the worker pool starts,
  so the kernel reads pages in parallel with worker decompression. Nothing
  to call — it's automatic.
- **Sequential runs** can opt in by calling `chain.prefetch()` once after
  open and before the loop:

  ```rust
  let chain = Chain::open(file)?;
  chain.prefetch();
  for ev in chain.events() { /* … */ }
  ```

If you are still I/O-bound after that, the levers are user-side:

- **File striping.** A Lustre file on a single OST is bandwidth-capped no
  matter the thread count. New outputs: `lfs setstripe -c 4 outfile.hipo`;
  existing files: `lfs migrate -c 4 file.hipo`.
- **Thread oversubscription.** Pass `threads = 2 × num_cpus` to
  `par_reduce` to hide network page-fault stalls.
- **Stage to local scratch.** `cp /volatile/.../file.hipo /scratch/$USER/`
  before analysing — local disk easily beats Lustre per single client.

## When LZ4 itself is the bottleneck: `Lz4Chunked` and `Lz4ByBank`

After page-fault stalls are masked, LZ4 inflate dominates wall time on
ifarm. The HIPO format stores **one LZ4 block per record**, so a record's
decompress is one sequential pass on one worker — idle cores on the same
record don't help, and you can't decompress part of a record without
inflating all of it.

`Compression::Lz4Chunked { events_per_chunk }` is an opt-in format
extension that splits each record's events into independently-compressed
LZ4 chunks with an offset table:

```rust
use oxihipo::{Compression, Writer};

# fn run(dict: &oxihipo::Dict) -> oxihipo::Result<()> {
let mut w = Writer::create("out.hipo")
    .schemas(dict)
    .compression(Compression::Lz4Chunked { events_per_chunk: 32 })
    .build()?;
// ... w.event(|ev| { ... })? ...
w.finish()?;
# Ok(()) }
```

What that unlocks today:

- **Intra-record parallel decompression.** The reader inflates chunks in
  parallel via `rayon::scope`. Sequential `chain.events()` loops use idle
  cores; `par_reduce` workers get finer-grained units.
- **Lays groundwork for partial decompression** — the inline
  `event_sizes[]` table sits outside any LZ4 stream, so a future
  filter-pushdown API can decompress only chunks containing wanted
  events.

Trade-offs:

- **Compression ratio.** Per-chunk LZ4 has less back-reference context
  than per-record. At `events_per_chunk = 32` the output is typically
  5–15 % larger; the sweet spot is 32–64.
- **C++ `hipo4` compatibility.** Files written with `Lz4Chunked` use a
  new compression tag (4) the C++ reader doesn't know about. Use it for
  Rust-only consumers, or alongside (not replacing) the standard `Lz4`
  output. The other variants (`None` / `Lz4` / `Lz4Best` / `Gzip`) stay
  byte-compatible with `hipo4`.

A `recook` example re-emits an existing `Lz4` file as `Lz4Chunked` for
A/B benchmarking:

```sh
cargo run --release --example recook -- \
    /volatile/.../in.hipo /scratch/$USER/out_chunked.hipo 32
cargo run --release --example bench_par -- /scratch/$USER/out_chunked.hipo 0
```

### `Lz4ByBank` — decompress only the banks you read

`Lz4Chunked` parallelises decompression of *every* bank for *every* event.
Real analyses typically touch 2–5 banks out of ~30; the other ~85 % is
wasted LZ4 work.

`Compression::Lz4ByBank` stores each bank type as its own LZ4 stream
within the record. The reader parses a small directory eagerly, but
inflates a bank's stream only when `ev.bank(name)` actually asks for it.
Banks the user never touches stay compressed for the record's lifetime.

```rust
use oxihipo::{Compression, Writer};

# fn run(dict: &oxihipo::Dict) -> oxihipo::Result<()> {
let mut w = Writer::create("out.hipo")
    .schemas(dict)
    .compression(Compression::Lz4ByBank)
    .build()?;
// ... w.event(|ev| { ... })? ...
w.finish()?;
# Ok(()) }
```

No reader-side API change — `for ev in chain.events() { ev.bank("X") }`
"just works". A scan that only ever calls `ev.bank("REC::Event")` will
**never** inflate `REC::Particle`'s stream; the partial-decompression
contract is asserted in tests (`wire::by_bank::tests::touching_one_bank_does_not_inflate_others`).

Measured on a 1.1 GB CLAS12 file (`rec0.hipo`, 289 k events, 195 records,
local SSD, `bench_par` reads `REC::Particle.rows()` only):

| Format | Sequential | Parallel | Size |
|---|---:|---:|---:|
| `Lz4` baseline | 980 kev/s | 5,073 kev/s | 1,135 MB |
| `Lz4Chunked` E=32 | 2,628 kev/s (2.7×) | 5,881 kev/s (1.2×) | 1,253 MB (+10 %) |
| **`Lz4ByBank`** | **4,025 kev/s (4.1×)** | **15,675 kev/s (3.1×)** | **1,225 MB (+8 %)** |

Trade-offs:

- **Compression ratio.** Per-bank streams see better cross-event
  back-reference locality (`REC::Particle` from consecutive events has
  near-identical layout) — file size is typically *smaller* than
  `Lz4Chunked` and within 5–10 % of `Lz4`.
- **No C++ `hipo4` compatibility.** New compression tag (5); same caveat
  as `Lz4Chunked`. Use for Rust-only consumers.
- **Memory.** Once a bank is touched anywhere in a record, its
  decompressed bytes stay alive until the record drops out of the
  iterator's window. Touching every bank ⇒ same memory profile as `Lz4`.

A `recook_by_bank` example re-emits an existing file as `Lz4ByBank`:

```sh
# Single file
cargo run --release --example recook_by_bank -- \
    /volatile/.../in.hipo /scratch/$USER/out_by_bank.hipo

# Whole directory in parallel (one file per rayon worker)
cargo run --release --example recook_by_bank -- --batch \
    /volatile/.../skim_slices/hipo /scratch/$USER/skim_by_bank/

cargo run --release --example bench_par -- /scratch/$USER/out_by_bank.hipo 0
```

### Measured on JLab ifarm

29.7 GB CLAS12 skim file (`pi0_skim_CxC_Outbending_slice000.hipo`,
1.85 M events, 29 430 records) on `ifarm2401` (64 logical cores).
`bench_par` reads `REC::Particle.rows()` only — exactly the partial-
decompression case `Lz4ByBank` is designed for.

| Location | Format | Sequential | par=10 | par=32 | par=64 | Size |
|---|---|---:|---:|---:|---:|---:|
| `/volatile` (Lustre, hot) | `Lz4` baseline | 72 kev/s | 1,437 | — | — | 29.7 GB |
| `/volatile` (Lustre, hot) | **`Lz4ByBank`** | 437 | 14,724 | 27,643 | **36,578** | **6.66 GB** (−77.6 %) |
| `/scratch` (local SSD) | `Lz4` baseline | 159 | 1,112 | — | — | 29.7 GB |
| `/scratch` (local SSD) | `Lz4ByBank` | 1,695 | 7,558 | — | — | 6.66 GB |

Headline: `par=64` on `/volatile` hits **36.6 Mev/s** — 25× the `Lz4`
baseline throughput at par=10. The compression ratio result is
exceptional for skim files (near-identical per-event topology gives
per-bank LZ4 streams enormous cross-event redundancy to dedup) — on
generic reco files expect closer to ±5 %.

Notes on the matrix:

- **`/volatile` beats `/scratch` parallel** when the `Lz4ByBank` file is
  ifarm-page-cache-hot from a just-completed recook. Cold-read Lustre
  numbers (after the cache evicts) land closer to the `/scratch` row.
- **Sequential is permanently Lustre-bound on `/volatile`** —
  single-stream RPCs cap you around 400–500 kev/s regardless of LZ4
  format. For sequential dev/debug, stage to `/scratch`.
- **Thread scaling is linear well past `num_cpus`** for `Lz4ByBank` on
  Lustre. Default `threads = 0` (one per logical CPU) is good; oversubscribing
  to `2 × num_cpus` hides page-fault stalls further.

End-to-end recipe for a real analysis:

```sh
# 1. One-time conversion (per slice, in parallel over the directory)
cargo run --release --example recook_by_bank -- --batch \
    /volatile/.../pi0_CxC_skim_slices/hipo \
    /volatile/clas12/$USER/pi0_by_bank/

# 2. Point your reader at the new directory — no code change.
#    Every `ctx.event().bank(name)` call benefits from partial
#    decompression automatically; no `Lz4ByBank`-aware code required.
```

Because the reader is polymorphic over the storage backend (`Bytes` vs
`ByBank`, on `OwnedEvent`), downstream code stays unchanged whether or
not the input is `Lz4ByBank` — banks the analysis never touches stay
compressed for the record's lifetime.

## Known gaps

- `SortedWriter` and `StreamWriter` (per-tag bin writers, auto-flush) —
  deferred.
- Bench-vs-`hipo4` comparator — deferred.
- **Sub-chunked `Lz4ByBank`**: combining `Lz4Chunked`-style intra-stream
  parallelism with per-bank streams, for very large records where one
  bank's stream is multi-MB. Per-bank streams already parallelise *across
  banks* in `par_reduce`; this is the next step if profiles say a single
  bank dominates.

## CI gates

Every PR runs:
- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- `cargo doc --no-deps` with `RUSTDOCFLAGS=-D warnings`

## License

Licensed under the [MIT License](LICENSE).
