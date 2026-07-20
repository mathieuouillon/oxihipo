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

**Only when I/O is the bottleneck.** This is the single most important thing to
understand about `workers=`:

- On a **parallel filesystem** (ifarm `/volatile`, Lustre), a single process
  can't saturate the available bandwidth. More processes, more streams, more
  throughput. This is what the feature is for.
- On a **local, already-cached disk**, the limit is decode and memory
  bandwidth — not I/O. Extra processes add spawn and IPC overhead and make
  things *slower*. Keep the default `workers=1` there.

Measured on a page-cached 9 GB local file (Apple M4 Pro), `workers=1` beats
`workers=2`; the multi-process path is farm-specific by design. Don't
benchmark it on your laptop and conclude it doesn't work.

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
[`Lz4ByBankV2`](../performance/compression.md) attacks that directly, and the two
compose: on ifarm the format change is worth considerably more than the process
count.
