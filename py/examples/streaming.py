"""Bounded-memory streaming — fold a histogram over a chain far larger than RAM.

    python streaming.py [FILE.hipo | DIR | GLOB ...]

Each chunk materializes, updates the running histogram, and is dropped before
the next is read, so resident memory stays ~= one chunk no matter how big the
input is. ``step_size`` caps a chunk by event count (int) or bytes ("200 MB").
"""

import os
import sys

import awkward as ak
import numpy as np

import oxihipo as ox

SAMPLE = os.path.join(os.path.dirname(__file__), "..", "tests", "data", "sample.hipo")
source = sys.argv[1] if len(sys.argv) > 1 else SAMPLE

bins = np.linspace(-1.0, 10.0, 23)
counts = np.zeros(len(bins) - 1, dtype=np.int64)
n_events = 0

# `report=True` yields (chunk, Report) so you can log progress / provenance.
for chunk, report in ox.iterate(source, "REC::Particle", ["px"], step_size="8 MB", report=True):
    px = ak.to_numpy(ak.flatten(chunk.px))
    counts += np.histogram(px, bins=bins)[0]
    n_events += len(chunk)
    print(
        f"  events [{report.entry_start:>8}:{report.entry_stop:<8}) "
        f"from {os.path.basename(report.file_path)}  (+{len(px)} particles)"
    )

print(f"\nstreamed {n_events} events; px histogram:")
print(counts.tolist())
