"""Feed ROOT's RDataFrame from a HIPO file — the Awkward bridge, whole-file and
streamed.

    python rdataframe.py [FILE.hipo | DIR | GLOB]

oxihipo reads the columns (GIL released) and hands them to RDataFrame through
Awkward's generated ``RDataSource``: a jagged bank column becomes an ``RVec<T>``,
a ``T#N`` array column a nested ``RVec``, with no copy of the view. Column names
are the ``bank/column`` keys sanitized to C++ identifiers, so
``REC::Particle/px`` → ``REC_Particle_px``.

Needs ROOT (PyROOT) and awkward — ``pip install oxihipo[root]`` plus an
importable ROOT (e.g. ``conda install -c conda-forge root``).
"""

import os
import sys

try:
    import ROOT  # noqa: F401  (imported for its side effect: PyROOT must load)
except ImportError:
    print("skipping: this example needs ROOT — `pip install oxihipo[root]` + a working PyROOT")
    raise SystemExit(0)

import oxihipo as ox

SAMPLE = os.path.join(os.path.dirname(__file__), "..", "tests", "data", "sample.hipo")
source = sys.argv[1] if len(sys.argv) > 1 else SAMPLE

# --- whole file → one RDataFrame -------------------------------------------
# Fits-in-RAM path: the selection is read into oxihipo's columnar buffers, then
# RDF runs its (implicitly multi-threaded) loop over that in-memory view.
# (The synthetic sample carries pid/px; real REC::Particle also has py/pz/…)
df = ox.rdataframe(source, "REC::Particle", ["px", "pid"])
print("RDF columns:", [str(c) for c in df.GetColumnNames()])
print("entries:    ", df.Count().GetValue())

# A per-event define over the RVec columns, then a histogram — ordinary RDF.
df = df.Define("mult", "(int) REC_Particle_pid.size()") \
       .Define("sum_px", "Sum(REC_Particle_px)")
h = df.Histo1D(("mult", "particles / event;n;events", 6, 0, 6), "mult").GetValue()
print(f"mean multiplicity: {h.GetMean():.3f}  (entries {int(h.GetEntries())})")

# --- bigger-than-RAM → stream one RDataFrame per chunk ---------------------
# Each chunk is an independent RDF, so book a result per chunk and merge across
# them (histograms with Add). Resident memory stays ~ one chunk.
total = None
for chunk_df in ox.iterate_rdataframe(source, "REC::Particle", ["px"], step_size="256 MB"):
    hc = chunk_df.Histo1D(("px", "px;px;particles", 50, -5, 5), "REC_Particle_px").GetValue()
    if total is None:
        total = hc.Clone()
        total.SetDirectory(0)  # detach so it outlives this chunk's RDF
    else:
        total.Add(hc)

if total is not None:
    print(f"streamed px histogram: {int(total.GetEntries())} particles, "
          f"mean {total.GetMean():.3f}")
