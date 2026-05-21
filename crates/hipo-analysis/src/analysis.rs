//! [`Analysis`] — an ordered chain of algorithms, and the runner that
//! drives it over a [`Chain`] in parallel.

use std::io::IsTerminal;

use hipo::{Chain, EventCtx};
use indicatif::{ProgressBar, ProgressStyle};

use crate::algorithm::{Algorithm, Flow};
use crate::context::Context;
use crate::output::{CutFlow, Output, Report};

/// An ordered sequence of [`Algorithm`]s — the analysis.
///
/// Build one with [`Analysis::new`] and [`Analysis::then`], then drive it
/// with [`run`](Analysis::run) (parallel) or
/// [`run_sequential`](Analysis::run_sequential).
#[derive(Default, Clone)]
pub struct Analysis {
    algorithms: Vec<Box<dyn Algorithm>>,
}

impl Analysis {
    /// An empty analysis.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an algorithm to the chain. Algorithms run in the order added.
    #[must_use]
    pub fn then(mut self, algorithm: impl Algorithm + 'static) -> Self {
        self.algorithms.push(Box::new(algorithm));
        self
    }

    /// Run the analysis over every event of `chain` in parallel.
    /// `threads = 0` lets rayon pick one worker per logical CPU.
    ///
    /// When stderr is a terminal a progress bar is drawn for the duration
    /// of the run; in pipelines and tests it stays silent.
    pub fn run(&self, chain: &Chain, threads: usize) -> hipo::Result<Report> {
        let bar = make_progress_bar(chain.event_count());
        let bar_ref = &bar;
        let state = chain.par_reduce(
            threads,
            || self.new_state(),
            |mut state, event| {
                state.process(event);
                bar_ref.inc(1);
                state
            },
            WorkerState::combined,
        )?;
        bar.finish_and_clear();
        Ok(Report {
            output: state.output,
            cutflow: state.cutflow,
        })
    }

    /// Run the analysis over every event of `chain` on a single thread,
    /// in input order. Shows the same progress bar as [`Self::run`].
    pub fn run_sequential(&self, chain: &Chain) -> hipo::Result<Report> {
        let bar = make_progress_bar(chain.event_count());
        let mut state = self.new_state();
        for event in chain.events() {
            state.process(&event.ctx());
            bar.inc(1);
        }
        bar.finish_and_clear();
        Ok(Report {
            output: state.output,
            cutflow: state.cutflow,
        })
    }

    fn new_state(&self) -> WorkerState {
        WorkerState {
            cutflow: CutFlow::new(self.algorithms.iter().map(|a| a.name())),
            algorithms: self.algorithms.clone(),
            output: Output::default(),
        }
    }
}

/// Per-worker accumulator: a private copy of the algorithm chain plus the
/// results it fills. This is the `H` of [`Chain::par_reduce`].
struct WorkerState {
    algorithms: Vec<Box<dyn Algorithm>>,
    output: Output,
    cutflow: CutFlow,
}

impl WorkerState {
    fn process(&mut self, event: &EventCtx<'_>) {
        let mut ctx = Context::new(*event);
        for (index, algorithm) in self.algorithms.iter_mut().enumerate() {
            self.cutflow.reached(index);
            self.output.set_current(algorithm.name());
            match algorithm.process(&mut ctx, &mut self.output) {
                Flow::Continue => self.cutflow.passed(index),
                Flow::Skip => break,
            }
        }
    }

    fn combined(mut self, other: WorkerState) -> WorkerState {
        self.output.merge(other.output);
        self.cutflow.merge(other.cutflow);
        self
    }
}

/// Build a progress bar over `total` events — drawn when stderr is a
/// terminal, silent otherwise (so tests and pipelines stay clean).
fn make_progress_bar(total: u64) -> ProgressBar {
    if std::io::stderr().is_terminal() {
        let bar = ProgressBar::new(total);
        bar.set_style(
            ProgressStyle::with_template(
                "  {elapsed_precise} [{bar:40.green/dim}] {percent:>3}% \
                 ({pos}/{len}, {per_sec}, eta {eta})",
            )
            .expect("static progress-bar template")
            .progress_chars("=> "),
        );
        bar
    } else {
        ProgressBar::hidden()
    }
}
