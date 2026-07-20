---
id: shared-filesystems
title: Shared filesystems
sidebar_position: 2
---

# Performance on shared filesystems

When the input lives on a network filesystem — JLab ifarm's `/volatile` and
`/cache` (Lustre), NFS — **I/O latency dominates wall time**. Everything on this
page is about that regime.

## What the reader already does

The reader issues one `pread` per record and relies on the kernel's
per-descriptor readahead to fetch the next record while the current one
decompresses. Parallel mode keeps several records in flight across workers.

Resident memory stays bounded — one record per worker — no matter how large the
file, so a wide parallel scan won't be OOM-killed by a memory-capped batch
allocation. That's what makes oversubscribing threads safe.

## If you're still I/O-bound

The levers are user-side, roughly in order of payoff:

### 1. Change the format

Converting to [`Lz4ByBankV2`](./compression.md) is usually worth more than every
other lever combined — on the ifarm skim it cut the file to a quarter of its
size *and* stopped inflating banks the analysis never reads. Fewer bytes off
Lustre, less LZ4 work. Start here.

### 2. Stripe the file

A Lustre file on a single OST is bandwidth-capped no matter the thread count:

```sh
lfs setstripe -c 4 outfile.hipo      # new outputs
lfs migrate  -c 4 file.hipo          # existing files
```

### 3. Oversubscribe threads

Pass `threads = 2 × num_cpus` to `for_each` to hide network page-fault stalls.
Thread scaling is linear well past `num_cpus` for the by-bank format on Lustre.

### 4. Stage to local scratch

```sh
cp /volatile/.../file.hipo /scratch/$USER/
```

Local disk easily beats Lustre per single client. This is the right move for
sequential dev/debug work in particular.

### 5. From Python, use more processes

A single *process* saturates below the filesystem's aggregate bandwidth — the
ceiling is per-process, not per-node. `workers=N` turns one I/O stream into `N`:

```python
a = ox.arrays("/volatile/run5042/*.hipo", "REC::Particle", ["px"], workers=8)
```

See [Parallel reading](../python/parallel.md) — including the `__main__` guard
it requires, and why it does nothing on a local cached disk.

## Things worth knowing

- **Sequential is permanently Lustre-bound on `/volatile`.** Single-stream RPCs
  cap you around 400–500 kev/s regardless of LZ4 format. For sequential
  dev/debug, stage to `/scratch`.
- **`/volatile` can beat `/scratch` in parallel** when the file is
  ifarm-page-cache-hot from a just-completed recook. Once the cache evicts,
  cold-read Lustre numbers land closer to the `/scratch` figures. Be careful not
  to fool yourself benchmarking right after a conversion.
- **Default `threads = 0`** (one worker per logical CPU) is a good starting
  point; go to `2 × num_cpus` if you're still stalling on page faults.

The measured numbers behind all of this are in
[Benchmarks](./benchmarks.md).
