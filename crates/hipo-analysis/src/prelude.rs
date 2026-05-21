//! Common imports for writing an analysis.
//!
//! ```no_run
//! use hipo_analysis::prelude::*;
//! ```

pub use crate::algorithm::{Algorithm, Flow};
pub use crate::analysis::Analysis;
pub use crate::context::Context;
pub use crate::hist::{Hist1D, Hist2D};
pub use crate::kinematics::LorentzVector;
pub use crate::kinematics::consts::*;
pub use crate::output::{CutFlow, Output, Report};
pub use crate::skim::{SkimStats, skim};

// Re-export the `hipo` types an analysis routinely names.
pub use hipo::{Bank, Chain, EventCtx, Filter, Result};
