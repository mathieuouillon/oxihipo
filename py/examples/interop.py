"""Hand HIPO columns to the rest of the PyData stack — NumPy, pandas, Arrow.

    python interop.py [FILE.hipo]

`arrays(..., library=...)` picks the container the same read is assembled into,
so a HIPO bank drops straight into whatever tool you already use:

    "ak"    -> awkward.Array   (default; jagged, zero-copy)
    "np"    -> dict of object-dtype ndarrays, one entry per column
    "pd"    -> pandas.DataFrame (one frame per bank)
    "arrow" -> pyarrow.Table    (large_list columns -> polars / duckdb)

`numpy()` is the rawest path: the flat values buffer plus the shared offsets,
with no Awkward import at all. Backends that aren't installed are skipped.
"""

import os
import sys

import numpy as np

import oxihipo as ox

SAMPLE = os.path.join(os.path.dirname(__file__), "..", "tests", "data", "sample.hipo")
source = sys.argv[1] if len(sys.argv) > 1 else SAMPLE

f = ox.open(source)
BANK, COLS = "REC::Particle", ["pid", "px"]
print(f"{f.num_entries} events from {os.path.basename(source)}\n")

# --- raw buffers: no Awkward needed ---------------------------------------
# One flat values array + the int64 offsets that slice it per event. This is
# exactly the layout the Rust side filled, handed over zero-copy.
col = f.numpy(BANK, "px")
print("numpy(): flat buffers, no awkward import")
print("  values   :", col.values.dtype, col.values.tolist())
print("  offsets  :", col.offsets.tolist(), "(len = n_events + 1)")
print("  inner_len:", col.inner_len, "(> 1 for a T#N array column)")
# Slice event 3 yourself:
lo, hi = col.offsets[3], col.offsets[4]
print("  event 3  :", col.values[lo:hi].tolist())

# --- library="np": a dict of per-event object arrays ----------------------
d = f.arrays(BANK, COLS, library="np")
print("\nlibrary='np':", {k: f"{type(v).__name__}[{len(v)}]" for k, v in d.items()})
print("  pid[3] :", d["pid"][3].tolist())

# --- library="pd": pandas ------------------------------------------------
try:
    df = f.arrays(BANK, COLS, library="pd")
    print("\nlibrary='pd': pandas DataFrame", df.shape)
    print(df.head(4).to_string().replace("\n", "\n  "))
except ImportError:
    print("\nlibrary='pd': skipped (pip install 'oxihipo[pandas]')")

# --- library="arrow": pyarrow, and on to polars / duckdb ------------------
try:
    tbl = f.arrays(BANK, COLS, library="arrow")
    print("\nlibrary='arrow': pyarrow.Table", tbl.shape)
    print("  schema:", str(tbl.schema).replace("\n", " | "))
except ImportError:
    tbl = None
    print("\nlibrary='arrow': skipped (pip install 'oxihipo[arrow]')")

if tbl is not None:
    # polars reads the Arrow table with no copy.
    try:
        import polars as pl

        lf = pl.from_arrow(tbl)
        counts = lf.select(pl.col("pid").list.len().alias("mult"))
        print("\n  polars: particles/event ->", counts["mult"].to_list())
    except ImportError:
        print("\n  polars: skipped (pip install polars)")

    # duckdb queries the Arrow table in place — SQL straight over a HIPO bank.
    try:
        import duckdb

        rows = duckdb.sql(
            "SELECT len(pid) AS mult, count(*) AS events "
            "FROM tbl GROUP BY mult ORDER BY mult"
        ).fetchall()
        print("  duckdb: (multiplicity, events) ->", rows)
    except ImportError:
        print("  duckdb: skipped (pip install duckdb)")

# --- everything is the same read ------------------------------------------
# The backends differ only in assembly, so the numbers agree exactly.
flat_np = np.concatenate([a for a in d["pid"] if len(a)])
print("\nall backends see the same data:",
      flat_np.tolist() == [x for sub in f.arrays(BANK, ["pid"], library="np")["pid"]
                           for x in sub.tolist()])
