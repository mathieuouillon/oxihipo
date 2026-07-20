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
| `.tag_names(names)` | persist a name↔bit tag registry (see [Tagging events](#tagging-events)) |
| `.max_record_events(n)` | flush a record after `n` events |
| `.max_record_bytes(n)` | flush a record once it reaches `n` bytes |
| `.build()` | produce the `Writer` |

The two `max_record_*` knobs control record granularity. Bigger records
compress better; smaller records give parallel readers finer-grained units and
lower the reader's resident memory.

## Choosing a compression

```rust
use oxihipo::Compression;

Compression::None            // no compression
Compression::Lz4             // stock, hipo4-compatible
Compression::Lz4Best         // HC level (needs the `lz4-c` feature)
Compression::Gzip            // stock, hipo4-compatible
Compression::Lz4PerBank     // per-bank streams, lazy inflate
Compression::Lz4PerColumn    // per-column streams, best ratio + finest reads
```

`None`, `Lz4`, `Lz4Best`, and `Gzip` stay byte-compatible with the C++ `hipo4`
reader. `Lz4PerBank` and `Lz4PerColumn` are **opt-in format extensions** with
new compression tags that `hipo4` doesn't know about — use them for Rust-only
(or oxihipo-Python-only) consumers.

If you're deciding between them, read
[Compression formats](../performance/compression.md) — the short version is that
`Lz4PerBank` is usually the one you want, and `Lz4PerColumn` (what `skim`
defaults to) squeezes the file smaller still.

## Tagging events

Every event carries a 32-bit tag (`EH_TAG`). Set it inside the event closure
with `ev.with_tag(...)`; a reader then filters on it without inflating any bank
(see [Reading](./reading.md)). A raw `u32` works, but a `tag_flags!` block turns
the bits into named physics categories:

```rust
oxihipo::tag_flags! { pub EventTag { Dvcs = 0, Sidis = 1, Elastic = 2 } }

let mut w = Writer::create("out.hipo")
    .schemas(dict)
    .tag_names(EventTag::NAMES)   // persist the name↔bit registry (optional)
    .build()?;

w.event(|ev| {
    ev.with_tag(EventTag::Dvcs | EventTag::Sidis);  // a TagSet, or a raw u32
    // ... banks ...
    Ok(())
})?;
```

`with_tag` accepts a raw `u32` **or** a `TagSet`. `tag_names` is optional: it
writes the `(name, bit)` table into the file's dictionary record so a consumer
can resolve names *without* the `tag_flags!` declaration —
`chain.tag_registry()` in Rust, `f.tag_names` / `filtered(event_tag="dvcs")` in
Python. The registry is additive (readers that don't know it skip it) and `skim`
copies it through, so a tagged DST stays self-describing. Writing no registry
leaves the output byte-for-byte as before.

### Writing tagged DSTs

To *produce* a tagged file — classify each event and stamp the result — use
`Chain::skim_tagged`. It copies the (filtered) chain like `skim`, but a
classifier closure computes each event's new `EH_TAG`, and `tag_names` records
the output registry so the DST rereads by name. This closes the
select→label→write→reread loop:

```rust
oxihipo::tag_flags! { pub Cat { Dvcs = 0, Sidis = 1 } }

Chain::open("run.hipo")?
    .with_filter(Filter::require(["REC::Particle"]))?     // select
    .skim_tagged("dvcs.hipo", Compression::Lz4PerColumn, Cat::NAMES, |ev| {
        if is_dvcs(ev) { Cat::Dvcs } else { Cat::Sidis }  // label
    })?;                                                  // write

// reread: Chain::open("dvcs.hipo")? carries the Cat registry, so
// Filter::event_tag_any(Cat::Dvcs) — or filtered(event_tag="dvcs") in Python.
```

The retag touches only the event header; banks are copied through unchanged, so
it is as cheap as a plain skim. The source file's own registry is *not* carried
over (the closure defines a fresh scheme). A runnable end-to-end demo is
[`examples/tag_and_skim.rs`](https://github.com/mathieuouillon/oxihipo/blob/main/examples/tag_and_skim.rs).

## Array columns

A column can hold a **fixed-length array** instead of a scalar. In schema text a
column type is a type letter optionally followed by `#N`: `F#3` is three
`float32` per row, `S#2` two `int16` (`F`=f32, `D`=f64, `I`=i32, `S`=i16,
`B`=i8, `L`=i64). Declare it as text, or from `(name, type, length)` triples
where `length > 1` makes the column an array:

```rust
use oxihipo::{DataType, Schema};

// text form — name/T#N
Schema::parse_text("{REC::Traj/100/1}{trk_id/I,cov/F#6,hits/S#3}")?;

// or programmatically
Schema::from_columns(
    "REC::Traj",
    100,
    1,
    [
        ("trk_id".into(), DataType::Int, 1),
        ("cov".into(), DataType::Float, 6),
        ("hits".into(), DataType::Short, 3),
    ],
);
```

Write a row's array with the same `set` you use for scalars — pass the array,
and its length must match the declared `N`:

```rust
b.row(|r| {
    r.set("trk_id", 7_i32)?;
    r.set("cov", [0.0_f32, 0.1, 0.2, 0.3, 0.4, 0.5])?;
    r.set("hits", [1_i16, 2, 3])?;
    Ok(())
})?;
```

Reading them back is covered in
[Reading · Array columns](./reading.md#array-columns); from Python they arrive
as fixed-size sublists — see
[Python · Array columns](../python/reading.md#array-columns). A runnable
end-to-end example is
[`examples/write_array.rs`](https://github.com/mathieuouillon/oxihipo/blob/main/examples/write_array.rs).

:::note Fixed length only
Every row of a `T#N` column has the same `N`. Genuinely ragged per-row lengths
aren't a column type — model those as separate bank rows cross-referenced by an
index column (the CLAS12 `pindex` pattern).
:::

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
