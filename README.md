# hipo-rs

Pure-Rust port of the HIPO data format library used at Jefferson Lab CLAS12.
The goal is **read throughput meaningfully exceeds the C++ `hipo4` reader**
on the same hardware, with an API that fits Rust idioms.

This crate reads and writes HIPO version 6 files. Physics, FFI, ROOT, and
XRootD layers are intentionally out of scope.

## Quick start

```rust
use hipo::{Chain, Filter};

# fn main() -> hipo::Result<()> {
// Single file or many — `Chain` is the sole reader entry point.
let chain = Chain::open("rec.hipo")?
    .with_filter(Filter::require(["REC::Particle"]))?;

// Plain `for` loop. Each `OwnedEvent` is a slice into a shared,
// ref-counted record buffer — no per-event allocation.
for ev in chain.events() {
    let p = hipo::or_continue!(ev.bank("REC::Particle"));
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
use hipo::Chain;

# fn main() -> hipo::Result<()> {
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
use hipo::Chain;

# fn main() -> hipo::Result<()> {
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
use hipo::{Compression, Writer};

# fn run(dict: &hipo::Dict) -> hipo::Result<()> {
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

- Single `hipo` library crate. No bundled binary; downstream consumers
  build whatever frontend they need on top.
- `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D
  warnings`, and `cargo fmt --check` all clean.
- Validated on a 1.7 GB CLAS12 file (`rec_clas_022050.evio.00000.hipo`):
  a sequential `Chain::events()` scan reads all 187,941 events at
  ~257 kev/s. `Chain::par_reduce` fans the same scan across cores; measure
  throughput on your hardware with the `bench_par` example.

## Layout

```
crates/
  hipo       library — error, wire, compress, schema, event, read, write
```

Inside `crates/hipo/src`:

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
cargo build --release --workspace
cargo test --workspace

# Examples
cargo run -p hipo --release --example write     -- /tmp/demo.hipo
cargo run -p hipo --release --example read      -- /tmp/demo.hipo
cargo run -p hipo --release --example parallel  -- /path/to/file.hipo 0
cargo run -p hipo --release --example bench_par -- /path/to/file.hipo 0
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

## Known gaps

- `SortedWriter` and `StreamWriter` (per-tag bin writers, auto-flush) —
  deferred.
- Bench-vs-`hipo4` comparator — deferred.

## CI gates

Every PR runs:
- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `cargo doc --no-deps` with `RUSTDOCFLAGS=-D warnings`
