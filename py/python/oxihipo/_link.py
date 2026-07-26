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


def link(
    banks: Any,
    *,
    particles: str = "REC::Particle",
    index: str = "pindex",
    directions: str = "both",
) -> Any:
    """Wire the ``pindex`` cross-references across a multi-bank read.

    `banks` is what ``arrays([...])`` returns — a record with one field per
    bank. Every bank carrying an `index` column is linked to `particles` in both
    directions, so the join stops being something you write and becomes
    something you follow::

        ev = ox.link(f.arrays(["REC::Particle", "REC::Calorimeter"]))

        ev["REC::Calorimeter"].particle.px      # the particle each row belongs to
        ev["REC::Particle"]["REC::Calorimeter"] # that particle's rows, grouped

    `directions` picks which links to build, because they do not cost the same:

    - ``"to_particle"`` — only the detector→particle side. This one *duplicates*
      the particle record onto every detector row, so a bank with ten times the
      rows carries ten copies of each momentum.
    - ``"to_detector"`` — only the particle→detector side, which regroups the
      detector rows without copying anything (see :func:`group_by_index`).
    - ``"both"`` (default) — both.

    A row whose `index` is out of range gets ``None`` on the detector→particle
    side, and is dropped on the other; either way it is never silently attached
    to the wrong particle. Banks with no `index` column are passed through
    untouched, so an event-level bank in the same read is not a problem.
    """
    import awkward as ak

    if directions not in ("both", "to_particle", "to_detector"):
        raise ValueError(
            f"directions must be 'both', 'to_particle' or 'to_detector', "
            f"not {directions!r}"
        )

    fields = ak.fields(banks)
    if particles not in fields:
        raise ValueError(
            f"link() needs the {particles!r} bank in the read to link against; "
            f"got {sorted(fields)}"
        )

    part = banks[particles]
    n_part = np.asarray(ak.num(part), dtype=np.int64)
    linkable = [
        b for b in fields if b != particles and index in ak.fields(banks[b])
    ]

    out = {b: banks[b] for b in fields}
    for b in linkable:
        det = banks[b]
        if directions in ("both", "to_particle"):
            out[b] = ak.with_field(det, _gather_particle(det, part, n_part, index), "particle")
        if directions in ("both", "to_detector"):
            out[particles] = ak.with_field(
                out[particles], group_by_index(det, n_part, index=index), b
            )
    return ak.zip(out, depth_limit=1)


def _gather_particle(detector: Any, part: Any, n_part: np.ndarray, index: str) -> Any:
    """For each detector row, the particle it points at (``None`` if invalid).

    Done as a flat gather through the same global-slot index space
    :func:`group_by_index` uses, rather than a jagged ``part[det.pindex]``:
    that form raises on an out-of-range index and has no natural answer for an
    event with no particles at all.
    """
    import awkward as ak

    n_det = np.asarray(ak.num(detector), dtype=np.int64)
    part_offsets = np.zeros(len(n_part) + 1, dtype=np.int64)
    np.cumsum(n_part, out=part_offsets[1:])
    event_of_row = np.repeat(np.arange(len(n_det), dtype=np.int64), n_det)
    pidx = np.asarray(ak.flatten(detector[index]), dtype=np.int64)

    valid = (pidx >= 0) & (pidx < n_part[event_of_row])
    # Clamp before gathering so the invalid rows index *something*; the mask
    # below is what actually removes them.
    slot = np.where(valid, part_offsets[event_of_row] + pidx, 0)

    flat_part = ak.flatten(part)
    if len(flat_part) == 0:
        # No particles anywhere: every row is unlinked, and a gather would have
        # nothing to index.
        return ak.unflatten(ak.Array([None] * len(pidx)), n_det)

    gathered = ak.mask(flat_part[slot], valid)
    return ak.unflatten(gathered, n_det)
