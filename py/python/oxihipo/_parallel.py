"""Multi-process reading — spawn worker processes so one chain is read by
several concurrent I/O streams.

On a parallel filesystem (JLab ifarm's ``/volatile``, Lustre) a single process
saturates well below the filesystem's aggregate bandwidth — the limit is
per-process, not per-node. Splitting the chain into disjoint, record-aligned
event ranges and reading them from ``workers`` separate processes turns one
stream into ``workers`` streams and scales the read up.

Each worker re-opens the source and runs the same Rust ``read_columns`` on its
range (with the GIL released); the parent stitches the returned NumPy buffers
back together (offsets shifted, values concatenated) and assembles the
requested library. `spawn` is forced — forking a process that already holds
rayon's thread pool is unsafe.
"""

from __future__ import annotations

import multiprocessing as mp
from collections import deque
from concurrent.futures import ProcessPoolExecutor


# One opened (and filtered) chain per worker process, keyed by what identifies
# it. The pool is persistent for the life of one arrays()/iterate() call, so a
# worker that handles several batches parses each file's header/dictionary/
# trailer once instead of once per batch. (Meaningful for iterate() over
# many-file chains; arrays() gives each worker ≈one range so it barely helps.)
_CHAIN_CACHE: dict = {}


def _worker_chain(source, require, record_tag):
    key = (
        tuple(source),
        tuple(require) if require is not None else None,
        tuple(record_tag) if record_tag is not None else None,
    )
    chain = _CHAIN_CACHE.get(key)
    if chain is None:
        import oxihipo

        chain = oxihipo.open(source)
        if require is not None or record_tag is not None:
            chain = chain.filtered(require=require, record_tag=record_tag)
        _CHAIN_CACHE[key] = chain
    return chain


def _read_range(source, require, record_tag, selection, start, stop, threads):
    """Worker entry point: open (once per process) the source, (re)apply the
    filter, read one global event range. Returns the raw ``read_columns``
    buffers, which are just NumPy arrays and pickle across the process
    boundary."""
    return _worker_chain(source, require, record_tag)._c.read_columns(
        selection, start, stop, threads
    )


def split_ranges(spans, workers, lo, hi):
    """Split records into ``<= workers`` contiguous event ranges, balanced by
    event count and aligned to record boundaries (so worker reads touch disjoint
    file regions). ``spans`` is ``record_spans()``; returns ``[(start, stop)]``
    within ``[lo, hi)``."""
    recs = [
        (max(gs, lo), min(gs + ec, hi))
        for (_fi, _ri, gs, ec) in spans
        if gs < hi and gs + ec > lo
    ]
    if not recs:
        return []
    total = sum(e - s for s, e in recs)
    target = max(1, -(-total // workers))  # ceil(total / workers)
    ranges, cur_start, cur_stop, acc = [], None, None, 0
    for s, e in recs:
        if cur_start is None:
            cur_start = s
        cur_stop = e
        acc += e - s
        if acc >= target:
            ranges.append((cur_start, cur_stop))
            cur_start, acc = None, 0
    if cur_start is not None:
        ranges.append((cur_start, cur_stop))
    return ranges


def _concat_raw(results):
    """Stitch per-chunk raw buffers into one. Chunk *k*'s offsets are shifted by
    the running row total (dropping their leading 0), and each column's values
    are concatenated — a cheap local-memory pass over already-read data."""
    import numpy as np

    results = [r for r in results if r]
    if not results:  # empty/non-matching selection → let the assembler build empty
        return []
    out = []
    for bi in range(len(results[0])):
        bank = results[0][bi][0]
        offs, running = [results[0][bi][1]], int(results[0][bi][1][-1])
        for r in results[1:]:
            o = r[bi][1]
            offs.append(o[1:] + running)
            running += int(o[-1])
        merged = np.concatenate(offs)
        cols = []
        for ci in range(len(results[0][bi][2])):
            name, _v, inner = results[0][bi][2][ci]
            vals = np.concatenate([r[bi][2][ci][1] for r in results])
            cols.append((name, vals, inner))
        out.append((bank, merged, cols))
    return out


def _pool(workers):
    return ProcessPoolExecutor(max_workers=workers, mp_context=mp.get_context("spawn"))


def parallel_arrays(source, require, record_tag, selection, ranges, workers, threads, assemble):
    """Read every range across ``workers`` processes, stitch, and assemble once.
    Holds the whole result in the parent (like a non-streaming read)."""
    with _pool(workers) as ex:
        futs = [
            ex.submit(_read_range, source, require, record_tag, selection, s, e, threads)
            for s, e in ranges
        ]
        results = [f.result() for f in futs]  # collected in submission (event) order
    return assemble(_concat_raw(results))


def parallel_iterate(source, require, record_tag, selection, batches, workers, threads, assemble):
    """Stream ``batches`` across ``workers`` processes, keeping ~``workers`` reads
    in flight and yielding ``(assembled_chunk, start, stop, file_idx)`` in order.
    Resident memory stays ≈ ``workers`` chunks."""
    with _pool(workers) as ex:
        it = iter(batches)
        inflight = deque()

        def submit(b):
            inflight.append(
                (ex.submit(_read_range, source, require, record_tag, selection, b[0], b[1], threads), b)
            )

        for _ in range(workers):
            b = next(it, None)
            if b is None:
                break
            submit(b)
        while inflight:
            fut, b = inflight.popleft()
            res = fut.result()
            nb = next(it, None)
            if nb is not None:
                submit(nb)
            yield assemble(res), b[0], b[1], b[2]
