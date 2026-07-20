"""Benchmark reading each compression format through the Python binding.

The Python counterpart of `examples/bench_read_compression.rs`: for every
compression mode, report the on-disk size and how long `arrays()` takes to read
one bank ("sel") versus every bank ("all"). The per-bank / per-column formats
inflate only what you touch, so "sel" is much cheaper than "all" for them and
about the same for the whole-record formats.

Two input modes:

    # skim a source file into each format (temp dir, cleaned up):
    python bench_compression.py <source.hipo> [read_events] [iters]

    # or read pre-made <Format>.hipo files from a directory — e.g. the ones the
    # Rust benchmark leaves behind, for apples-to-apples numbers:
    OXIHIPO_BENCH_KEEP=/tmp/fmt \\
        cargo run --release --example bench_read_compression -- src.hipo 5 200000
    python bench_compression.py /tmp/fmt [read_events] [iters]

`read_events` caps the read window (via `entry_stop`) so a pass over a huge file
stays quick; the size column is always the full file. Best-of-`iters`,
warm cache, single process — relative numbers between formats are the point.
"""

import os
import shutil
import sys
import tempfile
import time

import oxihipo as ox

# The formats the Python binding can write, in the same order as the Rust bench.
FORMATS = ["None", "Lz4", "Lz4Best", "Gzip", "Lz4ByBankV2", "Lz4PerColumn"]
SKIM_NAME = {
    "None": "none",
    "Lz4": "lz4",
    "Lz4Best": "lz4best",
    "Gzip": "gzip",
    "Lz4ByBankV2": "lz4bybankv2",
    "Lz4PerColumn": "lz4percolumn",
}


def best_of(iters, fn):
    fn()  # warm
    ts = []
    for _ in range(iters):
        t = time.perf_counter()
        fn()
        ts.append(time.perf_counter() - t)
    return min(ts)


def bench_file(path, sel_bank, read_events, iters):
    """(size_bytes, sel_seconds, all_seconds) for one already-encoded file."""
    f = ox.open(path)
    stop = min(read_events, f.num_entries) if read_events else None
    sel = best_of(iters, lambda: f.arrays(sel_bank, entry_stop=stop))
    allb = best_of(iters, lambda: f.arrays(filter_name="*", entry_stop=stop))
    return os.path.getsize(path), sel, allb


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    src = sys.argv[1]
    read_events = int(sys.argv[2]) if len(sys.argv) > 2 else 200_000
    iters = int(sys.argv[3]) if len(sys.argv) > 3 else 5

    tmp = None
    if os.path.isdir(src):
        # Pre-made <Format>.hipo files (e.g. from OXIHIPO_BENCH_KEEP).
        files = {name: os.path.join(src, f"{name}.hipo") for name in FORMATS}
        files = {n: p for n, p in files.items() if os.path.isfile(p)}
        if not files:
            sys.exit(f"no <Format>.hipo files found in {src}")
    else:
        # Skim the source into each format.
        tmp = tempfile.mkdtemp(prefix="oxihipo-bench-")
        base = ox.open(src)
        sel_bank = "REC::Particle" if "REC::Particle" in base else base.keys()[0]
        print(f"skimming {src} ({base.num_entries} events) into {len(FORMATS)} formats…")
        files = {}
        for name in FORMATS:
            out = os.path.join(tmp, f"{name}.hipo")
            base.skim(out, compression=SKIM_NAME[name])
            files[name] = out

    any_path = next(iter(files.values()))
    ref = ox.open(any_path)
    sel_bank = "REC::Particle" if "REC::Particle" in ref else ref.keys()[0]
    events = min(read_events, ref.num_entries) if read_events else ref.num_entries
    print(
        f"\nread window: {events} events; sel = arrays({sel_bank!r}); "
        f"all = arrays(filter_name='*'); best-of-{iters}\n"
    )

    rows = []
    for name in FORMATS:
        if name not in files:
            continue
        size, sel, allb = bench_file(files[name], sel_bank, read_events, iters)
        rows.append((name, size, sel, allb))

    base_bytes = max(next((s for n, s, *_ in rows if n == "None"), 1), 1)
    print(f"{'format':<13}{'size MB':>9}{'ratio':>7}{'sel ms':>9}{'all ms':>9}{'sel Mevt/s':>12}")
    print("-" * 59)
    for name, size, sel, allb in rows:
        print(
            f"{name:<13}{size / 1e6:>9.1f}{size / base_bytes:>6.2f}x"
            f"{sel * 1e3:>9.1f}{allb * 1e3:>9.1f}{events / 1e6 / sel:>12.1f}"
        )

    if tmp:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
