"""oxihipo — fast, columnar reading of HIPO (CLAS12) files, powered by Rust.

The compiled ``_oxihipo`` extension does the heavy lifting: one Rust pass over
the file materializes each requested column into a flat NumPy buffer plus a
shared ``int64`` offsets buffer, with the GIL released. This module layers the
uproot-shaped ergonomics on top — ``array`` / ``arrays`` return Awkward arrays
built *zero-copy* from those buffers; ``numpy`` returns the raw buffers so the
NumPy-only path needs no Awkward import.
"""

from __future__ import annotations

import fnmatch
import re
from dataclasses import dataclass
from typing import Sequence

from ._oxihipo import Chain as _RustChain, CorruptFileError, OxihipoError, __version__

__all__ = [
    "Chain",
    "open",
    "iterate",
    "Report",
    "CorruptFileError",
    "OxihipoError",
    "__version__",
]


@dataclass(frozen=True)
class Report:
    """Progress info yielded next to each chunk when ``iterate(report=True)``.

    ``entry_start``/``entry_stop`` are global event indices; ``file_path`` is
    the file the chunk's records came from (chunks are file-aligned)."""

    entry_start: int
    entry_stop: int
    file_path: str


_STEP_UNITS = {
    "b": 1, "kb": 10**3, "mb": 10**6, "gb": 10**9, "tb": 10**12,
    "kib": 2**10, "mib": 2**20, "gib": 2**30, "tib": 2**40,
}


def _parse_step_size(step_size):
    """``step_size`` → ``("events", n)`` (int) or ``("bytes", n)`` ("200 MB")."""
    if isinstance(step_size, int):
        if step_size <= 0:
            raise ValueError("step_size must be a positive number of events")
        return ("events", step_size)
    m = re.fullmatch(r"\s*([0-9.]+)\s*([a-zA-Z]+)\s*", str(step_size))
    if not m:
        raise ValueError(f"cannot parse step_size {step_size!r}")
    num, unit = float(m.group(1)), m.group(2).lower()
    if unit not in _STEP_UNITS:
        raise ValueError(f"unknown step_size unit {unit!r}")
    return ("bytes", max(1, int(num * _STEP_UNITS[unit])))


# --------------------------------------------------------------------------
# Awkward assembly (lazy `import awkward`, only when actually building arrays)
# --------------------------------------------------------------------------
def _wrap_column(ak, offsets, values, inner_len):
    """A jagged column: ListOffsetArray(Index64, [RegularArray] NumpyArray)."""
    node = ak.contents.NumpyArray(values)
    if inner_len > 1:  # T#N array column → inner fixed-size axis
        node = ak.contents.RegularArray(node, int(inner_len))
    return ak.contents.ListOffsetArray(ak.index.Index64(offsets), node)


def _bank_record(ak, offsets, cols):
    """A bank → list-of-records: ListOffsetArray(offsets, RecordArray([...]))."""
    fields, contents = [], []
    for name, values, inner_len in cols:
        node = ak.contents.NumpyArray(values)
        if inner_len > 1:
            node = ak.contents.RegularArray(node, int(inner_len))
        fields.append(name)
        contents.append(node)
    rec = ak.contents.RecordArray(contents, fields)
    return ak.contents.ListOffsetArray(ak.index.Index64(offsets), rec)


class _BankProxy:
    """A single bank, returned by ``chain["REC::Particle"]`` — the uproot-style
    "branch with sub-branches". Its columns are the sub-branches."""

    def __init__(self, chain: "Chain", bank: str):
        self._chain = chain
        self._bank = bank

    def keys(self) -> list[str]:
        return self._chain._c.columns(self._bank)

    def typenames(self) -> dict[str, str]:
        pref = self._bank + "/"
        return {
            k[len(pref):]: v
            for k, v in dict(self._chain._c.typenames()).items()
            if k.startswith(pref)
        }

    def array(self, column: str, **kw):
        return self._chain.array(self._bank, column, **kw)

    def arrays(self, columns: Sequence[str] | None = None, **kw):
        return self._chain.arrays(self._bank, columns, **kw)

    def __getitem__(self, column: str):
        return self.array(column)

    def __contains__(self, column: str) -> bool:
        return column in self.keys()

    def __repr__(self) -> str:
        return f"<oxihipo.Bank {self._bank!r}: {self.keys()}>"


