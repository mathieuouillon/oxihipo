# oxihipo

[![Documentation](https://img.shields.io/badge/📖_docs-mathieuouillon.github.io%2Foxihipo-b5410b)](https://mathieuouillon.github.io/oxihipo/)
[![CI](https://github.com/mathieuouillon/oxihipo/actions/workflows/ci.yml/badge.svg)](https://github.com/mathieuouillon/oxihipo/actions/workflows/ci.yml)
[![docs](https://github.com/mathieuouillon/oxihipo/actions/workflows/docs.yml/badge.svg)](https://github.com/mathieuouillon/oxihipo/actions/workflows/docs.yml)
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
  `Cow<[T]>` that borrows straight from the decompressed record buffer when
  the bytes are aligned (always for 4-byte types), with a one-shot copy
  fallback otherwise. Fixed-length array columns (`name/T#N`) read as `[T; N]`.
- **Bounded memory, any file size.** Records stream in one at a time via
  `pread` into a recycled buffer — the file is never mapped or read whole.
  A sequential scan of a 100 GB file holds ~one record resident (tens of MB),
  not the file; parallel scans hold one record per worker. Safe under a
  memory-capped batch allocation.
- **One reader: `Chain`.** `Chain::open` takes a single file, a directory,
  a glob, or a list of paths; multi-file chains share a single parsed
  dictionary and stream records on demand. `chain.events()` yields
  `Result<OwnedEvent>`, so a corrupt or truncated record surfaces as an
  `Err` (propagate with `?`) rather than panicking.
- **Data-parallel scans.** `Chain::for_each(threads, f)` fans the work
  across cores out of order (`threads = 0` ⇒ all cores, `1` ⇒ sequential,
  `n` ⇒ exactly `n`); shared state in `f` is atomic or locked.
- **Compression beyond stock LZ4 / Gzip.** Two opt-in format extensions
  that decompress only what an analysis actually reads: `Lz4ByBankV2`
  (per-bank streams) and `Lz4PerColumn` (per-column streams) — see the
  benchmarks below.
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

// Plain `for` loop. Each item is a `Result<OwnedEvent>`; `?` propagates a
// corrupt record. Each `OwnedEvent` is a slice into a shared, ref-counted
// record buffer — no per-event allocation.
for ev in chain.events() {
    let ev = ev?;
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
// `Chain::open` takes a single file, a directory, a glob (e.g.
// "data/*.hipo"), or a list of paths.
let chain = Chain::open("/data/cooked/run5042")?;

// Iterate every event of every file, in input order.
let mut total_rows: u64 = 0;
for ev in chain.events() {
    let ev = ev?;
    total_rows += ev.bank("REC::Particle").map_or(0, |b| b.rows() as u64);
}
println!("{total_rows} REC::Particle rows across the chain");
# Ok(()) }
```

Process every event with `for_each` — the `threads` argument is the only
difference between a single-threaded and a parallel scan (`1` = on the
calling thread in order, `0` = one worker per logical CPU, `n` = exactly
`n` workers). Shared state is held in atomics because the parallel modes
visit events out of order:

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use oxihipo::Chain;

# fn main() -> oxihipo::Result<()> {
let chain = Chain::open("/data/cooked/run5042")?;

let total_rows = AtomicU64::new(0);
chain.for_each(0, |ev| {                          // `0` → all cores; `1` → single-threaded
    if let Some(b) = ev.bank("REC::Particle") {
        total_rows.fetch_add(b.rows() as u64, Ordering::Relaxed);
    }
})?;
println!("{} REC::Particle rows across the chain", total_rows.into_inner());
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
  ~257 kev/s. `Chain::for_each` (parallel mode) fans the same scan across cores; measure
  throughput on your hardware with the `bench_par` example.

## Python bindings

A columnar, uproot-shaped Python binding lives in [`py/`](py/): a HIPO bank
reads like an [Awkward](https://awkward-array.org) jagged branch, with the
per-event loop running in Rust behind a released GIL and column buffers moved
into NumPy zero-copy.

```python
import oxihipo as ox

f = ox.open("run5042.hipo")                        # file | dir | glob | list
p = f.arrays("REC::Particle", ["pid", "px"])        # ak.Array: N * var * {pid, px}

for chunk in f.iterate("REC::Particle", step_size="200 MB"):   # bounded memory
    ...                                             # 10-100 GB inputs in ~constant RAM

# I/O-bound filesystem (ifarm /volatile): read with N processes to beat the
# per-process bandwidth ceiling — guard the script with `if __name__ == "__main__":`
p = ox.arrays("/volatile/run5042/*.hipo", "REC::Particle", ["px"], workers=8)
```

Build with [maturin](https://www.maturin.rs) (`cd py && maturin develop
--release`); see the [Python guide](https://mathieuouillon.github.io/oxihipo/docs/python/reading),
[`py/README.md`](py/README.md), and [`py/examples/`](py/examples/). Design notes:
[Python binding design](https://mathieuouillon.github.io/oxihipo/docs/design/python-binding).

**Nearly-native speed.** Reading `REC::Particle` `px,py,pz,pid` from a 9.1 GB
CLAS12 file (598,738 events, 4.7 M particles; Apple M4 Pro, all cores, warm
cache):

| | throughput | vs Rust |
|---|--:|--:|
| Rust `read_columns` | 6.3 GB/s | 1.00× |
| Python `read_columns` (NumPy) | 5.8 GB/s | 0.91× |
| Python `arrays` (Awkward) | 5.6 GB/s | 0.89× |

The per-event decode runs in Rust behind a released GIL and columns move into
NumPy zero-copy, so the binding costs ~10%. Method + reproduction:
[Python vs Rust benchmark](https://mathieuouillon.github.io/oxihipo/docs/design/python-vs-rust-benchmark)
(`examples/bench_columns.rs`, `py/examples/bench_columns.py`).

## Layout

Single-crate repo (`oxihipo` — error, wire, compress, schema, event, read,
write). Inside `src/`:

- `error.rs`, `prelude.rs`
- `wire/` (private) — constants, bytes, headers, record decompression
- `compress.rs` (private) — LZ4/gzip + reusable `ScratchBuf`
- `schema/` — `Schema`, `Dict`, `DataType`, typed `ColumnHandle<T>`
- `event/` — `Event` (raw), `EventCtx` (with `&Dict`), `Bank`,
  `Composite`, `OwnedEvent`, internal `BankBuilder` / `EventBuilder`
- `read/` — `Chain` (the sole reader, `Arc<FileInner>`-backed),
  `ChainEventIter`, `Filter`, parallel `for_each`
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
  multi-file chains share one parsed dictionary and stream records on demand
  (no whole-file mapping). `Chain::open` validates that every file in the
  chain has the same `Dict` — catches mismatched cooking versions at
  construction time.
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
`/cache` (Lustre), NFS, etc. — I/O latency dominates wall time. The reader
issues one `pread` per record and relies on the kernel's per-descriptor
readahead to fetch the next record while the current one decompresses;
parallel mode keeps several records in flight across workers. Resident memory
stays bounded (one record per worker) no matter how large the file, so a wide
parallel scan won't be OOM-killed by a memory-capped batch allocation.

If you are still I/O-bound, the levers are user-side:

- **File striping.** A Lustre file on a single OST is bandwidth-capped no
  matter the thread count. New outputs: `lfs setstripe -c 4 outfile.hipo`;
  existing files: `lfs migrate -c 4 file.hipo`.
- **Thread oversubscription.** Pass `threads = 2 × num_cpus` to
  `for_each` to hide network page-fault stalls.
- **Stage to local scratch.** `cp /volatile/.../file.hipo /scratch/$USER/`
  before analysing — local disk easily beats Lustre per single client.

## When LZ4 itself is the bottleneck

Once page-fault stalls are masked, LZ4 inflate dominates. The stock HIPO format
stores **one LZ4 block per record**, so reading any bank inflates *every* bank.
Two opt-in format extensions fix that by storing sub-record streams and inflating
only what an analysis touches:

- **`Compression::Lz4ByBankV2`** — one LZ4-HC stream per bank type, plus a
  compressed presence directory. `ev.bank("REC::Particle")` inflates only that
  bank's stream; untouched banks stay compressed. No reader-side code change.
- **`Compression::Lz4PerColumn`** — one LZ4-HC stream per `(bank, column)`,
  cross-event contiguous. Reads at *column* granularity and compresses better
  than a bank's interleaved bytes, so it beats `Lz4ByBankV2` on both size and
  selective reads. It's what `skim` defaults to.

Both carry Rust-only wire tags the C++ `hipo4` reader can't read (the stock
`None` / `Lz4` / `Lz4Best` / `Gzip` codecs stay compatible).

Every format, on 50k events of a real CLAS12 file (274 banks; Apple M4 Pro,
single thread, warm cache). `Ratio` is file size vs `None`; `sel`/`all` are the
ms to read one bank / all 274:

| Format | Size MB | Ratio | sel (1 bk) | all (274) |
|---|--:|--:|--:|--:|
| `None` | 1734 | 1.00× | 158 | 1589 |
| `Lz4` | 1081 | 0.62× | 396 | 1817 |
| `Lz4Best` | 922 | 0.53× | 395 | 1826 |
| `Gzip` | 852 | 0.49× | 2878 | 4348 |
| **`Lz4ByBankV2`** | 872 | 0.50× | **86** | 1529 |
| **`Lz4PerColumn`** | **813** | **0.47×** | **75** | **1280** |

`Lz4PerColumn` is the **smallest file** (beating even Gzip) *and* the **fastest
read at every scope** — one bank is ~5× faster than whole-record `Lz4` because it
inflates only that bank's columns. `Gzip` packs tightly but is an order of
magnitude slower to inflate. The same win reaches Python: `arrays("REC::Particle")`
is ~4× faster on the per-bank/per-column formats than on `Lz4`. And on a 29.7 GB
ifarm skim (parallel, Lustre) the by-bank format hit **36.6 Mev/s — 25× the `Lz4`
baseline**.

Reproduce (both benchmarks read the same per-format files):

```sh
OXIHIPO_BENCH_KEEP=/tmp/fmt \
  cargo run --release --example bench_read_compression -- rec.hipo 3 50000
python py/examples/bench_compression.py /tmp/fmt 20000 3   # Python side
# and re-emit a real file as Lz4ByBankV2:
cargo run --release --example recook_by_bank -- in.hipo out_by_bank.hipo
```

Full format guide, trade-offs, and the complete tables (all read scopes, Rust +
Python):
**[Compression formats](https://mathieuouillon.github.io/oxihipo/docs/performance/compression)**
and **[Benchmarks](https://mathieuouillon.github.io/oxihipo/docs/performance/benchmarks)**.

## Known gaps

- `SortedWriter` and `StreamWriter` (per-tag bin writers, auto-flush) —
  deferred.
- Bench-vs-`hipo4` comparator — deferred.
- **Intra-stream parallel inflate** for the by-bank / per-column formats, for
  very large records where a single bank's (or column's) stream is multi-MB.
  Those streams already parallelise *across* banks in `for_each`; splitting a
  single large stream is the next step if profiles say one dominates.

## CI gates

Every PR runs:
- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --all-targets` — unit + integration tests, including a broad
  end-to-end pass over the read/write API — metadata, sequential + random-access
  + parallel reads, every data type, filters, the columnar reader, `skim`, and
  multi-file chains (`tests/end_to_end.rs`) — plus an all-compression-format
  write → read → cross-format-`skim` round-trip (`tests/all_formats.rs`)
- a smoke-run of the core examples end-to-end (write → read → recook → read; plus
  a small all-format encode/read sweep) so a runtime break in an example fails CI
- `cargo doc --no-deps` with `RUSTDOCFLAGS=-D warnings`

The Python wheel workflow additionally runs pytest, `mypy`, and `mypy.stubtest`
against a freshly built wheel.

## License

Licensed under the [MIT License](LICENSE).
