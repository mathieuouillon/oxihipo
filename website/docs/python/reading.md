---
id: reading
title: Reading
sidebar_position: 1
---

# Reading

```python
import oxihipo as ox

f = ox.open("run5042.hipo")     # file | dir | glob | list of paths
```

A single string auto-detects — an existing file opens directly, a directory
expands to its `*.hipo` children, anything else is a glob. A list is taken
verbatim, so don't wrap a single path in one.

## The accessors

| Call | Returns |
|---|---|
| `f.arrays(bank, [cols])` | `ak.Array` — jagged record `N * var * {col: T}` |
| `f.arrays([bankA, bankB])` / `f.arrays(filter_name="REC::*")` | record with one field per bank |
| `f.array(bank, col)` | one column, `N * var * T` |
| `f.numpy(bank, col)` | `NumpyColumn(values, offsets, inner_len)` — plain NumPy, no Awkward import |
| `f.event_tags()` | per-event tag (`EH_TAG`) as `uint32[n_events]`, aligned 1:1 with `arrays()` |
| `f.tag_names` | persisted tag registry as `{name: bit}` (empty if none) — see [Filtering by tag name](#filtering-by-tag-name) |
| `f["REC::Particle"]` | a **bank proxy**: `.keys()`, `.typenames()`, `.array(col)`, `["col"]` |
| `f["REC::Particle/px"]` | the `px` column |

```python
p = f.arrays("REC::Particle", ["pid", "px", "py", "pz"])
p.px                          # jagged: p[event].px indexes particles
ak.sum(p.px, axis=1)          # per-event reductions, no Python loop
```

`numpy()` returns a named tuple, so it still unpacks positionally while giving
you `.values` / `.offsets` / `.inner_len`:

```python
values, offsets, inner = f.numpy("REC::Particle", "px")
col = f.numpy("REC::Particle", "px")
col.offsets                   # int64, length = n_events + 1
```

## Common knobs

These work on `arrays` / `array` / `numpy` / `iterate`:

- **`entry_start=` / `entry_stop=`** — restrict to a global event range.
- **`filter_name="REC::*"`** — glob over `bank` / `bank/column` keys.
- **`library=`** — `"ak"` (default, `ak.Array`), `"np"` (dict of object-dtype
  `ndarray`), `"pd"` (pandas, one frame per bank), `"arrow"` (`pyarrow.Table`,
  one `large_list` column per field — for polars / duckdb).
- **`threads=`** — `0` = all cores (default), `1` = sequential, `n` = an
  `n`-thread pool. This is parallelism *within* one process.
- **`workers=`** — read with `N` **processes**; see
  [Parallel reading](./parallel.md).

:::note Empty selections don't raise
A non-matching `filter_name` or an empty bank list yields an *empty* result
rather than an error — a typo'd glob gives you back an empty array, not a
traceback.
:::

`columns=` is only valid with a single bank name. To select columns across
several banks, use `filter_name="BANK/col*"`.

## Array columns

A fixed-length array column (declared `cov/F#3` on the Rust side — three
`float32` per row) comes back as an extra **fixed-size axis** nested inside the
jagged array. Indexing goes event → row → the cell:

```python
p = f.arrays("REC::Track", ["cov"])
p.cov                             # N * var * 3 * float32  (a RegularArray inside the per-event list)
p.cov[3, 0]                       # event 3, track 0 → a length-3 subarray
ak.sum(p.cov, axis=-1)            # reduce the innermost (size-3) axis

f.typenames()["REC::Track/cov"]   # 'float32[3]'
```

Through NumPy the fixed length surfaces as `inner_len`, and the values buffer is
flattened while the shared `offsets` still index by row:

```python
col = f.numpy("REC::Track", "cov")
col.inner_len                     # 3  (1 for a scalar column, N for a T#N array)
col.values                        # float32, length = total_rows * 3
```

The nesting carries through every `library=` backend. Because the array axis is
**regular** (every cell the same length), reductions like `ak.sum(..., axis=-1)`
and NumPy reshapes are exact — no ragged handling needed.

## Discovery

```python
f.keys()                       # bank names
f.keys(recursive=True)         # 'bank/column' keys
f.keys(filter_name="REC::*")   # globbed
f.typenames()                  # {'REC::Particle/px': 'float32', 'REC::Track/cov': 'float32[3]'}
"REC::Particle" in f
list(f)                        # iterates bank names
```

`len(f)` is the **event** count, not the number of banks — matching uproot,
where `len(tree)` is `num_entries`. So `len(f)` and `len(list(f))` deliberately
differ.

## Selecting and writing

```python
g = f.filtered(require=["REC::Particle"])       # events carrying a bank
g = f.filtered(record_tag=[0x42])               # by record tag
g = f.filtered(event_tag=[1, 4])                # by per-event tag (EH_TAG), exact set
g = f.filtered(event_tag_any=0b101)             # tag bitmask: any of these bits set
summary = g.skim("electrons.hipo", compression="lz4percolumn")   # SkimSummary(events, records, bytes)
```

`filtered()` returns a new chain; the filter reduces what `arrays()` / `skim()`
yield. Its `num_entries` stays the **pre-filter** total, as in uproot.

### Filtering by tag name

If the file carries a **tag registry** (written by the producer — see the Rust
[`tag_flags!`](../rust/reading.md) / `Writer::tag_names` docs), you can filter by
name instead of remembering bit positions. `f.tag_names` is the persisted
`{name: bit}` map, and a name (or list of names) passed to `filtered` keeps
events with *any* of those bits set:

```python
f.tag_names                              # {'dvcs': 0, 'sidis': 1, 'elastic': 2}
g = f.filtered(event_tag="dvcs")         # events with the dvcs bit
g = f.filtered(event_tag=["dvcs", "sidis"])   # dvcs OR sidis
g = f.filtered(event_tag_any="elastic")  # same, spelled as a mask
```

Names resolve in the parent process, so `workers=` reads inherit the filter for
free. An unknown name raises `KeyError`. A file written without a registry has
an empty `f.tag_names`, and the numeric forms above still work.

### Tag-and-skim

To *write* a tagged file, compute one `uint32` tag per event (vectorized, with
NumPy/Awkward over the columns you read) and pass it to `skim` as `tags=`, with
`tag_names=` to record the registry. The `tags` array must align 1:1 with the
events the chain yields — same order and length as `event_tags()` / `arrays()`:

```python
f = ox.open("run.hipo").filtered(require=["REC::Particle"])
p = f.arrays("REC::Particle", ["px"])
tags = np.where(p.px[:, 0] > 2, 1, 0).astype(np.uint32)   # one per event
f.skim("dvcs.hipo", tags=tags, tag_names={"dvcs": 0})     # label + write

ox.open("dvcs.hipo").filtered(event_tag="dvcs")           # reread by name
```

This closes the select→label→write→reread loop. A `tags` length that doesn't
match the events written raises `ValueError`.

### Updating a tag in place

To change one event's tag on an **existing** file without rewriting it,
`f.set_event_tag(entry, tag)` patches the 4 bytes on disk (and
`f.set_event_tags({entry: tag, ...})` a batch, all-or-nothing). It needs write
permission, and works **only for uncompressed files** (written with
`compression="none"`): a compressed file raises `ValueError` (its tag is inside
a compressed block — rewrite with `skim(tags=…)`), and an out-of-range entry
raises `IndexError`.

```python
f = ox.open("run.hipo")            # written with compression="none"
f.set_event_tag(42, 1)             # one 4-byte write, no rewrite
f.set_event_tags({10: 1, 20: 2})   # batch
```

## Resource management

The chain closes itself when it goes out of scope — the core reads with
positioned `pread` on a shared descriptor, so there's no mmap to unmap. If you
want an explicit scope, `with` works:

```python
with ox.open("run5042.hipo") as f:
    p = f.arrays("REC::Particle", ["px"])
```

Using a chain after `close()` raises a clear `ValueError` rather than an opaque
`NoneType` error.