class Chain:
    """A HIPO reader over one file, a directory, a glob, or a list of paths.

    Behaves like a uproot ``TTree``: ``keys()`` lists banks (``recursive=True``
    lists ``bank/column``), ``arrays(...)`` returns an Awkward record array, and
    ``array(bank, column)`` returns one jagged column.
    """

    def __init__(self, source):
        self._c = source if isinstance(source, _RustChain) else _RustChain(source)

    # --- delegation to the compiled reader ---------------------------------
    def __getattr__(self, name):  # num_entries, file_count, files, columns, …
        return getattr(self._c, name)

    def __len__(self):
        return len(self._c)

    def __contains__(self, bank):
        return bank in self._c

    def __repr__(self):
        return f"<oxihipo.Chain: {self._c.num_entries} events, {self._c.file_count} file(s)>"

    def keys(self, recursive: bool = False, filter_name: str | None = None) -> list[str]:
        """Bank names, or ``bank/column`` keys (``recursive=True``); optionally
        keep only those matching the ``filter_name`` glob."""
        out = self._c.keys(recursive)
        if filter_name is not None:
            out = [k for k in out if fnmatch.fnmatch(k, filter_name)]
        return out

    # --- selection resolution ---------------------------------------------
    def _resolve(self, banks, columns, filter_name):
        """→ (selection, single) where selection is [(bank, [cols])]."""
        if filter_name is not None:
            grouped: dict[str, list[str]] = {}
            for key in self._c.keys(True):
                bank = key.split("/", 1)[0]
                if fnmatch.fnmatch(key, filter_name) or fnmatch.fnmatch(bank, filter_name):
                    grouped.setdefault(bank, [])
                    if "/" in key and fnmatch.fnmatch(key, filter_name):
                        grouped[bank].append(key.split("/", 1)[1])
            # A bank matched by name (not a column glob) keeps all its columns.
            return [(b, cols) for b, cols in grouped.items()], False
        if banks is None:
            return [(b, []) for b in self._c.keys(False)], False
        if isinstance(banks, str):
            return [(banks, list(columns) if columns is not None else [])], True
        return [(b, []) for b in banks], False

    # --- the raw NumPy path (no Awkward needed) ----------------------------
    def numpy(self, bank: str, column: str, *, entry_start=None, entry_stop=None, threads=0):
        """``(values, offsets, inner_len)`` for one column — plain NumPy."""
        _, offsets, cols = self._c.read_columns(
            [(bank, [column])], entry_start, entry_stop, threads
        )[0]
        _, values, inner_len = cols[0]
        return values, offsets, inner_len

    # --- the Awkward path --------------------------------------------------
    def array(self, bank: str, column: str, *, entry_start=None, entry_stop=None, threads=0):
        """One jagged column as an ``ak.Array`` (type ``N * var * T``)."""
        import awkward as ak

        _, offsets, cols = self._c.read_columns(
            [(bank, [column])], entry_start, entry_stop, threads
        )[0]
        _, values, inner_len = cols[0]
        return ak.Array(_wrap_column(ak, offsets, values, inner_len))

    def arrays(
        self,
        banks=None,
        columns: Sequence[str] | None = None,
        *,
        filter_name: str | None = None,
        library: str = "ak",
        entry_start=None,
        entry_stop=None,
        threads=0,
    ):
        """Bank(s) → an array in the requested ``library``.

        - ``arrays("REC::Particle")`` → a jagged record (``var * {col: T}``).
        - ``arrays(["REC::Particle", "REC::Calorimeter"])`` or
          ``arrays(filter_name="REC::*")`` → a record with one field per bank.
        - ``library="ak"`` (default) → ``ak.Array``; ``"np"`` → ``dict`` of
          object-dtype ``ndarray``; ``"pd"`` → pandas ``DataFrame`` (one bank)
          or ``dict`` of frames; ``"arrow"`` → a ``pyarrow.Table`` (for
          polars / duckdb).
        - ``threads``: ``0`` = all cores (default), ``1`` = sequential, ``n`` =
          an ``n``-thread pool for the Rust read.
        """
        selection, single = self._resolve(banks, columns, filter_name)
        res = self._c.read_columns(selection, entry_start, entry_stop, threads)
        return self._assemble(res, single, library)

    def _assemble(self, res, single, library):
        if library == "ak":
            return self._assemble_ak(res, single)
        if library == "np":
            return self._assemble_np(res)
        if library == "pd":
            return self._assemble_pd(res)
        if library == "arrow":
            return self._assemble_arrow(res)
        raise ValueError(
            f"unknown library {library!r} (expected 'ak', 'np', 'pd', or 'arrow')"
        )

    def _assemble_arrow(self, res):
        import awkward as ak

        # A pyarrow.Table wants one (list-typed) column per field, i.e. a
        # top-level *record of lists* — so zip the jagged columns at depth 1
        # (not the default deep broadcast, which would nest them into one
        # column). Awkward's Arrow export uses the C Data Interface, so the
        # NumPy buffers pass through zero-copy (needs pyarrow installed).
        multi = len(res) > 1
        fields = {}
        for bname, offsets, cols in res:
            for name, values, inner in cols:
                key = f"{bname}/{name}" if multi else name
                fields[key] = ak.Array(_wrap_column(ak, offsets, values, inner))
        return ak.to_arrow_table(ak.zip(fields, depth_limit=1))

    def _assemble_ak(self, res, single):
        import awkward as ak

        built = {bname: _bank_record(ak, offsets, cols) for bname, offsets, cols in res}
        if single and len(built) == 1:
            return ak.Array(next(iter(built.values())))
        names = list(built.keys())
        return ak.Array(ak.contents.RecordArray([built[n] for n in names], names))

    def _assemble_np(self, res):
        import numpy as np

        multi = len(res) > 1
        out: dict[str, np.ndarray] = {}
        for bname, offsets, cols in res:
            split = offsets[1:-1]
            for name, values, inner in cols:
                v = values.reshape(-1, inner) if inner > 1 else values
                parts = np.split(v, split)
                arr = np.empty(len(parts), dtype=object)
                for i, part in enumerate(parts):
                    arr[i] = part
                out[f"{bname}/{name}" if multi else name] = arr
        return out

    def _assemble_pd(self, res):
        import awkward as ak

        frames = {
            bname: ak.to_dataframe(ak.Array(_bank_record(ak, offsets, cols)))
            for bname, offsets, cols in res
        }
        return next(iter(frames.values())) if len(frames) == 1 else frames

    # --- bounded-memory streaming -----------------------------------------
    def iterate(
        self,
        banks=None,
        columns: Sequence[str] | None = None,
        *,
        step_size=100_000,
        filter_name: str | None = None,
        library: str = "ak",
        report: bool = False,
        entry_start=None,
        entry_stop=None,
        threads=0,
    ):
        """Stream the chain in bounded-memory chunks.

        Each chunk is a fully materialized array (same shape as
        :meth:`arrays`) covering a contiguous run of events, yielded then
        dropped — resident memory stays ≈ one chunk, so 10–100 GB inputs read
        in constant memory. ``step_size`` is an event count (``int``) or a byte
        budget (``"200 MB"``, ``"1 GB"``). Chunks are aligned to record and
        file boundaries. With ``report=True`` each item is ``(chunk, Report)``.
        ``threads`` tunes the per-chunk Rust read (``0`` = all cores).
        """
        selection, single = self._resolve(banks, columns, filter_name)
        mode, size = _parse_step_size(step_size)
        spans = self._c.record_spans()
        sizes = self._c.record_decompressed_sizes() if mode == "bytes" else None
        files = self._c.files
        total = self._c.num_entries
        lo = 0 if entry_start is None else max(0, entry_start)
        hi = total if entry_stop is None else min(total, entry_stop)
        for start, stop, fi in self._iter_batches(spans, sizes, mode, size, lo, hi):
            res = self._c.read_columns(selection, start, stop, threads)
            chunk = self._assemble(res, single, library)
            if report:
                yield chunk, Report(start, stop, files[fi])
            else:
                yield chunk

    @staticmethod
    def _iter_batches(spans, sizes, mode, size, lo, hi):
        """Group records into file-aligned batches of ≤ ``size`` (events or
        bytes), yielding ``(start, stop, file_index)`` clamped to ``[lo, hi)``.
        A single oversized record is never split across batches."""
        cur_file = None
        start = stop = None
        acc = 0
        for i, (fi, _ri, gstart, ecount) in enumerate(spans):
            rstart, rstop = gstart, gstart + ecount
            if rstop <= lo or rstart >= hi:  # record entirely outside the range
                continue
            rsize = int(sizes[i]) if mode == "bytes" else ecount
            new = start is None or fi != cur_file or (acc > 0 and acc + rsize > size)
            if new and start is not None:
                yield max(start, lo), min(stop, hi), cur_file
            if new:
                cur_file, start, acc = fi, rstart, 0
            stop = rstop
            acc += rsize
        if start is not None:
            yield max(start, lo), min(stop, hi), cur_file

    # --- selection / write knobs ------------------------------------------
    def filtered(self, require=None, record_tag=None) -> "Chain":
        """A new chain restricted to events carrying every bank in ``require``
        (and, if given, whose record tag is in ``record_tag``)."""
        return Chain(self._c.filtered(require, record_tag))

    def skim(self, dst, compression: str = "lz4bybank") -> dict:
        """Copy the (filtered) chain to ``dst``, re-compressing. Returns
        ``{"events", "records", "bytes"}``."""
        return self._c.skim(str(dst), compression)

    def __getitem__(self, key: str):
        """``chain["REC::Particle/px"]`` → column; ``chain["REC::Particle"]`` →
        a bank proxy."""
        if "/" in key:
            bank, column = key.rsplit("/", 1)
            return self.array(bank, column)
        if key in self._c:
            return _BankProxy(self, key)
        raise KeyError(key)


def open(source) -> Chain:  # noqa: A001  (uproot-style: oxihipo.open(...))
    """Open a HIPO file, directory, glob, or list of paths → :class:`Chain`."""
    return Chain(source)


def iterate(source, banks=None, columns=None, **kwargs):
    """Stream chunks from a file/dir/glob/list without materializing it whole.

    Equivalent to ``open(source).iterate(banks, columns, **kwargs)`` — a
    generator, so a multi-file chain never opens more than it needs at once.
    """
    return open(source).iterate(banks, columns, **kwargs)
