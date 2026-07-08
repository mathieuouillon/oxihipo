# Python vs Rust read speed

How much does the Python binding cost versus reading the same columns straight
from the Rust core? On a real CLAS12 reconstruction file: **almost nothing** —
the per-event work runs in Rust behind a released GIL, and the columns are moved
into NumPy zero-copy, so Python lands within ~10% of native Rust.

## Setup

- **Machine:** Apple M4 Pro, 12 cores, 24 GB RAM.
- **File:** `rec_clas_022083.evio.00000-00009.hipo` — 9.1 GB, **598,738 events**,
  274 banks (standard whole-record HIPO compression).
- **Read:** `REC::Particle` → `px, py, pz, pid` (4 columns, **4,695,074 rows**).
  Extracting one bank still decodes every record, so this processes the whole
  9.1 GB.
- **Build:** release + `lto = "fat"` on both sides (the wheel matches the core's
  profile). Warm page cache, best-of-3.

Both harnesses are committed and reproducible:

```sh
cargo run --release --example bench_columns -- <file> REC::Particle px,py,pz,pid 0
python py/examples/bench_columns.py           <file> REC::Particle px,py,pz,pid 0
```

## Results

**All cores (`threads=0`, the default):**

| path | time | throughput | rows/s | vs Rust |
|---|--:|--:|--:|--:|
| **Rust** `read_columns` | 1.44 s | 6.3 GB/s | 3.3 M/s | 1.00× |
| **Python** `read_columns` (raw NumPy) | 1.58 s | 5.8 GB/s | 3.0 M/s | **0.91×** |
| **Python** `arrays` (Awkward) | 1.62 s | 5.6 GB/s | 2.9 M/s | **0.89×** |

**Single core (`threads=1`):**

| path | time | throughput | vs Rust |
|---|--:|--:|--:|
| **Rust** `read_columns` | 4.47 s | 2.0 GB/s | 1.00× |
| **Python** `read_columns` (raw) | 5.6 s | 1.6 GB/s | ~0.80× |

## Reading

- **The binding is nearly free.** Python's raw `read_columns` is within **~9%**
  of native Rust at full width — that gap is one PyO3 call plus the (zero-copy)
  move of each column `Vec` into a NumPy array. The whole per-event decode loop
  is the same Rust code, run with the GIL released.
- **Awkward assembly is cheap too.** Wrapping the flat buffers into a jagged
  `ak.Array` (`ListOffsetArray` / `RecordArray`) adds only ~2% on top of the raw
  path — it's pointer-wrapping, not copying.
- **It scales.** All 12 cores give ~3× the single-core throughput; the read is
  bandwidth/decode-bound, not GIL-bound (the GIL is released for the whole read).
- **Caveat on the single-core row:** under sustained multi-GB decoding, Apple
  Silicon P-cores throttle, so the later (Python) single-thread runs are
  depressed relative to the earlier (Rust) ones — that row overstates the gap.
  The all-cores numbers are bandwidth-bound and stable, and are the ones that
  matter for real analysis.

**Takeaway:** you get Rust read throughput from Python. Reach for `threads=1`
only when you're already parallel at the Python level; otherwise the default
(all cores, GIL released) is the fast path.
