"""Write HIPO files columnarly — declare banks, feed batches, close.

    python writing.py [OUT.hipo]

`create` opens a new file and returns a `Writer` with an uproot-shaped
`new_bank` / `extend` / `close` API. Columns go straight from NumPy or Awkward
into the file (zero-copy, GIL released) — you never build per-event Python
objects. Covers all three column shapes: jagged, fixed-length array (`T#N`), and
scalar-per-event. Writes to a temp file unless you pass a path.
"""

import os
import sys
import tempfile

import awkward as ak
import numpy as np

import oxihipo as ox

out = sys.argv[1] if len(sys.argv) > 1 else os.path.join(tempfile.mkdtemp(), "written.hipo")

w = ox.create(out, compression="lz4percolumn")

# --- declare the schema ----------------------------------------------------
# typechar: B/S/I/L/F/D (byte, short, int, long, float, double); `#N` makes it a
# fixed-length array column. `item` (the unique bank id) auto-assigns.
w.new_bank("REC::Particle", {"pid": "I", "px": "F", "cov": "F#3"})
w.new_bank("RUN::config", {"run": "I", "energy": "F"})

# --- one batch, written straight from Awkward / NumPy ----------------------
# Jagged columns carry one sublist per event; the `cov` array column adds a
# fixed inner axis (3 floats per particle). A scalar-per-event bank takes a
# plain 1-D array. Every bank in one `extend` must span the same events.
w.extend({
    "REC::Particle": {
        "pid": ak.Array([[11, -11], [], [211, 2212, 22]]),
        "px": ak.Array([[0.5, -0.4], [], [1.2, 0.3, 0.9]]),
        "cov": ak.Array([[[1, 0, 0], [0, 1, 0]], [], [[1, 0, 0], [0, 1, 0], [0, 0, 1]]]),
    },
    "RUN::config": {
        "run": np.array([5042, 5042, 5042], dtype=np.int32),
        "energy": np.array([10.6, 10.6, 10.6], dtype=np.float32),
    },
})

# --- stream the rest: call extend in a loop, memory stays bounded ----------
# This is how you write an output far larger than RAM — build one batch, hand
# it over, let it go. Here two batches of 4 events with random multiplicities.
rng = np.random.default_rng(0)
for _ in range(2):
    counts = rng.integers(0, 4, size=4)               # particles per event
    total = int(counts.sum())
    w.extend({
        "REC::Particle": {
            "pid": ak.unflatten(rng.choice([11, -11, 211, 2212], total).astype(np.int32), counts),
            "px": ak.unflatten(rng.normal(0, 1, total).astype(np.float32), counts),
            "cov": ak.unflatten(rng.normal(0, 1, (total, 3)).astype(np.float32), counts),
        },
        "RUN::config": {
            "run": np.full(4, 5042, dtype=np.int32),
            "energy": np.full(4, 10.6, dtype=np.float32),
        },
    })

summary = w.close()   # writes the trailer index
print(f"wrote {summary.events} events in {summary.records} record(s), "
      f"{summary.bytes / 1024:.1f} KiB -> {out}\n")

# --- read it back ----------------------------------------------------------
f = ox.open(out)
f.show()                                    # every bank and its column dtypes

p = f.arrays("REC::Particle")
print("\ntype:            ", p.type)        # N * var * {pid, px, cov: 3 * float32}
print("event 0 pids:    ", p[0].pid.tolist())
print("event 0 cov[0]:  ", p[0].cov[0].tolist())   # the fixed-length array cell
print("particles/event: ", ak.to_list(ak.num(p.pid)))
print("run (per event): ", ak.to_list(ak.flatten(f.array("RUN::config", "run"))))
