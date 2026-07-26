"""oxihipo — fast, columnar reading of HIPO (CLAS12) files, powered by Rust.

The compiled ``_oxihipo`` extension does the heavy lifting: one Rust pass over
the file materializes each requested column into a flat NumPy buffer plus a
shared ``int64`` offsets buffer, with the GIL released. This module layers the
uproot-shaped ergonomics on top — ``array`` / ``arrays`` return Awkward arrays
built *zero-copy* from those buffers; ``numpy`` returns the raw buffers so the
NumPy-only path needs no Awkward import.
"""

from __future__ import annotations

import ast
import fnmatch
import operator
import os
import re
import warnings
from collections.abc import Callable, Iterable, Iterator, Mapping, Sequence
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any, Literal, NamedTuple, cast, overload

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
    "update",
    "iterate",
    "numpy",
    "event_tags",
    "composite",
    "skim",
    "arrays",
    "rdataframe",
    "iterate_rdataframe",
    "Report",
    "NumpyColumn",
    "SkimSummary",
    "CorruptFileError",
    "OxihipoError",
    "FileHeader",
    "PDG_MASS_GEV",
    "PDG_NAME",
    "pdg_mass",
    "pdg_name",
    "to_vector",
    "group_by_index",
    "__version__",
]

# Masses live in their own module: a pure NumPy lookup table with no reason to
# know about chains, and re-exported here because `ox.pdg_mass(p.pid)` is how it
# is used.
from ._pdg import PDG_MASS_GEV, PDG_NAME, pdg_mass, pdg_name  # noqa: E402
from ._vector import to_vector  # noqa: E402
from ._link import group_by_index  # noqa: E402

# Library name → return type, for the `library=` annotations below.
_Library = Literal["ak", "np", "pd", "arrow"]


class NumpyColumn(NamedTuple):
    """Return of :meth:`Chain.numpy` — a flat column plus its jagged layout.

    Unpacks positionally like the old 3-tuple (``values, offsets, inner_len``)
    but self-documents and disambiguates from ``read_columns``' element order."""

    values: "np.ndarray"
    offsets: "np.ndarray"  # int64, length = n_events + 1
    inner_len: int  # > 1 for fixed-size ``T#N`` array columns


class FileHeader(NamedTuple):
    """Return of :attr:`Chain.file_header` — the HIPO **file** header of the
    chain's first file (multi-file chains share one dictionary, so that header
    is the canonical one).

    This is container metadata, not physics. CLAS12 keeps the run number in the
    ``RUN::config`` bank, not here — ``user_int1`` / ``user_int2`` /
    ``user_register`` are free fields whose meaning is up to whoever wrote the
    file, and are commonly zero.

    **Several fields are unreliable across writers**, because the reference
    writers do not set them. Measured on real files:

    * ``record_count`` is **0** from every writer checked, including this one.
      Use :attr:`Chain.record_count`, which counts the record index.
    * ``has_dictionary`` / ``has_trailer_index`` are ``False`` on files written
      by the C++ and Java writers even when a dictionary is present — as it is
      on every CLAS12 file. Do not branch on them.

    ``version``, ``endianness`` and ``file_number`` are dependable."""

    version: int
    file_number: int
    record_count: int
    endianness: str  # 'little' or 'big'
    has_dictionary: bool
    has_trailer_index: bool
    user_register: int
    user_int1: int
    user_int2: int
    bit_info: int


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




def _merge_require(parent, child):
    """Union: chaining `require` means every named bank must be present."""
    if parent is None:
        return child
    if child is None:
        return parent
    merged = list(parent) + [b for b in child if b not in parent]
    return merged


def _intersect_tags(parent, child, what):
    """Intersection: chaining narrows. An empty result can never match, which is
    a real (if useless) filter — but far more often a mistake, so say so."""
    if parent is None:
        return child
    if child is None:
        return parent
    keep = [t for t in child if t in set(parent)]
    if not keep:
        raise ValueError(
            f"chaining these `{what}` filters selects nothing: {list(parent)} "
            f"then {list(child)} have no value in common"
        )
    return keep


def _cut_identifiers(cut: str) -> set[str]:
    """Bare names a cut expression reads. Used to widen the read so a cut can
    reference a column the caller did not ask to get back."""
    try:
        tree = ast.parse(cut, mode="eval")
    except SyntaxError as e:
        raise ValueError(f"cut {cut!r} is not a valid Python expression: {e}") from e
    return {n.id for n in ast.walk(tree) if isinstance(n, ast.Name)}


def _cut_mask_to_rows(ak, np, mask, offsets):
    """Normalise an evaluated cut to ``(row_keep, kept_counts)``.

    Accepts the two shapes a cut can naturally produce, which is what makes one
    keyword serve both jobs:

    * **jagged** ``n_events * var * bool`` — a per-row predicate like
      ``pid == 11``: keeps matching rows, every event survives (possibly empty).
    * **flat** ``n_events * bool`` — a per-event predicate like
      ``ak.any(pid == 11, axis=1)``: keeps whole events, drops the rest.

    Returned as a flat row mask plus the surviving row count per event, so the
    caller can filter the flat column buffers without an awkward round-trip and
    without disturbing ``inner_len``.
    """
    counts = np.diff(offsets)
    ndim = mask.ndim if hasattr(mask, "ndim") else 1
    if ndim >= 2:
        if len(mask) != len(counts):
            raise ValueError(
                f"cut produced {len(mask)} entries for {len(counts)} events"
            )
        flat = np.asarray(ak.flatten(mask))
        if flat.dtype != bool:
            raise ValueError(f"cut must evaluate to booleans, got {flat.dtype}")
        if flat.size != int(offsets[-1]):
            raise ValueError(
                f"cut produced {flat.size} row flags for {int(offsets[-1])} rows"
            )
        return flat, np.asarray(ak.sum(mask, axis=1)).astype(np.int64)
    keep = np.asarray(mask)
    if keep.dtype != bool:
        raise ValueError(f"cut must evaluate to booleans, got {keep.dtype}")
    if keep.size != len(counts):
        raise ValueError(
            f"cut produced {keep.size} event flags for {len(counts)} events"
        )
    # Whole-event selection: a dropped event contributes no rows *and* no entry.
    return np.repeat(keep, counts), counts[keep].astype(np.int64)


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


# --------------------------------------------------------------------------
# ROOT RDataFrame bridge (lazy `import awkward`; ROOT is pulled in by awkward's
# `to_rdataframe`, only when the helper is actually called)
# --------------------------------------------------------------------------
_RDF_IDENT_RE = re.compile(r"\W+")  # runs of non-[A-Za-z0-9_] → one '_'


def _rdf_name(source_key: str) -> str:
    """A ``bank/column`` key → a valid C++ identifier for RDataFrame.

    RDataFrame JIT-compiles column names, so ``::`` and ``/`` (and any other
    non-word char) collapse to ``_`` — ``REC::Particle/px`` → ``REC_Particle_px``
    — and a leading digit gets an ``_`` prefix."""
    name = _RDF_IDENT_RE.sub("_", source_key)
    if not name or name[0].isdigit():
        name = "_" + name
    return name


def _rdf_columns(rec, selection, single):
    """An ``ak.Array`` from :meth:`Chain.arrays` → ``{cpp_name: ak.Array}`` for
    ``ak.to_rdataframe``. Keys are ``bank/column`` sanitized to C++ identifiers;
    two source columns mapping to one name is a hard error, not a silent drop."""
    cols: "dict[str, Any]" = {}

    def add(source_key: str, array) -> None:
        name = _rdf_name(source_key)
        if name in cols:
            raise ValueError(
                f"RDataFrame column-name collision: {source_key!r} and another "
                f"source column both map to {name!r} — rename a bank or column"
            )
        cols[name] = array

    if single:  # one bank → arrays() dropped the bank level; re-attach its name
        bank = selection[0][0]
        for field in rec.fields:
            add(f"{bank}/{field}", rec[field])
    else:  # a record with one sub-record per bank
        for bank in rec.fields:
            sub = rec[bank]
            for field in sub.fields:
                add(f"{bank}/{field}", sub[field])
    return cols


