"""Joining CLAS12 detector banks to their particles by ``pindex``.

Detector banks do not repeat a particle's momentum. Each of their rows carries a
``pindex`` — the row number of the particle it belongs to, *within the same
event's* ``REC::Particle``. The project's own tutorial calls learning this join
"the single most useful CLAS12-specific skill", and then does it by hand::

    ak.sum(cal.energy[cal.pindex == 0], axis=1)                    # particle 0
    ak.sum(cal.energy[(cal.pindex == 0) & (cal.layer == 1)], axis=1)

which only ever answers for one hardcoded particle.

:func:`group_by_index` does the general join: it regroups a detector bank so
there is one sublist **per particle**, in particle order. That result lines up
with the particle array itself, so a per-particle quantity is one reduction and
can be attached as a column.
"""

from __future__ import annotations

from typing import Any

import numpy as np

__all__ = ["group_by_index"]


def group_by_index(detector: Any, counts: Any, *, index: str = "pindex") -> Any:
    """Regroup `detector` so each particle owns its rows.

    `detector` is a detector bank as read — ``events * var * {pindex, ...}``.
    `counts` is the number of particles in each event, i.e. ``ak.num(particles)``
    for the ``REC::Particle`` array you want to align with. The result is
    ``events * particles * var * {...}``: one sublist per particle, holding that
    particle's detector rows, in particle order::

        part = f.arrays("REC::Particle", ["pid", "px", "py", "pz"])
        cal  = f.arrays("REC::Calorimeter", ["pindex", "layer", "energy"])

        by_particle = ox.group_by_index(cal, ak.num(part))
        part["cal_energy"] = ak.sum(by_particle.energy, axis=-1)

    The last line is the point: `cal_energy` is now a per-particle column beside
    `px`, for *every* particle, where the hand-written form answers for one.
    Selections still compose — ``by_particle[by_particle.layer == 1]`` narrows to
    PCAL before the sum.

    A row whose `index` falls outside its event's particle range is **dropped**,
    not clamped: it refers to a particle that is not there, and quietly folding
    it into particle 0 or the last particle would put energy on the wrong track.
    Rows are kept in their original order within each particle's sublist.
    """
    import awkward as ak

    fields = ak.fields(detector)
    if index not in fields:
        raise ValueError(
            f"group_by_index() needs a {index!r} field to join on; "
            f"got {sorted(fields)}"
        )

    n_part = np.asarray(counts, dtype=np.int64)
    n_det = np.asarray(ak.num(detector), dtype=np.int64)
    if len(n_part) != len(n_det):
        raise ValueError(
            f"counts describes {len(n_part)} events but the detector bank has "
            f"{len(n_det)} — they must come from the same read"
        )

    # Global particle slot for every detector row: the event's particle block
    # start, plus the row's pindex within it. Working in one flat index space is
    # what makes this a single sort rather than a loop over events.
    part_offsets = np.zeros(len(n_part) + 1, dtype=np.int64)
    np.cumsum(n_part, out=part_offsets[1:])
    event_of_row = np.repeat(np.arange(len(n_det), dtype=np.int64), n_det)
    pidx = np.asarray(ak.flatten(detector[index]), dtype=np.int64)

    keep = (pidx >= 0) & (pidx < n_part[event_of_row])
    slot = part_offsets[event_of_row] + pidx

    flat = ak.flatten(detector)
    if not keep.all():
        flat = flat[keep]
        slot = slot[keep]

    # Stable, so rows keep their file order inside each particle's sublist.
    order = np.argsort(slot, kind="stable")
    per_particle = np.bincount(slot, minlength=int(part_offsets[-1]))

    grouped = ak.unflatten(flat[order], per_particle)
    return ak.unflatten(grouped, n_part)
