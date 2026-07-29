//! Ready-made `bank_row!` types for common CLAS12 `REC::*` banks, plus a
//! tiny scan that decodes both as typed rows.
//!
//! ```sh
//! cargo run --release --example clas12 -- /tmp/demo.hipo
//! ```
//!
//! Wire `(group, item)` ids and columns match the standard CLAS12
//! reconstruction dictionary. Copy a struct from here, or roll your own for
//! any bank in a few lines with `oxihipo::bank_row!` — the library ships no
//! detector-specific banks, only the macro that generates them.

use std::env;

use oxihipo::{Chain, Result};

oxihipo::bank_row! {
    /// One row of `REC::Particle` — reconstructed particle kinematics.
    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    struct RecParticle for "REC::Particle" @ (300, 31) {
        pid: i32 => "pid",
        px: f32 => "px",
        py: f32 => "py",
        pz: f32 => "pz",
        vx: f32 => "vx",
        vy: f32 => "vy",
        vz: f32 => "vz",
        vt: f32 => "vt",
        charge: i8 => "charge",
        beta: f32 => "beta",
        chi2pid: f32 => "chi2pid",
        status: i16 => "status",
    }
}

oxihipo::bank_row! {
    /// One row of `REC::Event` — the event-level reconstruction summary.
    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    struct RecEvent for "REC::Event" @ (300, 30) {
        category: i64 => "category",
        topology: i64 => "topology",
        beam_charge: f32 => "beamCharge",
        live_time: f64 => "liveTime",
        start_time: f32 => "startTime",
        rf_time: f32 => "RFTime",
        helicity: i8 => "helicity",
        helicity_raw: i8 => "helicityRaw",
        proc_time: f32 => "procTime",
    }
}

fn main() -> Result<()> {
    let path = env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/demo.hipo".to_string());
    let chain = Chain::open(&path)?;

    let mut events = 0u64;
    let mut particles = 0u64;
    let mut electrons = 0u64;
    for ev in chain.events() {
        let ev = ev?;
        events += 1;

        // REC::Event is one row per event; decode it as a typed row.
        if events <= 1
            && let Some(e) = ev.rows::<RecEvent>().next()
        {
            println!("REC::Event[0]: {e:?}");
        }

        // REC::Particle is many rows per event.
        for p in ev.rows::<RecParticle>() {
            particles += 1;
            if p.pid == 11 {
                electrons += 1;
            }
            if events <= 1 && particles <= 3 {
                println!("  particle: {p:?}");
            }
        }
    }

    println!("{events} events, {particles} REC::Particle rows, {electrons} electrons");
    Ok(())
}
