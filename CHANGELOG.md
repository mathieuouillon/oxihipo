# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While the
version is below `1.0.0`, minor releases may contain breaking changes.

## [Unreleased]

## [0.1.1] - 2026-07-23

A documentation and examples release — **no library code changed**, so the API
and behaviour are identical to 0.1.0. It exists mainly to publish a corrected
PyPI page (a released project description cannot be edited in place).

### Fixed

- **PyPI project page links.** `py/README.md` is the PyPI long description, and
  PyPI resolves relative links against `https://pypi.org/project/oxihipo/` — so
  every `examples/…` link 404'd (e.g. `.../project/oxihipo/examples/`). All 14
  are now absolute GitHub URLs.
- `examples/parallel.py` requested `px,py,pz,pid` unconditionally and so failed
  against the bundled sample (whose `REC::Particle` has only `pid`/`px`/`cov`);
  it now intersects with the file's actual columns.

### Added

- **A CLAS12 analysis tutorial for Python** — eight pages on the docs site, from
  the HIPO/bank data model through particle selection, inclusive DIS kinematics,
  `pindex` detector joins and PID, invariant/missing-mass channels, and scaling
  to a batch job. Every snippet is runnable against a synthetic CLAS12-shaped
  sample produced by the new `py/examples/tutorial_sample.py`.
- Four runnable examples: `writing.py`, `decorate.py`, `event_tags.py`, and
  `interop.py` (NumPy / pandas / Arrow → polars, duckdb).
- A tutorial link and badge at the top of the PyPI page.

### Changed

- Tutorial figures and plotting snippets use [mplhep](https://mplhep.readthedocs.io)'s
  `histplot` / `hist2dplot`, without applying a `hep.style` theme.

## [0.1.0] - 2026-07-20

First public release: a pure-Rust HIPO (CLAS12) v6 reader and writer, with a
columnar, [uproot](https://uproot.readthedocs.io)-shaped Python binding whose
columns come back as zero-copy [Awkward](https://awkward-array.org) arrays.

### Added

- **Python reading** — `open` a file / directory / glob / list; `arrays`,
  `array`, and a raw-NumPy `numpy` accessor; `library=` backends `ak` (default),
  `np`, `pd` (pandas), and `arrow` (pyarrow); bank proxies (`f["REC::Particle"]`),
  `filter_name` globs, `entry_start`/`entry_stop`, and discovery (`keys`,
  `typenames`, `show`).
- **Bounded-memory streaming** — `iterate(step_size=…)` in event- or byte-sized,
  record/file-aligned chunks; multi-process reading with `workers=N` for
  I/O-bound parallel filesystems.
- **Python writing** — `create` a new file or `recreate` to *decorate* an
  existing one with a derived bank (verbatim event copy); columnar `new_bank` /
  `extend` from NumPy or Awkward, scalar and fixed-length `T#N` array columns.
- **Event tags** — pushdown `filtered(event_tag=…/event_tag_any=…)`, the
  `event_tags()` column, a persisted name↔bit registry (`tag_names`, filter by
  name), tag-and-skim (`skim(tags=…, tag_names=…)`), and in-place
  `set_event_tag` / `set_event_tags` for uncompressed files.
- **ROOT RDataFrame bridge** — `rdataframe` / `iterate_rdataframe` feed a
  selection to ROOT's RDataFrame through Awkward's generated (no-copy)
  `RDataSource`; optional `oxihipo[root]` extra.
- **Compression** — six formats (`none`, `lz4`, `lz4best`, `gzip`, `lz4perbank`,
  `lz4percolumn`); `skim` re-compresses and defaults to `lz4percolumn`.
- **Rust core** — HIPO v6 reader/writer, `Chain` with pushdown filters, typed
  bank rows and the `bank_row!` / `clas12` helpers, and a columnar `read_columns`
  materializer behind a released GIL.
- **Packaging** — `abi3` wheels (one per OS/arch, CPython ≥ 3.13) for Linux
  (x86_64/aarch64), macOS (x86_64/aarch64), and Windows (x64), plus an sdist;
  PEP 561 typed (`py.typed`, checked stub).

[Unreleased]: https://github.com/mathieuouillon/oxihipo/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/mathieuouillon/oxihipo/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/mathieuouillon/oxihipo/releases/tag/v0.1.0
