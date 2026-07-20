---
id: event-tagging
title: Event tagging
sidebar_position: 3
---

# Event tagging — design & roadmap

Every HIPO event already carries a 32-bit tag (`EH_TAG`, the per-event header
word). oxihipo turns that latent field into a first-class feature: a **pushed-down
filter**, **named flags**, a **self-describing on-disk registry**, and a way to
**write** freshly-tagged files — enough to close the whole
select → label → write → reread loop for physics event classification (DVCS,
SIDIS, elastics, trigger categories, …) without a heavyweight index.

Usage lives in the guides — [Rust · Reading](../rust/reading.md),
[Rust · Writing](../rust/writing.md#tagging-events), and
[Python · Reading](../python/reading.md#filtering-by-tag-name). This page is the
design record and the roadmap.

## What's shipped

The feature landed in phases, each wire-compatible (tags are just bits of a word
that has always been there):

| Phase | Capability | Surface |
|---|---|---|
| **1** | Pushdown event-tag filter — drop events by `EH_TAG` without inflating any bank | `Filter::event_tag([…])` / `event_tag_any(mask)` |
| **1b** | Tag column aligned 1:1 with the columnar read | `Chain::event_tags(range, threads)` · Python `f.event_tags()` |
| **2a** | Named single-bit flags as compile-time constants | `TagSet` + the `tag_flags!` macro |
| **2b** | Persist the name↔bit registry in the file, so tags self-describe | `Writer::tag_names` · `Chain::tag_registry` · Python `f.tag_names`, `filtered(event_tag="dvcs")` |
| **3** | Write tagged DSTs — classify each event and stamp the result | `Chain::skim_tagged` · Python `f.skim(…, tags=, tag_names=)` |

The tag is read from the event header (stock formats) or the record directory
(`Lz4PerBank` / `Lz4PerColumn`) — never by decompressing a bank. The name
registry rides in the file's dictionary record as one extra text bank, so it is
additive: a reader that doesn't know about it simply skips it.

## Performance: the pushdown is free

The filter runs *before* a bank is touched, so filtering by tag can only save
work. The question that matters is the other direction — does the per-event tag
check cost anything on a read? Measured with `examples/bench_event_tags.rs`
(Apple M4 Pro, single thread, warm cache, 200–300k synthetic events), comparing
an unfiltered `for_each` (which never calls the filter) against an all-pass
`event_tag_any` filter (which runs the check on **every** event):

| Path | Baseline | + all-pass tag filter | Δ |
|---|--:|--:|--:|
| `Lz4` (tag from event header) | ~7.9 ns/ev | ~7.9 ns/ev | **≈ 0** (±0.05) |
| `Lz4PerColumn` (tag from directory) | ~4.2 ns/ev | ~4.2 ns/ev | **≈ 0** (±0.05) |

The check is a single `u32` read plus a compare — below the measurement noise on
both paths, and an unfiltered read never enters the filter at all, so scans that
don't use tags are unchanged. `event_tags()` runs at the speed of a bare scan
(no bank inflation). Reproduce:

```sh
cargo run --release --example bench_event_tags -- 200000 15
```

## Roadmap

The five shipped phases cover the common case: a per-event `u32` used as up to 32
named bit-flags. Two heavier extensions are **deferred** — they solve real but
rarer needs and touch more of the format, so they wait until a workload asks.

### Phase 4 — writer-side record-tag routing

**What.** A public API on the `Writer` to set the per-*record* tag
(`user_word_1`) as records are flushed, and to route events into records by a
coarse key — so all events of a category land in the same records.

**Why.** oxihipo already reads record tags with pushdown
(`Filter::record_tag`), which skips a whole record (thousands of events) on a
single header read — strictly cheaper than the per-event check. But nothing on
the writer *produces* that layout today: `skim` renumbers output records and
tags them `0`. Phase 4 makes the coarse pushdown usable end-to-end: bin a skim
by category, and a later read of one category touches only its records.

**Shape (proposed).** A `skim_binned(|ev| -> u64)`-style routing closure that
groups consecutive same-key events into records and stamps `user_word_1`, plus a
`Writer::set_record_tag` for the manual path. It composes with Phase 3 —
per-event tags for fine selection, per-record tags for cheap coarse skipping.

**Cost / risk.** Buffering to group by key fights the streaming, bounded-memory
writer; the simplest version only groups *runs* of same-key events (no global
sort), which keeps memory bounded but needs the caller to pre-sort for full
benefit. Deferred until a binning workload is concrete.

### Phase 5 — a schema'd `TAG::Event` bank

**What.** An optional, dictionary-described bank (e.g. `TAG::Event`) written
alongside the data, holding richer per-event labels than 32 bits: named integer
categories, floats (a classifier score, a weight), or several tag columns.

**Why.** `EH_TAG` is exactly 32 bits with no room for payloads. Analyses that
want a BDT/DNN score per event, an MC-truth channel id, or more than 32
mutually-exclusive categories currently can't express that in the tag. A schema'd
bank reuses everything oxihipo already does well — typed columns, zero-copy
reads, per-column compression, `arrays()` in Python — so the labels read like any
other bank.

**Shape (proposed).** A convention (a reserved bank name + helper builders) plus
optional pushdown that reads just the `TAG::Event` bank's columns to filter,
analogous to today's per-column tag read. No new wire format — it's an ordinary
bank — so it is automatically C++-`hipo4`-readable, unlike the `EH_TAG`
directory path.

**Cost / risk.** Larger than a header word (a real bank per event), and it
overlaps with "just make your own bank," so the value is mostly the *pushdown*
and the ergonomic helpers. Deferred until >32 flags or per-tag payloads are
actually needed.

### Not planned

Anything that would pull physics, ROOT, or a schema-migration layer into the
reader stays out of scope, matching the crate's boundaries.
