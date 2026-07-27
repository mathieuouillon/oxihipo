---
id: parallel
title: Parallel reading
sidebar_position: 3
---

# Parallel reading (multi-process)

On a parallel filesystem — JLab ifarm's `/volatile`, Lustre — a single process
saturates well below the filesystem's aggregate bandwidth. The limit is
*per-process*, not per-node. `workers=N` splits the chain into `N` disjoint,
record-aligned event ranges, reads them from `N` separate processes, and
stitches the result: one I/O stream becomes `N`.

```python
# whole-array read, N processes, stitched into one ak.Array:
a = ox.arrays("/volatile/run5042/*.hipo", "REC::Particle", ["px", "py", "pz"], workers=8)

# streaming, ~N reads in flight (resident memory ≈ N chunks), yielded in order:
for chunk in ox.iterate("/volatile/run5042/*.hipo", "REC::Particle",
                        step_size="1 GB", workers=8):
    ...
```

:::danger Your script needs a `__main__` guard
Any script that passes `workers=` **must** be guarded by
`if __name__ == "__main__":`. Workers are *spawned*, not forked — forking a
process that already holds Rust's thread pool is unsafe — so each worker
re-imports your script. Without the guard it re-runs at import.

```python
def main():
    a = ox.arrays("/volatile/run5042/*.hipo", "REC::Particle", ["px"], workers=8)

if __name__ == "__main__":   # required
    main()
```

This also means `workers=` won't work from a heredoc or stdin, where there's no
importable main module. See
[`py/examples/parallel.py`](https://github.com/mathieuouillon/oxihipo/blob/main/py/examples/parallel.py).
:::

## When this actually helps

**Only when I/O is the bottleneck — and on ifarm, measurement did not find a
case where it was.** This page previously claimed a single process cannot
saturate a parallel filesystem, so more processes would mean more throughput.
Tested on ifarm2402 against a 9.1 GB DST in `/volatile`, that did not happen.

At a fixed budget of `workers × threads = 32`:

| `workers` × `threads` | cold | warm |
|---|---|---|
| 1 × 32 | 4.18 s | **0.44 s** |
| 2 × 16 | 4.14 s | 1.15 s |
| 4 × 8 | 4.10 s | 1.26 s |
| 16 × 2 | 4.10 s | 1.45 s |
| 32 × 1 | 5.06 s | 2.09 s |

("cold" = the file's page cache dropped with `posix_fadvise(DONTNEED)` before
each read.)

Cold is **flat** — there was no per-process ceiling left to beat. The reason is
the file layout, not the reader: `lfs getstripe` reported `stripe_count: 1`,
which is the directory default on `/volatile`, so every read of that file lands
on a **single Lustre OST** however many processes ask for it. Copying it into a
`stripe_count: 8` directory did not change the result either (best cold 2.59 s
striped against 2.44 s unstriped, inside the run-to-run scatter).

Warm is **monotonically worse** — page cache has no per-process limit, so the
extra processes only add spawn cost and pickling the buffers back to the parent.

So: prefer `threads=` for a single large file (it scales to about 16), and reach
for `workers=` only after measuring your own case. Where it should still pay is
a **many-file** chain, with each worker on different files — that case is not
measured here.

:::warning `workers>1` needs an importable `__main__`
Workers start with `spawn`, which re-imports your `__main__` in each one. A read
at module level therefore re-runs during that import and the workers die on
start-up; so does a script fed on stdin or `python -c`. Put the read behind
`if __name__ == "__main__":` and run it from a `.py` file. The error says this
now, having previously surfaced as a bare `BrokenProcessPool`.
:::

## Behaviour

- **Everything carries through.** `filter_name`, `entry_start` / `entry_stop`,
  `library=`, and `.filtered(...)` all apply inside the workers, and the
  stitched result is identical to the `workers=1` result.
- **Threads are divided, not multiplied.** Without an explicit `threads=`, the
  machine's cores are split across the workers (total ≈ all cores) rather than
  each worker grabbing every core. On an I/O-bound farm the surplus decode
  threads simply wait on the read.
- **One pool per call.** Each `arrays(workers=N)` / `iterate(workers=N)` spins
  up its own worker pool, so pay the spawn cost once: prefer a **single**
  `iterate(...)` over a many-file chain to a loop of small `arrays()` calls.
- **Bounded memory while streaming.** `iterate(workers=N)` keeps about `N`
  reads in flight and yields in order, so resident memory is ≈ `N` chunks.

## What it doesn't fix

`workers=` raises I/O throughput. It does nothing about *wasted* decompression —
if your file is stock `Lz4`, every process still inflates every bank to read the
one you asked for. Converting to
[`Lz4PerBank`](../performance/compression.md) attacks that directly, and the two
compose: on ifarm the format change is worth considerably more than the process
count.

## `map_reduce` — parallelise the physics too

`workers=` on `arrays` / `iterate` parallelises the **read**: the workers hand
raw buffers back and the parent does the analysis, serially. For a CLAS12
selection the Awkward expressions dominate, not the decode — so on its own that
is an I/O trick, not parallelism.

`map_reduce` runs your function where the chunk already is, and sends back only
what it returns:

```python
import hist

def analyze(chunk):                      # module level — it is pickled to workers
    h = hist.Hist(hist.axis.Regular(100, 0, 10, name="Q2"))
    h.fill(q2_of(chunk))
    return h

h = ox.open("/volatile/rga/*.hipo").map_reduce(analyze, "REC::Particle", workers=8)
hep.histplot(h)
```

That histogram pickles to a few hundred bytes; the chunk it was filled from is
hundreds of megabytes. That asymmetry is the point.

`reduce=` defaults to `operator.add`, which `hist.Hist`, `boost_histogram`,
`np.ndarray`, numbers, lists and `collections.Counter` all implement. For
anything else, pass your own:

```python
pids = f.map_reduce(collect_pids, "REC::Particle", reduce=set.union, initial=set())
```

Results are folded in **event order**, not completion order, so a
non-commutative `reduce` still gets them the right way round. `initial=` seeds
the accumulator and defines the answer when the selection is empty — without it,
an empty range raises rather than returning a silent `None`.

:::warning `fn` is pickled
It runs in another process, so it must be importable by name: a module-level
function, not a lambda or a closure. `workers=1` runs everything in-process,
which is how to debug one.
:::

