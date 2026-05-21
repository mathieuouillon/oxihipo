//! Histograms — thin wrappers over the [`ndhistogram`] crate.
//!
//! [`Hist1D`] and [`Hist2D`] use uniformly-spaced `f64` axes (with the
//! under/overflow bins `ndhistogram` adds automatically). Both are cheaply
//! *mergeable*: the parallel runner fills one per worker thread and merges
//! them with [`Hist1D::merge`] / [`Hist2D::merge`].

use std::io::BufWriter;
use std::path::Path;

use ndhistogram::axis::Uniform;
use ndhistogram::{AxesTuple, Histogram, VecHistogram, ndhistogram};
use serde::Serialize;

type Inner1 = VecHistogram<AxesTuple<(Uniform<f64>,)>, f64>;
type Inner2 = VecHistogram<AxesTuple<(Uniform<f64>, Uniform<f64>)>, f64>;

/// A 1-D histogram with uniformly-spaced bins.
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct Hist1D {
    inner: Inner1,
}

impl Hist1D {
    /// Create a histogram of `bins` uniform bins spanning `[lo, hi)`.
    ///
    /// # Panics
    /// Panics if `bins == 0` or `lo == hi` — an invalid binning is a
    /// programming error, not a runtime condition.
    pub fn new(bins: usize, lo: f64, hi: f64) -> Self {
        let axis =
            Uniform::new(bins, lo, hi).expect("histogram binning needs bins > 0 and lo != hi");
        Self {
            inner: ndhistogram!(axis),
        }
    }

    /// Fill the bin containing `x` with unit weight.
    pub fn fill(&mut self, x: f64) {
        self.inner.fill(&x);
    }

    /// Fill the bin containing `x` with an explicit `weight`.
    pub fn fill_weighted(&mut self, x: f64, weight: f64) {
        self.inner.fill_with(&x, weight);
    }

    /// The value of the bin containing `x`.
    pub fn value_at(&self, x: f64) -> f64 {
        self.inner.value(&x).copied().unwrap_or(0.0)
    }

    /// Sum of every bin, including under/overflow.
    pub fn sum(&self) -> f64 {
        self.inner.values().sum()
    }

    /// Merge `other` into `self` bin-by-bin — the associative combine step
    /// used by the parallel runner. Both must share the same binning.
    pub fn merge(&mut self, other: &Hist1D) {
        self.inner += &other.inner;
    }

    /// Serialize the histogram to a JSON file.
    pub fn write_json(&self, path: impl AsRef<Path>) -> hipo::Result<()> {
        write_json(self, path.as_ref())
    }
}

/// A 2-D histogram with uniformly-spaced bins on both axes.
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct Hist2D {
    inner: Inner2,
}

impl Hist2D {
    /// Create a 2-D histogram: `nx` bins over `[xlo, xhi)` on the x axis,
    /// `ny` bins over `[ylo, yhi)` on the y axis.
    ///
    /// # Panics
    /// Panics if either axis has zero bins or a zero-width range.
    pub fn new(nx: usize, xlo: f64, xhi: f64, ny: usize, ylo: f64, yhi: f64) -> Self {
        let ax = Uniform::new(nx, xlo, xhi).expect("x binning needs bins > 0 and lo != hi");
        let ay = Uniform::new(ny, ylo, yhi).expect("y binning needs bins > 0 and lo != hi");
        Self {
            inner: ndhistogram!(ax, ay),
        }
    }

    /// Fill the bin containing `(x, y)` with unit weight.
    pub fn fill(&mut self, x: f64, y: f64) {
        self.inner.fill(&(x, y));
    }

    /// Fill the bin containing `(x, y)` with an explicit `weight`.
    pub fn fill_weighted(&mut self, x: f64, y: f64, weight: f64) {
        self.inner.fill_with(&(x, y), weight);
    }

    /// The value of the bin containing `(x, y)`.
    pub fn value_at(&self, x: f64, y: f64) -> f64 {
        self.inner.value(&(x, y)).copied().unwrap_or(0.0)
    }

    /// Sum of every bin, including under/overflow.
    pub fn sum(&self) -> f64 {
        self.inner.values().sum()
    }

    /// Merge `other` into `self` bin-by-bin — the associative combine step
    /// used by the parallel runner. Both must share the same binning.
    pub fn merge(&mut self, other: &Hist2D) {
        self.inner += &other.inner;
    }

    /// Serialize the histogram to a JSON file.
    pub fn write_json(&self, path: impl AsRef<Path>) -> hipo::Result<()> {
        write_json(self, path.as_ref())
    }
}

fn write_json<T: Serialize>(value: &T, path: &Path) -> hipo::Result<()> {
    let file = std::fs::File::create(path)?;
    serde_json::to_writer_pretty(BufWriter::new(file), value).map_err(std::io::Error::other)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_value_and_merge_1d() {
        let mut a = Hist1D::new(10, 0.0, 10.0);
        a.fill(1.5);
        a.fill(1.5);
        a.fill_weighted(8.0, 3.0);
        assert_eq!(a.value_at(1.5), 2.0);
        assert_eq!(a.value_at(8.0), 3.0);
        assert_eq!(a.sum(), 5.0);

        let mut b = Hist1D::new(10, 0.0, 10.0);
        b.fill(1.5);
        a.merge(&b);
        assert_eq!(a.value_at(1.5), 3.0);
        assert_eq!(a.sum(), 6.0);
    }

    #[test]
    fn fill_value_and_merge_2d() {
        let mut a = Hist2D::new(4, 0.0, 4.0, 4, 0.0, 4.0);
        a.fill(1.0, 2.0);
        let mut b = Hist2D::new(4, 0.0, 4.0, 4, 0.0, 4.0);
        b.fill(1.0, 2.0);
        b.fill(3.0, 3.0);
        a.merge(&b);
        assert_eq!(a.value_at(1.0, 2.0), 2.0);
        assert_eq!(a.sum(), 3.0);
    }

    #[test]
    #[should_panic(expected = "binning")]
    fn zero_bins_panics() {
        let _ = Hist1D::new(0, 0.0, 1.0);
    }
}
