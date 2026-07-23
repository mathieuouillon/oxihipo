---
id: first-look
title: First look
sidebar_position: 3
---

# First look: open, explore, read

First, generate the sample (from the repo root):

```bash
python py/examples/tutorial_sample.py clas12_tutorial.hipo 20000
```

## Open a file and see what's in it

```python
import oxihipo as ox
import awkward as ak
import numpy as np

f = ox.open("clas12_tutorial.hipo")
f.num_entries          # 20000  — number of events
f.keys()               # ['RUN::config', 'REC::Particle', 'REC::Calorimeter', 'REC::Cherenkov']
```

`ox.open` takes a file, a directory, a glob, or a list of paths — so
`ox.open("/data/rga/*.hipo")` opens a whole run as one chain. `num_entries` is
the total event count (the same thing uproot calls it).

To see a bank's columns and their types, `show()` prints the dictionary:

```python
f.show("REC::Particle")
```
```
REC::Particle  (12 columns)
    pid                      int32
    px                       float32
    py                       float32
    pz                       float32
    vx                       float32
    vy                       float32
    vz                       float32
    vt                       float32
    charge                   int8
    beta                     float32
    chi2pid                  float32
    status                   int16
```

`f.show()` (no argument) dumps every bank. Programmatically, `f.typenames()`
returns the same as a `{"bank/column": "dtype"}` dict, and `f.keys(recursive=True)`
lists the `bank/column` keys.

## Read columns into arrays

`arrays(bank, columns)` reads columns into one Awkward array:

```python
p = f.arrays("REC::Particle", ["pid", "px", "py", "pz", "charge"])
p.type
# 20000 * var * {pid: int32, px: float32, py: float32, pz: float32, charge: int8}
```

Read that type left to right: **20000** events, each a **var**-iable-length list,
each element a **record** with fields `pid, px, …`. That's the jagged event model
from the [primer](./clas12-and-hipo.md#the-event-model) made concrete.

```python
p[0]                   # event 0: a list with 1 particle (a lone electron)
# [{pid: 11, px: 0.439, py: -0.530, pz: 1.856, charge: -1}]

ak.to_list(p.pid[:3])  # the pid lists of the first three events
# [[11], [11, 211, 22, 22], [11, 211, 2212]]
```

Event 1 has an electron, a $\pi^+$, and two photons; event 2 an electron, a
$\pi^+$, and a proton. Omit the column list to read the whole bank
(`f.arrays("REC::Particle")`), and pass several banks
(`f.arrays(["REC::Particle", "REC::Calorimeter"])`) to get a record with one
field per bank.

## Thinking in arrays, not loops

The golden rule: **never write a `for` loop over events.** Everything is an array
expression evaluated across all 20 000 events at once.

```python
mult = ak.num(p.pid)          # particles per event: [1, 4, 3, 4, 2, ...]
ak.mean(mult)                 # ~2.9 particles/event, averaged over the file
p.pid[:10]                    # jagged slice — still one list per event

# per-event reductions collapse the inner axis (axis=1):
total_px = ak.sum(p.px, axis=1)     # one number per event
leading  = ak.firsts(p.pid)         # first particle's pid per event (None if empty)

# masks select — an event-level mask keeps events…
busy = p[ak.num(p.pid) >= 3]        # …with ≥ 3 particles
# …a particle-level mask keeps particles, preserving the event grouping:
positives = p[p.charge > 0]         # only positive tracks, still grouped by event
```

`ak.flatten(p.pid)` drops the event structure entirely, giving one flat array of
every particle's pid — handy for a quick histogram of "all particles in the file":

```python
allpid = ak.to_numpy(ak.flatten(p.pid))
np.unique(allpid, return_counts=True)
# (array([  11,   22,  211, 2212]), array([20000, 17870, 12946, 8053]))
```

Every event has exactly one electron (this sample is built that way); there are
also photons, $\pi^+$, and protons. Selecting the ones you want is the next page.

## The raw path (no Awkward)

If you only want the numbers and would rather not import Awkward, `numpy()` hands
back the flat buffers plus the offsets that slice them per event:

```python
col = f.numpy("REC::Particle", "px")
col.values            # float32, every particle's px, flat
col.offsets           # int64, length n_events+1; event i is values[offsets[i]:offsets[i+1]]
col.inner_len         # 1 (would be >1 for a fixed-length array column)
```

This is the same data the Awkward path wraps — zero-copy, no Python loop. Most of
this tutorial uses Awkward because the jagged operations are so much cleaner, but
the raw buffers are always there.

Next: turning `REC::Particle` rows into physics.

[Particles & selection →](./particles-and-kinematics.md)
