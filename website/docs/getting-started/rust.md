---
id: rust
title: Rust
sidebar_position: 1
---

# Getting started with Rust

## Add to your project

Not yet published to crates.io — depend on it via git:

```toml
[dependencies]
oxihipo = { git = "https://github.com/mathieuouillon/oxihipo" }
```

Requires Rust 1.87+.

### Optional features

Pure-Rust by default; nothing below is required.

| Feature | What it gives you |
|---|---|
| `lz4-c` | C LZ4 bindings — faster decode, plus the `Lz4Best` HC level |
| `lz4-apple` | Apple `libcompression` decode |
| `mimalloc-allocator` | mimalloc, for allocation-heavy workloads where the system allocator underperforms on macOS |

## Read your first file

```rust
use oxihipo::{Chain, Filter};

fn main() -> oxihipo::Result<()> {
    // Single file or many — `Chain` is the sole reader entry point.
    let chain = Chain::open("rec.hipo")?
        .with_filter(Filter::require(["REC::Particle"]))?;

    // Each item is a `Result<OwnedEvent>`; `?` propagates a corrupt record.
    // Each `OwnedEvent` is a slice into a shared, ref-counted record buffer —
    // no per-event allocation.
    for ev in chain.events() {
        let ev = ev?;
        let p = oxihipo::or_continue!(ev.bank("REC::Particle"));
        for r in 0..p.rows() {
            let pid: i32 = p.get("pid", r);
            let px: f32 = p.get("px", r);
            let _ = (pid, px);
        }
    }
    Ok(())
}
```

That is the whole shape of the API. [Reading](../rust/reading.md) covers chains,
parallel scans, and the typed accessors; [Writing](../rust/writing.md) covers
the `Writer` builder.

## Build and run the examples

```sh
cargo build --release
cargo test

cargo run --release --example write     -- /tmp/demo.hipo
cargo run --release --example read      -- /tmp/demo.hipo
cargo run --release --example parallel  -- /path/to/file.hipo 0
cargo run --release --example bench_par -- /path/to/file.hipo 0
```

`bench_par` is the one to reach for when you want throughput numbers on your own
hardware — the second argument is the thread count (`0` = all cores).

## Where to go next

- [Reading](../rust/reading.md) — chains, filters, `for_each`, typed columns
- [Writing](../rust/writing.md) — the `Writer` builder and compression choices
- [Compression formats](../performance/compression.md) — `Lz4ByBankV2`, and why it
  matters more than thread count
- [Shared filesystems](../performance/shared-filesystems.md) — what to do when
  ifarm's Lustre is the bottleneck
