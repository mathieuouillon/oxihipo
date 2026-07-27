---
id: reading
title: Reading
sidebar_position: 1
---

# Reading

`Chain` is the only reader. A chain of one file is the common case; multi-file
chains share one parsed dictionary and stream records on demand.

## Opening a chain

`Chain::open` takes a single file, a directory, a glob, or a list of paths:

```rust
use oxihipo::Chain;

let one   = Chain::open("rec.hipo")?;
let dir   = Chain::open("/data/cooked/run5042")?;   // every *.hipo inside
let glob  = Chain::open("/data/*.hipo")?;
let list  = Chain::open(["a.hipo", "b.hipo"])?;     // verbatim, in order
```

A single path auto-detects: an existing **file** opens directly, an existing
**directory** expands to its sorted `*.hipo` children, and anything else is
treated as a **glob**. A slice or `Vec` is taken verbatim.

`Chain::open` validates that every file in the chain has the same `Dict`, which
catches mismatched cooking versions at construction time rather than halfway
through a scan.

## Iterating events

```rust
use oxihipo::{Chain, Filter};

let chain = Chain::open("/data/cooked/run5042")?
    .with_filter(Filter::require(["REC::Particle"]))?;

let mut total_rows: u64 = 0;
for ev in chain.events() {
    let ev = ev?;                       // corrupt/truncated record → Err
    total_rows += ev.bank("REC::Particle").map_or(0, |b| b.rows() as u64);
}
```

`events()` yields `Result<OwnedEvent>`, so a corrupt or truncated record
surfaces as an `Err` you propagate with `?` rather than a panic. Each
`OwnedEvent` is a slice into a shared, ref-counted record buffer — there is no
per-event allocation.

## Random access

When you already know which events you want — a list of indices from an earlier
pass, say — `chain.event(i)` fetches one directly:

```rust
let chain = Chain::open("rec.hipo")?;
for &i in &interesting {
    let Some(ev) = chain.event(i) else { continue };   // None if out of range
    let _ = ev.bank("REC::Particle");
}
```

HIPO stores events inside compressed records, so reaching event `i` means
decoding the record that contains it. The chain keeps **the last decoded record**
(one entry, shared across clones), so a lookup landing in the same record as the
previous one costs a slice or an index rather than a fresh inflate. All three
record layouts are cached — the classic decompressed payload and the lazy
by-bank / per-column records.

That makes the access *pattern* matter more than the call count: an ascending
run of indices mostly hits the cache, while a wide scatter mostly misses it and
pays a record decode each time. If you are visiting most events anyway, iterate
with `events()` instead.

:::info
The cache is consulted **only** by `Chain::event`. `events()` and `for_each`
never touch it, so sequential and parallel scan throughput are unaffected.
:::

:::tip Filters
`Filter::require([...])` keeps only events carrying every named bank;
`.record_tag([…])` skips whole records by their tag; `.event_tag([…])` /
`.event_tag_any(mask)` keep events by their per-event `EH_TAG` (read without
inflating any bank). All clauses AND together, and `with_filter` is cheap — it
clones the shared file handles rather than reopening.

