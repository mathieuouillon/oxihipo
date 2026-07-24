#!/usr/bin/env python3
"""Cross-implementation read benchmark — Python (oxihipo bindings).

    xbench_py.py <file.hipo> <scenario> [iters]

Same scenarios and checksums as the Rust / C++ / Java programs. Python's
idiomatic path is *columnar*: one `read_columns` pass materializes the
requested columns into NumPy buffers (decode happens in Rust with the GIL
released), then NumPy sums them. Writing a per-event Python loop instead would
measure the interpreter, not the reader, so this is what a user would actually
write.
"""
import sys
import time

import numpy as np
import oxihipo as ox

SCENARIOS = {
    "count": [],
    "bank1": [("REC::Event", ["evno"])],
    "col1": [("REC::Particle", ["pid"])],
    "scan2": [("REC::Particle", ["pid", "px"])],
    "wide": [
        (
            "REC::Particle",
            ["pid", "px", "py", "pz", "vz", "charge", "status", "chi2pid"],
        )
    ],
}


def main() -> None:
    path, scen = sys.argv[1], sys.argv[2]
    iters = int(sys.argv[3]) if len(sys.argv) > 3 else 10
    # Threads must be explicit: the binding defaults to 0 = every core, which
    # would silently compare a parallel Python read against a serial Rust one.
    threads = int(sys.argv[4]) if len(sys.argv) > 4 else 1
    sel = SCENARIOS[scen]

    best, first = float("inf"), None
    events = 0
    csum = 0.0
    for _ in range(iters):
        f = ox.open(path)
        t0 = time.perf_counter()
        if scen == "count":
            # No bank is touched; `event_tags` is the cheapest full scan the
            # binding exposes (it walks every event's header).
            tags = f.event_tags(threads=threads)
            n = len(tags)
            s = 0.0
        else:
            bufs = f.read_columns(sel, threads=threads)
            s = 0.0
            n = 0
            for _bank, offsets, cols in bufs:
                n = len(offsets) - 1
                for _name, values, _inner in cols:
                    s += float(np.asarray(values, dtype=np.float64).sum())
        dt = time.perf_counter() - t0
        events, csum = n, s
        if first is None:
            first = dt
        best = min(best, dt)

    print(f"python\t{scen}\t{first:.5f}\t{best:.5f}\t{events}\t{csum:.3f}")


if __name__ == "__main__":
    main()
