//! A worked CLAS12-style analysis, built from a chain of algorithms.
//!
//! This example demonstrates a realistic multi-bank analysis flow:
//! electron + neutral-pion identification with calorimeter quality cuts,
//! plus a couple of kinematic histograms. Each algorithm only touches
//! the banks it needs — combined with `Compression::Lz4ByBank` source
//! files, only the banks named here are ever decompressed.
//!
//! Banks touched:
//! - `REC::Event` — beam helicity & event number (the trigger spine).
//! - `REC::Particle` — pid, px/py/pz for electron + photon candidates.
//! - `REC::Calorimeter` — photon-quality cut (photon must register a
//!   PCAL hit).
//!
//! Run on any HIPO file (`Lz4`, `Lz4Chunked`, `Lz4ByBank` — the reader
//! is format-agnostic and `Lz4ByBank` files automatically benefit from
//! per-bank partial decompression):
//!
//! ```sh
//! cargo run --release -p hipo-analysis --example analysis -- \
//!     /volatile/clas12/$USER/pi0_by_bank/
//! ```

use hipo_analysis::prelude::*;

// ---------------------------------------------------------------------
// Typed products published into the per-event Context store.
// ---------------------------------------------------------------------

struct Electron(LorentzVector);
struct Photons(Vec<LorentzVector>);
struct Pi0Candidate(LorentzVector);

// ---------------------------------------------------------------------
// Cut: require the three banks we'll be touching downstream.
// ---------------------------------------------------------------------

#[derive(Clone)]
struct RequireRecBanks;

impl Algorithm for RequireRecBanks {
    fn name(&self) -> &str {
        "require-rec-banks"
    }
    fn process(&mut self, ctx: &mut Context<'_>, _out: &mut Output) -> Flow {
        if ctx.event().has("REC::Event") && ctx.event().has("REC::Particle") {
            Flow::Continue
        } else {
            Flow::Skip
        }
    }
}

// ---------------------------------------------------------------------
// Identify the scattered electron (pid=11, leading momentum).
// ---------------------------------------------------------------------

#[derive(Clone)]
struct FindElectron;

impl Algorithm for FindElectron {
    fn name(&self) -> &str {
        "find-electron"
    }
    fn process(&mut self, ctx: &mut Context<'_>, _out: &mut Output) -> Flow {
        let Some(particles) = ctx.event().bank("REC::Particle") else {
            return Flow::Skip;
        };
        // Pick the highest-momentum pid=11; CLAS12 reconstruction
        // already orders status<0 (forward-tagger) before the rest,
        // so this is essentially "the trigger electron".
        let mut best: Option<(u32, f64)> = None;
        for r in 0..particles.rows() {
            if particles.get::<i32>("pid", r) != 11 {
                continue;
            }
            let Some(e) = LorentzVector::from_row(&particles, r, M_ELECTRON) else {
                continue;
            };
            if best.is_none_or(|(_, p)| e.p() > p) {
                best = Some((r, e.p()));
            }
        }
        let Some((row, _)) = best else {
            return Flow::Skip;
        };
        let electron =
            LorentzVector::from_row(&particles, row, M_ELECTRON).expect("validated above");
        ctx.put(Electron(electron));
        Flow::Continue
    }
}

// ---------------------------------------------------------------------
// Cut: trigger electron momentum threshold.
// ---------------------------------------------------------------------

#[derive(Clone)]
struct ElectronMomentum {
    min_gev: f64,
}

impl Algorithm for ElectronMomentum {
    fn name(&self) -> &str {
        "electron-momentum"
    }
    fn process(&mut self, ctx: &mut Context<'_>, _out: &mut Output) -> Flow {
        let Some(Electron(e)) = ctx.get::<Electron>() else {
            return Flow::Skip;
        };
        if e.p() > self.min_gev {
            Flow::Continue
        } else {
            Flow::Skip
        }
    }
}

// ---------------------------------------------------------------------
// Find photon candidates (pid=22), filter on a calorimeter quality cut.
// ---------------------------------------------------------------------

#[derive(Clone)]
struct FindPhotons {
    min_energy_gev: f64,
}