`event_tag_any` (and the writer's `with_tag`) accept a raw `u32` **or** a
`TagSet` — declare named flags with the `tag_flags!` macro so a tag reads like
the physics it encodes:

```rust
oxihipo::tag_flags! { pub EventTag { Dvcs = 0, Sidis = 1 } }
let g = chain.with_filter(Filter::new().event_tag_any(EventTag::Dvcs | EventTag::Sidis))?;
```

Record the names in the file with [`Writer::tag_names`](./writing.md#tagging-events)
(`.tag_names(EventTag::NAMES)`) and a reader recovers them without the
`tag_flags!` declaration — `chain.tag_registry()` returns a `TagRegistry` whose
`.mask("dvcs")` feeds straight back into `event_tag_any`. This is what powers
`filtered(event_tag="dvcs")` in the Python binding, and `skim` copies the
registry into the output.
:::

## Parallel scans

`for_each` fans the work across cores. The `threads` argument is the *only*
difference between a sequential and a parallel scan:

| `threads` | Behaviour |
|---|---|
| `0` | one worker per logical CPU |
| `1` | on the calling thread, in order |
| `n` | exactly `n` workers |

Parallel modes visit events **out of order**, so shared state must be atomic or
locked:

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use oxihipo::Chain;

let chain = Chain::open("/data/cooked/run5042")?;

let total_rows = AtomicU64::new(0);
chain.for_each(0, |ev| {                    // 0 → all cores
    if let Some(b) = ev.bank("REC::Particle") {
        total_rows.fetch_add(b.rows() as u64, Ordering::Relaxed);
    }
})?;
println!("{}", total_rows.into_inner());
```

Resident memory stays bounded — one record per worker — no matter how large the
file, so a wide parallel scan won't be OOM-killed by a memory-capped batch
allocation.

## Reading columns

There are three accessors, in increasing order of how much you care about the
inner loop.

### `get` — the infallible scalar

The one for hot loops. The type is inferred from the binding, and a
missing or wrong-type column returns `T::default()`:

```rust
let pid: i32 = b.get("pid", row);
let px:  f32 = b.get("px",  row);
```

### `col` — the whole column, usually without copying

```rust
let px: std::borrow::Cow<[f32]> = b.col::<f32>("px")?;
```

Returns `Cow<[T]>`: **zero-copy** when the bank's bytes are aligned to `T`
(always for 4-byte types, usually for 8-byte types), and a one-shot
`read_unaligned` copy otherwise — matching the C++ reader's memcpy semantics.

### `ColumnHandle<T>` — resolve the name once

For loops where even a name lookup per event is too much. Resolve against the
`Schema` once, then `bank.read(h)` is a constant-time cast:

```rust
let h = schema.handle::<f32>("px");
// ... inside the loop:
let px = bank.read(h);
```

### Array columns

A column declared `name/T#N` (see [Writing](./writing.md#array-columns)) holds a
fixed-length array per row. Read it three ways:

```rust
let cov = bank.col::<[f32; 3]>("cov")?;          // whole column — one [f32; 3] per row
let one = bank.get::<[f32; 3]>("cov", row);       // one row's array (infallible; default on mismatch)
let dynamic = bank.array_at::<f32>("cov", row)?;  // runtime length → Cow<[f32]>
```

`col` / `get` take the length as a const generic — the same zero-copy fast path
as the scalar reads, and `get` can be inferred from the binding
(`let cov: [f32; 3] = bank.get("cov", row);`). `array_at` is the escape hatch for
when `N` isn't known at compile time (e.g. a generic dump tool walking a
dictionary): it returns a `Cow<[T]>` slice.

## What's in the file — `bank_occupancy`

Which banks carry data, in how many events, and how many rows — **without
inflating a single bank or column**:

```rust
let occ = chain.bank_occupancy(None, 0)?;   // whole chain, all cores
for b in &occ.banks {
    if b.events == 0 {
        continue;                            // declared, never populated
    }
    let pct = 100.0 * b.events as f64 / occ.events_scanned as f64;
    println!("{:<24} {:>7} events ({pct:.1}%)  {} rows, max {}",
             b.name, b.events, b.total_rows, b.max_rows);
}
```

Every number is a function of a bank's per-event *byte extent*, which both
columnar layouts already record in their bank-offset tables. So on `Lz4PerBank`
and `Lz4PerColumn` nothing beyond each record's header and offset tables is
decompressed. The classic layouts have no such table, so their records are
inflated and their events walked — but with no per-event allocation.

`range` restricts to global event indices `[start, stop)`; `threads` follows the
usual convention (`0` = all cores, `1` = sequential). The chain filter applies.

`events_scanned` is part of the result rather than something you compute,
because it is the denominator of every percentage above and you cannot recover
it from the bank counts. **`event_count()` is not it** — that is pre-filter, so
under a filter every rate derived from it is wrong.

Banks declared but never written come back with zero counts rather than being
omitted, so "never populated" stays distinguishable from "not in the
dictionary". A bank opened with no rows counts as carrying no data.

:::tip Don't rebuild this from `events()`
It is tempting to walk [`events()`](#iterating-events) and enumerate each
event's structures. That costs an `OwnedEvent` — a copy of every event's bytes —
and on a per-column file enumerating an event's structures *synthesises the
whole event* out of separate column streams first. Measured on 20,000 events of
a CLAS12 run: 19 µs/event on `Lz4PerBank`, 26 µs/event on `Lz4PerColumn`,
against 1.5 µs/event here.

`EventCtx` looks like the way around the copy and is not: it cannot enumerate a
per-column record's banks at all, because that needs exactly the synthesis it
exists to avoid. A caller who tried it got **4 banks out of 71** — fast,
plausible, and wrong with no error anywhere.
:::

## Typed rows

`ev.rows::<T>()` decodes a bank into a generated row struct; `bank_row!` builds
those structs, and the `clas12` module ships pre-generated ones for the common
CLAS12 banks. `rows_for_pindex` / `rows_for_index` cover the usual
cross-referencing patterns.

## Skimming

`skim` copies the (filtered) chain to a new file, re-compressing as it goes:

```rust
let summary = chain.skim("electrons.hipo", oxihipo::Compression::Lz4PerColumn)?;
```

`skim_tagged` is the same copy but **retags** each event from a classifier
closure (and records an output tag registry) — the way to *write* a tagged DST.
See [Writing · Tagging events](./writing.md#tagging-events).

See [Writing](./writing.md) for full control over the output.
