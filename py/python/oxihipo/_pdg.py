"""PDG particle masses, vectorized and jagged-preserving.

Every CLAS12 tutorial hardcodes the masses it needs — ``M_PIP = 0.139570``,
``M_P = 0.938272`` — because there was nothing to call. The obvious escape,
``particle.Particle.from_pdgid``, fails on exactly the two things a CLAS12
``REC::Particle`` column contains that nothing else does:

* ``pid == 0`` — real rows carry it (the reconstruction found a track it could
  not identify) and ``from_pdgid(0)`` raises ``InvalidParticle``;
* ``pid == 45`` — CLAS12 writes **Geant3** codes for light nuclei, and
  ``from_pdgid(45)`` raises ``ParticleNotFound`` even though the deuteron is a
  perfectly ordinary particle. (45/46/47/49 are D/T/He4/He3.)

It is also per-row Python, where a column read is a NumPy array of millions.

So: a baked table, looked up with one ``searchsorted``. No new dependency, and
no dependence on *whether* ``particle`` is installed — the numbers below were
generated from it, but the answer here does not change with the environment.

Masses are in **GeV**, matching CLAS12 momenta. ``particle`` reports MeV, which
is a trap worth stating: mixing the two silently gives kinematics off by 10³.

Light nuclei are **bare nuclei**, not neutral atoms. ``particle`` tabulates the
neutral atom for the ``10LZZZAAAI`` codes — for the deuteron, 2.014101778 u
exactly — which is heavier than the nucleus by Z·m_e. A detector sees a stripped
ion, so the atomic value overstates its energy by 0.511 MeV for a deuteron and
1.022 MeV for an alpha. Small, systematic, and wrong for the one thing this
function exists to do.
"""

from __future__ import annotations

import math
from typing import TYPE_CHECKING, Any

import numpy as np

if TYPE_CHECKING:
    import awkward as ak

__all__ = ["PDG_MASS_GEV", "PDG_NAME", "pdg_mass", "pdg_name"]

#: PDG code → mass in GeV. Values generated from ``particle`` 1.0.0, so they are
#: the PDG values rather than the rounded constants that get typed by hand.
#:
#: Assign into this to add or override a code — ``pdg_mass`` reads it on every
#: call rather than caching a derived table, precisely so that works.
PDG_MASS_GEV: dict[int, float] = {
    # leptons
    11: 0.0005109989506900001,
    -11: 0.0005109989506900001,
    13: 0.1056583755,
    -13: 0.1056583755,
    15: 1.7769300000000001,
    -15: 1.7769300000000001,
    12: 0.0,
    -12: 0.0,
    14: 0.0,
    -14: 0.0,
    16: 0.0,
    -16: 0.0,
    # gauge
    22: 0.0,
    # mesons
    211: 0.13957039000000002,
    -211: 0.13957039000000002,
    111: 0.1349768,
    321: 0.49367700000000003,
    -321: 0.49367700000000003,
    130: 0.49761099999999997,
    310: 0.49761099999999997,
    221: 0.547862,
    223: 0.78266,
    331: 0.95778,
    333: 1.01946,
    # baryons
    2212: 0.9382720894300001,
    -2212: 0.9382720894300001,
    2112: 0.9395654219,
    -2112: 0.9395654219,
    3122: 1.115683,
    -3122: 1.115683,
    3222: 1.1893699999999998,
    3212: 1.192642,
    3112: 1.197449,
    3322: 1.31486,
    3312: 1.32171,
    3334: 1.67245,
    # Nuclei, PDG form. Bare nuclei — the neutral-atom masses `particle`
    # reports are heavier by Z*m_e; see the module docstring.
    1000010020: 1.87561293134931,
    1000010030: 2.80892112314931,
    1000020030: 2.80839153219862,
    1000020040: 3.72737933279862,
    # nuclei, Geant3 form — what CLAS12 actually writes into REC::Particle.pid.
    # Kept as distinct keys rather than remapped, because `pid == 45` is what a
    # user reads out of the file and what the docs' PID table lists.
    45: 1.87561293134931,
    46: 2.80892112314931,
    47: 3.72737933279862,
    49: 2.80839153219862,
}

