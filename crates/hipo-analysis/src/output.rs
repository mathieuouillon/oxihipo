//! [`Output`] — accumulated, mergeable analysis results, plus the
//! automatic [`CutFlow`] and the [`Report`] the runner returns.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use crate::hist::{Hist1D, Hist2D};

/// Histograms and counters produced by an analysis.
///
/// Entries are keyed by `(algorithm, name)` — each algorithm's histograms
/// live in their own namespace, so two algorithms can both book a `"mass"`
/// histogram without colliding. The parallel runner gives each worker its
/// own `Output` and merges them at the end.
#[derive(Default, Debug)]
pub struct Output {
    current: String,
    h1: BTreeMap<String, BTreeMap<String, Hist1D>>,
    h2: BTreeMap<String, BTreeMap<String, Hist2D>>,
    counters: BTreeMap<String, BTreeMap<String, u64>>,
}

impl Output {
    /// Book-or-get a 1-D histogram in the running algorithm's namespace.
    /// The binning is used only the first time the histogram is booked.
    pub fn h1(&mut self, name: &str, bins: usize, lo: f64, hi: f64) -> &mut Hist1D {
        entry(&mut self.h1, &self.current, name, || {
            Hist1D::new(bins, lo, hi)
        })
    }

    /// Book-or-get a 2-D histogram in the running algorithm's namespace.
    // Booking a 2-D histogram is inherently a name plus two `(bins, lo, hi)`
    // axes — the same shape as ROOT's `TH2` constructor.
    #[allow(clippy::too_many_arguments)]
    pub fn h2(
        &mut self,
        name: &str,
        nx: usize,
        xlo: f64,
        xhi: f64,
        ny: usize,
        ylo: f64,
        yhi: f64,
    ) -> &mut Hist2D {
        entry(&mut self.h2, &self.current, name, || {
            Hist2D::new(nx, xlo, xhi, ny, ylo, yhi)
        })
    }

    /// Book-or-get a `u64` counter in the running algorithm's namespace.
    pub fn count(&mut self, name: &str) -> &mut u64 {
        entry(&mut self.counters, &self.current, name, || 0)
    }

    /// Borrow a booked 1-D histogram by `(algorithm, name)`.
    pub fn h1_ref(&self, algorithm: &str, name: &str) -> Option<&Hist1D> {
        self.h1.get(algorithm)?.get(name)
    }

    /// Borrow a booked 2-D histogram by `(algorithm, name)`.
    pub fn h2_ref(&self, algorithm: &str, name: &str) -> Option<&Hist2D> {
        self.h2.get(algorithm)?.get(name)
    }

    /// Read a counter by `(algorithm, name)`; `0` if it was never booked.
    pub fn counter(&self, algorithm: &str, name: &str) -> u64 {
        self.counters
            .get(algorithm)
            .and_then(|m| m.get(name))
            .copied()
            .unwrap_or(0)
    }

    /// Write every booked histogram to `dir` as `<algorithm>.<name>.json`,
    /// creating `dir` if needed.
    pub fn write_all_json(&self, dir: impl AsRef<Path>) -> hipo::Result<()> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        for (algo, map) in &self.h1 {
            for (name, h) in map {
                h.write_json(dir.join(format!("{algo}.{name}.json")))?;
            }
        }
        for (algo, map) in &self.h2 {
            for (name, h) in map {
                h.write_json(dir.join(format!("{algo}.{name}.json")))?;
            }
        }
        Ok(())
    }

    pub(crate) fn set_current(&mut self, algorithm: &str) {
        self.current.clear();
        self.current.push_str(algorithm);
    }

    pub(crate) fn merge(&mut self, other: Output) {
        merge_maps(&mut self.h1, other.h1, |a, b| a.merge(&b));
        merge_maps(&mut self.h2, other.h2, |a, b| a.merge(&b));
        merge_maps(&mut self.counters, other.counters, |a, b| *a += b);
    }
}

/// Book-or-get an entry in a `(namespace, name)`-keyed nested map. No
/// allocation on the steady-state path — only the first booking of a
/// given key allocates.
fn entry<'m, V>(
    outer: &'m mut BTreeMap<String, BTreeMap<String, V>>,
    namespace: &str,
    name: &str,
    make: impl FnOnce() -> V,
) -> &'m mut V {
    if !outer.contains_key(namespace) {
        outer.insert(namespace.to_string(), BTreeMap::new());
    }
    let inner = outer.get_mut(namespace).expect("just inserted");
    if !inner.contains_key(name) {
        inner.insert(name.to_string(), make());
    }
    inner.get_mut(name).expect("just inserted")
}

