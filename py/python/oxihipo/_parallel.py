"""Multi-process reading — spawn worker processes so one chain is read by
several concurrent I/O streams.

Splitting the chain into disjoint, record-aligned event ranges and reading them
from ``workers`` separate processes turns one stream into ``workers`` streams.

**Measured on ifarm, this did not speed anything up** — read that before
reaching for it. On a 9.1 GB DST in ``/volatile``, at a fixed budget of
``workers x threads = 32``:

- **cold** (page cache dropped per read): flat at ~4.1 s for every split from
  1x32 to 16x2, and *worse* at 32x1 (5.1 s). There was no per-process ceiling
  left to beat: the file had ``stripe_count: 1`` — the directory default — so
  every read of it lands on a single Lustre OST no matter how many processes
  ask. Re-striping a copy across 8 OSTs did not change it either.
- **warm**: strictly worse, and monotonically so — 0.44 s at 1x32 against
  1.26 s at 4x8 and 2.09 s at 32x1. Page cache has no per-process limit, so all
  the extra processes add is pickling the buffers back to the parent.

Where it should still pay is a *many-file* chain, where each worker opens
different files and the parent is not the bottleneck; that case is not
measured here. For a single large file, prefer ``threads`` (which scales to
~16) and, if you control how the file is written, a per-bank or per-column
codec — that was worth ~3x on a warm read, far more than anything ``workers``
did.

Each worker re-opens the source and runs the same Rust ``read_columns`` on its
range (with the GIL released); the parent stitches the returned NumPy buffers
back together (offsets shifted, values concatenated) and assembles the
requested library. `spawn` is forced — forking a process that already holds
rayon's thread pool is unsafe.
"""

from __future__ import annotations

import multiprocessing as mp
from collections import deque
from concurrent.futures import BrokenExecutor, ProcessPoolExecutor

# `spawn` re-imports the caller's `__main__` in every worker. A script whose
# read sits at module level therefore re-runs that read while the worker starts,
# and the children die during import — surfacing as a bare `BrokenProcessPool`
# that names neither the cause nor the fix. It is the first thing anyone hits
# who passes `workers=` from a plain script, so the exception says it.
_SPAWN_HINT = (
    "reading with workers>1 starts worker processes with 'spawn', which "
    "re-imports your __main__ module in every worker. The workers died during "
    "start-up. The two things that cause this:\n\n"
    "  * the read runs at module level, so it re-runs during that import. "
    'Put it behind:  if __name__ == "__main__":\n'
    "  * __main__ is not an importable file - a script fed on stdin "
    "(python - < s.py), `python -c`, or a REPL. Save it to a .py file and run "
    "that.\n\n"
    "Either way workers=1 reads in this process and always works."
)


# One opened (and filtered) chain per worker process, keyed by what identifies
# it. The pool is persistent for the life of one arrays()/iterate() call, so a
# worker that handles several batches parses each file's header/dictionary/
# trailer once instead of once per batch. (Meaningful for iterate() over
# many-file chains; arrays() gives each worker ≈one range so it barely helps.)
_CHAIN_CACHE: dict = {}


def _worker_chain(source, require, record_tag, event_tag, event_tag_any):
    key = (
        tuple(source),
        tuple(require) if require is not None else None,
        tuple(record_tag) if record_tag is not None else None,
        tuple(event_tag) if event_tag is not None else None,
        event_tag_any,
    )
    chain = _CHAIN_CACHE.get(key)
    if chain is None:
        import oxihipo

        chain = oxihipo.open(source)
        if any(x is not None for x in (require, record_tag, event_tag, event_tag_any)):
            chain = chain.filtered(
                require=require,
                record_tag=record_tag,
                event_tag=event_tag,
                event_tag_any=event_tag_any,
            )
        _CHAIN_CACHE[key] = chain
    return chain


def _read_range(source, require, record_tag, event_tag, event_tag_any, selection, start, stop, threads):
    """Worker entry point: open (once per process) the source, (re)apply the
    filter, read one global event range. Returns the raw ``read_columns``
    buffers, which are just NumPy arrays and pickle across the process
    boundary."""
    return _worker_chain(source, require, record_tag, event_tag, event_tag_any)._c.read_columns(
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


def parallel_arrays(source, require, record_tag, event_tag, event_tag_any, selection, ranges, workers, threads, assemble):
    """Read every range across ``workers`` processes, stitch, and assemble once.
    Holds the whole result in the parent (like a non-streaming read)."""
    try:
        with _pool(workers) as ex:
            futs = [
                ex.submit(_read_range, source, require, record_tag, event_tag, event_tag_any, selection, s, e, threads)
                for s, e in ranges
            ]
            results = [f.result() for f in futs]  # collected in submission (event) order
    except BrokenExecutor as e:
        raise RuntimeError(f"{_SPAWN_HINT}\n\nunderlying error: {e}") from e
    return assemble(_concat_raw(results))


def parallel_iterate(source, require, record_tag, event_tag, event_tag_any, selection, batches, workers, threads, assemble):
    """Stream ``batches`` across ``workers`` processes, keeping ~``workers`` reads
    in flight and yielding ``(assembled_chunk, start, stop, file_idx)`` in order.
    Resident memory stays ≈ ``workers`` chunks."""
    try:
        yield from _iter_batches(source, require, record_tag, event_tag, event_tag_any, selection, batches, workers, threads, assemble)
    except BrokenExecutor as e:
        raise RuntimeError(f"{_SPAWN_HINT}\n\nunderlying error: {e}") from e


def _iter_batches(source, require, record_tag, event_tag, event_tag_any, selection, batches, workers, threads, assemble):
    with _pool(workers) as ex:
        it = iter(batches)
        inflight = deque()

        def submit(b):
            inflight.append(
                (ex.submit(_read_range, source, require, record_tag, event_tag, event_tag_any, selection, b[0], b[1], threads), b)
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


def _map_range(spec, fn, banks, columns, kw, start, stop):
    """Worker entry point for :meth:`oxihipo.Chain.map_reduce`.

    Unlike :func:`_read_range`, this assembles the chunk *and* runs the user's
    function here, in the worker. Only `fn`'s return value crosses the process
    boundary — which is the whole point: a histogram pickles to a few hundred
    bytes where the chunk it was filled from is hundreds of megabytes.
    """
    chain = _worker_chain(*spec)
    chunk = chain.arrays(banks, columns, entry_start=start, entry_stop=stop, **kw)
    return fn(chunk)


def parallel_map_reduce(spec, fn, banks, columns, kw, batches, workers, reduce, initial):
    """Run `fn` over every batch across `workers` processes and combine.

    Results are consumed in **submission order**, not completion order, and
    folded one at a time. Order matters because `reduce` is not required to be
    commutative, and folding as they arrive keeps the parent holding one
    accumulator rather than every chunk's result at once.
    """
    acc = initial
    with _pool(workers) as ex:
        futs = [
            ex.submit(_map_range, spec, fn, banks, columns, kw, s, e) for s, e in batches
        ]
        for f in futs:
            r = f.result()
            acc = r if acc is None else reduce(acc, r)
    return acc