#: PDG code → short name, for labelling. Same codes as :data:`PDG_MASS_GEV`.
PDG_NAME: dict[int, str] = {
    11: "e-", -11: "e+", 13: "mu-", -13: "mu+", 15: "tau-", -15: "tau+",
    12: "nu(e)", -12: "nu(e)~", 14: "nu(mu)", -14: "nu(mu)~",
    16: "nu(tau)", -16: "nu(tau)~", 22: "gamma",
    211: "pi+", -211: "pi-", 111: "pi0", 321: "K+", -321: "K-",
    130: "K(L)0", 310: "K(S)0", 221: "eta", 223: "omega(782)",
    331: "eta'(958)", 333: "phi(1020)",
    2212: "p", -2212: "p~", 2112: "n", -2112: "n~",
    3122: "Lambda", -3122: "Lambda~", 3222: "Sigma+", 3212: "Sigma0",
    3112: "Sigma-", 3322: "Xi0", 3312: "Xi-", 3334: "Omega-",
    1000010020: "D2", 1000010030: "T3", 1000020030: "He3", 1000020040: "He4",
    45: "D2", 46: "T3", 47: "He4", 49: "He3",
}  # fmt: skip


def _table() -> tuple[np.ndarray, np.ndarray]:
    """`PDG_MASS_GEV` as sorted (codes, masses) arrays for `searchsorted`.

    Rebuilt per call on purpose. The table is ~44 entries, so this is a few
    microseconds against a lookup over millions of rows, and it is what lets a
    user's assignment into `PDG_MASS_GEV` take effect immediately instead of
    landing behind a cache that has no reliable way to notice it.
    """
    codes = np.fromiter(PDG_MASS_GEV.keys(), dtype=np.int64, count=len(PDG_MASS_GEV))
    masses = np.fromiter(
        PDG_MASS_GEV.values(), dtype=np.float64, count=len(PDG_MASS_GEV)
    )
    order = np.argsort(codes)
    return codes[order], masses[order]


def _lookup(codes: np.ndarray, unknown: float, scale: float) -> np.ndarray:
    """Vectorized code → mass. Unlisted codes (including `0`) give `unknown`."""
    table_codes, table_masses = _table()
    idx = np.searchsorted(table_codes, codes)
    # `searchsorted` can return len(table) for a code past the end; clip before
    # indexing, then the equality test rejects it anyway.
    safe = np.clip(idx, 0, len(table_codes) - 1)
    hit = table_codes[safe] == codes
    out = np.where(hit, table_masses[safe] * scale, unknown)
    return out


def _scale(unit: str) -> float:
    u = unit.lower()
    if u == "gev":
        return 1.0
    if u == "mev":
        return 1000.0
    raise ValueError(f"unit must be 'GeV' or 'MeV', not {unit!r}")


def pdg_mass(
    pid: Any,
    *,
    unit: str = "GeV",
    unknown: float = math.nan,
) -> Any:
    """The mass of each PDG code in `pid`, keeping whatever shape it came in.

    Give it a scalar, a NumPy array, or the jagged ``ak.Array`` that
    ``arrays("REC::Particle", ["pid"]).pid`` returns — the result matches, so it
    lines up with the momentum columns beside it::

        p = f.arrays("REC::Particle", ["pid", "px", "py", "pz"])
        m = ox.pdg_mass(p.pid)
        E = np.sqrt(p.px**2 + p.py**2 + p.pz**2 + m**2)

    A code that is not in :data:`PDG_MASS_GEV` yields `unknown` (``nan`` by
    default) rather than raising. That is deliberate and it is the common case:
    ``pid == 0`` marks a track the reconstruction could not identify, and real
    files are full of them. ``nan`` propagates into the kinematics it touches
    and can be cut with ``~np.isnan(m)``; pass ``unknown=0.0`` for the massless
    convention instead.

    ``unit`` is ``"GeV"`` (CLAS12 momenta) or ``"MeV"`` (what ``particle``
    reports).
    """
    scale = _scale(unit)

    layout = getattr(pid, "layout", None)
    if layout is not None:
        import awkward as ak

        def apply(lay, **_):
            if lay.is_numpy:
                data = np.asarray(lay.data)
                return ak.contents.NumpyArray(_lookup(data, unknown, scale))
            return None

        # `transform` rebuilds the same nesting around new leaf buffers, so this
        # holds for any depth rather than assuming one level of jaggedness.
        return ak.transform(apply, pid)

    codes = np.asarray(pid)
    if codes.ndim == 0:
        return float(_lookup(codes.reshape(1).astype(np.int64), unknown, scale)[0])
    return _lookup(codes.astype(np.int64), unknown, scale)


def pdg_name(pid: Any, *, unknown: str = "?") -> Any:
    """The short name of each PDG code — ``11`` → ``'e-'``, ``45`` → ``'D2'``.

    For labelling plots and tables. A scalar gives a string; an array gives a
    list, since neither NumPy nor Awkward has a natural string-per-row form that
    is cheaper than that.
    """
    codes = np.asarray(pid.layout.to_backend_array() if hasattr(pid, "layout") else pid)
    if codes.ndim == 0:
        return PDG_NAME.get(int(codes), unknown)
    return [PDG_NAME.get(int(c), unknown) for c in codes.reshape(-1)]