impl Algorithm for FindPhotons {
    fn name(&self) -> &str {
        "find-photons"
    }
    fn process(&mut self, ctx: &mut Context<'_>, _out: &mut Output) -> Flow {
        let Some(particles) = ctx.event().bank("REC::Particle") else {
            return Flow::Skip;
        };
        // Per-photon calorimeter quality: require at least one PCAL hit
        // (REC::Calorimeter layer == 1) associated with the particle row.
        let cal = ctx.event().bank("REC::Calorimeter");
        let particle_rows: Vec<u32> = if let Some(c) = cal {
            (0..c.rows())
                .filter(|&r| c.get::<i32>("layer", r) == 1)
                .map(|r| c.get::<i16>("pindex", r) as u32)
                .collect()
        } else {
            Vec::new()
        };

        let mut photons: Vec<LorentzVector> = Vec::with_capacity(4);
        for r in 0..particles.rows() {
            if particles.get::<i32>("pid", r) != 22 {
                continue;
            }
            if !particle_rows.contains(&r) {
                // Photon without a PCAL hit — likely a calorimeter
                // junk cluster; drop it.
                continue;
            }
            let Some(g) = LorentzVector::from_row(&particles, r, 0.0) else {
                continue;
            };
            if g.e < self.min_energy_gev {
                continue;
            }
            photons.push(g);
        }
        if photons.is_empty() {
            return Flow::Skip;
        }
        ctx.put(Photons(photons));
        Flow::Continue
    }
}

// ---------------------------------------------------------------------
// Reconstruct a π⁰ candidate: the two-photon pair closest to m_π0.
// ---------------------------------------------------------------------

#[derive(Clone)]
struct ReconstructPi0 {
    mass_window: (f64, f64),
}

impl Algorithm for ReconstructPi0 {
    fn name(&self) -> &str {
        "reconstruct-pi0"
    }
    fn process(&mut self, ctx: &mut Context<'_>, _out: &mut Output) -> Flow {
        let Some(Photons(photons)) = ctx.get::<Photons>() else {
            return Flow::Skip;
        };
        if photons.len() < 2 {
            return Flow::Skip;
        }
        // PI0 mass in GeV. Not in the crate's standard constants; the
        // mass of the neutral pion is well-known ≈ 0.135 GeV.
        const M_PI0: f64 = 0.134_977;

        let mut best: Option<(LorentzVector, f64)> = None;
        for i in 0..photons.len() {
            for j in (i + 1)..photons.len() {
                let pair = photons[i] + photons[j];
                let dm = (pair.mass() - M_PI0).abs();
                if best.is_none_or(|(_, prev)| dm < prev) {
                    best = Some((pair, dm));
                }
            }
        }
        let Some((pair, _)) = best else {
            return Flow::Skip;
        };
        let m = pair.mass();
        if m < self.mass_window.0 || m > self.mass_window.1 {
            return Flow::Skip;
        }
        ctx.put(Pi0Candidate(pair));
        Flow::Continue
    }
}

// ---------------------------------------------------------------------
// Final histograms.
// ---------------------------------------------------------------------

#[derive(Clone)]
struct FillKinematics;

impl Algorithm for FillKinematics {
    fn name(&self) -> &str {
        "fill-kinematics"
    }
    fn process(&mut self, ctx: &mut Context<'_>, out: &mut Output) -> Flow {
        let Some(Electron(e)) = ctx.get::<Electron>() else {
            return Flow::Skip;
        };
        let Some(Pi0Candidate(pi0)) = ctx.get::<Pi0Candidate>() else {
            return Flow::Skip;
        };

        out.h1("electron-momentum-gev", 100, 0.0, 11.0).fill(e.p());
        out.h1("electron-theta-deg", 90, 0.0, 45.0)
            .fill(e.theta_deg());
        out.h1("pi0-mass-gev", 60, 0.10, 0.18).fill(pi0.mass());
        out.h1("pi0-energy-gev", 100, 0.0, 11.0).fill(pi0.e);
        out.h2("electron-theta-phi", 90, 0.0, 45.0, 180, -180.0, 180.0)
            .fill(e.theta_deg(), e.phi_deg());
        Flow::Continue
    }
}

// ---------------------------------------------------------------------

fn main() -> hipo::Result<()> {
    let path = std::env::args()
        .nth(1)
        .expect("usage: analysis <file|dir|glob>");
    let chain = Chain::open(&path)?;
    println!(
        "analysing {} events from {}",
        chain.event_count(),
        chain
            .file_header()
            .map(|_| path.as_str())
            .unwrap_or("(empty chain)")
    );

    let report = Analysis::new()
        .then(RequireRecBanks)
        .then(FindElectron)
        .then(ElectronMomentum { min_gev: 2.0 })
        .then(FindPhotons {
            min_energy_gev: 0.2,
        })
        .then(ReconstructPi0 {
            mass_window: (0.10, 0.18),
        })
        .then(FillKinematics)
        .run(&chain, 0)?;

    println!("\n{}", report.cutflow);
    if let Some(h) = report.output.h1_ref("fill-kinematics", "pi0-mass-gev") {
        println!(
            "π⁰ mass histogram: {} entries, peak near {:.4} GeV",
            h.sum(),
            0.135,
        );
    }
    Ok(())
}
