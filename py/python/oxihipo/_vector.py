"""Lorentz-vector behaviours over a momentum bank.

A HIPO momentum bank is three flat columns — ``px``, ``py``, ``pz`` — so every
analysis rebuilds the same kinematics by hand: energy from a hardcoded mass,
:math:`p_T` from a square root, invariant mass from a four-vector sum written
out longhand. `vector <https://vector.readthedocs.io>`_ already does all of it
on Awkward arrays; it just needs the columns handed over under the names it
expects, with a mass.

That is what :func:`to_vector` does. It is a function rather than a keyword on
:meth:`~oxihipo.Chain.arrays` so it composes with everything that produces an
array — an ``iterate`` chunk, a ``to_dask`` partition, the result of a ``cut=``
— instead of only the one call.
"""

from __future__ import annotations

from typing import Any

__all__ = ["to_vector"]

_REGISTERED = False


def _register() -> None:
    """Install vector's Awkward behaviours once per process."""
    global _REGISTERED
    if _REGISTERED:
        return
    try:
        import vector
    except ImportError as e:  # pragma: no cover - depends on environment
        raise ImportError(
            "to_vector() needs vector: pip install 'oxihipo[vector]'"
        ) from e
    vector.register_awkward()
    _REGISTERED = True


def to_vector(array: Any, *, mass: Any = None, momentum: str = "auto") -> Any:
    """Give a momentum bank Lorentz-vector behaviour.

    `array` is a record with ``px``/``py``/``pz`` fields — what
    ``arrays("REC::Particle", ["px", "py", "pz"])`` returns. The result carries
    the same rows with `vector`'s methods on top::

        p = f.arrays("REC::Particle", ["pid", "px", "py", "pz"])
        v = ox.to_vector(p, mass="pdg")

        v.E, v.pt, v.eta, v.phi, v.mass
        (v[:, 0] + v[:, 1]).mass          # invariant mass of the first two
        v[:, 0].deltaR(v[:, 1])

    ``mass`` decides whether you get a 4-vector and what its mass is:

    - ``"pdg"`` — look each row's ``pid`` up with :func:`~oxihipo.pdg_mass`.
      Needs a ``pid`` field, which is why reading it alongside the momenta is
      worth the column. Unidentified rows (``pid == 0``) get ``nan``, so their
      energy is ``nan`` too rather than silently zero.
    - a number — one mass for every row, for a bank you have already cut to a
      single species.
    - an array — your own per-row masses, any shape that broadcasts.
    - ``None`` (default) — no mass, so a **3-vector**: ``pt``/``eta``/``phi`` and
      angles work, ``E``/``mass`` do not exist. This is the honest default; a
      massless assumption dressed up as a 4-vector is how wrong invariant masses
      happen.

    An ``E`` or ``energy`` field already on the record is used as-is, and
    ``mass`` must then be left ``None`` — passing both would let them disagree.

    Needs ``vector``, which is **not** a dependency of this package
    (``pip install 'oxihipo[vector]'``).
    """
    _register()
    import awkward as ak

    fields = set(ak.fields(array))
    missing = {"px", "py", "pz"} - fields
    if missing:
        raise ValueError(
            f"to_vector() needs px/py/pz fields; missing {sorted(missing)}. "
            f"Got {sorted(fields)} — read the momentum columns, or select one "
            f"bank out of a multi-bank result first."
        )

    energy = next((f for f in ("E", "energy") if f in fields), None)
    if energy is not None and mass is not None:
        raise ValueError(
            f"{energy!r} is already a field, so the four-vector is complete; "
            f"passing mass= as well would let the two disagree"
        )

    cols = {"px": array.px, "py": array.py, "pz": array.pz}
    if energy is not None:
        cols["E"] = array[energy]
    elif mass is not None:
        if isinstance(mass, str):
            if mass != "pdg":
                raise ValueError(f"mass must be 'pdg', a number, or an array, not {mass!r}")
            if "pid" not in fields:
                raise ValueError(
                    "mass='pdg' needs a `pid` field to look masses up from; "
                    "read it alongside the momenta"
                )
            from ._pdg import pdg_mass

            cols["mass"] = pdg_mass(array.pid)
        else:
            # An array, or a scalar — `ak.zip` broadcasts the latter to the
            # array's jaggedness itself, so there is nothing to do here.
            cols["mass"] = mass

    name = _name(momentum, four=len(cols) == 4)
    # Carry the other columns through — `pid`, `chi2pid`, whatever was read —
    # so the vector is a superset of the bank and nothing has to be re-read.
    for f in fields:
        if f not in cols and f != energy:
            cols[f] = array[f]
    return ak.zip(cols, with_name=name)


def _name(momentum: str, *, four: bool) -> str:
    if momentum == "auto":
        return "Momentum4D" if four else "Momentum3D"
    if momentum not in ("Momentum3D", "Momentum4D"):
        raise ValueError(
            f"momentum must be 'auto', 'Momentum3D' or 'Momentum4D', not {momentum!r}"
        )
    if momentum == "Momentum4D" and not four:
        raise ValueError(
            "momentum='Momentum4D' needs an energy or a mass; pass mass="
        )
    return momentum
