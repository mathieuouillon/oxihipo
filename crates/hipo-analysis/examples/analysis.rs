//! A worked CLAS12-style analysis, built from a chain of algorithms.
//!
//! Usage:
//!   cargo run --release -p hipo-analysis --example analysis -- <file|dir|glob>

use hipo_analysis::prelude::*;

/// A typed product published into the per-event [`Context`] store.
struct Electron(LorentzVector);

/// Cut: keep only events with a non-empty `REC::Particle` bank.
#[derive(Clone)]
struct RequireParticles;

impl Algorithm for RequireParticles {
    fn name(&self) -> &str {
        "require-particles"
    }
    fn process(&mut self, ctx: &mut Context<'_>, _out: &mut Output) -> Flow {
        match ctx.event().bank("REC::Particle") {
            Some(p) if p.rows() > 0 => Flow::Continue,
            _ => Flow::Skip,
        }
    }
}

/// Derive: find the scattered electron and publish its four-vector.
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
        let Some(row) = (0..particles.rows()).find(|&r| particles.get::<i32>("pid", r) == 11)
        else {
            return Flow::Skip;
        };
        let Some(electron) = LorentzVector::from_row(&particles, row, M_ELECTRON) else {
            return Flow::Skip;
        };
        ctx.put(Electron(electron));
        Flow::Continue
    }
}

/// Cut: require the scattered electron above a momentum threshold.
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

/// Histogram: fill the scattered-electron kinematics.
#[derive(Clone)]
struct ElectronKinematics;

impl Algorithm for ElectronKinematics {
    fn name(&self) -> &str {
        "electron-kinematics"
    }
    fn process(&mut self, ctx: &mut Context<'_>, out: &mut Output) -> Flow {
        let Some(Electron(e)) = ctx.get::<Electron>() else {
            return Flow::Skip;
        };
        out.h1("momentum", 100, 0.0, 11.0).fill(e.p());
        out.h2("theta-vs-phi", 90, 0.0, 90.0, 180, -180.0, 180.0)
            .fill(e.theta_deg(), e.phi_deg());
        Flow::Continue
    }
}

fn main() -> hipo::Result<()> {
    let path = std::env::args()
        .nth(1)
        .expect("usage: analysis <file|dir|glob>");
    let chain = Chain::open(&path)?;
    println!("analysing {} events", chain.event_count());

    let report = Analysis::new()
        .then(RequireParticles)
        .then(FindElectron)
        .then(ElectronMomentum { min_gev: 0.2 })
        .then(ElectronKinematics)
        .run(&chain, 0)?;

    println!("{}", report.cutflow);
    if let Some(h) = report.output.h1_ref("electron-kinematics", "momentum") {
        println!("electron-momentum histogram: {} entries", h.sum());
    }
    Ok(())
}
