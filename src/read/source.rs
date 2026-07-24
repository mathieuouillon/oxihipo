//! `IntoSources` — the single input-resolution trait behind
//! [`Chain::open`](crate::Chain::open).
//!
//! One opinionated constructor accepts every shape of input:
//!
//! - a **single path** (`&str` / `String` / `&Path` / `PathBuf`) is
//!   auto-detected — an existing **file** opens as itself, an existing
//!   **directory** expands to its `*.hipo` children (sorted), and anything
//!   else is treated as a **glob** pattern (e.g. `"data/*.hipo"`);
//! - an **explicit list** (`&[P]` / `Vec<P>` / `[P; N]` where
//!   `P: AsRef<Path>`) is taken verbatim, in the given order.

use std::path::{Path, PathBuf};

use crate::error::{HipoError, Result};

mod sealed {
    pub trait Sealed {}
}

/// Resolve a [`Chain::open`](crate::Chain::open) argument into the ordered
/// list of `.hipo` files to open. Sealed: the crate owns every impl.
///
/// A single path (`&str` / `String` / `&Path` / `PathBuf`, by value or
/// ref) is auto-detected — an existing file opens as itself, a directory
/// expands to its sorted `*.hipo` children, and anything else is treated
/// as a glob. A slice / array / `Vec` of paths is taken verbatim, in order.
pub trait IntoSources: sealed::Sealed {
    /// Resolve to the ordered list of files to open.
    fn into_sources(self) -> Result<Vec<PathBuf>>;
}

/// Auto-detect a single path: existing file → that file; existing
/// directory → its `*.hipo` children; otherwise treat it as a glob.
fn resolve_one(path: &Path) -> Result<Vec<PathBuf>> {
    if path.is_dir() {
        return resolve_dir(path);
    }
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    // Not present on disk as a file or dir. A path containing glob
    // metacharacters is treated as a pattern; a wildcard-free path is a
    // mistake (typo, wrong cwd) — error rather than silently returning an
    // empty chain (0 events), which is a sharp footgun for a reader.
    match path.to_str() {
        Some(pattern) if pattern.contains(['*', '?', '[']) => resolve_glob(pattern),
        _ => Err(HipoError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no such file or directory: {}", path.display()),
        ))),
    }
}

/// Every `*.hipo` file in `dir` (case-insensitive), sorted by path.
/// Non-recursive.
fn resolve_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("hipo"))
        })
        .collect();
    paths.sort();
    Ok(paths)
}

/// Every file matching the glob `pattern` (e.g. `"data/*.hipo"`), sorted
/// by path. A pattern that matches nothing yields an empty list; a
/// malformed pattern returns [`HipoError::InvalidGlob`].
fn resolve_glob(pattern: &str) -> Result<Vec<PathBuf>> {
    let entries = glob::glob(pattern).map_err(|e| HipoError::InvalidGlob {
        pattern: pattern.to_string(),
        reason: e.to_string(),
    })?;
    let mut paths: Vec<PathBuf> = entries.filter_map(|r| r.ok()).collect();
    paths.sort();
    Ok(paths)
}

// ---- single-path impls (auto-detect file / dir / glob) ------------------

impl sealed::Sealed for &str {}
impl IntoSources for &str {
    fn into_sources(self) -> Result<Vec<PathBuf>> {
        resolve_one(Path::new(self))
    }
}

impl sealed::Sealed for String {}
impl IntoSources for String {
    fn into_sources(self) -> Result<Vec<PathBuf>> {
        resolve_one(Path::new(&self))
    }
}

impl sealed::Sealed for &Path {}
impl IntoSources for &Path {
    fn into_sources(self) -> Result<Vec<PathBuf>> {
        resolve_one(self)
    }
}

impl sealed::Sealed for PathBuf {}
impl IntoSources for PathBuf {
    fn into_sources(self) -> Result<Vec<PathBuf>> {
        resolve_one(&self)
    }
}

impl sealed::Sealed for &PathBuf {}
impl IntoSources for &PathBuf {
    fn into_sources(self) -> Result<Vec<PathBuf>> {
        resolve_one(self.as_path())
    }
}

impl sealed::Sealed for &String {}
impl IntoSources for &String {
    fn into_sources(self) -> Result<Vec<PathBuf>> {
        resolve_one(Path::new(self))
    }
}

// ---- explicit-list impls (taken verbatim, in order) ---------------------

impl<P: AsRef<Path>> sealed::Sealed for Vec<P> {}
impl<P: AsRef<Path>> IntoSources for Vec<P> {
    fn into_sources(self) -> Result<Vec<PathBuf>> {
        Ok(self.into_iter().map(|p| p.as_ref().to_path_buf()).collect())
    }
}

impl<P: AsRef<Path>> sealed::Sealed for &[P] {}
impl<P: AsRef<Path>> IntoSources for &[P] {
    fn into_sources(self) -> Result<Vec<PathBuf>> {
        Ok(self.iter().map(|p| p.as_ref().to_path_buf()).collect())
    }
}

impl<P: AsRef<Path>, const N: usize> sealed::Sealed for [P; N] {}
impl<P: AsRef<Path>, const N: usize> IntoSources for [P; N] {
    fn into_sources(self) -> Result<Vec<PathBuf>> {
        Ok(self.into_iter().map(|p| p.as_ref().to_path_buf()).collect())
    }
}
