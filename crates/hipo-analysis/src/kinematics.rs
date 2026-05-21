//! Derived kinematic quantities — a [`LorentzVector`] and the physics
//! helpers analysis algorithms compute from particle banks.

use std::ops::{Add, Sub};

use hipo::Bank;

/// A relativistic four-momentum `(px, py, pz, E)`, in GeV.
///
/// Add four-vectors to form composite systems — `(e + p).mass()` is an
/// invariant mass, `(beam + target - scattered).mass()` a missing mass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LorentzVector {
    /// x momentum component (GeV).
    pub px: f64,
    /// y momentum component (GeV).
    pub py: f64,
    /// z momentum component (GeV).
    pub pz: f64,
    /// energy (GeV).
    pub e: f64,
}

impl LorentzVector {
    /// A four-vector from explicit components.
    pub fn new(px: f64, py: f64, pz: f64, e: f64) -> Self {
        Self { px, py, pz, e }
    }

    /// A four-vector from a three-momentum and a (rest) `mass`; the energy
    /// is `√(p² + mass²)`.
    pub fn with_mass(px: f64, py: f64, pz: f64, mass: f64) -> Self {
        let e = (px * px + py * py + pz * pz + mass * mass).sqrt();
        Self { px, py, pz, e }
    }

    /// Build a four-vector from row `row` of a particle `bank` (reading the
    /// `px` / `py` / `pz` columns) and a particle-hypothesis `mass`.
    ///
    /// Returns `None` if `row` is out of range or the bank lacks any of the
    /// momentum columns.
    pub fn from_row(bank: &Bank<'_>, row: u32, mass: f64) -> Option<Self> {
        if row >= bank.rows() {
            return None;
        }
        let schema = bank.schema();
        schema.column_index("px")?;
        schema.column_index("py")?;
        schema.column_index("pz")?;
        let px = f64::from(bank.get::<f32>("px", row));
        let py = f64::from(bank.get::<f32>("py", row));
        let pz = f64::from(bank.get::<f32>("pz", row));
        Some(Self::with_mass(px, py, pz, mass))
    }

    /// Magnitude of the three-momentum, `√(px² + py² + pz²)`.
    pub fn p(&self) -> f64 {
        (self.px * self.px + self.py * self.py + self.pz * self.pz).sqrt()
    }

    /// Transverse momentum, `√(px² + py²)`.
    pub fn pt(&self) -> f64 {
        (self.px * self.px + self.py * self.py).sqrt()
    }

    /// Invariant mass, `√(E² − p²)` (clamped at 0 against rounding error).
    pub fn mass(&self) -> f64 {
        (self.e * self.e - self.p().powi(2)).max(0.0).sqrt()
    }

    /// Polar angle from the +z axis, in radians (`0..π`).
    pub fn theta(&self) -> f64 {
        self.pt().atan2(self.pz)
    }

    /// Polar angle from the +z axis, in degrees.
    pub fn theta_deg(&self) -> f64 {
        self.theta().to_degrees()
    }

    /// Azimuthal angle, in radians (`−π..π`).
    pub fn phi(&self) -> f64 {
        self.py.atan2(self.px)
    }

    /// Azimuthal angle, in degrees.
    pub fn phi_deg(&self) -> f64 {
        self.phi().to_degrees()
    }
}

impl Add for LorentzVector {
    type Output = LorentzVector;
    fn add(self, rhs: LorentzVector) -> LorentzVector {
        LorentzVector {
            px: self.px + rhs.px,
            py: self.py + rhs.py,
            pz: self.pz + rhs.pz,
            e: self.e + rhs.e,
        }
    }
}

impl Sub for LorentzVector {
    type Output = LorentzVector;
    fn sub(self, rhs: LorentzVector) -> LorentzVector {
        LorentzVector {
            px: self.px - rhs.px,
            py: self.py - rhs.py,
            pz: self.pz - rhs.pz,
            e: self.e - rhs.e,
        }
    }
}

/// Common particle rest masses, in GeV (PDG values, rounded).
pub mod consts {
    /// Electron rest mass.
    pub const M_ELECTRON: f64 = 0.000_510_999;
    /// Charged pion (π±) rest mass.
    pub const M_PION: f64 = 0.139_570_4;
    /// Charged kaon (K±) rest mass.
    pub const M_KAON: f64 = 0.493_677;
    /// Proton rest mass.
    pub const M_PROTON: f64 = 0.938_272_1;
    /// Neutron rest mass.
    pub const M_NEUTRON: f64 = 0.939_565_4;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn momentum_and_mass() {
        let v = LorentzVector::with_mass(3.0, 4.0, 0.0, 0.0);
        assert!((v.p() - 5.0).abs() < 1e-12);
        assert!((v.pt() - 5.0).abs() < 1e-12);
        assert!(v.mass().abs() < 1e-9);
    }

    #[test]
    fn invariant_mass_of_sum() {
        // Two back-to-back 1 GeV massless particles → invariant mass 2 GeV.
        let a = LorentzVector::new(1.0, 0.0, 0.0, 1.0);
        let b = LorentzVector::new(-1.0, 0.0, 0.0, 1.0);
        assert!(((a + b).mass() - 2.0).abs() < 1e-12);
    }

    #[test]
    fn angles() {
        let along_z = LorentzVector::new(0.0, 0.0, 1.0, 1.0);
        assert!(along_z.theta_deg().abs() < 1e-9);

        let transverse = LorentzVector::new(1.0, 0.0, 0.0, 1.0);
        assert!((transverse.theta_deg() - 90.0).abs() < 1e-9);
        assert!(transverse.phi_deg().abs() < 1e-9);
    }
}
