"""Benchmark the ROOT RDataFrame bridge (`oxihipo.rdataframe`).

Answers two questions on the same oxihipo columnar read:

  1. *What does the bridge cost?*  `rdataframe` = `arrays` + `ak.to_rdataframe`.
     The wrap is a no-copy view, so `rdataframe` should sit right on top of the
     bare `arrays` read.
  2. *RDF vs. just using Awkward?*  The same analysis — per-particle
     ``pt = sqrt(px^2 + py^2)`` filled into a histogram — run two ways: a
     declarative RDataFrame graph, and vectorized NumPy/Awkward.

ROOT JIT-compiles the RDataSource template and each `Define` expression the
*first* time they're seen (cling), a fixed per-process cost. That warmup is
measured separately from the steady-state loop, which is what the best-of-N
timings below report (cache + cling warmed by one untimed pass).

    python bench_rdataframe.py [FILE.hipo] [n_events_if_generating] [repeats]

With no FILE it writes a synthetic REC::Particle file (Poisson multiplicity,
Gaussian momenta) with oxihipo's own writer. Needs ROOT + awkward.

Note: ROOT's implicit multithreading (`EnableImplicitMT`) is **not** used — the
Awkward-generated RDataSource does not cooperate with it (the loop hangs), so
the RDF side here is single-threaded. That's a real limitation of the pure-Python
bridge; a native C++ RHipoDS would own the threads instead.
"""

import os
import sys
import time
import tempfile

try:
    import ROOT
except ImportError:
    print("skipping: this benchmark needs ROOT — `pip install oxihipo[root]` + PyROOT")
    raise SystemExit(0)

import numpy as np

import oxihipo as ox

BANK = "REC::Particle"
BINS, LO, HI = 100, 0.0, 6.0
PT_EXPR = f"sqrt({BANK.replace('::', '_')}_px*{BANK.replace('::', '_')}_px" \
          f" + {BANK.replace('::', '_')}_py*{BANK.replace('::', '_')}_py)"


def make_sample(path: str, n_events: int, avg_mult: float = 3.0, seed: int = 1) -> int:
    """Write a synthetic REC::Particle file; return the particle count."""
    import awkward as ak

    rng = np.random.default_rng(seed)
    counts = rng.poisson(avg_mult, n_events).astype(np.int64)
    total = int(counts.sum())
    cols = {
        "pid": ak.unflatten(rng.choice([11, -11, 211, -211, 2212], total).astype(np.int32), counts),
        "px": ak.unflatten(rng.normal(0, 1, total).astype(np.float32), counts),
        "py": ak.unflatten(rng.normal(0, 1, total).astype(np.float32), counts),
        "pz": ak.unflatten(rng.normal(0, 2, total).astype(np.float32), counts),
    }
    with ox.create(path, compression="lz4percolumn") as w:
        w.new_bank(BANK, {"pid": "I", "px": "F", "py": "F", "pz": "F"})
        w.extend({BANK: cols})
    return total


def bench(fn, repeats):
    fn()  # warm (cache + cling)
    ts = []
    for _ in range(repeats):
        t = time.perf_counter()
        fn()
        ts.append(time.perf_counter() - t)
    ts.sort()
    return ts[0], ts[len(ts) // 2]


# --- workloads -------------------------------------------------------------
def read_arrays(file):
    return ox.arrays(file, BANK, ["px", "py"])


def analysis_awkward(file):
    """pt histogram, the vectorized NumPy/Awkward way (no ROOT)."""
    import awkward as ak

    p = ox.arrays(file, BANK, ["px", "py"])
    px = ak.to_numpy(ak.flatten(p.px))
    py = ak.to_numpy(ak.flatten(p.py))
    pt = np.sqrt(px * px + py * py)
    return np.histogram(pt, bins=BINS, range=(LO, HI))


def analysis_rdataframe(file):
    """The same pt histogram, as an RDataFrame graph (single-threaded)."""
    df = ox.rdataframe(file, BANK, ["px", "py"])
    return df.Define("pt", PT_EXPR).Histo1D(("pt", "", BINS, LO, HI), "pt").GetValue()


def cold_start(file):
    """One-time cling costs in a fresh interpreter state: RDataSource codegen
    (first to_rdataframe) then expression JIT (first Define+run)."""
    t = time.perf_counter()
    df = ox.rdataframe(file, BANK, ["px", "py"])
    t_wrap = time.perf_counter() - t
    t = time.perf_counter()
    df.Define("pt", PT_EXPR).Histo1D(("pt", "", BINS, LO, HI), "pt").GetValue()
    return t_wrap, time.perf_counter() - t


def main():
    arg_file = sys.argv[1] if len(sys.argv) > 1 else None
    n_events = int(sys.argv[2]) if len(sys.argv) > 2 else 500_000
    repeats = int(sys.argv[3]) if len(sys.argv) > 3 else 7

    tmp = None
    if arg_file:
        file, nparts = arg_file, None
    else:
        tmp = tempfile.mkdtemp()
        file = os.path.join(tmp, "bench.hipo")
        nparts = make_sample(file, n_events)

    try:
        f = ox.open(file)
        events = f.num_entries
        file_mb = os.path.getsize(file) / 1e6

        # Cold-start (measured before any warmup touches cling).
        wrap_cold, jit_cold = cold_start(file)

        read = bench(lambda: read_arrays(file), repeats)
        rbuild = bench(lambda: ox.rdataframe(file, BANK, ["px", "py"]), repeats)
        ak_full = bench(lambda: analysis_awkward(file), repeats)
        rdf_full = bench(lambda: analysis_rdataframe(file), repeats)

        parts = f"{nparts / 1e6:.2f} M particles" if nparts else "particles: n/a"
        print(f"\n{BANK}  px,py   {events / 1e6:.2f} M events, {parts}, "
              f"{file_mb:.1f} MB on disk   (best-of-{repeats}, single-thread)\n")

        def line(label, res):
            best, med = res
            print(f"  {label:32}  best {best * 1e3:7.1f} ms   median {med * 1e3:7.1f} ms"
                  f"   {events / 1e6 / best:6.1f} Mevt/s")

        line("arrays read (baseline)", read)
        line("rdataframe build (read + wrap)", rbuild)
        line("analysis: Awkward/NumPy", ak_full)
        line("analysis: RDataFrame", rdf_full)

        wrap_over = (rbuild[0] - read[0]) * 1e3
        print(f"\n  bridge wrap over the bare read : {wrap_over:+.1f} ms  "
              f"(no-copy view — should be ~0)")
        print(f"  one-time cling warmup          : {wrap_cold * 1e3:.0f} ms RDataSource codegen"
              f" + {jit_cold * 1e3:.0f} ms first-Define JIT (per process, amortized)")
    finally:
        if tmp:
            import shutil
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
