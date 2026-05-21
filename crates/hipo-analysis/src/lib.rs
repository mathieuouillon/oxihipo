//! `hipo-analysis` — an algorithm-based analysis framework for HIPO data.
//!
//! Built on the [`hipo`] reader, this crate adds the *analysis layer*: a
//! way to express a physics analysis as an ordered chain of **algorithms**
//! run on every event, in the style of the LHC experiment frameworks
//! (CMSSW modules, Gaudi algorithms, JLab's JANA).
//!
//! # The model
//!
//! - An [`Algorithm`] is the first-class unit — a `#[derive(Clone)]` struct
//!   implementing one [`process`](Algorithm::process) method.
//! - An [`Analysis`] is an ordered sequence of algorithms. Each event is
//!   passed through the chain; an algorithm returning [`Flow::Skip`] drops
//!   the event so later algorithms never see it.
//! - Algorithms share derived data through a typed per-event [`Context`]
//!   store (`ctx.put(electron)` / `ctx.get::<Electron>()`).
//! - Results — [`Hist1D`] / [`Hist2D`] and counters — accumulate in a
//!   mergeable [`Output`]; the **cut-flow** is tracked automatically.
//! - [`Analysis::run`] drives the whole chain over a [`hipo::Chain`] in
//!   parallel and returns a [`Report`].
//!
//! See `examples/analysis.rs` for a complete worked analysis.
//!
//! ```ignore
//! use hipo_analysis::prelude::*;
//!
//! let report = Analysis::new()
//!     .then(RequireParticles)
//!     .then(FindElectron)
//!     .then(ElectronKinematics)
//!     .run(&Chain::open("data/*.hipo")?, 0)?;
//! println!("{}", report.cutflow);
//! ```

#![forbid(unsafe_code)]

mod algorithm;
mod analysis;
mod context;
mod hist;
mod kinematics;
mod output;
mod skim;

pub mod prelude;

pub use crate::algorithm::{Algorithm, Flow};
pub use crate::analysis::Analysis;
pub use crate::context::Context;
pub use crate::hist::{Hist1D, Hist2D};
pub use crate::kinematics::{LorentzVector, consts};
pub use crate::output::{CutFlow, Output, Report};
pub use crate::skim::{SkimStats, skim};
