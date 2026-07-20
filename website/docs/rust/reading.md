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

See [Writing](./writing.md) for full control over the output.
