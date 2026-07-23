---
id: scaling-up
title: Scaling up
sidebar_position: 8
---

# From a notebook to a batch job

A run period is thousands of files and hundreds of GB — you can't `arrays()` it
into memory. The same code scales with four tools: **streaming**, **multiple
processes**, **skims**, and **persisting** derived quantities. None of them change
the physics you wrote; they change how it's fed.

## Stream instead of loading

`iterate` yields the chain in bounded-memory chunks — materialize a chunk, fold it
into your histograms, drop it, repeat. Memory stays at ≈ one chunk no matter how
big the input:

```python
import numpy as np, awkward as ak, oxihipo as ox

BEAM, M_P = 10.604, 0.938272
q2_hist = np.zeros(100)
edges = np.linspace(0, 10, 101)

for chunk in ox.iterate("/data/rga/*.hipo", "REC::Particle",
                        ["pid", "px", "py", "pz", "status"], step_size="500 MB"):
    e  = chunk[:, 0]                                  # trigger electron
    Ee = np.sqrt(e.px**2 + e.py**2 + e.pz**2)
    Q2 = 4 * BEAM * Ee * np.sin(np.arccos(e.pz / Ee) / 2)**2
    q2_hist += np.histogram(ak.to_numpy(Q2), bins=edges)[0]
```

`step_size` is an event count or a byte budget (`"500 MB"`, `"2 GB"`); chunks are
aligned to record and file boundaries. Pass `report=True` to get a `(chunk,
Report)` pair with the event range and source file for logging.

## Use more processes

On a parallel filesystem (JLab ifarm `/volatile`, Lustre) one process reads well
below the disk's aggregate bandwidth. `workers=N` splits the chain into `N`
record-aligned ranges read by `N` processes and stitches the result:

```python
if __name__ == "__main__":                            # REQUIRED for workers=
    a = ox.arrays("/volatile/rga/*.hipo", "REC::Particle",
                  ["px", "py", "pz", "pid"], workers=8)
```

`iterate(..., workers=8)` streams across processes the same way. The `if __name__`
guard is mandatory — workers are *spawned* (they re-import your script), so without
it they'd re-run it. This helps only when I/O is the bottleneck; on a local cached
disk it just adds overhead, so keep the default there.

## Skim: write the events you want, once

Reading a hundred files to keep 2% of the events is wasteful to repeat. A **skim**
does it once — filter, then write a small file you re-analyse instantly. `filtered`
applies pushdown cuts (events are dropped before their banks are decoded); `skim`
writes the survivors:

```python
# keep events that have a Forward-Detector electron, re-compress tightly
src = ox.open("/data/rga/*.hipo")
summary = src.filtered(require=["REC::Particle"]).skim("dis_skim.hipo",
                                                       compression="lz4percolumn")
summary.events, summary.bytes                          # what was written
```

`filtered` composes: `require=` (event must carry these banks), `record_tag=`, and
event-tag cuts. For *physics* selections (a missing-mass window, a $Q^2$ cut) you
compute a per-event boolean and pair it with **event tags**.

## Label events with tags

An event tag is a 32-bit label oxihipo reads and filters on without inflating a
bank. Compute one tag per event, write a tagged skim, and later select by name —
turning your physics selection into a reusable, self-describing dataset:

```python
p = src.arrays("REC::Particle", ["pid", "px", "py", "pz", "status"])
# ... compute your DIS + exclusivity booleans over the chunk/chain ...
tag = np.where(is_dvcs, 1 << 0, 0).astype(np.uint32)   # bit 0 = "dvcs"

src.skim("dvcs.hipo", tags=tag, tag_names={"dvcs": 0})

# months later, no need to remember the cuts:
ox.open("dvcs.hipo").filtered(event_tag="dvcs").arrays("REC::Particle")
```

The `tags` array must line up 1:1 with the events the chain yields. See the
[Reading guide](../python/reading.md#tag-and-skim) for the full tag workflow
(including retagging in place).

## Write a derived bank {#write-a-derived-bank}

Once your corrections and kinematics are settled, don't recompute them every run —
**decorate** the file with a new bank holding them. `recreate` copies every event
verbatim and attaches the banks you add:

```python
f = ox.open("dis_skim.hipo")
kin = dis_kinematics(select_electron(f.arrays("REC::Particle")))   # your functions

w = ox.recreate("dis_skim.hipo", "dis_skim_kin.hipo")
w.new_bank("ANA::dis", {"Q2": "F", "W": "F", "xB": "F"})
w.extend({"ANA::dis": {"Q2": kin.Q2, "W": kin.W, "xB": kin.xB}})    # one row / event
w.close()
```

Now `ox.open("dis_skim_kin.hipo").arrays("ANA::dis")` reads $Q^2$/$W$/$x_B$
directly — no beam energy, no recomputation. This is exactly how CLAS12 "trains"
add derived quantities to cooked files. Full guide: [Writing](../python/writing.md).

## Structure a real analysis

Putting the pieces together, a production job looks like:

```python
def analyze(chunk):
    """Pure function: a chunk of REC::Particle in, filled histograms out."""
    ele = select_electron(chunk)                 # ch. 3 — trigger-electron ID
    kin = dis_kinematics(ele)                    # ch. 4 — Q², W, xB
    dis = (kin.Q2 > 1) & (kin.W > 2) & (kin.y < 0.85)
    ...                                          # ch. 5–6 — detector cuts, channels
    return histograms

if __name__ == "__main__":
    hists = accumulate(analyze(c) for c in
                       ox.iterate("/data/rga/*.hipo", step_size="1 GB", workers=8))
    save(hists)
```

Same expressions you developed interactively, now folded over the whole dataset in
bounded memory across many cores.

## Going further

- **[Reading](../python/reading.md)** / **[Writing](../python/writing.md)** /
  **[Streaming](../python/streaming.md)** / **[Parallel](../python/parallel.md)** —
  the complete reference for every knob touched here.
- **[RDataFrame](../python/rdataframe.md)** — if your group works in ROOT,
  `ox.rdataframe(...)` hands a selection to ROOT's declarative dataframe.
- **Runnable examples** — [`py/examples/`](https://github.com/mathieuouillon/oxihipo/tree/main/py/examples)
  has `analysis.py`, `streaming.py`, `parallel.py`, `writing.py`, `decorate.py`,
  `event_tags.py`, and the generator that built this tutorial's data,
  `tutorial_sample.py`.
- **The physics** — for the real cuts, corrections, and fiducial maps, the CLAS12
  collaboration's analysis notes and software (`clas12-analysis`, `coatjava`) are
  the authority; this tutorial teaches the *tools*, not the official selections.

You now have the whole arc: open a DST, identify particles, compute kinematics,
join detector banks, reconstruct channels, and scale it out. Go run it on real
data.
