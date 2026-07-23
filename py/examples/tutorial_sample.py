"""Generate a small, CLAS12-shaped HIPO file for the Python tutorial.

    python tutorial_sample.py [OUT.hipo] [n_events]

This is **synthetic, illustrative data** — not a physics simulation. It is built
so the tutorial's code runs end to end and the distributions look sensible
(a real DIS Q^2-x_B correlation, an electron sampling-fraction band near 0.25,
a pi0 peak at 0.135 GeV), but it has none of the detector effects, backgrounds,
or correlations of real CLAS12 data. Use it to learn the mechanics; run the same
code on real DSTs for physics.

Banks written (a realistic subset):
  RUN::config       run, torus, solenoid                      (one row / event)
  REC::Particle     pid px py pz vx vy vz vt charge beta chi2pid status
  REC::Calorimeter  pindex detector sector layer energy lv lw  (rows link to
                    a REC::Particle row via `pindex`)
  REC::Cherenkov    pindex detector nphe
"""

import os
import sys
import tempfile

import awkward as ak
import numpy as np

import oxihipo as ox

BEAM = 10.604        # GeV — RG-A inbending beam energy (NOT stored in the file)
M_P = 0.938272       # proton mass
M_N = 0.939565       # neutron mass
M_PIP = 0.139570     # charged pion
M_PI0 = 0.134977     # neutral pion


def _dis_electron(rng, n):
    """Sample `n` scattered electrons with a valid DIS phase space.

    Returns (p, theta, phi, Q2, W, xB, nu) arrays. We sample the scattering
    angle and scattered energy, compute the kinematics, and reject events
    outside the deep-inelastic region (Q^2 > 1, W > 2)."""
    p, th, ph, Q2, W, xB, nu = (np.empty(n) for _ in range(7))
    filled = 0
    while filled < n:
        k = n - filled
        theta = np.deg2rad(rng.uniform(5.0, 35.0, k))          # forward electron
        phi = rng.uniform(-np.pi, np.pi, k)
        eprime = rng.uniform(0.6, BEAM - 0.5, k)               # scattered energy
        q2 = 4.0 * BEAM * eprime * np.sin(theta / 2.0) ** 2
        v = BEAM - eprime                                       # energy transfer nu
        w2 = M_P**2 + 2.0 * M_P * v - q2
        xb = np.where(v > 0, q2 / (2.0 * M_P * v), -1.0)
        ok = (q2 > 1.0) & (w2 > 4.0) & (xb > 0) & (xb < 1.0)
        m = int(ok.sum())
        s = slice(filled, filled + m)
        p[s], th[s], ph[s] = eprime[ok], theta[ok], phi[ok]    # p ~= E' (massless)
        Q2[s], W[s], xB[s], nu[s] = q2[ok], np.sqrt(w2[ok]), xb[ok], v[ok]
        filled += m
    return p, th, ph, Q2, W, xB, nu


def _pxyz(p, theta, phi):
    return (p * np.sin(theta) * np.cos(phi),
            p * np.sin(theta) * np.sin(phi),
            p * np.cos(theta))


def _two_body(rng, P4, m1, m2):
    """Decay a parent 4-vector ``P4 = (E, px, py, pz)`` into daughters of mass
    ``m1``, ``m2`` — isotropic in the parent rest frame, boosted to the lab.
    Returns two ``(E, px, py, pz)`` tuples. Drives both pi0 -> gamma gamma and
    the exclusive gamma*p -> pi+ n below, so the mass peaks are physically real."""
    E, px, py, pz = P4
    M2 = E * E - px * px - py * py - pz * pz
    M = np.sqrt(max(M2, 1e-9))
    E1 = (M2 + m1 * m1 - m2 * m2) / (2 * M)
    pstar = np.sqrt(max(E1 * E1 - m1 * m1, 0.0))
    ct = rng.uniform(-1, 1)
    st = np.sqrt(1 - ct * ct)
    az = rng.uniform(-np.pi, np.pi)
    d1 = np.array([E1, pstar * st * np.cos(az), pstar * st * np.sin(az), pstar * ct])
    d2 = np.array([(M2 + m2 * m2 - m1 * m1) / (2 * M), -d1[1], -d1[2], -d1[3]])
    beta = np.array([px, py, pz]) / E
    b2 = float(beta @ beta)
    gamma = E / M
    out = []
    for d in (d1, d2):
        bp = beta[0] * d[1] + beta[1] * d[2] + beta[2] * d[3]
        fac = ((gamma - 1.0) * bp / b2 + gamma * d[0]) if b2 > 0 else 0.0
        out.append((gamma * (d[0] + bp),
                    d[1] + fac * beta[0], d[2] + fac * beta[1], d[3] + fac * beta[2]))
    return out


