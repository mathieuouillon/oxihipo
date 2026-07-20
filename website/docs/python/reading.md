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
summary = g.skim("electrons.hipo", compression="lz4percolumn")   # {events, records, bytes}
```

`filtered()` returns a new chain; the filter reduces what `arrays()` / `skim()`
yield. Its `num_entries` stays the **pre-filter** total, as in uproot.

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
