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

### Files with different banks

The files in a chain do not have to declare the same banks. A run period rarely
does — a pass-2 cook adds a bank, an MC file carries `MC::Lund` — so `keys()`
reports the **union** of every bank any file declares, and a bank absent from a
file gives empty entries for that file's events:

```python
f = ox.open("/cache/clas12/rg-a/production/pass2/*/dst/*.hipo")
f.keys()                              # every bank in any of them
f.arrays("ML::pred")                  # [] for events from files without it
```

Because the empties still occupy their events, columns from different banks stay
length-aligned and `ak.zip`-able however the chain is composed.

Two disagreements are refused rather than papered over, since neither can be
read correctly: one bank name describing two different layouts, and one
`(group, item)` id claimed by two different banks. The second matters most —
banks are located by id, so a collision would decode one file's bytes against
another file's schema and hand back wrong numbers instead of failing. Both
errors name the files and banks involved.

:::note Previously
Every file's dictionary had to match file 0's exactly, and the comparison was
even sensitive to the *order* the banks were written in — so a glob over a real
run period could fail on files that were, for a reader's purposes, identical.
:::

## The accessors

| Call | Returns |
|---|---|
| `f.arrays(bank, [cols])` | `ak.Array` — jagged record `N * var * {col: T}` |
| `f.arrays([bankA, bankB])` / `f.arrays(filter_name="REC::*")` | record with one field per bank |
| `f.array(bank, col)` | one column, `N * var * T` |
| `f.numpy(bank, col)` | `NumpyColumn(values, offsets, inner_len)` — plain NumPy, no Awkward import |
| `f.event_tags()` | per-event tag (`EH_TAG`) as `uint32[n_events]`, aligned 1:1 with `arrays()` |
| `f.tag_names` | persisted tag registry as `{name: bit}` (empty if none) — see [Filtering by tag name](#filtering-by-tag-name) |
| `f.show()` / `f.show(bank)` | print every bank and its `column: dtype` (human-readable) |
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
- **`entries=[...]`** (`arrays` only) — read exactly these global event indices
  instead of a range. See [Replaying a list of events](#replaying-a-list-of-events).
- **`cut="..."`** (`arrays` only) — filter rows or events with an expression.
  See [Cuts](#cuts).
- **`filter_name="REC::*"`** — glob over `bank` / `bank/column` keys.
- **`library=`** — `"ak"` (default, `ak.Array`), `"np"` (dict of object-dtype
  `ndarray`), `"pd"` (pandas, one frame per bank), `"arrow"` (`pyarrow.Table`,
  one `large_list` column per field — for polars / duckdb). All four, plus the
  raw-buffer `numpy()` path, are demonstrated in
  [`py/examples/interop.py`](https://github.com/mathieuouillon/oxihipo/tree/main/py/examples/interop.py).
- **`threads=`** — `0` = all cores (default), `1` = sequential, `n` = an
  `n`-thread pool. This is parallelism *within* one process.
- **`workers=`** — read with `N` **processes**; see
  [Parallel reading](./parallel.md).

:::note Empty selections don't raise
A non-matching `filter_name` or an empty bank list yields an *empty* result
rather than an error — a typo'd glob gives you back an empty array, not a
traceback.
:::

:::tip Feeding ROOT's RDataFrame
For ROOT users, `f.rdataframe(bank, cols)` (and the streaming
`iterate_rdataframe`) hands the same selection to
[RDataFrame](./rdataframe.md) — jagged banks become `RVec` columns.
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

## PDG masses

`pid` is a code, and kinematics need a mass. `ox.pdg_mass` maps one to the other
in bulk, keeping whatever shape you hand it — so the result lines up with the
momentum columns read beside it:

```python
p = f.arrays("REC::Particle", ["pid", "px", "py", "pz"])
m = ox.pdg_mass(p.pid)                       # jagged, same shape as p.pid
E = np.sqrt(p.px**2 + p.py**2 + p.pz**2 + m**2)
```

Masses are in **GeV**, matching CLAS12 momenta (`unit="MeV"` if you want the
other convention). `ox.pdg_name(11)` gives `'e-'`, for labelling.

Two CLAS12 details are handled that general PDG helpers get wrong:

* **`pid == 0`** — a track the reconstruction could not identify. Real files are
  full of them. You get `nan`, not an exception; it propagates into the
  kinematics it touches and cuts with `~np.isnan(m)`. Pass `unknown=0.0` for the
  massless convention.
* **`pid == 45`** — CLAS12 writes **Geant3** codes for light nuclei (45/46/47/49
  = deuteron/triton/He4/He3), which are not PDG codes at all. They resolve to
  the same masses as their PDG spellings (`1000010020` and friends).

`ox.PDG_MASS_GEV` is the table, and assigning into it takes effect immediately —
nothing caches a derived copy:

```python
ox.PDG_MASS_GEV[9999] = 1.234    # some exotic your cook writes
```

## Joining detector banks by `pindex`

Detector banks don't repeat a particle's momentum — each row carries a `pindex`,
the row number of the particle it belongs to within the same event. Joining on it
is the CLAS12-specific skill, and by hand it answers for one particle at a time:

```python
ak.sum(cal.energy[cal.pindex == 0], axis=1)     # particle 0, and only particle 0
```

`group_by_index` does it for all of them at once, regrouping the bank so there is
one sublist **per particle**, in particle order:

```python
part = f.arrays("REC::Particle", ["pid", "px", "py", "pz"])
cal  = f.arrays("REC::Calorimeter", ["pindex", "layer", "energy"])

by_particle = ox.group_by_index(cal, ak.num(part))   # events * particles * var * {...}
part["cal_energy"] = ak.sum(by_particle.energy, axis=-1)
```

That last line is the payoff: `cal_energy` is now a per-particle column sitting
beside `px`, so cuts and plots treat it like any other. Selections compose
before the reduction:

```python
pcal = ak.sum(by_particle[by_particle.layer == 1].energy, axis=-1)
```

A row whose `pindex` points outside its event's particle range is **dropped**,
not clamped — it names a particle that isn't there, and folding it onto particle
0 would put its energy on a real track. Particles with no detector rows get an
empty sublist, so the result always lines up with the particle array.

### Following the links instead of building them

`ox.link` wires both directions across a whole multi-bank read at once, so the
join becomes something you follow rather than something you write:

```python
ev = ox.link(f.arrays(["REC::Particle", "REC::Calorimeter", "REC::Event"]))

ev["REC::Calorimeter"].particle.px        # the particle each cal row belongs to
ev["REC::Particle"]["REC::Calorimeter"]   # that particle's cal rows, grouped
```

Banks with no `pindex` — an event-level bank in the same read — pass through
untouched. A row whose `pindex` is out of range gets `None` going forward and is
dropped going back; it is never attached to whichever particle happens to be
there.

The two directions do not cost the same, so `directions=` picks:

| `directions=` | builds | cost |
|---|---|---|
| `"to_particle"` | `cal.particle` | **copies** the particle record onto every detector row |
| `"to_detector"` | `part["REC::Calorimeter"]` | regroups, copies nothing |
| `"both"` (default) | both | |

A bank with ten times the particle rows carries ten copies of each momentum
under `"to_particle"`, so on a wide DST it is worth asking for only the side you
use.

## Lorentz vectors

Three momentum columns plus a mass is a four-vector, and
[vector](https://vector.readthedocs.io) already knows the kinematics. `to_vector`
hands the columns over under the names it expects:

```python
p = f.arrays("REC::Particle", ["pid", "px", "py", "pz"])
v = ox.to_vector(p, mass="pdg")

v.E, v.pt, v.eta, v.phi, v.mass
(v[:, 0] + v[:, 1]).mass          # invariant mass of the first two
v[:, 0].deltaR(v[:, 1])
```

`mass=` is what decides the dimensionality, and the default is deliberate:

| `mass=` | you get |
|---|---|
| `"pdg"` | 4-vector, each row's mass from its `pid` (needs the `pid` column) |
| a number | 4-vector, one mass for every row |
| an array | 4-vector, your own per-row masses |
| omitted | **3-vector** — `pt`/`eta`/`phi` and angles, no `E`/`mass` |

Leaving `mass` out gives a 3-vector rather than assuming massless, because a
massless assumption dressed as a four-vector is exactly how a wrong invariant
mass gets published. Unidentified rows (`pid == 0`) carry `nan` through to `E`
for the same reason.

Columns you read but didn't need for the vector — `chi2pid`, `status`, `pid`
itself — come through untouched, so cuts still work on the result. An `E` or
`energy` field already on the record is used directly.

It is a function, not a keyword on `arrays()`, so the same call works on an
`iterate` chunk, a `to_dask` partition, or anything you have already cut.

Needs `pip install oxihipo[vector]`.

## Discovery

```python
f.keys()                       # bank names
f.keys(recursive=True)         # 'bank/column' keys
f.keys(filter_name="REC::*")   # globbed
f.typenames()                  # {'REC::Particle/px': 'float32', 'REC::Track/cov': 'float32[3]'}
f.bank_ids                     # {'REC::Particle': (300, 31)} — the wire (group, item)
"REC::Particle" in f
list(f)                        # iterates bank names
```

`len(f)` is the **event** count, not the number of banks — matching uproot,
where `len(tree)` is `num_entries`. So `len(f)` and `len(list(f))` deliberately
differ.

`bank_ids` gives the on-the-wire `(group, item)` pair for each bank, which is
how the C++ and Java tools address banks. You need it when handing a bank
identifier to something outside this library; reading here is by name.

## Cuts

`cut=` filters the result with a Python expression, evaluated with the bank's
columns bound to jagged `ak.Array` values. **One keyword covers two
granularities**, and which you get is decided by what the expression evaluates
to:

```python
# per-ROW — keeps matching rows; every event survives, some possibly empty
electrons = f.arrays("REC::Particle", ["px", "py", "pz"], cut="pid == 11")

# per-EVENT — keeps whole events, drops the rest
has_e = f.arrays("REC::Particle", ["px"], cut="ak.any(pid == 11, axis=1)")
```

The difference is the shape of the mask: `pid == 11` is jagged (one flag per
row), while `ak.any(..., axis=1)` reduces to one flag per event. Anything else —
a non-boolean result, or the wrong length — raises `ValueError` rather than
silently mis-slicing.

A column named **only** in the cut is read for it and then dropped, so you do not
have to ask for `pid` just to cut on it:

```python
px = f.arrays("REC::Particle", ["px"], cut="pid == 11")
px.fields          # ['px'] — pid was read for the cut, then discarded
```

`ak`, `np` and `abs` are in scope. Array columns (`T#N`) keep their inner width
through a cut. It composes with `entries=`, `entry_start`/`entry_stop`, every
`library=`, and `workers=`.

:::warning A cut is code
The expression is `eval`'d in a namespace with builtins removed. That blocks the
obvious mistakes but it is **not** a sandbox — build cuts from your own source,
never from user input or a file you did not write.
:::

:::note One bank at a time
A cut needs exactly one bank, because the column names have to be unambiguous.
For a multi-bank read, cut each bank separately or mask the result yourself.
:::

## Replaying a list of events

When an earlier pass has already told you which events are interesting,
`entries=` reads exactly those:

```python
interesting = [12, 40, 41, 42, 900]           # e.g. from a previous scan
p = f.arrays("REC::Particle", ["px", "py"], entries=interesting)
```

The result is aligned 1:1 with the list, so your order is preserved and
duplicates are honoured; an out-of-range index gives an empty entry rather than
an error. Mutually exclusive with `entry_start`/`entry_stop`.

:::note Order no longer matters for speed
Entries are grouped by the record that holds them before anything is read, so
each record is decompressed once however the list is arranged, and the groups run
in parallel (`threads=`). Sorting used to matter a great deal — a shuffled list
re-decoded a whole record *per index*, 7 ms against 13 µs for the same 256
ascending — which is exactly the case a list of interesting events found by an
earlier pass falls into.

`entries=` is not supported on a filtered chain: the indices address the file's
event stream and would ignore the filter.
:::

## Handing off to other tools

### Parquet

`to_parquet()` writes the selection straight out — the handover format for
polars / duckdb / pandas. Each bank column becomes a `large_list`, one list per
event, so the jagged structure survives:

```python
f.to_parquet("electrons.parquet", "REC::Particle", ["px", "py", "pz"],
             cut="pid == 11")
```

`step_size=` streams instead of materialising, writing one row group per chunk,
so inputs far bigger than RAM work in about one chunk of memory. `compression=`
is the Parquet codec (`"zstd"` by default).

:::note The schema is non-nullable
A HIPO bank has no null concept — a row either exists, or the event simply has
fewer rows, which the list offsets already express. The Arrow schema says so:
fields are declared `not null`, so `ak.from_arrow(pq.read_table(...))` gives back
`var * {px: float32}`, the same type you started with.

Before 0.3.1 the schema was inferred rather than declared, which left every field
nullable and made the round-trip come back as `option[var * ?float32]`. The
values were always identical; only the declared type was wrong.
:::

### Dask

`to_dask()` returns a lazy [dask-awkward](https://dask-awkward.readthedocs.io)
array — the counterpart to `uproot.dask`. One partition per `step_size` batch,
aligned to records exactly as `iterate` is; nothing is read until `.compute()`,
not even to discover the layout:

```python
import dask_awkward as dak
p = f.to_dask("REC::Particle")
dak.sum(p.px).compute()          # reads px, not the whole bank
```

**Columns are projected.** dask-awkward works out which columns the graph
actually touches, and each partition reads only those — within one bank and
across several, so a two-bank array reduced over one column of one bank reads
exactly that. `dak.report_necessary_columns()` shows what it settled on:

```python
dak.report_necessary_columns(dak.sum(p.px))
# {'from-hipo-…': frozenset({'px'})}
```

**Entry boundaries are known**, so `len(p)`, `p.partitions[i]` and entry slices
work without reading anything. The exception is `cut=`: a per-event cut drops
events, so the batch boundaries stop being entry boundaries and `to_dask` leaves
divisions unknown rather than report ones that later prove wrong.

Needs `pip install oxihipo[dask]` — dask-awkward is deliberately **not** a
default dependency.

:::tip Reach for this last
For a scan on one machine, `iterate()` or `workers=` is simpler, has no extra
dependency, and is usually faster. `to_dask()` earns its keep when you already
have a cluster, or want HIPO to be one node in a bigger dask graph. Note
`threads` defaults to `1` here: dask already runs partitions concurrently, so
letting each also fan out over rayon oversubscribes the machine.
:::

## File and container metadata

```python
f.record_count            # records in the chain, from the record index
f.file_header             # FileHeader(version=6, endianness='little', ...)
f.config                  # user key/value store written into the dictionary
```

`record_count` is worth knowing before you write: a reader parallelises over
**records**, so it bounds how many cores a scan of this file can use. If it is
below your core count, the file was written with too large a
[`max_record_bytes`](../performance/compression.md#record-size-and-parallel-scaling).

:::warning Some header fields are not set by the reference writers
Measured on real files, so do not branch on these:

* `file_header.record_count` is **0** from every writer checked, including this
  one. Use `f.record_count`.
* `has_dictionary` / `has_trailer_index` are `False` on C++ and Java written
  files even though a dictionary is present — as it is on every CLAS12 file.

`version`, `endianness` and `file_number` are dependable. And the run number is
in the `RUN::config` bank, not the header.
:::


## Composite banks

A few CLAS12 structures (`RUN::scaler` and friends) carry an **inline format
string** in place of a schema: the dictionary names them, but the layout lives in
the structure itself. Read those with `composite()`:

```python
ev = f.composite("RUN::scaler")
ev.f0                                   # first field, one sublist per event
f.composite("RUN::scaler", library="np")  # {'f0': ndarray(object), ...}
```

Composite fields are **positional** — the format string carries types, not
names — so they come back as `f0`, `f1`, … in format order. `library` is `"ak"`
(default) or `"np"`; `entry_start` / `entry_stop` narrow the range as elsewhere.

:::warning `keys()` cannot tell you which banks are composite
Composites are resolved *through* the dictionary, exactly like schema'd banks, so
they appear in `keys()` alongside everything else — being listed says nothing
about which reader to use. What makes a bank composite is the shape of its
structure on the wire, not its dictionary entry.

There is no discovery call yet. Until there is, `composite()` at least tells you
which case you are in: it reports a bank that exists but is schema'd separately
from one that is not in the file at all.
:::

:::tip
There is a module-level shorthand for one-off reads:
`ox.composite("run5042.hipo", "RUN::scaler")`.
:::

:::warning Composite banks and the split codecs
`Lz4PerBank` and `Lz4PerColumn` store bank payloads separately and discard the
structure headers that mark a bank as composite. Before **0.7.0** they had
nowhere to record that, so `composite()` returned `None` on any file written
with either codec.

0.7.0 fixes this for newly written files. It cannot fix files already written:
if you converted with 0.6.0 or earlier, the marker was never on disk and
`composite()` still returns `None`. Re-convert from the original.

The four stock codecs (`none`, `lz4`, `lz4best`, `gzip`) were never affected —
see [Format versions](../performance/compression.md#format-versions-and-cross-version-compatibility).
:::

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

To *author* files — write new banks columnarly, or decorate an existing file
with a derived bank — see [Writing](./writing.md).

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

All of the above — reading tags, filtering by name, tag-and-skim, and the
in-place patch — run end to end in
[`py/examples/event_tags.py`](https://github.com/mathieuouillon/oxihipo/tree/main/py/examples/event_tags.py).

## Scaler banks (and record tags)

oxihipo reads **every** record in a file, so scaler banks (`RUN::scaler`, …) are
available like any other bank — no special flag. (The C++ `hipo4` reader and
hipopy need `reader.setTags(1)` to see them, because scalers live in tag-1
records; oxihipo indexes all records instead.) They show up in `keys()`, and you
read them with `arrays("RUN::scaler")`. If you want a *subset* by tag, that's the
pushdown filter — `filtered(record_tag=[…])` skips whole records by their tag,
`filtered(event_tag=…)` drops individual events (both without inflating a bank).

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
