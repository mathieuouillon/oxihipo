"""Decorate a cooked file — attach a derived bank without rewriting the physics.

    python decorate.py [IN.hipo] [OUT.hipo]

The workflow analyzers actually want: cook once, then add per-event quantities
later (an ML score, a computed kinematic) *without* touching the existing banks.
`recreate` copies every source event through verbatim — existing banks, array
columns and all — and attaches the new banks you declare. The new banks must
line up 1:1 with the source events, so `close()` errors rather than silently
truncating. Pass ``dst=None`` to replace the source in place (via a temp file).
"""

import os
import sys
import tempfile

import awkward as ak
import numpy as np

import oxihipo as ox

SAMPLE = os.path.join(os.path.dirname(__file__), "..", "tests", "data", "sample.hipo")
src = sys.argv[1] if len(sys.argv) > 1 else SAMPLE
dst = sys.argv[2] if len(sys.argv) > 2 else os.path.join(tempfile.mkdtemp(), "decorated.hipo")

# --- 1. compute the derived quantities, vectorized over all events ---------
f = ox.open(src)
p = f.arrays("REC::Particle", ["pid", "px"])

mult = ak.to_numpy(ak.num(p.pid)).astype(np.int32)          # particles / event
sum_px = ak.to_numpy(ak.sum(p.px, axis=1)).astype(np.float32)  # scalar / event
# stand-in for `model.predict(...)` — one score per event
score = (1.0 / (1.0 + np.exp(-sum_px))).astype(np.float32)

print(f"{f.num_entries} source events; banks: {f.keys()}")
print(f"  multiplicity : {mult.tolist()}")
print(f"  sum(px)      : {[round(float(x), 2) for x in sum_px]}")

# --- 2. copy the file through, attaching the new bank ---------------------
w = ox.recreate(src, dst)          # or ox.recreate(src) to replace in place
w.new_bank("KIN::event", {"mult": "I", "sum_px": "F", "score": "F"})
w.extend({"KIN::event": {"mult": mult, "sum_px": sum_px, "score": score}})
summary = w.close()
print(f"\nwrote {summary.events} events -> {dst}")

# --- 3. read it back: originals untouched, new bank present ---------------
g = ox.open(dst)
print("banks after decorating:", g.keys())

orig_evno = ak.to_list(f.array("REC::Event", "evno"))
new_evno = ak.to_list(g.array("REC::Event", "evno"))
print(f"\nREC::Event/evno preserved: {orig_evno == new_evno}")

orig_cov = ak.to_list(f.array("REC::Particle", "cov"))
new_cov = ak.to_list(g.array("REC::Particle", "cov"))
print(f"REC::Particle/cov (F#3) preserved: {orig_cov == new_cov}")

got = ak.to_numpy(ak.flatten(g.array("KIN::event", "score")))
print(f"KIN::event/score round-trips: {np.allclose(got, score)}")
print(f"  scores: {[round(float(x), 3) for x in got]}")
