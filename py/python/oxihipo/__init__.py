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
import os
import re
from collections.abc import Iterable, Iterator, Mapping, Sequence
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any, Literal, NamedTuple, overload

from ._oxihipo import (
    Chain as _RustChain,
    CorruptFileError,
    OxihipoError,
    Writer as _RustWriter,
    __version__,
)

if TYPE_CHECKING:  # optional/lazy deps — imported only for annotations, never at runtime
    import awkward as ak
    import numpy as np
    import pandas as pd
    import pyarrow as pa

# Path-like inputs. Python 3.13 (the minimum we support) evaluates these `|`
# unions and the `os.PathLike[str]` subscription at runtime, so they can be
# plain module-level aliases — usable by `typing.get_type_hints`, not just
# checkers.
StrPath = str | os.PathLike[str]
Source = StrPath | Sequence[StrPath]

__all__ = [
    "Chain",
    "Writer",
    "open",
    "create",
    "recreate",
    "iterate",
    "arrays",
    "Report",
    "NumpyColumn",
    "SkimSummary",
    "CorruptFileError",
    "OxihipoError",
    "__version__",
]

# Library name → return type, for the `library=` annotations below.
_Library = Literal["ak", "np", "pd", "arrow"]


class NumpyColumn(NamedTuple):
    """Return of :meth:`Chain.numpy` — a flat column plus its jagged layout.

    Unpacks positionally like the old 3-tuple (``values, offsets, inner_len``)
    but self-documents and disambiguates from ``read_columns``' element order."""

    values: "np.ndarray"
    offsets: "np.ndarray"  # int64, length = n_events + 1
    inner_len: int  # > 1 for fixed-size ``T#N`` array columns


class SkimSummary(NamedTuple):
    """Return of :meth:`Chain.skim` — what was written to the new file.

    Attribute access (``s.events``) or positional unpacking
    (``events, records, nbytes = s``); ``bytes`` is the total on-disk size."""

    events: int
    records: int
    bytes: int


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
    # `bool` is an `int` subclass — reject it so `step_size=True` isn't read as
    # "1 event" by accident.
    if isinstance(step_size, bool):
        raise TypeError("step_size must be an int or byte-budget string, not bool")
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
    n = int(num * _STEP_UNITS[unit])
    if n <= 0:  # e.g. "0 MB" — mirror the int path instead of silently clamping to 1
        raise ValueError("step_size must be a positive byte budget")
    return ("bytes", n)


