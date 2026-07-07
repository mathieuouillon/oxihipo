"""oxihipo — fast, columnar reading of HIPO (CLAS12) files, powered by Rust.

The compiled ``_oxihipo`` extension does the heavy lifting: one Rust pass over
the file materializes each requested column into a flat NumPy buffer plus a
shared ``int64`` offsets buffer, with the GIL released. This module layers the
uproot-shaped ergonomics on top — ``array`` / ``arrays`` return Awkward arrays
built *zero-copy* from those buffers; ``numpy`` returns the raw buffers so the
NumPy-only path needs no Awkward import.
"""

from __future__ import annotations

from typing import Iterable, Sequence

from ._oxihipo import Chain as _RustChain, CorruptFileError, OxihipoError, __version__

__all__ = ["Chain", "open", "CorruptFileError", "OxihipoError", "__version__"]


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


class Chain:
    """A HIPO reader over one file, a directory, a glob, or a list of paths.

    Behaves like a uproot ``TTree``: ``keys()`` lists banks (``recursive=True``
    lists ``bank/column``), ``arrays(...)`` returns an Awkward record array, and
    ``array(bank, column)`` returns one jagged column.
    """

    def __init__(self, source):
        self._c = source if isinstance(source, _RustChain) else _RustChain(source)

    # --- delegation to the compiled reader ---------------------------------
    def __getattr__(self, name):  # num_entries, file_count, files, keys, …
        return getattr(self._c, name)

    def __len__(self):
        return len(self._c)

    def __contains__(self, bank):
        return bank in self._c

    def __repr__(self):
        return f"<oxihipo.Chain: {self._c.num_entries} events, {self._c.file_count} file(s)>"

    # --- the raw NumPy path (no Awkward needed) ----------------------------
    def numpy(self, bank: str, column: str, *, entry_start=None, entry_stop=None):
        """``(values, offsets, inner_len)`` for one column — plain NumPy."""
        _, offsets, cols = self._c.read_columns(
            [(bank, [column])], entry_start, entry_stop
        )[0]
        _, values, inner_len = cols[0]
        return values, offsets, inner_len

    # --- the Awkward path --------------------------------------------------
    def array(self, bank: str, column: str, *, entry_start=None, entry_stop=None):
        """One jagged column as an ``ak.Array`` (type ``N * var * T``)."""
        import awkward as ak

        _, offsets, cols = self._c.read_columns(
            [(bank, [column])], entry_start, entry_stop
        )[0]
        _, values, inner_len = cols[0]
        return ak.Array(_wrap_column(ak, offsets, values, inner_len))

    def arrays(
        self,
        banks,
        columns: Sequence[str] | None = None,
        *,
        entry_start=None,
        entry_stop=None,
    ):
        """Bank(s) → an Awkward record array.

        ``arrays("REC::Particle")`` → a jagged record (``var * {col: T}``) so
        ``p[event].px`` works. ``arrays(["REC::Particle", "REC::Calorimeter"])``
        → a top-level record with one jagged field per bank.
        """
        import awkward as ak

        single = isinstance(banks, str)
        if single:
            selection = [(banks, list(columns) if columns is not None else [])]
        else:
            selection = [(b, []) for b in banks]

        res = self._c.read_columns(selection, entry_start, entry_stop)
        built = {bname: _bank_record(ak, offsets, cols) for bname, offsets, cols in res}

        if single:
            return ak.Array(built[selection[0][0]])
        names = list(built.keys())
        return ak.Array(ak.contents.RecordArray([built[n] for n in names], names))

    def __getitem__(self, key: str):
        """``chain["REC::Particle/px"]`` → column; ``chain["REC::Particle"]`` → bank."""
        if "/" in key:
            bank, column = key.rsplit("/", 1)
            return self.array(bank, column)
        return self.arrays(key)


def open(source) -> Chain:  # noqa: A001  (uproot-style: oxihipo.open(...))
    """Open a HIPO file, directory, glob, or list of paths → :class:`Chain`."""
    return Chain(source)
