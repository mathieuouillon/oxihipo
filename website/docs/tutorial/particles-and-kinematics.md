---
id: particles-and-kinematics
title: Particles & selection
sidebar_position: 4
---

# Particles, four-vectors, and selecting the electron

`REC::Particle` is where an analysis starts. Each row is one reconstructed
particle:

| column | meaning |
|---|---|
| `pid` | PDG code — the reconstruction's identity guess |
| `px, py, pz` | momentum components (GeV), lab frame |
| `vx, vy, vz` | vertex position (cm) — where the track came from |
| `vt` | vertex time (ns) |
| `charge` | −1, 0, +1 |
| `beta` | $v/c$ from time-of-flight — the hadron-PID workhorse |
| `chi2pid` | how well the timing matches the assigned `pid` (smaller = better) |
| `status` | which detector system, encoded (see below) |

## Four-vectors

Momentum plus a mass gives a four-vector, and four-vectors give you everything
else — energy, invariant mass, boosts. There's no dedicated type in the data;
you build it from the columns. A couple of tiny helpers keep the rest of the
tutorial readable:

```python
import awkward as ak
import numpy as np

def p3(part):                      # |p|
    return np.sqrt(part.px**2 + part.py**2 + part.pz**2)

def energy(part, mass):            # E = sqrt(p² + m²)
    return np.sqrt(part.px**2 + part.py**2 + part.pz**2 + mass**2)

def theta(part):                   # polar angle (rad)
    return np.arccos(part.pz / p3(part))
```

These work on a single particle *or* a whole jagged column — that's the point of
Awkward. Everything downstream is array math over all events at once.

:::tip Real analyses use `vector`
The scikit-hep [`vector`](https://vector.readthedocs.io) library adds proper
`Momentum4D` records with `.mass`, `.boost`, `.deltaphi`, etc., and integrates
with Awkward. We keep it explicit here so you see the arithmetic; on real work,
`vector.zip({"px":…, "py":…, "pz":…, "E":…})` is worth it.
:::

## `status`: which detector, and the trigger

`status` encodes the detector system a particle was reconstructed in. The
magnitude tells you the region:

```python
region = np.abs(p.status) // 1000     # 1 = Forward Tagger, 2 = Forward Detector, 4 = Central
```

- **Forward Detector** (`|status|` in 2000–3999): tracked in the drift chambers,
  seen by HTCC + calorimeters. Where the scattered electron lives.
- **Central Detector** (`|status|` ≥ 4000): tracked in the solenoid CVT.
- **Forward Tagger** (`|status|` in 1000–1999): very forward, electrons/photons.

The **trigger electron** — the scattered electron that fired the data-acquisition
trigger — is a Forward-Detector electron, and in a DST it is conventionally the
first `REC::Particle` row. The robust way to pick it (and what you'd do on real
data) is: take Forward-Detector electrons and keep the highest-momentum one.

## Selecting the scattered electron

```python
p = f.arrays("REC::Particle", ["pid", "px", "py", "pz", "vz", "charge", "beta", "chi2pid", "status"])

is_e   = p.pid == 11
in_fd  = (np.abs(p.status) >= 2000) & (np.abs(p.status) < 4000)
electrons = p[is_e & in_fd]                 # jagged: FD electrons per event

# require at least one, then take the highest-momentum electron per event
has_e = ak.num(electrons.pid) > 0
electrons = electrons[has_e]                 # drop events with no FD electron
lead = ak.argmax(p3(electrons), axis=1, keepdims=True)
ele  = ak.firsts(electrons[lead])            # one electron record per surviving event

len(ele)          # events with a scattered-electron candidate
p3(ele)           # its momentum, one value per event
```

`ak.argmax(..., keepdims=True)` then `ak.firsts` is the idiom for "the row that
maximises something, per event": argmax gives the index, `keepdims` keeps the
jagged shape so the index selects correctly, and `firsts` unwraps the length-1
list to a plain per-event record.

:::note Alignment
Once you filter events (here, `has_e`), *every* array you compare must be filtered
the same way, or they won't line up. A clean pattern is to compute all your
per-event masks first, combine them into one event mask, and apply it once at the
end.
:::

## Other particles

The same masking picks out anything:

```python
photons = p[p.pid == 22]
pips    = p[p.pid == 211]      # π⁺
protons = p[p.pid == 2212]

# quick momentum histogram of every π⁺ in the file
import numpy as np
pip_p = ak.to_numpy(ak.flatten(p3(pips)))
np.histogram(pip_p, bins=40, range=(0, 6))
```

For **charged hadrons**, identity from `pid` alone is only as good as the
timing; the honest separation of $\pi^+$/$K^+$/$p$ uses `beta` vs momentum, and
you'll often re-derive it yourself. For **electrons and photons**, the
calorimeter and Cherenkov banks are the real discriminators — which is
[the next chapter but one](./detector-and-pid.md).

But first, with a clean scattered electron in hand, we can already do real
physics: the inclusive DIS kinematics.

[Inclusive DIS →](./inclusive-dis.md)