def _worker_threads(threads, workers):
    """Per-worker rayon thread count for a ``workers``-process read. An explicit
    ``threads`` wins; otherwise the machine's cores are split across the workers
    (total ≈ all cores) so the decode keeps up without N×oversubscribing the
    CPU. On an I/O-bound farm the surplus threads simply wait on the read."""
    if threads:
        return threads
    return max(1, (os.cpu_count() or 1) // workers)


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
        # `typenames()` already returns a dict — iterate it directly, no re-copy.
        pref = self._bank + "/"
        return {
            k[len(pref):]: v
            for k, v in self._chain._c.typenames().items()
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

    def __iter__(self):
        return iter(self.keys())

    def __len__(self) -> int:
        return len(self.keys())

    def __repr__(self) -> str:
        return f"<oxihipo.Bank {self._bank!r}: {self.keys()}>"


class Chain:
    """A HIPO reader over one file, a directory, a glob, or a list of paths.

    Behaves like a uproot ``TTree``: ``keys()`` lists banks (``recursive=True``
    lists ``bank/column``), ``arrays(...)`` returns an Awkward record array, and
    ``array(bank, column)`` returns one jagged column.
    """

    # `close()` sets this to None; typed as the reader so member access checks
    # against the real surface (use-after-close raises naturally at runtime).
    _c: "_RustChain"

    if TYPE_CHECKING:
        # These live on the compiled reader and reach users through __getattr__.
        # Declaring them here (no runtime attribute is created) lets type
        # checkers and IDEs see the real surface instead of collapsing it to
        # ``Any`` — which is what makes the shipped ``py.typed`` marker useful.
        num_entries: int
        file_count: int
        files: list[str]

        def columns(self, bank: str) -> list[str]: ...
        def typenames(self) -> dict[str, str]: ...
        def record_spans(self) -> list[tuple[int, int, int, int]]: ...
        def record_decompressed_sizes(self) -> list[int]: ...

    def __init__(self, source):
        self._c = source if isinstance(source, _RustChain) else _RustChain(source)
        # The resolved file list + any filter, so worker processes can re-open
        # an identical chain (see `workers=` on arrays/iterate).
        self._source = list(self._c.files)
        self._require = None
        self._record_tag = None
        self._event_tag = None
        self._event_tag_any = None
        # Per-record metadata is immutable (the Rust chain is frozen) and
        # `filtered()` builds a fresh wrapper, so it is safe to memoize.
        self._spans_cache: list | None = None
        self._sizes_cache: list | None = None

    # --- delegation to the compiled reader ---------------------------------
    def _reader(self) -> "_RustChain":
        """The compiled reader, or a clean error if the chain has been closed."""
        if self._c is None:
            raise ValueError("operation on a closed Chain")
        return self._c

    def __getattr__(self, name):  # num_entries, file_count, files, columns, …
        # __getattr__ runs only when normal lookup fails. Guard the delegate so
        # a half-built Chain (copy/pickle/__new__, before `_c` exists) raises a
        # clean AttributeError instead of recursing on `self._c` forever, and a
        # closed chain raises a clear ValueError rather than a NoneType error.
        if name.startswith("__") or name == "_c":
            raise AttributeError(name)
        c = self.__dict__.get("_c", None)
        if c is None:
            if "_c" in self.__dict__:  # present but None → closed
                raise ValueError("operation on a closed Chain")
            raise AttributeError(name)  # absent → half-built
        return getattr(c, name)

    def __dir__(self):
        # Surface the delegated reader methods to dir()/help()/autocomplete.
        extra = set(dir(self._c)) if self._c is not None else set()
        return sorted(set(super().__dir__()) | extra)

    def _record_spans(self):
        if self._spans_cache is None:
            self._spans_cache = self._reader().record_spans()
        return self._spans_cache

    def _record_decompressed_sizes(self):
        if self._sizes_cache is None:  # per-record header I/O — worth caching
            self._sizes_cache = self._reader().record_decompressed_sizes()
        return self._sizes_cache

    def __len__(self):
        return len(self._reader())

    def __iter__(self):
        """Iterate bank names — dict-style, consistent with ``bank in chain``
        and ``chain[bank]``. Note ``len(chain)`` is the *event* count (uproot
        parity), so it intentionally differs from ``len(list(chain))``."""
        return iter(self.keys())

    def __contains__(self, bank):
        return bank in self._reader()

    def __repr__(self):
        c = self.__dict__.get("_c", None)
        if c is None:
            return "<oxihipo.Chain: closed>"
        return f"<oxihipo.Chain: {c.num_entries} events, {c.file_count} file(s)>"

    # --- resource management (uproot parity) -------------------------------
    def close(self) -> None:
        """Release the underlying reader (idempotent). ``with`` closes for you.

        The core reads with positioned ``pread`` on a shared file descriptor
        (no mmap), so CPython already drops handles when the chain goes out of
        scope — this just makes ``with ox.open(...) as f`` work."""
        self._c = None  # type: ignore[assignment]

    def __enter__(self) -> "Chain":
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    def keys(self, recursive: bool = False, filter_name: str | None = None) -> list[str]:
        """Bank names, or ``bank/column`` keys (``recursive=True``); optionally
        keep only those matching the ``filter_name`` glob."""
        out = self._reader().keys(recursive)
        if filter_name is not None:
            out = [k for k in out if fnmatch.fnmatch(k, filter_name)]
        return out

    def _schema_lines(self, bank: str | None = None) -> list[str]:
        """Human-readable ``bank → column: dtype`` lines (all banks, or one)."""
        grouped: dict[str, list[tuple[str, str]]] = {}
        for key, dt in self._reader().typenames().items():
            b, col = key.split("/", 1)
            grouped.setdefault(b, []).append((col, dt))
        names = [bank] if bank is not None else sorted(grouped)
        lines = []
        for b in names:
            cols = grouped.get(b, [])
            lines.append(f"{b}  ({len(cols)} columns)")
            lines += [f"    {c:<24} {dt}" for c, dt in cols]
        return lines

    def show(self, bank: str | None = None) -> None:
        """Print the dictionary — every bank and its ``column: dtype`` (or, with
        ``bank=``, just that one). A human-readable view of :meth:`typenames`."""
        print("\n".join(self._schema_lines(bank)))

    # --- selection resolution ---------------------------------------------
    def _resolve(self, banks, columns, filter_name):
        """→ (selection, single) where selection is [(bank, [cols])]."""
        # `columns=` only makes sense for a single named bank; across banks,
        # columns are selected via filter_name. Fail loudly rather than drop it.
        if columns is not None and (filter_name is not None or not isinstance(banks, str)):
            raise TypeError(
                "`columns=` is only valid with a single bank name; select "
                "columns across banks via filter_name='BANK/col*'"
            )
        if filter_name is not None:
            grouped: dict[str, list[str]] = {}
            for key in self._reader().keys(True):
                bank = key.split("/", 1)[0]
                if fnmatch.fnmatch(key, filter_name) or fnmatch.fnmatch(bank, filter_name):
                    grouped.setdefault(bank, [])
                    if "/" in key and fnmatch.fnmatch(key, filter_name):
                        grouped[bank].append(key.split("/", 1)[1])
            # A bank matched by name (not a column glob) keeps all its columns.
            return [(b, cols) for b, cols in grouped.items()], False
        if banks is None:
            return [(b, []) for b in self._reader().keys(False)], False
        if isinstance(banks, str):
            return [(banks, list(columns) if columns is not None else [])], True
        return [(b, []) for b in banks], False

    # --- the raw NumPy path (no Awkward needed) ----------------------------
    def numpy(
        self,
        bank: str,
        column: str,
        *,
        entry_start: int | None = None,
        entry_stop: int | None = None,
        threads: int = 0,
    ) -> NumpyColumn:
        """``(values, offsets, inner_len)`` for one column — plain NumPy.

        Returns a :class:`NumpyColumn` named tuple (still unpacks positionally)."""
        _, offsets, cols = self._reader().read_columns(
            [(bank, [column])], entry_start, entry_stop, threads
        )[0]
        _, values, inner_len = cols[0]
        return NumpyColumn(values, offsets, inner_len)

    def event_tags(
        self,
        *,
        entry_start: int | None = None,
        entry_stop: int | None = None,
        threads: int = 0,
    ) -> "np.ndarray":
        """The per-event tag (``EH_TAG``) as a flat ``uint32`` NumPy array — one
        per event, in the same order and under the same filter as
        :meth:`arrays` / :meth:`numpy`. So ``f.event_tags()`` lines up 1:1 with
        ``f.arrays(...)`` for per-event cuts and histograms::

            p = f.arrays("REC::Particle", ["px"])
            t = f.event_tags()          # one uint32 per event, aligned with p
            dvcs = p[(t & DVCS) != 0]   # select DVCS events

        Honors ``entry_start`` / ``entry_stop`` and the chain filter; the tag is
        read from the header/directory without inflating any bank."""
        return self._reader().event_tags(entry_start, entry_stop, threads)

    # --- the Awkward path --------------------------------------------------
    def array(
        self,
        bank: str,
        column: str,
        *,
        entry_start: int | None = None,
        entry_stop: int | None = None,
        threads: int = 0,
    ) -> "ak.Array":
        """One jagged column as an ``ak.Array`` (type ``N * var * T``)."""
        import awkward as ak

        _, offsets, cols = self._reader().read_columns(
            [(bank, [column])], entry_start, entry_stop, threads
        )[0]
        _, values, inner_len = cols[0]
        return ak.Array(_wrap_column(ak, offsets, values, inner_len))

    # Overloads give the exact return type per `library` (ak.Array / np dict /
    # pandas / pyarrow) so checkers and IDEs don't collapse it to a union.
    @overload
    def arrays(
        self, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
        *, filter_name: str | None = ..., library: Literal["ak"] = ...,
        entry_start: int | None = ..., entry_stop: int | None = ...,
        threads: int = ..., workers: int = ...,
    ) -> "ak.Array": ...
    @overload
    def arrays(
        self, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
        *, filter_name: str | None = ..., library: Literal["np"],
        entry_start: int | None = ..., entry_stop: int | None = ...,
        threads: int = ..., workers: int = ...,
    ) -> "dict[str, np.ndarray]": ...
    @overload
    def arrays(
        self, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
        *, filter_name: str | None = ..., library: Literal["pd"],
        entry_start: int | None = ..., entry_stop: int | None = ...,
        threads: int = ..., workers: int = ...,
    ) -> "pd.DataFrame | dict[str, pd.DataFrame]": ...
    @overload
    def arrays(
        self, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
        *, filter_name: str | None = ..., library: Literal["arrow"],
        entry_start: int | None = ..., entry_stop: int | None = ...,
        threads: int = ..., workers: int = ...,
    ) -> "pa.Table": ...
    def arrays(
        self,
        banks: str | Sequence[str] | None = None,
        columns: Sequence[str] | None = None,
        *,
        filter_name: str | None = None,
        library: _Library = "ak",
        entry_start: int | None = None,
        entry_stop: int | None = None,
        threads: int = 0,
        workers: int = 1,
    ):
        """Bank(s) → an array in the requested ``library``.

        - ``arrays("REC::Particle")`` → a jagged record (``var * {col: T}``).
        - ``arrays(["REC::Particle", "REC::Calorimeter"])`` or
          ``arrays(filter_name="REC::*")`` → a record with one field per bank.
        - ``library="ak"`` (default) → ``ak.Array``; ``"np"`` → ``dict`` of
          object-dtype ``ndarray``; ``"pd"`` → pandas ``DataFrame`` (one bank)
          or ``dict`` of frames; ``"arrow"`` → a ``pyarrow.Table`` (for
          polars / duckdb).
        - ``threads``: rayon threads *within* the read (``0`` = all cores).
        - ``workers``: read with ``N`` **processes** (disjoint record ranges,
          stitched into one result). On a parallel filesystem (ifarm) this is
          the way to beat the per-process I/O ceiling. Without an explicit
          ``threads``, the cores are split across workers (total ≈ all cores);
          on the farm the extra decode threads simply wait on I/O.
        """
        c = self._reader()
        selection, single = self._resolve(banks, columns, filter_name)
        if workers and workers > 1:
            from . import _parallel

            total = c.num_entries
            lo = 0 if entry_start is None else max(0, entry_start)
            hi = total if entry_stop is None else min(total, entry_stop)
            ranges = _parallel.split_ranges(self._record_spans(), workers, lo, hi)
            # Only fan out when the range actually splits AND something is
            # selected; otherwise fall through to the single-process read, which
            # handles empty/degenerate cases (a filter that matched nothing,
            # entry_start past the end, an empty bank list) as one empty result.
            if len(ranges) > 1 and selection:
                return _parallel.parallel_arrays(
                    self._source, self._require, self._record_tag,
                    self._event_tag, self._event_tag_any, selection, ranges,
                    workers, _worker_threads(threads, workers),
                    lambda res: self._assemble(res, single, library),
                )
        res = c.read_columns(selection, entry_start, entry_stop, threads)
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

    def _assemble_arrow(self, res) -> "pa.Table":
        import pyarrow as pa

        # Build the pyarrow.Table straight from the returned NumPy buffers — one
        # (large-)list column per field — instead of round-tripping through
        # Awkward. `pa.array(offsets)`/`pa.array(values)` wrap the int64/numeric
        # buffers zero-copy, so the whole path stays copy-free and needs no
        # awkward import (only pyarrow).
        multi = len(res) > 1
        cols: dict[str, pa.Array] = {}
        for bname, offsets, bcols in res:
            off = pa.array(offsets)  # int64 → LargeList offsets, zero-copy
            for name, values, inner in bcols:
                child = pa.array(values)  # numeric, no nulls → zero-copy
                if inner > 1:  # T#N array column → fixed-size inner list
                    child = pa.FixedSizeListArray.from_arrays(child, int(inner))
                key = f"{bname}/{name}" if multi else name
                cols[key] = pa.LargeListArray.from_arrays(off, child)
        return pa.table(cols)  # empty selection → a valid empty table (no crash)

    def _assemble_ak(self, res, single):
        import awkward as ak

        built = {bname: _bank_record(ak, offsets, cols) for bname, offsets, cols in res}
        if not built:
            # Empty/non-matching selection → a length-0 array, not a crash.
            # length=0 (rather than num_entries) keeps this consistent across a
            # sub-range, a filtered chain, and the arrow backend (0 rows).
            return ak.Array(ak.contents.RecordArray([], [], length=0))
        if single and len(built) == 1:
            return ak.Array(next(iter(built.values())))
        names = list(built.keys())
        return ak.Array(ak.contents.RecordArray([built[n] for n in names], names))

    def _assemble_np(self, res):
        import numpy as np

        multi = len(res) > 1
        out: dict[str, np.ndarray] = {}
        for bname, offsets, cols in res:
            # One bulk int64→pyint conversion, shared by every column of the
            # bank; `n == 0` yields a length-0 array (np.split gave a spurious
            # length-1 one). Then fill the object array in a single pass whose
            # slices are zero-copy views into the flat buffer.
            bounds = offsets.tolist()
            n = len(bounds) - 1
            for name, values, inner in cols:
                v = values.reshape(-1, inner) if inner > 1 else values
                arr = np.empty(n, dtype=object)
                for i in range(n):
                    arr[i] = v[bounds[i]:bounds[i + 1]]
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
    # Overloads capture the useful distinction: ``report=True`` yields
    # ``(chunk, Report)`` pairs, otherwise bare chunks. The chunk itself is
    # ``Any`` because its type depends on ``library=`` (see ``arrays`` for the
    # per-library types).
    @overload
    def iterate(
        self, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
        *, step_size: int | str = ..., filter_name: str | None = ..., library: _Library = ...,
        report: Literal[False] = ..., entry_start: int | None = ..., entry_stop: int | None = ...,
        threads: int = ..., workers: int = ...,
    ) -> "Iterator[Any]": ...
    @overload
    def iterate(
        self, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
        *, step_size: int | str = ..., filter_name: str | None = ..., library: _Library = ...,
        report: Literal[True], entry_start: int | None = ..., entry_stop: int | None = ...,
        threads: int = ..., workers: int = ...,
    ) -> "Iterator[tuple[Any, Report]]": ...
    def iterate(
        self,
        banks: str | Sequence[str] | None = None,
        columns: Sequence[str] | None = None,
        *,
        step_size: int | str = 100_000,
        filter_name: str | None = None,
        library: _Library = "ak",
        report: bool = False,
        entry_start: int | None = None,
        entry_stop: int | None = None,
        threads: int = 0,
        workers: int = 1,
    ):
        """Stream the chain in bounded-memory chunks.

        Each chunk is a fully materialized array (same shape as
        :meth:`arrays`) covering a contiguous run of events, yielded then
        dropped — resident memory stays ≈ one chunk, so 10–100 GB inputs read
        in constant memory. ``step_size`` is an event count (``int``) or a byte
        budget (``"200 MB"``, ``"1 GB"``). Chunks are aligned to record and
        file boundaries. With ``report=True`` each item is ``(chunk, Report)``.
        ``threads`` tunes the per-chunk Rust read (``0`` = all cores).

        ``workers=N`` reads the batches across ``N`` **processes** — the way to
        beat the per-process I/O ceiling on a parallel filesystem (ifarm). It
        keeps ~``N`` reads in flight (resident memory ≈ ``N`` chunks) and yields
        in order. Without an explicit ``threads``, cores are split across the
        workers (total ≈ all cores).
        """
        c = self._reader()
        selection, single = self._resolve(banks, columns, filter_name)
        mode, size = _parse_step_size(step_size)
        spans = self._record_spans()
        sizes = self._record_decompressed_sizes() if mode == "bytes" else None
        files = c.files
        total = c.num_entries
        lo = 0 if entry_start is None else max(0, entry_start)
        hi = total if entry_stop is None else min(total, entry_stop)
        batches = list(self._iter_batches(spans, sizes, mode, size, lo, hi))

        if workers and workers > 1:
            from . import _parallel

            stream = _parallel.parallel_iterate(
                self._source, self._require, self._record_tag,
                self._event_tag, self._event_tag_any, selection, batches,
                workers, _worker_threads(threads, workers),
                lambda res: self._assemble(res, single, library),
            )
            for chunk, start, stop, fi in stream:
                yield (chunk, Report(start, stop, files[fi])) if report else chunk
            return

        for start, stop, fi in batches:
            res = c.read_columns(selection, start, stop, threads)
            chunk = self._assemble(res, single, library)
            yield (chunk, Report(start, stop, files[fi])) if report else chunk

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
    @property
    def tag_names(self) -> dict[str, int]:
        """The file's persisted tag registry as ``{name: bit_position}`` (empty
        if none was written). Lets :meth:`filtered` resolve tag names — e.g.
        ``filtered(event_tag="dvcs")`` — without the Rust ``tag_flags!`` block
        that produced them::

            f.tag_names          # {'dvcs': 0, 'sidis': 1, 'elastic': 2}
            f.filtered(event_tag="dvcs")           # events with the dvcs bit
            f.filtered(event_tag=["dvcs", "sidis"])  # dvcs OR sidis
        """
        return dict(self._reader().tag_names())

    def filtered(
        self,
        require: Sequence[str] | None = None,
        record_tag: Sequence[int] | None = None,
        event_tag: str | int | Sequence[str | int] | None = None,
        event_tag_any: str | int | Sequence[str | int] | None = None,
    ) -> "Chain":
        """A new chain keeping only events that carry every bank in ``require``,
        whose record tag is in ``record_tag``, and whose per-event tag matches
        ``event_tag`` / ``event_tag_any``. Every clause is applied with pushdown
        — non-matching events are dropped before their banks are inflated.
        ``num_entries`` stays the pre-filter total.

        Tag **names** (resolved via :attr:`tag_names`) are accepted anywhere a
        tag is: ``event_tag="dvcs"`` or ``["dvcs", "sidis"]`` keeps events with
        *any* of those named bits set (a named flag *is* a bit). A purely
        numeric ``event_tag`` keeps its exact-set-of-``uint32`` meaning;
        ``event_tag_any`` is always an OR'd bitmask. An unknown name raises
        ``KeyError``."""
        numeric, any_mask = self._resolve_tag_filters(event_tag, event_tag_any)
        new = Chain(self._reader().filtered(require, record_tag, numeric, any_mask))
        new._require = require
        new._record_tag = record_tag
        new._event_tag = numeric
        new._event_tag_any = any_mask
        return new

    def _resolve_tag_filters(
        self, event_tag, event_tag_any
    ) -> "tuple[list[int] | None, int | None]":
        """Resolve the two tag params to ``(numeric_exact_set, any_mask)``.
        Names route to the ``any`` bitmask; a pure-int ``event_tag`` stays an
        exact-set. Returns numeric values so worker processes need no registry."""
        any_mask = None if event_tag_any is None else self._tag_mask(event_tag_any)
        numeric = None
        if event_tag is not None:
            items = [event_tag] if isinstance(event_tag, (str, int)) else list(event_tag)
            if any(isinstance(x, str) for x in items):
                any_mask = (any_mask or 0) | self._tag_mask(items)
            else:
                numeric = [int(x) for x in items]
        return numeric, any_mask

    def _tag_mask(self, spec) -> int:
        """A tag name, an int bitmask, or an iterable of them → one OR'd bitmask.
        Names are looked up in :attr:`tag_names`; a raw int is used as-is."""
        items = [spec] if isinstance(spec, (str, int)) else list(spec)
        mask = 0
        registry = None
        for x in items:
            if isinstance(x, bool):
                raise TypeError("tag flag must be a name or int, not bool")
            if isinstance(x, str):
                if registry is None:
                    registry = self.tag_names
                if x not in registry:
                    raise KeyError(
                        f"unknown tag name {x!r}; file registry has {sorted(registry)}"
                    )
                mask |= 1 << registry[x]
            else:
                mask |= int(x)
        return mask

    def skim(
        self,
        dst: StrPath,
        compression: str = "lz4percolumn",
        *,
        tags: "np.ndarray | Sequence[int] | None" = None,
        tag_names: dict[str, int] | None = None,
    ) -> SkimSummary:
        """Copy the (filtered) chain to ``dst``, re-compressing. Returns a
        :class:`SkimSummary` (``events`` / ``records`` / ``bytes``).

        Pass ``tags`` — a ``uint32`` array aligned 1:1 with the events this chain
        yields (same order and length as :meth:`event_tags` / :meth:`arrays`) —
        to **retag** each event as it is written, producing a tagged DST.
        ``tag_names={"dvcs": 0, ...}`` records the output's tag registry so the
        result is self-describing. Together they close the
        select→label→write→reread loop::

            f = ox.open("run.hipo").filtered(require=["REC::Particle"])
            p = f.arrays("REC::Particle", ["px"])
            tags = np.where(p.px[:, 0] > 2, 1, 0).astype(np.uint32)  # one per event
            f.skim("dvcs.hipo", tags=tags, tag_names={"dvcs": 0})
            ox.open("dvcs.hipo").filtered(event_tag="dvcs")          # rereads by name

        A ``tags`` length that doesn't match the events written raises
        ``ValueError``."""
        if tags is None:
            d = self._reader().skim(str(dst), compression)
        else:
            import numpy as np

            t = np.ascontiguousarray(tags, dtype=np.uint32)
            names = list(tag_names.items()) if tag_names else None
            d = self._reader().skim(str(dst), compression, t, names)
        return SkimSummary(d["events"], d["records"], d["bytes"])

    def set_event_tag(self, entry: int, tag: int) -> None:
        """Overwrite one event's tag (``EH_TAG``) **in place** on disk, without
        rewriting the file — a single 4-byte write. Requires write permission.

        Only uncompressed files (written with ``compression="none"``) can be
        patched: for a compressed file the tag lives inside a compressed block,
        so this raises ``ValueError`` (rewrite with ``skim(tags=…)`` instead).
        An out-of-range ``entry`` raises ``IndexError``. The new tag is visible
        to a fresh :func:`open` (and to ``event_tags()``) immediately."""
        self._reader().set_event_tag(entry, tag)

    def set_event_tags(self, updates: "Mapping[int, int] | Iterable[tuple[int, int]]") -> int:
        """Batch :meth:`set_event_tag`. ``updates`` is a ``{entry: tag}`` mapping
        or an iterable of ``(entry, tag)`` pairs. Every update is validated (index
        in range, record uncompressed) before any write — all-or-nothing — so a
        bad update leaves the file unchanged. Returns the number patched."""
        if isinstance(updates, Mapping):
            pairs = [(int(k), int(v)) for k, v in updates.items()]
        else:
            pairs = [(int(e), int(t)) for e, t in updates]
        return self._reader().set_event_tags(pairs)

    def __getitem__(self, key: str | tuple[str, str | Sequence[str]]):
        """Index into the chain:

        - ``chain["REC::Particle"]`` → a bank proxy;
        - ``chain["REC::Particle/px"]`` or ``chain["REC::Particle", "px"]`` → one column;
        - ``chain["REC::Particle", ["px", "py"]]`` → a record of those columns.
        """
        if isinstance(key, tuple):
            if len(key) != 2:
                raise KeyError(key)
            bank, cols = key
            if isinstance(cols, str):
                return self.array(bank, cols)
            return self.arrays(bank, list(cols))
        if "/" in key:
            bank, column = key.rsplit("/", 1)
            return self.array(bank, column)
        if key in self._reader():
            return _BankProxy(self, key)
        raise KeyError(key)


def open(source: Source) -> Chain:  # noqa: A001  (uproot-style: oxihipo.open(...))
    """Open a HIPO file, directory, glob, or list of paths → :class:`Chain`."""
    return Chain(source)


@overload
def iterate(
    source: Source, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
    *, step_size: int | str = ..., filter_name: str | None = ..., library: _Library = ...,
    report: Literal[False] = ..., entry_start: int | None = ..., entry_stop: int | None = ...,
    threads: int = ..., workers: int = ...,
) -> "Iterator[Any]": ...
@overload
def iterate(
    source: Source, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
    *, step_size: int | str = ..., filter_name: str | None = ..., library: _Library = ...,
    report: Literal[True], entry_start: int | None = ..., entry_stop: int | None = ...,
    threads: int = ..., workers: int = ...,
) -> "Iterator[tuple[Any, Report]]": ...
def iterate(
    source: Source,
    banks: str | Sequence[str] | None = None,
    columns: Sequence[str] | None = None,
    *,
    step_size: int | str = 100_000,
    filter_name: str | None = None,
    library: _Library = "ak",
    report: bool = False,
    entry_start: int | None = None,
    entry_stop: int | None = None,
    threads: int = 0,
    workers: int = 1,
):
    """Stream chunks from a file/dir/glob/list without materializing it whole.

    Equivalent to ``open(source).iterate(...)`` — a generator, so a multi-file
    chain never opens more than it needs at once. Pass ``workers=N`` to stream
    the batches across ``N`` processes. (The keyword surface is spelled out here
    — not hidden behind ``**kwargs`` — so ``help()`` and IDEs see it.)
    """
    # Widen to Any so forwarding the union `library` / plain-`bool` `report`
    # doesn't re-run overload resolution on Chain.iterate — the module-level
    # overloads above already give callers the precise return type.
    chain: Any = open(source)
    return chain.iterate(
        banks, columns, step_size=step_size, filter_name=filter_name,
        library=library, report=report, entry_start=entry_start,
        entry_stop=entry_stop, threads=threads, workers=workers,
    )


@overload
def arrays(
    source: Source, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
    *, filter_name: str | None = ..., library: Literal["ak"] = ...,
    entry_start: int | None = ..., entry_stop: int | None = ...,
    threads: int = ..., workers: int = ...,
) -> "ak.Array": ...
@overload
def arrays(
    source: Source, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
    *, filter_name: str | None = ..., library: Literal["np"],
    entry_start: int | None = ..., entry_stop: int | None = ...,
    threads: int = ..., workers: int = ...,
) -> "dict[str, np.ndarray]": ...
@overload
def arrays(
    source: Source, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
    *, filter_name: str | None = ..., library: Literal["pd"],
    entry_start: int | None = ..., entry_stop: int | None = ...,
    threads: int = ..., workers: int = ...,
) -> "pd.DataFrame | dict[str, pd.DataFrame]": ...
@overload
def arrays(
    source: Source, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
    *, filter_name: str | None = ..., library: Literal["arrow"],
    entry_start: int | None = ..., entry_stop: int | None = ...,
    threads: int = ..., workers: int = ...,
) -> "pa.Table": ...
def arrays(
    source: Source,
    banks: str | Sequence[str] | None = None,
    columns: Sequence[str] | None = None,
    *,
    filter_name: str | None = None,
    library: _Library = "ak",
    entry_start: int | None = None,
    entry_stop: int | None = None,
    threads: int = 0,
    workers: int = 1,
):
    """Read banks/columns from a file/dir/glob/list into one array.

    Equivalent to ``open(source).arrays(...)``; pass ``workers=N`` to read with
    ``N`` processes (disjoint record ranges, stitched into one result) — the
    fast path on a parallel filesystem.
    """
    chain: Any = open(source)  # widen: see the note in `iterate` above
    return chain.arrays(
        banks, columns, filter_name=filter_name, library=library,
        entry_start=entry_start, entry_stop=entry_stop, threads=threads,
        workers=workers,
    )


# --------------------------------------------------------------------------
# Writing (columnar, uproot-shaped)
# --------------------------------------------------------------------------
# HIPO type char → NumPy dtype (what the columns are cast to before the write).
_NP_DTYPE = {"B": "int8", "S": "int16", "I": "int32", "L": "int64", "F": "float32", "D": "float64"}


def _flatten_column(col):
    """One column → ``(offsets | None, flat 1-D ndarray)``. ``None`` offsets
    means one row per event (a scalar-per-event column)."""
    import numpy as np

    if hasattr(col, "layout"):  # an awkward array
        import awkward as ak

        if col.ndim == 1:  # flat ak → scalar per event
            return None, ak.to_numpy(col)
        counts = ak.to_numpy(ak.num(col, axis=1))
        flat = ak.to_numpy(ak.flatten(col, axis=1))
        offsets = np.concatenate(([0], np.cumsum(counts))).astype(np.int64)
        return offsets, flat

    arr = np.asarray(col)
    if arr.ndim == 1:  # 1-D NumPy → scalar per event
        return None, arr
    if arr.ndim == 2:  # (n_events, rows) rectangular → constant per-event counts
        n, r = arr.shape
        return (np.arange(n + 1, dtype=np.int64) * r), np.ascontiguousarray(arr).reshape(-1)
    raise TypeError(f"unsupported column shape {arr.shape!r} (need 1-D, 2-D, or a jagged ak.Array)")


def _to_columnar(bank, bdata, schema):
    """One bank's ``extend`` data → ``(offsets int64, [(col, values ndarray)])``.
    Accepts an ``ak.Array`` record (as ``arrays(bank)`` returns) or a dict of columns."""
    import numpy as np

    if hasattr(bdata, "fields") and bdata.fields:  # ak record array
        cols_in = {f: bdata[f] for f in bdata.fields}
    elif hasattr(bdata, "items"):
        cols_in = dict(bdata)
    else:
        raise TypeError(
            f"bank {bank!r}: extend data must be an ak.Array record or a dict of columns"
        )

    offsets = None
    out_cols = []
    for name, col in cols_in.items():
        off, flat = _flatten_column(col)
        if off is None:
            off = np.arange(len(flat) + 1, dtype=np.int64)
        if offsets is None:
            offsets = off
        elif not np.array_equal(offsets, off):
            raise ValueError(
                f"bank {bank!r}: column {name!r} has different per-event row counts than the others"
            )
        out_cols.append((name, np.ascontiguousarray(flat, dtype=_NP_DTYPE.get(schema.get(name)))))
    if offsets is None:
        offsets = np.zeros(1, dtype=np.int64)
    return np.ascontiguousarray(offsets, dtype=np.int64), out_cols


class Writer:
    """A columnar HIPO writer — uproot-shaped (:meth:`newtree` / :meth:`extend`).

    Build a fresh file with :func:`create`, or *decorate* an existing one (copy
    its events, attaching new banks) with :func:`recreate`. Declare each new
    bank with :meth:`newtree`, feed columnar batches (NumPy or Awkward) with
    :meth:`extend`, then :meth:`close` — or use it as a context manager.

    Array columns (``T#N``) are not yet supported by the writer; existing array
    columns of a decorated file are copied through verbatim.
    """

    _w: "_RustWriter | None"

    def __init__(self, path, compression="lz4percolumn", source=None, _inplace=None):
        self._w = _RustWriter(str(path), compression, None if source is None else str(source))
        self._schemas: dict[str, dict[str, str]] = {}
        self._inplace = _inplace  # (final_path, temp_path) for recreate(dst=None)
        self._summary: SkimSummary | None = None

    def _writer(self) -> "_RustWriter":
        if self._w is None:
            raise ValueError("operation on a closed Writer")
        return self._w

    def newtree(
        self,
        bank: str,
        columns: "dict[str, str] | Sequence[tuple[str, str]]",
        group: int = 1,
        item: int | None = None,
    ) -> None:
        """Declare a new bank. ``columns`` maps ``name → typechar`` (or a list of
        ``(name, typechar)``); typechar ∈ ``B/S/I/L/F/D``. ``item`` (the unique
        bank id) auto-assigns when omitted. Mirrors uproot's ``newtree``."""
        cols: list[tuple[str, str]] = (
            list(columns.items())
            if isinstance(columns, dict)
            else [(c[0], c[1]) for c in columns]
        )
        self._writer().add_schema(bank, cols, group, item)
        self._schemas[bank] = dict(cols)

    def extend(self, data: "dict[str, Any]") -> None:
        """Append a batch of events. ``data`` is ``{bank: array}`` where each
        value is an ``ak.Array`` record (as :meth:`Chain.arrays` returns) or a
        dict of columns — a jagged ``ak.Array`` per column, or a 1-D NumPy array
        for a scalar-per-event bank. Every bank in one call must span the same
        number of events. Mirrors uproot's ``extend``."""
        banks = []
        for bank, bdata in data.items():
            offsets, cols = _to_columnar(bank, bdata, self._schemas.get(bank, {}))
            banks.append((bank, offsets, cols))
        self._writer().extend(banks)

    def close(self) -> SkimSummary:
        """Finish the file (write the trailer index). Returns a
        :class:`SkimSummary` (``events`` / ``records`` / ``bytes``). Idempotent;
        ``with`` closes for you."""
        if self._summary is None:
            d = self._writer().close()
            self._summary = SkimSummary(d["events"], d["records"], d["bytes"])
            if self._inplace is not None:
                final, temp = self._inplace
                self._w = None  # release the source read handles before replacing
                os.replace(temp, final)
        return self._summary

    write = close  # uproot / hipopy alias

    def __enter__(self) -> "Writer":
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    def __repr__(self) -> str:
        return f"<oxihipo.Writer: {'closed' if self._summary else 'open'}>"


def create(path: StrPath, compression: str = "lz4percolumn") -> Writer:
    """Open a new HIPO file for writing (overwrites). Declare banks with
    :meth:`Writer.newtree`, feed batches with :meth:`Writer.extend`, then
    :meth:`Writer.close`. Compression is one of ``none`` / ``lz4`` / ``lz4best``
    / ``gzip`` / ``lz4perbank`` / ``lz4percolumn``."""
    return Writer(path, compression=compression)


def recreate(
    source: StrPath, dst: StrPath | None = None, compression: str = "lz4percolumn"
) -> Writer:
    """Decorate an existing file: copy every event of ``source`` and attach the
    new banks you :meth:`~Writer.newtree` + :meth:`~Writer.extend` (which must
    align 1:1 with the source events). Existing banks are copied through
    verbatim. Writes to ``dst``; if ``dst`` is ``None``, replaces ``source`` in
    place via a temp file."""
    if dst is None:
        temp = str(source) + ".oxitmp"
        return Writer(temp, compression=compression, source=source, _inplace=(str(source), temp))
    return Writer(dst, compression=compression, source=source)
