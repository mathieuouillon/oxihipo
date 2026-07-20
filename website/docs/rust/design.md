---
id: design
title: Design decisions
sidebar_position: 3
---

# Design decisions

Why the API looks the way it does.

## `Chain` is the only reader

A chain of one file is the common case, so there is no separate single-file
type. Multi-file chains share one parsed dictionary and stream records on
demand — nothing is whole-file mapped. `Chain::open` validates that every file
in the chain carries the same `Dict`, which turns "mismatched cooking versions"
from a confusing mid-scan failure into a construction-time error.

## Borrowed bytes, with the dictionary attached

`Event<'a>` carries only borrowed bytes. The handle you actually use is
`EventCtx<'a>`, which is `(Event<'a>, &Dict)` — that pairing is what lets
`ev.bank("REC::Particle")` resolve a schema without you juggling the dictionary
separately.

## One typed getter, not six

`Bank::col::<T>("name")` is the single typed column accessor, generic over `T`,
replacing what would otherwise be six per-type methods. It returns `Cow<[T]>`:

- **Zero-copy** when the bank's bytes are aligned to `T` — always true for
  4-byte types, usually true for 8-byte types.
- **A one-shot `read_unaligned` copy** otherwise, which matches the C++ reader's
  memcpy semantics.

`Bank::get::<T>("name", row)` is the infallible scalar accessor for hot loops:
the type is inferred from the binding, and a missing or wrong-type column
returns `T::default()` rather than forcing a `Result` into the inner loop.

`ColumnHandle<T>` lives on `Schema` and is resolved once via
`schema.handle::<T>("name")`. Inside a hot loop, `bank.read(h)` is a
constant-time cast with no per-event name lookup.

## Fallible iteration

`chain.events()` yields `Result<OwnedEvent>`. An infallible variant existed once
and was removed: a corrupt or truncated record is a normal thing to encounter on
a shared filesystem, and it should be a value you can propagate with `?`, not a
panic in someone's overnight job.

## Bounded memory by construction

Records stream in one at a time via `pread` into a recycled buffer. Nothing is
mapped; nothing reads the file whole. A sequential scan of a 100 GB file holds
roughly one record resident (tens of MB); a parallel scan holds one per worker.
This is what makes the reader safe to run under a memory-capped batch
allocation, and it's why `for_each` can oversubscribe threads without blowing up
RSS.

## Storage-polymorphic events

`OwnedEvent` is polymorphic over its storage backend (`Bytes` vs `ByBank`).
That's the reason [`Lz4PerBank`](../performance/compression.md) needs **no
reader-side API change**: downstream code is identical whether or not the input
uses per-bank streams, and banks an analysis never touches simply stay
compressed for the record's lifetime.

## mimalloc is opt-in

Gated behind the `mimalloc-allocator` feature, off by default. It helps
allocation-heavy workloads where the system allocator underperforms on macOS,
but it isn't something a library should impose on every consumer.

## Crate layout

Single crate (`oxihipo`). Inside `src/`:

| Module | Contents |
|---|---|
| `error.rs`, `prelude.rs` | error type, common re-exports |
| `wire/` *(private)* | constants, byte readers, headers, record decompression |
| `compress.rs` *(private)* | LZ4/gzip plus a reusable `ScratchBuf` |
| `schema/` | `Schema`, `Dict`, `DataType`, typed `ColumnHandle<T>` |
| `event/` | `Event`, `EventCtx`, `Bank`, `Composite`, `OwnedEvent`, internal builders |
| `read/` | `Chain` (`Arc<FileInner>`-backed), the event iterator, `Filter`, parallel `for_each`, `ChainStats` |
| `write/` | `Writer` builder, `BankWriter`, `RowWriter`, `Compression` |
