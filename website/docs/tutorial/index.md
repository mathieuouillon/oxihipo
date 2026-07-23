---
id: tutorial-index
title: "Tutorial: CLAS12 analysis in Python"
sidebar_label: Overview
sidebar_position: 1
---

# A CLAS12 analysis, from first read to physics

This is a hands-on tutorial for analysing **CLAS12** data in Python with
`oxihipo`. It starts from "what even is a HIPO file" and builds, step by step, to
a real multi-particle analysis: identifying the scattered electron, computing
deep-inelastic kinematics, joining detector banks through `pindex`, and pulling
signals out of invariant- and missing-mass spectra.

It assumes you're **comfortable with Python and NumPy** but **new to CLAS12** —
so it explains the physics vocabulary (banks, `pindex`, PID, DIS variables) as it
goes, and doesn't assume you've seen a HIPO file before.

## What you'll be able to do by the end

- Open real CLAS12 DSTs, find the banks you need, and read columns as arrays.
- Reconstruct four-vectors and identify particles (electrons, pions, protons, photons).
- Compute the inclusive DIS variables — $Q^2$, $\nu$, $x_B$, $W$, $y$.
- Join `REC::Particle` to the detector banks (`REC::Calorimeter`, `REC::Cherenkov`)
  and build a real electron ID from the sampling fraction and Cherenkov response.
- Apply PID, vertex, and fiducial cuts, and know where momentum corrections go.
- Reconstruct a $\pi^0$ from two photons, compute SIDIS kinematics, and isolate an
  exclusive channel with a missing-mass cut.
- Scale the same code from a notebook to a hundred-file batch job.

## The data

Real CLAS12 DSTs are large and live on collaboration storage (see
[CLAS12 & HIPO](./clas12-and-hipo.md#where-real-data-comes-from)). So this
tutorial ships a **small synthetic sample** you can generate in a second, and
every code block runs against it:

```bash
python py/examples/tutorial_sample.py clas12_tutorial.hipo 20000
```

That writes `clas12_tutorial.hipo` — 20 000 events with a realistic subset of
banks (`RUN::config`, `REC::Particle`, `REC::Calorimeter`, `REC::Cherenkov`).

:::warning This data is illustrative, not physics
The sample is *hand-built to have the right shapes* — a real DIS $Q^2$–$x_B$
correlation, an electron sampling-fraction band near 0.25, a $\pi^0$ peak at
0.135 GeV, a neutron missing-mass peak — so the mechanics and the code are real.
But it has **none of the backgrounds, detector effects, acceptance, or physics
correlations** of real data. Learn the techniques here; run the identical code on
real DSTs for physics. Every page flags what changes on real data.
:::

## Setup

```bash
pip install "oxihipo[all]"      # oxihipo + awkward + pandas + pyarrow
pip install matplotlib          # for the plots in this tutorial
```

We use [Awkward Array](https://awkward-array.org) throughout — it's how oxihipo
returns jagged, variable-length-per-event data. If you've used NumPy, the mental
jump is small and [First look](./first-look.md) walks you through it.

## Roadmap

| # | Page | What it covers |
|---|---|---|
| 1 | [CLAS12 & HIPO](./clas12-and-hipo.md) | the detector, DSTs, banks, `pindex`, the event model |
| 2 | [First look](./first-look.md) | open a file, find banks, read columns, the jagged structure |
| 3 | [Particles & selection](./particles-and-kinematics.md) | `REC::Particle`, four-vectors, identifying the electron |
| 4 | [Inclusive DIS](./inclusive-dis.md) | $Q^2$, $\nu$, $x_B$, $W$, $y$ — the first physics result |
| 5 | [Detector banks & PID](./detector-and-pid.md) | `pindex` joins, sampling fraction, fiducial cuts, corrections |
| 6 | [Exclusive channels](./exclusive-channels.md) | $\pi^0\to\gamma\gamma$, SIDIS, missing mass |
| 7 | [Scaling up](./scaling-up.md) | streaming, multi-process, skims, tags, batch jobs |

Start with [CLAS12 & HIPO →](./clas12-and-hipo.md)
