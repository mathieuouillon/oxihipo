//! Ready-made [`BankRow`](crate::event::BankRow) types for common CLAS12
//! `REC::*` banks, so the typed-row path works with no setup:
//!
//! ```ignore
//! use oxihipo::clas12::RecParticle;
//!
//! for ev in chain.events() {
//!     let ev = ev?;
//!     for p in ev.rows::<RecParticle>() {
//!         if p.pid == 11 { /* electron */ }
//!     }
//! }
//! ```
//!
//! Wire `(group, item)` ids and columns match the standard CLAS12
//! reconstruction dictionary. Need another bank, or a column not mapped
//! here? Declare your own in three lines with
//! [`bank_row!`](crate::bank_row).

crate::bank_row! {
    /// One row of `REC::Particle` — reconstructed particle kinematics.
    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    pub struct RecParticle for "REC::Particle" @ (300, 31) {
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

crate::bank_row! {
    /// One row of `REC::Event` — the event-level reconstruction summary.
    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    pub struct RecEvent for "REC::Event" @ (300, 30) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::BankRow;

    #[test]
    fn ids_match_clas12_dictionary() {
        assert_eq!(RecParticle::NAME, "REC::Particle");
        assert_eq!((RecParticle::GROUP, RecParticle::ITEM), (300, 31));
        assert_eq!(RecEvent::NAME, "REC::Event");
        assert_eq!((RecEvent::GROUP, RecEvent::ITEM), (300, 30));
    }
}