def _to_rdataframe(cols):
    """``{name: ak.Array}`` → ``ROOT.RDataFrame`` through Awkward's generated
    ``RDataSource``, with a friendly error when ROOT/awkward isn't importable."""
    if not cols:
        raise ValueError(
            "no columns selected for RDataFrame (the bank/filter matched nothing)"
        )
    try:
        import awkward as ak
    except ModuleNotFoundError as e:  # awkward itself missing
        raise ModuleNotFoundError(
            "oxihipo.rdataframe needs awkward (with ROOT interop) and a working "
            "ROOT — `pip install oxihipo[root]` plus an importable ROOT"
        ) from e
    try:
        return ak.to_rdataframe(cols)
    except ModuleNotFoundError as e:  # awkward is here but `import ROOT` failed
        raise ModuleNotFoundError(
            "oxihipo.rdataframe needs ROOT (PyROOT) importable in this "
            "interpreter. Install ROOT so `import ROOT` works (e.g. "
            "`conda install -c conda-forge root`), then `pip install oxihipo[root]`"
        ) from e


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

    def __exit__(self, exc_type, exc, tb) -> None:
        # Only finalise if the block succeeded. Closing on the way out of a
        # failed block produced a complete-looking file from a failed run — and
        # for the in-place `recreate(dst=None)` it ran `os.replace(temp, final)`,
        # overwriting the user's source with a partial result. `close()` can also
        # raise (e.g. "decorate covered 0 of 8 events"), which then replaced the
        # user's real exception with a confusing one.
        if exc_type is None:
            self.close()
        else:
            self._abort()

    def _abort(self) -> None:
        """Discard the partial output without finalising it.

        Never raises: it runs while another exception is propagating, and
        masking that is exactly the bug this avoids.
        """
        if self._summary is not None:
            return  # already finished successfully
        self._w = None  # drop the writer (and the source handles) unfinalised
        # A HIPO file with no trailer is unreadable, so leaving it behind is
        # litter that looks like output.
        victim = self._inplace[1] if self._inplace is not None else self._path
        try:
            os.remove(victim)
        except OSError:
            pass

    def keys(self, recursive: bool = False, filter_name: "str | Sequence[str] | None" = None) -> list[str]:
        """Bank names, or ``bank/column`` keys (``recursive=True``); optionally
        keep only those matching ``filter_name`` — one glob, or a sequence of
        them taken as a union."""
        out = self._reader().keys(recursive)
        if filter_name is not None:
            globs = [filter_name] if isinstance(filter_name, str) else list(filter_name)
            out = [k for k in out if any(fnmatch.fnmatch(k, g) for g in globs)]
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
        # `filter_name` replaces the bank selection outright, so passing both
        # silently discarded `banks` — `arrays("REC::Particle",
        # filter_name="REC::Event*")` returned REC::Event. Refuse instead.
        if filter_name is not None and banks is not None:
            raise TypeError(
                "pass either `banks=` or `filter_name=`, not both — `filter_name` "
                "selects across every bank and would ignore `banks`"
            )
        if filter_name is not None:
            # A sequence of globs is a union — uproot accepts one or many, and
            # needing two calls to ask for "REC::* plus RUN::config" was an
            # arbitrary limit.
            globs = [filter_name] if isinstance(filter_name, str) else list(filter_name)
            grouped: dict[str, list[str]] = {}
            for key in self._reader().keys(True):
                bank = key.split("/", 1)[0]
                if any(fnmatch.fnmatch(key, g) or fnmatch.fnmatch(bank, g) for g in globs):
                    grouped.setdefault(bank, [])
                    if "/" in key and any(fnmatch.fnmatch(key, g) for g in globs):
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
        *, filter_name: "str | Sequence[str] | None" = ..., library: Literal["ak"] = ...,
        entries: "Sequence[int] | None" = ..., cut: str | None = ...,
        entry_start: int | None = ..., entry_stop: int | None = ...,
        threads: int = ..., workers: int = ...,
    ) -> "ak.Array": ...
    @overload
    def arrays(
        self, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
        *, filter_name: "str | Sequence[str] | None" = ..., library: Literal["np"],
        entries: "Sequence[int] | None" = ..., cut: str | None = ...,
        entry_start: int | None = ..., entry_stop: int | None = ...,
        threads: int = ..., workers: int = ...,
    ) -> "dict[str, np.ndarray]": ...
    @overload
    def arrays(
        self, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
        *, filter_name: "str | Sequence[str] | None" = ..., library: Literal["pd"],
        entries: "Sequence[int] | None" = ..., cut: str | None = ...,
        entry_start: int | None = ..., entry_stop: int | None = ...,
        threads: int = ..., workers: int = ...,
    ) -> "pd.DataFrame | dict[str, pd.DataFrame]": ...
    @overload
    def arrays(
        self, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
        *, filter_name: "str | Sequence[str] | None" = ..., library: Literal["arrow"],
        entries: "Sequence[int] | None" = ..., cut: str | None = ...,
        entry_start: int | None = ..., entry_stop: int | None = ...,
        threads: int = ..., workers: int = ...,
    ) -> "pa.Table": ...
    def arrays(
        self,
        banks: str | Sequence[str] | None = None,
        columns: Sequence[str] | None = None,
        *,
        filter_name: "str | Sequence[str] | None" = None,
        library: _Library = "ak",
        entry_start: int | None = None,
        entry_stop: int | None = None,
        entries: "Sequence[int] | None" = None,
        cut: str | None = None,
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
        - ``entries``: read exactly these global event indices instead of a
          contiguous range — the columnar way to replay a list of interesting
          events found by an earlier pass. Output is aligned 1:1 with the list,
          so your order and any duplicates are preserved, and an out-of-range
          index yields an empty entry rather than an error. The order of the
          list does not affect cost — entries are grouped by the record holding
          them, so each record is read once — and ``threads`` applies.
          Mutually exclusive with ``entry_start``/``entry_stop``; ``workers``
          does not apply.
        - ``cut``: a Python expression filtering the result, with the bank's
          columns bound to jagged ``ak.Array`` values. One keyword covers both
          granularities, chosen by what the expression evaluates to:

          * per-row — ``cut="pid == 11"`` keeps matching **rows**; every event
            survives, some possibly empty.
          * per-event — ``cut="ak.any(pid == 11, axis=1)"`` keeps whole
            **events** and drops the rest.

          A column named only in the cut is read for it and then dropped, so
          ``arrays("REC::Particle", ["px"], cut="pid == 11")`` returns just
          ``px``. Needs exactly one bank. ``ak``, ``np`` and ``abs`` are in
          scope. **The cut is evaluated as code** — build it from your own
          source, never from untrusted input.
        - ``threads``: rayon threads *within* the read (``0`` = all cores).
        - ``workers``: read with ``N`` **processes** (disjoint record ranges,
          stitched into one result). On a parallel filesystem (ifarm) this is
          the way to beat the per-process I/O ceiling. Without an explicit
          ``threads``, the cores are split across workers (total ≈ all cores);
          on the farm the extra decode threads simply wait on I/O.
        """
        c = self._reader()
        selection, single = self._resolve(banks, columns, filter_name)
        keep_columns: set[str] | None = None
        if cut is not None:
            selection, keep_columns = self._cut_plan(selection, cut)

        def finish(res):
            if cut is not None:
                res = self._apply_cut(res, cut, keep_columns)
            return self._assemble(res, single, library)

        if entries is not None:
            if entry_start is not None or entry_stop is not None:
                raise ValueError(
                    "pass either `entries` or `entry_start`/`entry_stop`, not both"
                )
            idx = [int(i) for i in entries]
            if any(i < 0 for i in idx):
                raise ValueError("`entries` indices must be non-negative")
            # `entries=` addresses the *file's* event stream and does not consult
            # the chain filter, so on a filtered chain it silently returned
            # events the filter excludes. Refuse rather than answer wrongly.
            #
            # Note the index space itself is not the problem: `entry_start` /
            # `entry_stop` are pre-filter global indices too (the range test in
            # the core runs on the raw global index, before the filter, and
            # `num_entries` likewise ignores the filter). What differs is the
            # *output*: a range read compacts filtered-out events away, while
            # `entries=` stays 1:1 with the request. Making them agree means
            # deciding what a non-surviving index should yield here, which is a
            # bigger change than a guard.
            if any(
                x is not None
                for x in (self._require, self._record_tag, self._event_tag, self._event_tag_any)
            ):
                raise ValueError(
                    "`entries=` is not supported on a filtered chain: the indices "
                    "would address the unfiltered event stream, so the filter "
                    "would be ignored. Read with entry_start/entry_stop instead, "
                    "or take the entries from the unfiltered chain."
                )
            return finish(c.read_columns_at(selection, idx, threads))
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
                    finish,
                )
        return finish(c.read_columns(selection, entry_start, entry_stop, threads))

    def to_dask(
        self,
        banks: "str | Sequence[str] | None" = None,
        columns: "Sequence[str] | None" = None,
        *,
        step_size: "int | str" = 100_000,
        filter_name: "str | Sequence[str] | None" = None,
        cut: str | None = None,
        entry_start: int | None = None,
        entry_stop: int | None = None,
        threads: int = 1,
    ) -> "Any":
        """A lazy `dask-awkward <https://dask-awkward.readthedocs.io>`_ array
        over the chain — the counterpart to ``uproot.dask``.

        One partition per ``step_size`` batch, aligned to record and file
        boundaries exactly as :meth:`iterate` is, so nothing is read until you
        ``.compute()`` — not even to discover the layout::

            import dask_awkward as dak
            p = f.to_dask("REC::Particle")
            dak.sum(p.px).compute()      # reads px, not the whole bank

        Columns are projected: dask-awkward works out which ones the graph
        actually touches and each partition reads only those, across banks as
        well (``dak.report_necessary_columns`` reports what it settled on).
        Entry boundaries are known too, so ``len()``, ``.partitions[i]`` and
        entry slices work without reading anything — except under ``cut=``,
        which may drop events and so forfeits them.

        Needs ``dask-awkward``, which is **not** a dependency of this package
        (``pip install 'oxihipo[dask]'``).

        ``threads`` defaults to ``1``, not all cores: dask is already running
        partitions concurrently, so letting each one also fan out over rayon
        oversubscribes the machine. Raise it only for a single-partition read.

        Prefer :meth:`iterate` or ``workers=`` for a single-machine scan — they
        are simpler and have no extra dependency. This is for when you already
        have a dask cluster, or want to compose HIPO into a larger dask graph.
        """
        try:
            import dask_awkward as dak
        except ImportError as e:  # pragma: no cover - depends on environment
            raise ImportError(
                "to_dask() needs dask-awkward: pip install 'oxihipo[dask]'"
            ) from e
        from . import _dask

        c = self._reader()
        mode, size = _parse_step_size(step_size)
        spans = self._record_spans()
        sizes = self._record_decompressed_sizes() if mode == "bytes" else None
        total = c.num_entries
        lo = 0 if entry_start is None else max(0, entry_start)
        hi = total if entry_stop is None else min(total, entry_stop)
        # Drop zero-width batches: `_iter_batches` emits one when the requested
        # range collapses inside a record (entry_start == entry_stop), and a
        # partition spanning no events would give dask two equal divisions,
        # which are required to increase.
        batches = [
            (start, stop)
            for start, stop, _fi in self._iter_batches(spans, sizes, mode, size, lo, hi)
            if stop > start
        ]
        if not batches:
            raise ValueError("nothing to read: the requested range is empty")

        spec = (
            self._source, self._require, self._record_tag,
            self._event_tag, self._event_tag_any,
        )
        # One bag, splatted into both the meta read below and every partition
        # read — which is what keeps the advertised layout and the delivered one
        # from drifting. `Any` because it is a passthrough: mypy cannot pick an
        # `arrays` overload from a heterogeneous dict, and narrowing it here
        # would only mean spelling the same kwargs out twice.
        kw: dict[str, Any] = {"filter_name": filter_name, "cut": cut,
                              "library": "ak", "threads": threads}

        # The form, with no I/O beyond the already-open chain: a zero-event read
        # touches no record but returns the exact layout. Supplying it is what
        # stops dask-awkward reading partition 0 during graph construction just
        # to discover the type — the graph was advertised as lazy and was not.
        import awkward as ak

        meta = self.arrays(banks, columns, entry_start=0, entry_stop=0, **kw)
        form = meta.layout.form

        # Divisions are the entry boundaries we already computed. Without them
        # `len()` raises, `.partitions[i]` cannot be pruned, and an entry slice
        # scans everything; the documented workaround re-reads the whole chain
        # to recover numbers `num_entries` already knows.
        #
        # Only sound when nothing drops events: a per-EVENT cut does, and we
        # cannot tell which kind a cut is without running it, so any cut forfeits
        # them rather than reporting boundaries that later prove wrong.
        divisions = (
            None
            if cut is not None
            else tuple(s for s, _ in batches) + (batches[-1][1],)
        )

        return dak.from_map(
            _dask.HipoRead(spec, form, banks, columns, kw),
            batches,
            meta=ak.Array(form.length_zero_array(highlevel=False).to_typetracer(forget_length=True)),
            divisions=divisions,
            label="from-hipo",
        )

    def map_reduce(
        self,
        fn: "Callable[[Any], Any]",
        banks: "str | Sequence[str] | None" = None,
        columns: "Sequence[str] | None" = None,
        *,
        step_size: "int | str" = "1 GB",
        workers: int = 0,
        reduce: "Callable[[Any, Any], Any]" = operator.add,
        initial: "Any | None" = None,
        filter_name: "str | Sequence[str] | None" = None,
        cut: str | None = None,
        entry_start: int | None = None,
        entry_stop: int | None = None,
        threads: int = 1,
    ) -> "Any":
        """Run `fn` on each chunk **inside the worker**, and combine the results.

        ``workers=`` on :meth:`arrays` and :meth:`iterate` parallelises the
        *read* — the workers hand raw buffers back and the parent does the
        physics, serially. For a CLAS12 selection the Awkward expressions
        dominate, not the decode, so that is an I/O trick rather than
        parallelism. Here `fn` runs where the chunk already is, and only what it
        returns is sent back::

            import hist

            def analyze(chunk):
                h = hist.Hist(hist.axis.Regular(100, 0, 10, name="Q2"))
                h.fill(q2_of(chunk))
                return h

            h = ox.open("/volatile/rga/*.hipo").map_reduce(
                analyze, "REC::Particle", workers=8
            )

        A filled histogram pickles to a few hundred bytes where the chunk behind
        it is hundreds of megabytes, which is what makes this worth doing.

        `reduce` defaults to ``operator.add`` — what ``hist.Hist``,
        ``boost_histogram``, ``np.ndarray`` and a plain number all implement.
        Give `initial` to start from a fixed accumulator (and to define the
        answer when the selection is empty). Results are folded in **event
        order**, so a non-commutative `reduce` still gets them the right way
        round.

        `fn` is sent to another process, so it has to be picklable: a
        module-level function, not a lambda or a closure. ``workers=1`` runs
        everything in this process instead, which is the way to debug one.

        The read arguments mirror :meth:`iterate` — `banks`, `columns`,
        `filter_name`, `cut`, `entry_start`/`entry_stop`, `step_size`. `threads`
        defaults to ``1`` because the workers are already the parallelism.
        """
        c = self._reader()
        mode, size = _parse_step_size(step_size)
        spans = self._record_spans()
        sizes = self._record_decompressed_sizes() if mode == "bytes" else None
        total = c.num_entries
        lo = 0 if entry_start is None else max(0, entry_start)
        hi = total if entry_stop is None else min(total, entry_stop)
        # Zero-width batches are dropped, so an empty range behaves the same
        # whether it collapsed inside a record or fell past the end — otherwise
        # `fn` would be handed an empty chunk in one case and not the other.
        batches = [
            (start, stop)
            for start, stop, _fi in self._iter_batches(spans, sizes, mode, size, lo, hi)
            if stop > start
        ]
        kw: dict[str, Any] = {
            "filter_name": filter_name,
            "cut": cut,
            "library": "ak",
            "threads": threads,
        }

        if not batches:
            # Nothing to read. `initial` is the only defensible answer, and
            # without one there is no value to invent.
            if initial is None:
                raise ValueError(
                    "nothing to read: the requested range is empty, and no "
                    "`initial=` was given to define the result"
                )
            return initial

        if workers == 1:
            acc = initial
            for start, stop in batches:
                r = fn(self.arrays(banks, columns, entry_start=start, entry_stop=stop, **kw))
                acc = r if acc is None else reduce(acc, r)
            return acc

        from . import _parallel

        spec = (
            self._source, self._require, self._record_tag,
            self._event_tag, self._event_tag_any,
        )
        n = len(batches) if workers == 0 else workers
        return _parallel.parallel_map_reduce(
            spec, fn, banks, columns, kw, batches, max(1, n), reduce, initial
        )

    def to_parquet(
        self,
        path: StrPath,
        banks: "str | Sequence[str] | None" = None,
        columns: "Sequence[str] | None" = None,
        *,
        filter_name: "str | Sequence[str] | None" = None,
        cut: str | None = None,
        step_size: "int | str | None" = None,
        compression: str = "zstd",
        entry_start: int | None = None,
        entry_stop: int | None = None,
        threads: int = 0,
    ) -> int:
        """Write the selection to a Parquet file. Returns the row-group count.

        Parquet is the handover format for the polars / duckdb / pandas side of
        an analysis, and a HIPO bank maps onto it directly: each bank column
        becomes a ``large_list`` column, one list per event, so the jagged
        structure survives the round-trip.

            f.to_parquet("electrons.parquet", "REC::Particle",
                         ["px", "py", "pz"], cut="pid == 11")

        ``step_size`` streams instead of materialising: the file is written in
        row groups of that many events (or that many bytes, e.g. ``"200 MB"``),
        so resident memory stays about one chunk and inputs far bigger than RAM
        work. Without it the whole selection is built in memory as one row group.

        ``compression`` is passed to Parquet (``"zstd"`` default, or
        ``"snappy"`` / ``"gzip"`` / ``"lz4"`` / ``None``). ``banks`` / ``columns``
        / ``filter_name`` / ``cut`` / ``entry_start`` / ``entry_stop`` /
        ``threads`` mean what they do on :meth:`arrays`.
        """
        import pyarrow.parquet as pq

        common: dict[str, Any] = {
            "filter_name": filter_name,
            "library": "arrow",
            "entry_start": entry_start,
            "entry_stop": entry_stop,
            "threads": threads,
        }
        if step_size is None:
            table = self.arrays(banks, columns, cut=cut, **common)
            pq.write_table(table, str(path), compression=compression)
            return 1

        # Streaming: one row group per chunk. The writer needs a schema up
        # front, so it is opened from the first chunk and closed in `finally`
        # (a half-written Parquet file with no footer is unreadable).
        writer = None
        groups = 0
        try:
            for chunk in self.iterate(
                banks, columns, step_size=step_size, cut=cut, **common
            ):
                if writer is None:
                    writer = pq.ParquetWriter(
                        str(path), chunk.schema, compression=compression
                    )
                writer.write_table(chunk)
                groups += 1
        finally:
            if writer is not None:
                writer.close()
        return groups

    def _cut_plan(self, selection, cut: str):
        """Widen ``selection`` so the cut's columns are read, and report which
        columns the caller actually asked to keep. Shared by ``arrays`` and
        ``iterate`` so there is one cut implementation, not two."""
        if len(selection) != 1:
            raise ValueError(
                f"`cut` needs exactly one bank; the selection has {len(selection)}"
            )
        bank_name, requested = selection[0]
        available = self._reader().columns(bank_name)
        extra = sorted(_cut_identifiers(cut) & set(available) - set(requested))
        if requested and extra:
            return [(bank_name, list(requested) + extra)], set(requested)
        return selection, None

    def _apply_cut(self, res, cut: str, keep_columns: "set[str] | None"):
        """Filter the raw column buffers by ``cut``, then drop any column that
        was read only because the cut mentioned it.

        The expression is evaluated with each of the bank's columns bound to a
        jagged ``ak.Array``, which is what lets one keyword express both a
        per-row cut (``pid == 11``) and a per-event one
        (``ak.any(pid == 11, axis=1)``) — see :func:`_cut_mask_to_rows`.

        The mask is applied to the flat buffers with NumPy rather than by
        rebuilding through Awkward, so ``inner_len`` (array columns, ``T#N``)
        survives untouched and every ``library=`` assembler downstream is
        unchanged.
        """
        import awkward as ak
        import numpy as np

        if len(res) != 1:
            raise ValueError(
                "`cut` needs exactly one bank; got "
                f"{len(res)}. Cut per bank, or filter the result yourself."
            )
        bname, offsets, cols = res[0]
        offsets = np.asarray(offsets)
        env: dict[str, Any] = {"ak": ak, "np": np, "numpy": np, "abs": abs}
        local = {
            name: ak.Array(_bank_record(ak, offsets, [(name, values, inner)]))[name]
            for name, values, inner in cols
        }
        try:
            # Restricted namespace, not a sandbox: `cut` is code and is
            # evaluated. Do not build one from untrusted input.
            mask = eval(cut, {"__builtins__": {}, **env}, local)  # noqa: S307
        except ValueError:
            raise
        except Exception as e:
            names = ", ".join(sorted(local)) or "<none>"
            raise ValueError(
                f"could not evaluate cut {cut!r} on {bname}: "
                f"{type(e).__name__}: {e}. Available columns: {names}"
            ) from e

        row_keep, kept_counts = _cut_mask_to_rows(ak, np, mask, offsets)
        new_offsets = np.zeros(len(kept_counts) + 1, dtype=np.int64)
        np.cumsum(kept_counts, out=new_offsets[1:])

        out_cols = []
        for name, values, inner in cols:
            if keep_columns is not None and name not in keep_columns:
                continue
            if inner > 1:
                out_cols.append((name, values.reshape(-1, inner)[row_keep].reshape(-1), inner))
            else:
                out_cols.append((name, values[row_keep], inner))
        return [(bname, new_offsets, out_cols)]

    def _assemble(self, res, single, library):
        # `single` — did the caller name ONE bank as a bare string? — decides the
        # key namespace, and it must reach every backend. It used to reach only
        # `ak`, so the other three keyed off `len(res) > 1` and a list-of-one or a
        # glob matching one bank produced bare column keys under np/pd/arrow while
        # ak produced bank-namespaced ones. That made the namespace depend on the
        # *data* (how many banks happened to match) rather than the request, so a
        # loop keyed on "BANK/col" worked until it met a file with one match.
        if library == "ak":
            return self._assemble_ak(res, single)
        if library == "np":
            return self._assemble_np(res, single)
        if library == "pd":
            return self._assemble_pd(res, single)
        if library == "arrow":
            return self._assemble_arrow(res, single)
        raise ValueError(
            f"unknown library {library!r} (expected 'ak', 'np', 'pd', or 'arrow')"
        )

    def _assemble_arrow(self, res, single=False) -> "pa.Table":
        import pyarrow as pa

        # Build the pyarrow.Table straight from the returned NumPy buffers — one
        # (large-)list column per field — instead of round-tripping through
        # Awkward. `pa.array(offsets)`/`pa.array(values)` wrap the int64/numeric
        # buffers zero-copy, so the whole path stays copy-free and needs no
        # awkward import (only pyarrow).
        # Namespace by bank unless the caller named exactly one bank as a
        # bare string; mirrors `_assemble_ak`.
        multi = not (single and len(res) == 1)
        cols: dict[str, pa.Array] = {}
        for bname, offsets, bcols in res:
            off = pa.array(offsets)  # int64 → LargeList offsets, zero-copy
            for name, values, inner in bcols:
                child = pa.array(values)  # numeric, no nulls → zero-copy
                if inner > 1:  # T#N array column → fixed-size inner list
                    child = pa.FixedSizeListArray.from_arrays(child, int(inner))
                key = f"{bname}/{name}" if multi else name
                cols[key] = pa.LargeListArray.from_arrays(off, child)
        # Declare the schema non-nullable rather than letting `pa.table` infer.
        # A HIPO bank has no null concept: a row either exists or the event has
        # fewer rows, which the list offsets already express. Inferring left
        # every field `nullable=True`, so a Parquet round-trip came back as
        # `option[var * ?float32]` and downstream tools planned for nulls that
        # cannot occur. The buffers are unchanged — only the schema is stated.
        if not cols:
            return pa.table(cols)  # empty selection → a valid empty table
        schema = pa.schema(
            [
                pa.field(
                    name,
                    pa.large_list(pa.field("item", arr.type.value_type, nullable=False)),
                    nullable=False,
                )
                for name, arr in cols.items()
            ]
        )
        return pa.table(cols, schema=schema)

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

    def _assemble_np(self, res, single=False):
        import numpy as np

        # Namespace by bank unless the caller named exactly one bank as a
        # bare string; mirrors `_assemble_ak`.
        multi = not (single and len(res) == 1)
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

    def _assemble_pd(self, res, single=False):
        import awkward as ak

        frames = {}
        for bname, offsets, cols in res:
            df = ak.to_dataframe(ak.Array(_bank_record(ak, offsets, cols)))
            # An event with no rows contributes no rows, so it is absent from the
            # (entry, subentry) MultiIndex entirely: 8 events can yield 6 entry
            # levels. Positional joins against `event_tags()` or a `library="np"`
            # read are then silently misaligned.
            #
            # Deliberately NOT reindexed to the full range: that would insert a
            # NaN row per empty event, i.e. invent a particle that was not
            # measured, and make `pd` disagree with `ak`/`np` on row counts. The
            # sparse index is the truthful shape. Instead the true event count
            # travels with the frame so the mismatch is detectable.
            df.attrs["num_entries"] = len(offsets) - 1
            frames[bname] = df
        # A bare single-bank request gets the frame itself; a list or a glob gets
        # a dict keyed by bank, even when only one bank matched — so the shape
        # follows the request rather than the file's contents.
        if single and len(frames) == 1:
            return next(iter(frames.values()))
        return frames

    # --- the ROOT RDataFrame path ------------------------------------------
    def rdataframe(
        self,
        banks: str | Sequence[str] | None = None,
        columns: Sequence[str] | None = None,
        *,
        filter_name: "str | Sequence[str] | None" = None,
        entry_start: int | None = None,
        entry_stop: int | None = None,
        threads: int = 0,
    ) -> "Any":
        """Read the selection into a ROOT ``RDataFrame`` (needs ROOT + awkward).

        The columns are read into oxihipo's buffers with the GIL released, then
        handed to RDataFrame through Awkward's generated ``RDataSource`` — a
        jagged bank column becomes an ``RVec<T>`` column, a ``T#N`` array column
        a nested ``RVec``, *without copying the view*. Column names are the
        ``bank/column`` keys sanitized to valid C++ identifiers::

            REC::Particle/px   ->   REC_Particle_px

        so a per-event define reads naturally::

            df = f.rdataframe("REC::Particle", ["px", "py"])
            df.Define("pt", "sqrt(REC_Particle_px*REC_Particle_px"
                            " + REC_Particle_py*REC_Particle_py)").Histo1D("pt")

        The whole selection is materialized into memory first; RDF then runs its
        implicit-MT loop over that in-memory view — ideal for a per-run analysis
        that fits in RAM. For bigger-than-RAM inputs stream with
        :meth:`iterate_rdataframe`. ``filter_name`` / ``entry_start`` /
        ``entry_stop`` / ``threads`` behave exactly as in :meth:`arrays`, and any
        :meth:`filtered` chain filter carries through."""
        selection, single = self._resolve(banks, columns, filter_name)
        rec = self.arrays(
            banks, columns, filter_name=filter_name, library="ak",
            entry_start=entry_start, entry_stop=entry_stop, threads=threads,
        )
        return _to_rdataframe(_rdf_columns(rec, selection, single))

    def iterate_rdataframe(
        self,
        banks: str | Sequence[str] | None = None,
        columns: Sequence[str] | None = None,
        *,
        step_size: int | str = 100_000,
        filter_name: "str | Sequence[str] | None" = None,
        report: bool = False,
        entry_start: int | None = None,
        entry_stop: int | None = None,
        threads: int = 0,
    ) -> "Iterator[Any]":
        """Stream the selection as one ROOT ``RDataFrame`` per bounded-memory
        chunk — :meth:`iterate` composed with :meth:`rdataframe`, for inputs
        bigger than RAM.

        Each chunk (≈ one ``step_size`` of events, record- and file-aligned) is
        read, presented as its own small ``RDataFrame``, and yielded; it is
        dropped before the next, so resident memory stays ≈ one chunk. Because
        each chunk is an independent RDF, you book a result per chunk and merge
        across chunks yourself — histograms with ``Add``, counts by summing.
        Clone the first result and detach it so it outlives its chunk::

            total = None
            for df in f.iterate_rdataframe("REC::Particle", ["px"], step_size="1 GB"):
                h = df.Histo1D(("pt", "", 100, 0, 10), "REC_Particle_px").GetValue()
                if total is None:
                    total = h.Clone()
                    total.SetDirectory(0)     # detach: outlive this chunk's RDF
                else:
                    total.Add(h)

        With ``report=True`` each item is ``(df, Report)``. Column naming and the
        ROOT requirement are exactly as for :meth:`rdataframe`."""
        selection, single = self._resolve(banks, columns, filter_name)
        # Widen `self` so forwarding a plain-`bool` `report` doesn't re-run
        # iterate's overload resolution (same idiom as the module-level funcs).
        src: Any = self
        for item in src.iterate(
            banks, columns, step_size=step_size, filter_name=filter_name,
            library="ak", report=report, entry_start=entry_start,
            entry_stop=entry_stop, threads=threads,
        ):
            if report:
                chunk, rep = item
                yield _to_rdataframe(_rdf_columns(chunk, selection, single)), rep
            else:
                yield _to_rdataframe(_rdf_columns(item, selection, single))

    # --- bounded-memory streaming -----------------------------------------
    # Overloads capture the useful distinction: ``report=True`` yields
    # ``(chunk, Report)`` pairs, otherwise bare chunks. The chunk itself is
    # ``Any`` because its type depends on ``library=`` (see ``arrays`` for the
    # per-library types).
    @overload
    def iterate(
        self, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
        *, step_size: int | str = ..., filter_name: "str | Sequence[str] | None" = ..., library: _Library = ...,
        report: Literal[False] = ..., cut: str | None = ..., entry_start: int | None = ...,
        entry_stop: int | None = ..., threads: int = ..., workers: int = ...,
    ) -> "Iterator[Any]": ...
    @overload
    def iterate(
        self, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
        *, step_size: int | str = ..., filter_name: "str | Sequence[str] | None" = ..., library: _Library = ...,
        report: Literal[True], cut: str | None = ..., entry_start: int | None = ...,
        entry_stop: int | None = ..., threads: int = ..., workers: int = ...,
    ) -> "Iterator[tuple[Any, Report]]": ...
    def iterate(
        self,
        banks: str | Sequence[str] | None = None,
        columns: Sequence[str] | None = None,
        *,
        step_size: int | str = 100_000,
        filter_name: "str | Sequence[str] | None" = None,
        library: _Library = "ak",
        report: bool = False,
        cut: str | None = None,
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
        keep_columns: set[str] | None = None
        if cut is not None:
            selection, keep_columns = self._cut_plan(selection, cut)

        def finish(res):
            if cut is not None:
                res = self._apply_cut(res, cut, keep_columns)
            return self._assemble(res, single, library)

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
                finish,
            )
            for chunk, start, stop, fi in stream:
                yield (chunk, Report(start, stop, files[fi])) if report else chunk
            return

        for start, stop, fi in batches:
            chunk = finish(c.read_columns(selection, start, stop, threads))
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

    def composite(
        self,
        bank: str,
        entry_start: int | None = None,
        entry_stop: int | None = None,
        library: str = "ak",
    ) -> "Any":
        """Read a **composite** bank — the CLAS12 structures that carry an inline
        format string instead of a dictionary schema.

        Composite fields are positional, so they come back named ``f0``, ``f1``,
        … in format order. :meth:`arrays` / :meth:`read_columns` cannot read
        them — those need a schema — but the name still resolves through the
        dictionary, so a composite bank does appear in :meth:`keys` and being
        listed there does not tell you which reader to use.

            ev = f.composite("RUN::scaler")
            ev.f0            # first field, one sublist per event

        ``library`` is ``"ak"`` (default, jagged Awkward array) or ``"np"``
        (dict of object-dtype arrays, one entry per field)."""
        offsets, fields = self._reader().composite_columns(bank, entry_start, entry_stop)
        if not fields:
            # Two very different causes, and saying "no composite bank X" for
            # both is actively misleading: on a real CLAS12 file *every*
            # dictionary bank takes the second branch, and X plainly is in the
            # file. Composites resolve through the dictionary like any other
            # bank, so being in `keys()` says nothing about being composite.
            if bank in self._reader().keys(False):
                raise KeyError(
                    f"{bank!r} is in this file but is not a composite bank — it has a "
                    "dictionary schema, so read it with arrays()/read_columns(). "
                    "Composite banks are the ones carrying an inline format string "
                    "instead of a schema."
                )
            raise KeyError(
                f"no bank {bank!r} in this file (looked in the dictionary; "
                f"{len(self._reader().keys(False))} banks present)"
            )
        if library == "np":
            import numpy as _np

            # Fill a pre-sized object array rather than letting `np.array`
            # infer: with a constant number of rows per event every slice has
            # the same length, and NumPy collapses the list into a rank-2
            # (n_events, rows) array of boxed scalars instead of the rank-1
            # array-of-arrays every ragged file produces. Same call, different
            # shape depending on the data — so downstream `len(x[e])` breaks on
            # exactly the files that look tidiest.
            def _object_column(values):
                out = _np.empty(len(offsets) - 1, dtype=object)
                for e in range(len(offsets) - 1):
                    out[e] = values[offsets[e] : offsets[e + 1]]
                return out

            return {f"f{i}": _object_column(v) for i, (_dtype, v) in enumerate(fields)}
        if library != "ak":
            raise ValueError(f"unknown library {library!r} (expected 'ak' or 'np')")
        import awkward as ak
        import numpy as _np

        counts = _np.diff(offsets)
        return ak.zip(
            {
                f"f{i}": ak.unflatten(values, counts)
                for i, (_dtype, values) in enumerate(fields)
            },
            depth_limit=2,
        )

    @property
    def bank_ids(self) -> dict[str, tuple[int, int]]:
        """``{bank: (group, item)}`` — the wire identifiers of every bank in the
        dictionary. Needed when talking to tools that address banks by id."""
        # Unpack rather than `tuple(ids)`: the latter widens to
        # `tuple[int, ...]`, which does not satisfy the declared
        # `tuple[int, int]`.
        return {name: (group, item) for name, (group, item) in self._reader().bank_ids()}

    @property
    def record_count(self) -> int:
        """Number of records across the chain, from the record index.

        Worth knowing when writing: a reader parallelises over **records**, so
        this bounds how many cores a scan of this file can use. If it is smaller
        than your core count, the file was written with too large a
        ``max_record_bytes``.

        Not to be confused with ``file_header.record_count``, which is
        writer-dependent and in practice 0."""
        return self._reader().record_count()

    @property
    def file_header(self) -> "FileHeader | None":
        """The HIPO file header (:class:`FileHeader`), or ``None`` for an empty
        chain. Container metadata — version, record count, endianness, and the
        writer's free ``user_*`` fields. The run number lives in ``RUN::config``,
        not here."""
        raw = self._reader().file_header()
        return None if raw is None else FileHeader(*raw)

    @property
    def config(self) -> dict[str, str]:
        """The file's user key/value configuration (the ``(32555,…)`` run-config
        store shared with the C++/Java writers) as ``{key: value}`` — empty if
        the file carries none. Write it with ``create(..., config={...})``::

            f.config             # {'run': '5042', 'torus': '-1.0'}
        """
        return dict(self._reader().user_config())

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

        # Compose with whatever this chain already filters on. Each call used to
        # build a filter from its own arguments alone, so
        # `f.filtered(require=…).filtered(event_tag=…)` silently dropped the
        # `require` — and the record-tag clause was *widened* rather than
        # dropped, because the core unioned them. Chaining narrows, which is what
        # every other library in this space does.
        require = _merge_require(self._require, require)
        record_tag = _intersect_tags(self._record_tag, record_tag, "record_tag")
        numeric = _intersect_tags(self._event_tag, numeric, "event_tag")
        if self._event_tag_any is not None and any_mask is not None:
            # "any of A" AND "any of B" is not a single mask: an event carrying
            # only a∈A and only c∈B satisfies both clauses but shares no bit with
            # A&B. Rather than pick a wrong mask, say so.
            raise ValueError(
                "cannot chain two `event_tag_any` filters — 'any of A' and 'any "
                "of B' is not expressible as one bitmask. Combine them into a "
                "single filtered(event_tag_any=[...]) call, or apply the second "
                "as a cut on the result."
            )
        any_mask = self._event_tag_any if any_mask is None else any_mask

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
    *, step_size: int | str = ..., filter_name: "str | Sequence[str] | None" = ..., library: _Library = ...,
    report: Literal[False] = ..., cut: str | None = ..., entry_start: int | None = ...,
    entry_stop: int | None = ..., threads: int = ..., workers: int = ...,
) -> "Iterator[Any]": ...
@overload
def iterate(
    source: Source, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
    *, step_size: int | str = ..., filter_name: "str | Sequence[str] | None" = ..., library: _Library = ...,
    report: Literal[True], cut: str | None = ..., entry_start: int | None = ...,
    entry_stop: int | None = ..., threads: int = ..., workers: int = ...,
) -> "Iterator[tuple[Any, Report]]": ...
def iterate(
    source: Source,
    banks: str | Sequence[str] | None = None,
    columns: Sequence[str] | None = None,
    *,
    step_size: int | str = 100_000,
    filter_name: "str | Sequence[str] | None" = None,
    library: _Library = "ak",
    report: bool = False,
    cut: str | None = None,
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
        library=library, report=report, cut=cut, entry_start=entry_start,
        entry_stop=entry_stop, threads=threads, workers=workers,
    )


@overload
def arrays(
    source: Source, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
    *, filter_name: "str | Sequence[str] | None" = ..., library: Literal["ak"] = ...,
    entries: "Sequence[int] | None" = ..., cut: str | None = ...,
    entry_start: int | None = ..., entry_stop: int | None = ...,
    threads: int = ..., workers: int = ...,
) -> "ak.Array": ...
@overload
def arrays(
    source: Source, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
    *, filter_name: "str | Sequence[str] | None" = ..., library: Literal["np"],
    entries: "Sequence[int] | None" = ..., cut: str | None = ...,
    entry_start: int | None = ..., entry_stop: int | None = ...,
    threads: int = ..., workers: int = ...,
) -> "dict[str, np.ndarray]": ...
@overload
def arrays(
    source: Source, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
    *, filter_name: "str | Sequence[str] | None" = ..., library: Literal["pd"],
    entries: "Sequence[int] | None" = ..., cut: str | None = ...,
    entry_start: int | None = ..., entry_stop: int | None = ...,
    threads: int = ..., workers: int = ...,
) -> "pd.DataFrame | dict[str, pd.DataFrame]": ...
@overload
def arrays(
    source: Source, banks: str | Sequence[str] | None = ..., columns: Sequence[str] | None = ...,
    *, filter_name: "str | Sequence[str] | None" = ..., library: Literal["arrow"],
    entries: "Sequence[int] | None" = ..., cut: str | None = ...,
    entry_start: int | None = ..., entry_stop: int | None = ...,
    threads: int = ..., workers: int = ...,
) -> "pa.Table": ...
def arrays(
    source: Source,
    banks: str | Sequence[str] | None = None,
    columns: Sequence[str] | None = None,
    *,
    filter_name: "str | Sequence[str] | None" = None,
    library: _Library = "ak",
    entries: "Sequence[int] | None" = None,
    cut: str | None = None,
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
        entries=entries, cut=cut,
        entry_start=entry_start, entry_stop=entry_stop, threads=threads,
        workers=workers,
    )


def rdataframe(
    source: Source,
    banks: str | Sequence[str] | None = None,
    columns: Sequence[str] | None = None,
    *,
    filter_name: "str | Sequence[str] | None" = None,
    entry_start: int | None = None,
    entry_stop: int | None = None,
    threads: int = 0,
) -> "Any":
    """Read a file/dir/glob/list into a ROOT ``RDataFrame`` — the module-level
    shortcut for ``open(source).rdataframe(...)``. Needs ROOT + awkward; see
    :meth:`Chain.rdataframe` for the column naming and jagged→``RVec`` mapping."""
    chain: Any = open(source)
    return chain.rdataframe(
        banks, columns, filter_name=filter_name, entry_start=entry_start,
        entry_stop=entry_stop, threads=threads,
    )


def iterate_rdataframe(
    source: Source,
    banks: str | Sequence[str] | None = None,
    columns: Sequence[str] | None = None,
    *,
    step_size: int | str = 100_000,
    filter_name: "str | Sequence[str] | None" = None,
    report: bool = False,
    entry_start: int | None = None,
    entry_stop: int | None = None,
    threads: int = 0,
) -> "Iterator[Any]":
    """Stream a file/dir/glob/list as one ROOT ``RDataFrame`` per chunk — the
    module-level ``open(source).iterate_rdataframe(...)``, for bigger-than-RAM
    inputs. See :meth:`Chain.iterate_rdataframe` for the per-chunk merge idiom."""
    chain: Any = open(source)
    return chain.iterate_rdataframe(
        banks, columns, step_size=step_size, filter_name=filter_name,
        report=report, entry_start=entry_start, entry_stop=entry_stop,
        threads=threads,
    )


# --------------------------------------------------------------------------
# Writing (columnar, uproot-shaped)
# --------------------------------------------------------------------------
# HIPO type char → NumPy dtype (what the columns are cast to before the write).
_NP_DTYPE = {"B": "int8", "S": "int16", "I": "int32", "L": "int64", "F": "float32", "D": "float64"}


def _inner_of(typechar: str | None) -> int:
    """The per-row array length of a type char: ``"F"`` → 1, ``"F#3"`` → 3."""
    if not typechar or "#" not in typechar:
        return 1
    return int(typechar.split("#", 1)[1])


def _flatten_column(col, inner):
    """One column → ``(offsets | None, flat 1-D ndarray)`` for a schema column of
    per-row length ``inner``. ``None`` offsets means one row per event. The flat
    array is row-major with ``inner`` values per row (``total_rows * inner``)."""
    import numpy as np

    if hasattr(col, "layout"):  # an awkward array
        import awkward as ak

        # A jagged column (rows vary per event) carries one more list axis than
        # a "flat" one-row-per-event column.
        if col.ndim >= (3 if inner > 1 else 2):
            counts = ak.to_numpy(ak.num(col, axis=1))
            flat = np.ascontiguousarray(ak.to_numpy(ak.flatten(col, axis=1))).reshape(-1)
            offsets = np.concatenate(([0], np.cumsum(counts))).astype(np.int64)
            return offsets, flat
        return None, np.ascontiguousarray(ak.to_numpy(col)).reshape(-1)  # one row per event

    arr = np.ascontiguousarray(col)
    flat_ndim = 2 if inner > 1 else 1
    if arr.ndim == flat_ndim:  # one row per event  (n_events[, inner])
        return None, arr.reshape(-1)
    if arr.ndim == flat_ndim + 1:  # (n_events, rows[, inner]) rectangular → constant counts
        n, r = arr.shape[0], arr.shape[1]
        return (np.arange(n + 1, dtype=np.int64) * r), arr.reshape(-1)
    raise TypeError(f"unsupported column shape {arr.shape!r} for a length-{inner} column")


def _to_columnar(bank, bdata, schema):
    """One bank's ``extend`` data → ``(offsets int64, [(col, values ndarray)])``.
    Accepts an ``ak.Array`` record (as ``arrays(bank)`` returns) or a dict of
    columns; ``schema`` maps ``col → typechar`` (with ``#N`` for array columns)."""
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
        typechar = schema.get(name)
        inner = _inner_of(typechar)
        off, flat = _flatten_column(col, inner)
        if off is None:  # one row per event → derive row-unit offsets
            off = np.arange(len(flat) // inner + 1, dtype=np.int64)
        if offsets is None:
            offsets = off
        elif not np.array_equal(offsets, off):
            raise ValueError(
                f"bank {bank!r}: column {name!r} has different per-event row counts than the others"
            )
        base = typechar.split("#", 1)[0] if typechar else None
        out_cols.append((name, np.ascontiguousarray(flat, dtype=_NP_DTYPE.get(base))))
    if offsets is None:
        offsets = np.zeros(1, dtype=np.int64)
    return np.ascontiguousarray(offsets, dtype=np.int64), out_cols


class Writer:
    """A columnar HIPO writer (:meth:`new_bank` / :meth:`extend` / :meth:`close`).

    Build a fresh file with :func:`create`, or *decorate* an existing one (copy
    its events, attaching new banks) with :func:`recreate`. Declare each new
    bank with :meth:`new_bank`, feed columnar batches (NumPy or Awkward) with
    :meth:`extend`, then :meth:`close` — or use it as a context manager.

    Scalar and fixed-length array columns (``T#N``) are both supported.
    """

    _w: "_RustWriter | None"

    def __init__(
        self,
        path,
        compression="lz4percolumn",
        source=None,
        _inplace=None,
        config=None,
        max_record_bytes=None,
    ):
        self._w = _RustWriter(
            str(path),
            compression,
            None if source is None else str(source),
            max_record_bytes,
        )
        if config:
            for key, value in dict(config).items():
                self._w.add_config(str(key), str(value))
        self._schemas: dict[str, dict[str, str]] = {}
        self._inplace = _inplace  # (final_path, temp_path) for recreate(dst=None)
        self._summary: SkimSummary | None = None
        self._path = str(path)  # what to remove if the block aborts

    def _writer(self) -> "_RustWriter":
        if self._w is None:
            raise ValueError("operation on a closed Writer")
        return self._w

    def new_bank(
        self,
        bank: str,
        columns: "dict[str, str] | Sequence[tuple[str, str]]",
        group: int = 1,
        item: int | None = None,
    ) -> None:
        """Declare a new bank. ``columns`` maps ``name → typechar`` (or a list of
        ``(name, typechar)``); typechar ∈ ``B/S/I/L/F/D`` (byte/short/int/long/
        float/double), optionally with ``#N`` for a fixed-length array column
        (e.g. ``"F#3"``). ``item`` (the unique bank id) auto-assigns when omitted."""
        cols: list[tuple[str, str]] = (
            list(columns.items())
            if isinstance(columns, dict)
            else [(c[0], c[1]) for c in columns]
        )
        self._writer().add_schema(bank, cols, group, item)
        self._schemas[bank] = dict(cols)

    def extend(self, data: "dict[str, Any]", tags: "Any | None" = None) -> None:
        """Append a batch of events. ``data`` is ``{bank: array}`` where each
        value is an ``ak.Array`` record (as :meth:`Chain.arrays` returns) or a
        dict of columns — a jagged ``ak.Array`` per column, or a 1-D NumPy array
        for a scalar-per-event bank. Every bank in one call must span the same
        number of events. Mirrors uproot's ``extend``.

        ``tags`` optionally stamps a per-event tag: a sequence of ``uint32``,
        one per event in the batch (read back via :meth:`Chain.event_tags` /
        :meth:`Chain.filtered`)."""
        banks = []
        for bank, bdata in data.items():
            offsets, cols = _to_columnar(bank, bdata, self._schemas.get(bank, {}))
            banks.append((bank, offsets, cols))
        tag_list = None if tags is None else [int(t) for t in tags]
        self._writer().extend(banks, tag_list)

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

    def __exit__(self, exc_type, exc, tb) -> None:
        # Only finalise if the block succeeded. Closing on the way out of a
        # failed block produced a complete-looking file from a failed run — and
        # for the in-place `recreate(dst=None)` it ran `os.replace(temp, final)`,
        # overwriting the user's source with a partial result. `close()` can also
        # raise (e.g. "decorate covered 0 of 8 events"), which then replaced the
        # user's real exception with a confusing one.
        if exc_type is None:
            self.close()
        else:
            self._abort()

    def _abort(self) -> None:
        """Discard the partial output without finalising it.

        Never raises: it runs while another exception is propagating, and
        masking that is exactly the bug this avoids.
        """
        if self._summary is not None:
            return  # already finished successfully
        self._w = None  # drop the writer (and the source handles) unfinalised
        # A HIPO file with no trailer is unreadable, so leaving it behind is
        # litter that looks like output.
        victim = self._inplace[1] if self._inplace is not None else self._path
        try:
            os.remove(victim)
        except OSError:
            pass

    def __repr__(self) -> str:
        return f"<oxihipo.Writer: {'closed' if self._summary else 'open'}>"


def create(
    path: StrPath,
    compression: str = "lz4percolumn",
    config: "Mapping[str, str] | None" = None,
    max_record_bytes: int | None = None,
) -> Writer:
    """Open a **new** HIPO file for writing, refusing to overwrite an existing
    one — use :func:`recreate` for that. Declare banks with
    :meth:`Writer.new_bank`, feed batches with :meth:`Writer.extend`, then
    :meth:`Writer.close`. Compression is one of ``none`` / ``lz4`` / ``lz4best``
    / ``gzip`` / ``lz4perbank`` / ``lz4percolumn``.

    Portability, if the file will be read by other HIPO implementations:

    * ``none`` / ``lz4`` / ``lz4best`` — read by every implementation. Prefer
      these.
    * ``gzip`` — read by the Java reader, **not** by the reference C++ reader,
      which has no gzip decode path (it LZ4-decodes every non-``none`` tag).
    * ``lz4perbank`` / ``lz4percolumn`` — format extensions with true partial
      decompression; not read by the released C++/Java readers.

    ``config`` writes a user key/value store into the dictionary record (read
    back via :attr:`Chain.config`); it interoperates with the C++/Java writers.

    ``max_record_bytes`` sets the record-flush target. The default (32 MB for
    the per-bank/per-column formats, 8 MB otherwise) is tuned for compression
    ratio and single-thread selective reads. **A reader parallelises over whole
    records**, so it cannot use more cores than the file has records — on a
    mid-sized file the default leaves cores idle. Aim for at least a few records
    per core: on a 134 MB file, 4 MB records took a full parallel scan from
    73 ms to 59 ms for 1% more size. Smaller records cost ratio because each
    one restarts LZ4's match window."""
    # uproot's `create` raises here rather than truncating, and muscle memory
    # from it is the common case. Truncating silently is how an afternoon of
    # farm output disappears.
    if os.path.exists(path):
        raise FileExistsError(
            f"{os.fspath(path)!r} already exists. Use recreate(...) to replace it, "
            f"or update(...) to add banks to it."
        )
    return Writer(
        path, compression=compression, config=config, max_record_bytes=max_record_bytes
    )


_UNSET = object()


def recreate(
    path: StrPath,
    dst: "StrPath | None | object" = _UNSET,
    compression: str = "lz4percolumn",
    config: "Mapping[str, str] | None" = None,
    max_record_bytes: int | None = None,
    *,
    overwrite: bool = False,
) -> Writer:
    """Create a fresh file, **replacing** one that already exists.

    This is uproot's meaning of ``recreate`` — "creates a new file, deleting any
    existing file with that name". Use :func:`create` when overwriting should be
    an error, and :func:`update` to add banks to an existing file.

    .. warning::
       **This name changed meaning.** It used to mean "decorate an existing
       file", which is now :func:`update`. During one release a call that could
       plausibly have meant the old thing raises rather than acting:

       * ``recreate(src, dst)`` — two paths — warns and behaves as
         :func:`update`, as it always did.
       * ``recreate(path)`` where ``path`` **exists** raises, because the old
         meaning decorated that file and the new one destroys it. Pass
         ``overwrite=True`` to say you mean the new behaviour.
    """
    if dst is not _UNSET:
        warnings.warn(
            "recreate(source, dst) has been renamed update(source, dst); "
            "`recreate(path)` now creates a fresh file, matching uproot. "
            "This compatibility shim will be removed in a future release.",
            DeprecationWarning,
            stacklevel=2,
        )
        return update(
            path,
            None if dst is None else cast("StrPath", dst),
            compression=compression,
            max_record_bytes=max_record_bytes,
        )

    if os.path.exists(path) and not overwrite:
        raise FileExistsError(
            f"{os.fspath(path)!r} exists, and `recreate` changed meaning: it now "
            f"creates a fresh file (destroying that one), where it used to "
            f"decorate it in place. Say which you mean:\n"
            f"  update({os.fspath(path)!r})                    "
            f"# add banks to it — the old behaviour\n"
            f"  recreate({os.fspath(path)!r}, overwrite=True)  "
            f"# replace it — the new behaviour\n"
            f"This guard is temporary and goes away once the rename has settled."
        )
    return Writer(
        path, compression=compression, config=config, max_record_bytes=max_record_bytes
    )


def numpy(path: StrPath, bank: str, column: str, **kwargs: "Any") -> "Any":
    """Module-level :meth:`Chain.numpy` — ``oxihipo.numpy(path, bank, col)``."""
    return open(path).numpy(bank, column, **kwargs)


def event_tags(path: StrPath, **kwargs: "Any") -> "Any":
    """Module-level :meth:`Chain.event_tags`."""
    return open(path).event_tags(**kwargs)


def composite(path: StrPath, bank: str, **kwargs: "Any") -> "Any":
    """Module-level :meth:`Chain.composite`."""
    return open(path).composite(bank, **kwargs)


def skim(path: StrPath, dst: StrPath, **kwargs: "Any") -> "SkimSummary":
    """Module-level :meth:`Chain.skim`."""
    return open(path).skim(dst, **kwargs)


def update(
    source: StrPath,
    dst: StrPath | None = None,
    compression: str = "lz4percolumn",
    max_record_bytes: int | None = None,
) -> Writer:
    """Decorate an existing file: copy every event of ``source`` and attach the
    new banks you :meth:`~Writer.new_bank` + :meth:`~Writer.extend` (which must
    align 1:1 with the source events). Existing banks are copied through
    verbatim. Writes to ``dst``; if ``dst`` is ``None``, replaces ``source`` in
    place via a temp file."""
    if dst is None:
        temp = str(source) + ".oxitmp"
        return Writer(
            temp,
            compression=compression,
            source=source,
            _inplace=(str(source), temp),
            max_record_bytes=max_record_bytes,
        )
    return Writer(
        dst, compression=compression, source=source, max_record_bytes=max_record_bytes
    )
