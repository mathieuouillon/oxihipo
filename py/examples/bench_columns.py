"""Benchmark reading columns through the Python binding — the Python half of
the Rust-vs-Python comparison (pairs with `examples/bench_columns.rs`).

Times two paths on the same columns:
  * ``read_columns`` (raw) — the materializer + zero-copy NumPy handoff; the
    apples-to-apples counterpart of the Rust ``read_columns``.
  * ``arrays`` (ak)         — the same, plus the Awkward wrapping.

The cache is warmed by one untimed read, so the numbers are decode-bound.

    python bench_columns.py <file> [bank] [c1,c2,..] [threads] [repeats]
"""

import os
import sys
import time

import oxihipo as ox

file = sys.argv[1]
bank = sys.argv[2] if len(sys.argv) > 2 else "REC::Particle"
cols = (sys.argv[3] if len(sys.argv) > 3 else "px,py,pz,pid").split(",")
threads = int(sys.argv[4]) if len(sys.argv) > 4 else 0
repeats = int(sys.argv[5]) if len(sys.argv) > 5 else 5

f = ox.open(file)
events = f.num_entries
file_gb = os.path.getsize(file) / 1e9


def bench(fn):
    fn()  # warm
    ts = []
    for _ in range(repeats):
        t = time.perf_counter()
        fn()
        ts.append(time.perf_counter() - t)
    ts.sort()
    return ts[0], ts[len(ts) // 2]


# read_columns(selection, entry_start, entry_stop, threads) — no Awkward wrap.
raw_best, raw_med = bench(lambda: f._c.read_columns([(bank, cols)], None, None, threads))
ak_best, ak_med = bench(lambda: f.arrays(bank, cols, threads=threads))

print(f"PY    bank={bank}  cols={cols}  threads={threads}  ({events} events, {file_gb:.2f} GB)")
for label, best, med in [("read_columns (raw)", raw_best, raw_med), ("arrays (awkward)", ak_best, ak_med)]:
    print(
        f"  {label:20}  best {best:.3f}s  median {med:.3f}s  |  "
        f"{events / 1e6 / best:.2f} Mevt/s  {file_gb / best:.2f} GB/s"
    )
