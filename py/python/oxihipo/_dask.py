"""dask-awkward source for :meth:`oxihipo.Chain.to_dask`.

Its own module because it imports ``dask_awkward`` at module scope, and
dask-awkward is an optional extra — importing it from ``_parallel`` would have
made the multiprocessing ``workers=`` path require dask.
"""

from __future__ import annotations

import re

from dask_awkward.lib.io.columnar import ColumnProjectionMixin

from ._parallel import _worker_chain

# We build `filter_name` globs out of names the file's own dictionary supplied,
# so a bank called `RE*` would otherwise widen its own projection. `]` is literal
# outside a class and needs no escaping; `[x]` is how fnmatch spells "literal x".
_GLOB_SPECIAL = re.compile(r"([*?\[])")


def _glob_escape(name: str) -> str:
    return _GLOB_SPECIAL.sub(r"[\g<1>]", name)


# --- dask-awkward source ----------------------------------------------------
#
# `from_map` over a plain function gives a graph that is lazy in name only:
# dask-awkward cannot introspect a bare callable, so it reads partition 0 during
# graph construction just to learn the type, and it cannot prune columns.
#
# Implementing `ColumnProjectionMixin` is what makes `dak.sum(p.px)` read one
# column instead of every bank in the file; carrying the form is what makes the
# construction actually read nothing. (`divisions` is the third piece, and comes
# from `to_dask` itself — the batch boundaries it already computed.)


class HipoRead(ColumnProjectionMixin):
    """The IO callable behind :meth:`Chain.to_dask`.

    A class rather than a closure for three reasons: dask-awkward's optimizer
    only projects columns for objects implementing its protocol; the form has to
    travel with the callable so no partition is read to discover it; and
    `project_columns` has to return *another* one of these, narrowed.
    """

    def __init__(self, spec, form, banks, columns, kw):
        self.spec = spec          # (source, require, record_tag, event_tag, event_tag_any)
        self.form = form          # ak.forms.Form — the meta, known without I/O
        self.banks = banks
        self.columns = columns
        self.kw = kw
        self.behavior = None
        self.attrs = None

    # dask-awkward protocol ------------------------------------------------
    @property
    def use_optimization(self) -> bool:
        return True

    def __call__(self, batch):
        start, stop = batch
        chain = _worker_chain(*self.spec)
        return chain.arrays(
            self.banks, self.columns, entry_start=start, entry_stop=stop, **self.kw
        )

    def project_columns(self, columns):
        """Narrow the read to `columns`, given as dotted form paths.

        A read of one named bank has no outer bank record, so its paths are bare
        column names (``px``) and narrowing means passing ``columns=``. Every
        other selection — ``banks=[...]``, ``banks=None``, ``filter_name=`` —
        keeps a bank-keyed outer record and gives ``BANK.column`` paths.

        `form` deliberately stays the *unprojected* one: the collection's meta
        was fixed at `from_map` time and the optimizer only rebuilds the input
        layer around this callable, so narrowing it here would describe a layout
        nothing asks for.
        """
        wanted = {c for c in columns if c}
        if not wanted:
            return self

        if isinstance(self.banks, str):
            # `BANK.col.sub` for a nested record still means "read column col".
            cols = sorted({c.split(".", 1)[0] for c in wanted})
            return HipoRead(self.spec, self.form, self.banks, cols, self.kw)

        # `columns=` is rejected across banks, so narrow through `filter_name`
        # globs — `_resolve` turns those into exactly the per-bank column lists
        # we want, and, unlike a bare bank name, keeps the outer record whatever
        # the projection leaves behind. Collapsing to a single bank string here
        # dropped that record and returned a layout the meta did not describe.
        whole: set[str] = set()
        by_bank: dict[str, set[str]] = {}
        for path in wanted:
            bank, _, rest = path.partition(".")
            if rest:
                by_bank.setdefault(bank, set()).add(rest.split(".", 1)[0])
            else:
                whole.add(bank)
        globs = [_glob_escape(b) for b in sorted(whole)]
        # A bank wanted whole must not also carry column globs: `_resolve` reads
        # a non-empty column list as the *complete* selection for that bank.
        globs += [
            f"{_glob_escape(b)}/{_glob_escape(c)}"
            for b, cols in sorted(by_bank.items())
            if b not in whole
            for c in sorted(cols)
        ]
        return HipoRead(self.spec, self.form, None, None, {**self.kw, "filter_name": globs})
