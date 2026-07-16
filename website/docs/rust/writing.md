---
id: writing
title: Writing
sidebar_position: 2
---

# Writing

Writing is closure-driven: a `Writer` builder, then one closure per event, one
per bank, one per row.

```rust
use oxihipo::{Compression, Writer};

let mut w = Writer::create("out.hipo")
    .schemas(dict)
    .compression(Compression::Lz4)
    .build()?;

w.event(|ev| {
    ev.bank("REC::Particle", |b| {
        b.row(|r| {
            r.set("pid", 11_i32)?;
            r.set("px", 0.5_f32)
        })?;
        Ok(())
    })?;
    Ok(())
})?;

w.finish()?;
```

`finish()` writes the trailer and index — a `Writer` dropped without it leaves a
file no reader will accept, so don't skip it.

## The builder

`Writer::create(path)` returns a builder:

| Method | Purpose |
|---|---|
| `.schemas(dict)` | the `Dict` describing every bank you'll write (required) |
| `.compression(c)` | see below (defaults to `Lz4`) |
| `.max_record_events(n)` | flush a record after `n` events |
| `.max_record_bytes(n)` | flush a record once it reaches `n` bytes |
| `.build()` | produce the `Writer` |

The two `max_record_*` knobs control record granularity. Bigger records
compress better; smaller records give parallel readers finer-grained units and
lower the reader's resident memory.

## Choosing a compression

```rust
use oxihipo::Compression;

Compression::None                                   // no compression
Compression::Lz4                                    // stock, hipo4-compatible
Compression::Lz4Best                                // HC level (needs the `lz4-c` feature)
Compression::Gzip                                   // stock, hipo4-compatible
Compression::Lz4Chunked { events_per_chunk: 32 }    // intra-record parallel inflate
Compression::Lz4ByBank                              // per-bank streams, lazy inflate
Compression::Lz4ByBankV2
Compression::Lz4PerColumn
```

`None`, `Lz4`, `Lz4Best`, and `Gzip` stay byte-compatible with the C++ `hipo4`
reader. The `Lz4Chunked` / `Lz4ByBank` family are **opt-in format extensions**
with new compression tags that `hipo4` doesn't know about — use them for
Rust-only (or oxihipo-Python-only) consumers.

If you're deciding between them, read
[Compression formats](../performance/compression.md) — the short version is that
`Lz4ByBank` is usually the one you want, and it's what `skim` defaults to.

## Copying events verbatim

`append_raw(&[u8])` writes an already-encoded event through unchanged. This is
what `Chain::skim` uses internally:

```rust
for ev in chain.events() {
    w.append_raw(ev?.bytes())?;
}
```

It skips decode and re-encode entirely, so a skim is bounded by I/O and
recompression rather than by parsing.