def generate(path, n_events=20000, seed=1):
    rng = np.random.default_rng(seed)
    ep_p, ep_th, ep_ph, Q2, W, xB, nu = _dis_electron(rng, n_events)

    # per-event column accumulators (flat, with a counts array per bank)
    P = {k: [] for k in
         ("pid", "px", "py", "pz", "vx", "vy", "vz", "vt", "charge", "beta", "chi2pid", "status")}
    C = {k: [] for k in ("pindex", "detector", "sector", "layer", "energy", "lv", "lw")}
    H = {k: [] for k in ("pindex", "detector", "nphe")}
    n_part, n_cal, n_cher = [], [], []

    def add_particle(pid, px, py, pz, charge, beta, chi2, status, vz):
        # fold in a crude detector resolution so the mass peaks have a realistic
        # width (tracks ~0.7% in |p|, calorimeter photons ~6% in E) instead of
        # being delta functions — this is what makes a "cut a window" lesson real.
        s = rng.normal(1.0, 0.06 if pid == 22 else 0.007)
        px, py, pz = px * s, py * s, pz * s
        P["pid"].append(pid); P["px"].append(px); P["py"].append(py); P["pz"].append(pz)
        P["vx"].append(rng.normal(0, 0.1)); P["vy"].append(rng.normal(0, 0.1))
        P["vz"].append(vz); P["vt"].append(rng.normal(0, 0.3))
        P["charge"].append(charge); P["beta"].append(beta)
        P["chi2pid"].append(chi2); P["status"].append(status)
        return len(P["pid"]) - 1  # its (global) row index; per-event pindex fixed up later

    def add_calo(pidx, sector, layer, energy):
        C["pindex"].append(pidx); C["detector"].append(7); C["sector"].append(sector)
        C["layer"].append(layer); C["energy"].append(max(energy, 0.0))
        C["lv"].append(rng.uniform(5, 400)); C["lw"].append(rng.uniform(5, 400))

    def add_htcc(pidx, nphe):
        H["pindex"].append(pidx); H["detector"].append(15); H["nphe"].append(nphe)

    base_part = base_cal = base_cher = 0
    for i in range(n_events):
        p0, p1, p2 = len(P["pid"]), len(C["pindex"]), len(H["pindex"])
        sector = int(rng.integers(1, 7))
        vz_e = rng.normal(-2.0, 2.5)

        # --- scattered electron (the trigger particle, row 0) ---
        ex, ey, ez = _pxyz(ep_p[i], ep_th[i], ep_ph[i])
        e_row = add_particle(11, ex, ey, ez, -1, rng.normal(1.0, 0.005),
                             rng.normal(0, 0.6), -2000 - sector, vz_e)
        sf = rng.normal(0.248, 0.018)                       # ECAL sampling fraction
        etot = sf * ep_p[i]
        add_calo(e_row, sector, 1, etot * rng.uniform(0.35, 0.5))   # PCAL
        add_calo(e_row, sector, 4, etot * rng.uniform(0.3, 0.4))    # ECIN
        add_calo(e_row, sector, 7, etot * rng.uniform(0.15, 0.3))   # ECOUT
        add_htcc(e_row, rng.normal(13, 4))                          # HTCC photoelectrons

        # --- a pi+ hadron: exclusive ep->e'pi+n (30%) or SIDIS (55%) ---
        # In the exclusive branch the pi+ recoils against an (undetected) neutron,
        # so the missing mass of (e', pi+) peaks at the neutron mass; the SIDIS
        # branch has extra unseen particles, giving a continuum above it.
        r_had = rng.random()
        if r_had < 0.30:                                           # exclusive pi+ n
            # virtual-photon-target system  W = beam + target - e'
            W4 = (BEAM + M_P - ep_p[i], -ex, -ey, BEAM - ez)
            (Eh, hx, hy, hz), _neutron = _two_body(rng, W4, M_PIP, M_N)
            ph = np.sqrt(hx * hx + hy * hy + hz * hz)
            h_row = add_particle(211, hx, hy, hz, 1, ph / Eh,
                                 rng.normal(0, 1.2), 2000 + sector, rng.normal(vz_e, 0.8))
            add_calo(h_row, sector, 1, rng.uniform(0.02, 0.12))
            add_htcc(h_row, max(rng.normal(0.4, 0.5), 0.0))
        elif r_had < 0.85:                                         # SIDIS pi+
            z = rng.uniform(0.2, 0.8)
            eh = z * nu[i]
            ph = np.sqrt(max(eh**2 - M_PIP**2, 1e-4))
            hx, hy, hz = _pxyz(ph, np.deg2rad(rng.uniform(5, 40)), rng.uniform(-np.pi, np.pi))
            h_row = add_particle(211, hx, hy, hz, 1, ph / eh,
                                 rng.normal(0, 1.5), 2000 + sector, rng.normal(vz_e, 0.8))
            add_calo(h_row, sector, 1, rng.uniform(0.02, 0.12))     # MIP-like deposit
            add_htcc(h_row, max(rng.normal(0.4, 0.5), 0.0))         # below e- threshold

        # --- a proton in ~40% of events ---
        if rng.random() < 0.40:
            pp = rng.uniform(0.4, 3.0)
            px, py, pz = _pxyz(pp, np.deg2rad(rng.uniform(10, 60)), rng.uniform(-np.pi, np.pi))
            add_particle(2212, px, py, pz, 1, pp / np.sqrt(pp**2 + M_P**2),
                         rng.normal(0, 2), 4000 + sector, rng.normal(vz_e, 1.0))

        # --- a pi0 -> gamma gamma in ~45% of events (two photons in ECAL) ---
        if rng.random() < 0.45:
            ppi = rng.uniform(1.0, 5.0)
            pi0 = (np.sqrt(ppi**2 + M_PI0**2),
                   *_pxyz(ppi, np.deg2rad(rng.uniform(5, 25)), rng.uniform(-np.pi, np.pi)))
            for (E, x, y, z) in _two_body(rng, pi0, 0.0, 0.0):      # two photons
                g_row = add_particle(22, x, y, z, 0, 1.0, 9999.0, -2000 - sector, vz_e)
                add_calo(g_row, sector, 1, E * rng.uniform(0.9, 1.0))   # photon cluster

        # fix per-event pindex (detector banks index the *event-local* particle row)
        for j in range(p1, len(C["pindex"])):
            C["pindex"][j] -= p0
        for j in range(p2, len(H["pindex"])):
            H["pindex"][j] -= p0
        n_part.append(len(P["pid"]) - p0)
        n_cal.append(len(C["pindex"]) - p1)
        n_cher.append(len(H["pindex"]) - p2)

    # --- assemble jagged columns and write ---
    def jag(d, counts, dtypes):
        return {k: ak.unflatten(np.asarray(v, dtype=dtypes[k]), counts) for k, v in d.items()}

    pdt = dict(pid="i4", px="f4", py="f4", pz="f4", vx="f4", vy="f4", vz="f4",
               vt="f4", charge="i1", beta="f4", chi2pid="f4", status="i2")
    cdt = dict(pindex="i2", detector="i1", sector="i1", layer="i1", energy="f4", lv="f4", lw="f4")
    hdt = dict(pindex="i2", detector="i1", nphe="f4")

    with ox.create(path, compression="lz4percolumn") as w:
        w.new_bank("RUN::config", {"run": "I", "torus": "F", "solenoid": "F"})
        w.new_bank("REC::Particle", {"pid": "I", "px": "F", "py": "F", "pz": "F",
                                     "vx": "F", "vy": "F", "vz": "F", "vt": "F",
                                     "charge": "B", "beta": "F", "chi2pid": "F", "status": "S"})
        w.new_bank("REC::Calorimeter", {"pindex": "S", "detector": "B", "sector": "B",
                                        "layer": "B", "energy": "F", "lv": "F", "lw": "F"})
        w.new_bank("REC::Cherenkov", {"pindex": "S", "detector": "B", "nphe": "F"})
        w.extend({
            "RUN::config": {
                "run": np.full(n_events, 5197, dtype=np.int32),
                "torus": np.full(n_events, -1.0, dtype=np.float32),     # inbending
                "solenoid": np.full(n_events, -1.0, dtype=np.float32),
            },
            "REC::Particle": jag(P, n_part, pdt),
            "REC::Calorimeter": jag(C, n_cal, cdt),
            "REC::Cherenkov": jag(H, n_cher, hdt),
        })
    return path


if __name__ == "__main__":
    out = sys.argv[1] if len(sys.argv) > 1 else os.path.join(tempfile.gettempdir(), "clas12_tutorial.hipo")
    n = int(sys.argv[2]) if len(sys.argv) > 2 else 20000
    generate(out, n)
    f = ox.open(out)
    print(f"wrote {f.num_entries} events, {os.path.getsize(out) / 1e6:.1f} MB -> {out}")
    f.show()
