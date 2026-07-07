"""oxihipo quick start — open a HIPO file, inspect it, read whole columns.

    python quickstart.py [FILE.hipo | DIR | GLOB]

With no argument it reads the bundled synthetic sample. Needs ``awkward``.
"""

import os
import sys

import awkward as ak
import numpy as np

import oxihipo as ox

SAMPLE = os.path.join(os.path.dirname(__file__), "..", "tests", "data", "sample.hipo")
source = sys.argv[1] if len(sys.argv) > 1 else SAMPLE

f = ox.open(source)
print(f"{f.num_entries} events across {f.file_count} file(s)")
print("banks:              ", f.keys())
print("REC::Particle cols: ", f["REC::Particle"].keys())
print("types:              ", dict(f.typenames()))

# One Rust pass materializes these columns (GIL released) into an Awkward array
# shaped exactly like a uproot jagged branch: one sublist of particles per event.
p = f.arrays("REC::Particle", ["pid", "px"])
print("\narray type:", p.type)  # e.g. 8 * var * {pid: int32, px: float32}
print("event 2 pids:", p[2].pid.tolist())

# Columnar summaries — no Python event loop.
mult = ak.num(p.pid)  # particles per event
print("\nmean multiplicity:", float(ak.mean(mult)))
px = ak.to_numpy(ak.flatten(p.px))
counts, _ = np.histogram(px, bins=8)
print("px histogram:", counts.tolist())

# The NumPy-only path needs no Awkward at all:
values, offsets, inner_len = f.numpy("REC::Particle", "px")
print(f"\nnumpy(): {values.dtype} values, {offsets.dtype} offsets, inner_len={inner_len}")
