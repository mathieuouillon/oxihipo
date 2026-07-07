"""A tiny columnar analysis with Awkward — the shape of a real CLAS12 study
(particle selection + per-event kinematics), here on the synthetic sample.

    python analysis.py [FILE.hipo | DIR | GLOB]

Every operation is vectorized across *all* events at once — there is no Python
per-event loop, and nothing is copied past decompression. On real data you'd
swap the toy cuts below for e.g. ``p.pid == 11`` (electrons) and build invariant
masses from ``px, py, pz``.
"""

import os
import sys

import awkward as ak

import oxihipo as ox

SAMPLE = os.path.join(os.path.dirname(__file__), "..", "tests", "data", "sample.hipo")
f = ox.open(sys.argv[1] if len(sys.argv) > 1 else SAMPLE)

# Read the whole particle bank as a jagged record (one sublist per event).
p = f.arrays("REC::Particle", ["pid", "px", "cov"])
print(f"{len(p)} events, {ak.count(p.pid)} particles total\n")

# --- event-level cuts (boolean masks over the jagged structure) ------------
n = ak.num(p.pid)  # particles per event
busy = p[n >= 2]  # keep only events with >= 2 particles
print(f"events with >= 2 particles: {len(busy)} / {len(p)}")

# --- per-event reductions --------------------------------------------------
sum_px = ak.sum(p.px, axis=1)  # scalar per event
leading_pid = ak.firsts(p.pid)  # first particle's pid, or None if empty
print("sum(px) per event:  ", [round(x, 2) for x in ak.to_list(sum_px)])
print("leading pid:        ", ak.to_list(leading_pid))

# --- particle-level cut, then regroup by event -----------------------------
positive_px = p[p.px > 0.0]  # drop particles with px <= 0, keep event grouping
print("particles with px>0:", ak.to_list(ak.num(positive_px.px)))

# --- an array (fixed-length) column: cov is float32[3] ---------------------
print("\ncov of (event 3, particle 0):", ak.to_list(p.cov[3, 0]))
