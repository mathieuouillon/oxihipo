---
id: writing
title: Writing
sidebar_position: 5
---

# Writing

`oxihipo.create` opens a new file; `oxihipo.recreate` *decorates* an existing
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
  of `B`/`S`/`I`/`L`/`F`/`D` (byte, short, int, long, float, double). The unique
  bank `item` auto-assigns (pass `item=`/`group=` to override).
- **`extend({bank: data})`** appends a batch of events. `data` is an `ak.Array`
  record (exactly what `arrays(bank)` returns) or a dict of columns — a jagged
  `ak.Array` per column, or a 1-D NumPy array for a **scalar-per-event** bank.
  Call `extend` in a loop to stream large outputs in bounded memory; every bank
  in one call must span the same number of events.
- **`close()`** (or leaving the `with`) writes the trailer index and returns a
  `SkimSummary` (`events` / `records` / `bytes`).

A round-trip through `arrays` is exact:

```python
p = ox.open("in.hipo").arrays("REC::Particle")     # ak record array
with ox.create("copy.hipo") as w:
    w.new_bank("REC::Particle", {"px": "F", "py": "F", "pz": "F", "pid": "I"})
    w.extend({"REC::Particle": p})
```

:::note Scalar columns only (for now)
The writer handles scalar columns; fixed-length array columns (`T#N`) aren't
supported yet. Decorating (below) still copies a file's *existing* array columns
through verbatim.
:::

## Decorating an existing file (add a bank)

The workflow physicists actually want: cook once, then add derived per-event
banks later — an ML score, a computed kinematic — **without rewriting the
physics banks**. `recreate` copies every source event verbatim and attaches the
new banks you declare:

```python
f = ox.open("dst.hipo")
scores = my_model.predict(f.arrays("REC::Particle"))   # one float32 per event

w = ox.recreate("dst.hipo", "decorated.hipo")   # or dst=None to replace in place
w.new_bank("ML::pred", {"score": "F"})
w.extend({"ML::pred": {"score": scores.astype("float32")}})
w.close()

ox.open("decorated.hipo").keys()   # the existing banks + ML::pred
```

The new banks must align 1:1 with the source events (extend all of them, in
order) — `close` errors if you cover fewer. Existing banks, **including array
columns**, are copied through unchanged. With `dst=None` the source is replaced
in place via a temporary file.