fn merge_maps<V>(
    into: &mut BTreeMap<String, BTreeMap<String, V>>,
    from: BTreeMap<String, BTreeMap<String, V>>,
    mut merge: impl FnMut(&mut V, V),
) {
    for (namespace, from_inner) in from {
        let into_inner = into.entry(namespace).or_default();
        for (name, value) in from_inner {
            match into_inner.get_mut(&name) {
                Some(existing) => merge(existing, value),
                None => {
                    into_inner.insert(name, value);
                }
            }
        }
    }
}

/// Per-algorithm event counts: how many events reached each algorithm and
/// how many it passed. Built and maintained automatically by the runner —
/// a cut is simply an algorithm that returns [`Flow::Skip`](crate::Flow).
#[derive(Clone, Debug, Default)]
pub struct CutFlow {
    rows: Vec<CutRow>,
}

#[derive(Clone, Debug)]
struct CutRow {
    name: String,
    reached: u64,
    passed: u64,
}

impl CutFlow {
    pub(crate) fn new<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            rows: names
                .into_iter()
                .map(|name| CutRow {
                    name: name.to_string(),
                    reached: 0,
                    passed: 0,
                })
                .collect(),
        }
    }

    pub(crate) fn reached(&mut self, index: usize) {
        self.rows[index].reached += 1;
    }

    pub(crate) fn passed(&mut self, index: usize) {
        self.rows[index].passed += 1;
    }

    pub(crate) fn merge(&mut self, other: CutFlow) {
        for (a, b) in self.rows.iter_mut().zip(other.rows) {
            a.reached += b.reached;
            a.passed += b.passed;
        }
    }

    /// Events that passed the algorithm named `name`, or `None` if there is
    /// no such algorithm in the chain.
    pub fn passed_count(&self, name: &str) -> Option<u64> {
        self.rows.iter().find(|r| r.name == name).map(|r| r.passed)
    }
}

impl fmt::Display for CutFlow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cut-flow:\n  {:<28} {:>12} {:>12} {:>9}",
            "algorithm", "reached", "passed", "efficiency"
        )?;
        for r in &self.rows {
            let eff = if r.reached > 0 {
                100.0 * r.passed as f64 / r.reached as f64
            } else {
                0.0
            };
            write!(
                f,
                "\n  {:<28} {:>12} {:>12} {:>8.2}%",
                r.name, r.reached, r.passed, eff
            )?;
        }
        Ok(())
    }
}

/// The result of an analysis run: every booked histogram/counter, plus the
/// cut-flow.
#[derive(Debug)]
pub struct Report {
    /// Histograms and counters filled by the analysis.
    pub output: Output,
    /// Per-algorithm reached/passed counts.
    pub cutflow: CutFlow,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespacing_and_merge() {
        let mut a = Output::default();
        a.set_current("algo-x");
        a.h1("h", 10, 0.0, 10.0).fill(5.0);
        *a.count("c") += 2;

        let mut b = Output::default();
        b.set_current("algo-x");
        b.h1("h", 10, 0.0, 10.0).fill(5.0);
        *b.count("c") += 3;

        a.merge(b);
        assert_eq!(a.h1_ref("algo-x", "h").unwrap().sum(), 2.0);
        assert_eq!(a.counter("algo-x", "c"), 5);
        assert!(a.h1_ref("other-algo", "h").is_none());
        assert_eq!(a.counter("other-algo", "c"), 0);
    }

    #[test]
    fn cutflow_counts_and_merges() {
        let mut cf = CutFlow::new(["a", "b"]);
        cf.reached(0);
        cf.passed(0);
        cf.reached(1); // reached "b" but did not pass it

        let mut other = CutFlow::new(["a", "b"]);
        other.reached(0);
        other.passed(0);
        other.reached(1);
        other.passed(1);

        cf.merge(other);
        assert_eq!(cf.passed_count("a"), Some(2));
        assert_eq!(cf.passed_count("b"), Some(1));
        assert_eq!(cf.passed_count("missing"), None);
    }
}
