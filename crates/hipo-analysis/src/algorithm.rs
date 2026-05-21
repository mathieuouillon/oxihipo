//! The [`Algorithm`] trait — the first-class unit of analysis.

use dyn_clone::DynClone;

use crate::context::Context;
use crate::output::Output;

/// What an [`Algorithm`] decides for the current event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Continue to the next algorithm in the chain.
    Continue,
    /// Drop this event — the rest of the chain is skipped for it.
    Skip,
}

/// A single step of an analysis.
///
/// An [`Analysis`](crate::Analysis) is an ordered sequence of `Algorithm`s
/// run on every event, one after another. An algorithm reads the event and
/// the per-event [`Context`] store, may publish derived products into it,
/// fills histograms/counters in [`Output`], and returns a [`Flow`] —
/// returning [`Flow::Skip`] drops the event so later algorithms never see
/// it (and the runner records it in the cut-flow).
///
/// Implement this on a `#[derive(Clone)]` struct — that derive is the only
/// boilerplate; the framework clones the chain once per worker thread.
pub trait Algorithm: DynClone + Send + Sync {
    /// A short, stable name. Used to namespace this algorithm's histograms
    /// and to label its row in the cut-flow report.
    fn name(&self) -> &str;

    /// Process one event.
    fn process(&mut self, ctx: &mut Context<'_>, out: &mut Output) -> Flow;
}

dyn_clone::clone_trait_object!(Algorithm);
