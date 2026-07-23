---
id: clas12-and-hipo
title: CLAS12 & HIPO
sidebar_position: 2
---

# CLAS12 & HIPO in ten minutes

Before any code, here's the vocabulary. If you already know CLAS12, skim to
[The event model](#the-event-model).

## The experiment

**CLAS12** is a large-acceptance spectrometer in **Hall B** at **Jefferson Lab**.
A continuous-wave electron beam from the CEBAF accelerator — polarized, up to
**10.6 GeV** — strikes a fixed target (often liquid hydrogen, i.e. protons). The
scattered electron and the produced hadrons fly out, are bent by magnetic fields,
and are recorded by two detector systems:

- The **Forward Detector (FD)** — drift chambers for tracking (bent by a
  six-coil **torus** magnet), a high-threshold Cherenkov counter (**HTCC**) and
  electromagnetic calorimeters (**PCAL**, **ECIN**, **ECOUT**) for electron/photon
  ID, and time-of-flight scintillators. Covers polar angles ~5–35°.
- The **Central Detector (CD)** — a silicon + micromegas vertex tracker inside a
  **solenoid**, plus time-of-flight. Covers ~35–125°.

You don't need the hardware details to analyse data, but two things matter later:
which detector a particle went through (it sets what information you have on it),
and the **beam energy** (which you'll need for kinematics — and which, as we'll
see, is *not* stored in the file).

## From beam to file: the reconstruction chain

1. The detectors record raw signals for each **event** (one beam–target interaction).
2. Offline **reconstruction** (the CLAS12 `coatjava` software) turns those signals
   into tracks, clusters, and finally a list of **particles** per event.
3. The result is written as a **DST** (Data Summary Tape) — a file in the **HIPO**
   format, the CLAS12 binary container. This is what you analyse.

`oxihipo` reads HIPO files. It does *not* do reconstruction or physics — it gets
the reconstructed columns into Python as fast as possible.

## Banks: the columns of a DST

A HIPO file is organised into **banks**. A bank is a named table with typed
columns, filled once per event — think of it as one Arrow/pandas table per event,
stacked. The ones this tutorial uses:

| Bank | One row per… | Key columns |
|---|---|---|
| `RUN::config` | event | `run`, `torus`, `solenoid` (magnet polarities) |
| `REC::Particle` | reconstructed particle | `pid`, `px/py/pz`, `vx/vy/vz`, `charge`, `beta`, `chi2pid`, `status` |
| `REC::Calorimeter` | calorimeter cluster | `pindex`, `layer`, `energy`, `sector`, `lv/lw` |
| `REC::Cherenkov` | Cherenkov hit | `pindex`, `detector`, `nphe` |

Real DSTs have dozens more (`REC::Track`, `REC::Scintillator`, `REC::Traj`,
`RUN::scaler`, `MC::*` for simulation, …). `oxihipo` reads them all —
`f.keys()` lists what a given file actually has.

`REC::Particle` is the heart of an analysis: one row per particle the
reconstruction found, with its identity and momentum. The other `REC::*` banks
hold the **detector-level** information that backs up (or overrides) that identity.

### `pid`: what a particle is

`pid` is the **PDG particle code**. The ones you'll meet constantly:

| pid | particle | | pid | particle |
|---|---|---|---|---|
| `11` | electron | | `2212` | proton |
| `-11` | positron | | `2112` | neutron |
| `22` | photon | | `211` / `-211` | $\pi^+$ / $\pi^-$ |
| `321` / `-321` | $K^+$ / $K^-$ | | `45` | deuteron |

The reconstruction assigns `pid` from timing, tracking, and calorimetry. It's a
*best guess* — a serious analysis re-checks it with the detector banks
([Detector banks & PID](./detector-and-pid.md)).

### `pindex`: how the banks link

Here's the one idea that trips everyone up. Detector banks don't repeat a
particle's momentum; instead each of their rows carries a **`pindex`** — the row
number of the particle it belongs to, *within the same event's* `REC::Particle`.

```
event 42   REC::Particle          REC::Calorimeter
           row 0  e-  (pid 11)     pindex=0  layer=1  energy=0.31   ← electron's PCAL
           row 1  π+  (pid 211)    pindex=0  layer=4  energy=0.72   ← electron's ECIN
           row 2  p   (pid 2212)   pindex=2  layer=1  energy=0.05   ← proton's cluster
```

So "what was the electron's calorimeter energy?" means "sum `REC::Calorimeter.energy`
over the rows whose `pindex` equals the electron's row number." That join is a core
skill — [Detector banks & PID](./detector-and-pid.md) is all about it.

## The event model

Every event has a *different* number of particles — 1, 5, 0, 12. That
variable-length structure is why CLAS12 data is **jagged**, and why `oxihipo`
hands you [Awkward arrays](https://awkward-array.org) rather than flat NumPy: an
Awkward array holds one sub-list per event without padding or Python loops.

```python
import oxihipo as ox
f = ox.open("clas12_tutorial.hipo")
p = f.arrays("REC::Particle", ["pid", "px"])
p.pid            # a jagged array: p.pid[event] is that event's list of pids
```

`p.pid[42]` is event 42's pids; `ak.num(p.pid)` is the per-event particle count;
`ak.flatten(p.pid)` drops the event structure to one long list. You operate on
*all events at once* with array expressions — never a `for` loop over events.
[First look](./first-look.md) makes this concrete.

## Where real data comes from

Real CLAS12 DSTs are produced per **run period** (RG-A, RG-B, RG-K, …) and stored
on Jefferson Lab computing (`/cache`, `/volatile`, tape) and mirrored to sites
like ifarm. Access requires CLAS collaboration membership; the run groups, their
beam energies and target types are documented in the collaboration's wiki and
run-condition database (RCDB). A few gotchas that bite newcomers:

- **Beam energy is not in the file.** You set it from the run period (e.g. 10.604
  GeV for RG-A inbending). `RUN::config` gives you the magnet polarities (`torus`
  = ±1) but not the beam energy.
- **Inbending vs outbending.** The torus polarity (`torus`) determines whether
  negative particles curl toward or away from the beamline — it changes acceptance
  and fiducial cuts.
- **One "cooking" ≠ another.** Reconstruction (`recon`) versions differ; stick to
  one pass for a given analysis.

That's the whole map. Next we actually open a file.

[First look →](./first-look.md)
