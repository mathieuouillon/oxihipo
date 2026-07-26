---
id: writing
title: Writing
sidebar_position: 5
---

# Writing

:::warning `create` / `recreate` / `update` follow uproot
These names mean what they mean in
[uproot](https://uproot.readthedocs.io/en/latest/basic.html), which is **not**
what `recreate` meant here before 0.4:

| you want | call |
|---|---|
| a new file, error if it exists | `create(path)` |
| a new file, replacing any existing one | `recreate(path)` |
| add banks to an existing file | `update(source, dst=None)` |

`recreate(source, dst)` still works with a `DeprecationWarning` and behaves as
`update`. `recreate(path)` on a file that already exists **raises** for one
release — the old meaning decorated that file and the new one destroys it, so
guessing is not safe. Pass `overwrite=True` once you mean the new behaviour.
:::


`oxihipo.create` opens a new file; `oxihipo.update` *decorates* an existing
one (copies its events, attaching new banks). Both return a `Writer` with a
`new_bank` / `extend` / `close` API that writes columns **zero-copy from NumPy
or Awkward**.

## A new file

```python
import oxihipo as ox
import awkward as ak

with ox.create("out.hipo", compression="lz4percolumn") as w:
    w.new_bank("NEW::bank", {"px": "F", "py": "F", "pid": "I"})   # declare a bank
    w.extend({"NEW::bank": {                                     # append a batch
        "px":  ak.Array([[1.0, 2.0], [], [3.0]]),               # jagged: rows per event
        "py":  ak.Array([[0.1, 0.2], [], [0.3]]),
        "pid": ak.Array([[11, -11], [], [211]]),
    }})
```

- **`new_bank(bank, {col: typechar})`** declares a bank; each `typechar` is one
  of `B`/`S`/`I`/`L`/`F`/`D` (byte, short, int, long, float, double), optionally
  with `#N` for a fixed-length **array** column (e.g. `"F#3"`). The unique bank
  `item` auto-assigns (pass `item=`/`group=` to override).
- **`extend({bank: data})`** appends a batch of events. `data` is an `ak.Array`
  record (exactly what `arrays(bank)` returns) or a dict of columns — a jagged
  `ak.Array` per column, or a 1-D NumPy array for a **scalar-per-event** bank.
  (Array columns take a jagged `N * var * K` `ak.Array`, or an `(n_events, K)`
  NumPy array for one array-row per event.) Call `extend` in a loop to stream
  large outputs in bounded memory; every bank in one call must span the same
  number of events.
- **`close()`** (or leaving the `with`) writes the trailer index and returns a
  `SkimSummary` (`events` / `records` / `bytes`).

A round-trip through `arrays` is exact — array columns (`REC::Particle`'s
`cov/F#3`) and all:

```python
p = ox.open("in.hipo").arrays("REC::Particle")     # ak record array
with ox.create("copy.hipo") as w:
    w.new_bank("REC::Particle", {"px": "F", "py": "F", "pz": "F", "pid": "I", "cov": "F#3"})
    w.extend({"REC::Particle": p})
```

## Decorating an existing file (add a bank)

The workflow physicists actually want: cook once, then add derived per-event
banks later — an ML score, a computed kinematic — **without rewriting the
physics banks**. `update` copies every source event verbatim and attaches the
new banks you declare:

```python
f = ox.open("dst.hipo")
scores = my_model.predict(f.arrays("REC::Particle"))   # one float32 per event

w = ox.update("dst.hipo", "decorated.hipo")   # or dst=None to replace in place
w.new_bank("ML::pred", {"score": "F"})
w.extend({"ML::pred": {"score": scores.astype("float32")}})
w.close()

ox.open("decorated.hipo").keys()   # the existing banks + ML::pred
```

The new banks must align 1:1 with the source events (extend all of them, in
order) — `close` errors if you cover fewer. Existing banks, **including array
columns**, are copied through unchanged. With `dst=None` the source is replaced
in place via a temporary file.

## Runnable examples

- [`py/examples/writing.py`](https://github.com/mathieuouillon/oxihipo/tree/main/py/examples/writing.py)
  — a new file end to end: jagged, `T#N` array, and scalar-per-event columns,
  plus a streaming `extend` loop for bounded-memory output.
- [`py/examples/decorate.py`](https://github.com/mathieuouillon/oxihipo/tree/main/py/examples/decorate.py)
  — compute per-event quantities and attach them to a cooked file, verifying the
  original banks survive untouched.
