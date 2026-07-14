"""Multi-process reading — spawn worker processes so one chain is read by
several concurrent I/O streams.

On a parallel filesystem (JLab ifarm ``/volatile``, Lustre) a single process
saturates well below the filesystem's aggregate bandwidth, so ``workers=N``
splits the chain into N disjoint record ranges read by N processes and stitches
the result. (On a local, already-cached disk it won't speed up — the bottleneck
there is decode/bandwidth, not I/O — so run this against files on the farm.)

    python parallel.py [FILE.hipo | DIR | GLOB] [max_workers]

IMPORTANT: any script that uses ``workers=`` must be guarded by
``if __name__ == "__main__":`` (below) — spawned workers re-import this file, and
without the guard they would re-run it.
"""

import os
import sys
import time

import oxihipo as ox

SAMPLE = os.path.join(os.path.dirname(__file__), "..", "tests", "data", "sample.hipo")


def main():
    source = sys.argv[1] if len(sys.argv) > 1 else SAMPLE
    max_workers = int(sys.argv[2]) if len(sys.argv) > 2 else 8
    cols = ["px", "py", "pz", "pid"]

    events = ox.open(source).num_entries
    print(f"{events} events; reading REC::Particle {cols}\n")

    # Correctness: N processes must give the same array as one.
    import awkward as ak

    ref = ox.arrays(source, "REC::Particle", cols)
    par = ox.arrays(source, "REC::Particle", cols, workers=4)
    assert ak.to_list(ref.pid[:100]) == ak.to_list(par.pid[:100]) and len(ref) == len(par)
    print("workers=4 matches workers=1 ✓\n")

    ws = [w for w in (1, 2, 4, 8, 16) if w <= max_workers]
    for w in ws:
        ox.arrays(source, "REC::Particle", cols, workers=w)  # warm
        best = min(_time(lambda: ox.arrays(source, "REC::Particle", cols, workers=w)) for _ in range(2))
        print(f"  workers={w:2d}: {best:5.2f}s  ({events / 1e6 / best:.2f} Mevt/s)")


def _time(fn):
    t = time.perf_counter()
    fn()
    return time.perf_counter() - t


if __name__ == "__main__":  # required for multiprocessing `spawn`
    main()
