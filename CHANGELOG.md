# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While the
version is below `1.0.0`, minor releases may contain breaking changes.

## [Unreleased]

### Changed

- **`create` / `recreate` / `update` follow uproot.** They were inverted: nothing
  refused to overwrite, `create` clobbered, and `recreate(source, dst)` meant
  "decorate". Now `create(path)` raises `FileExistsError`, `recreate(path)`
  replaces, and `update(source, dst=None)` decorates.

  Migration is guarded rather than silent. `recreate(source, dst)` still works
  with a `DeprecationWarning` and behaves as `update`. `recreate(path)` on a file
  that **already exists raises for one release** — the old meaning decorated that
  file and the new one destroys it, so acting on either guess could lose data.
  Pass `overwrite=True` when you mean the new behaviour.
- **The sdist no longer advertises the wrong README.** `readme` resolves relative
  to the manifest directory, and maturin's sdist re-roots `py/pyproject.toml` to
  the tarball root — next to the *Rust* README. A wheel built directly carried
  the Python description while one built from the sdist carried the Rust one.
  Both now resolve through `README-pypi.md`, a symlink present at each root.
- **The Python floor is back to 3.10**, reversing the 0.2.1 raise to 3.13. The
  floor is a support decision, not a syntax one: the package needs only
  `from __future__ import annotations` plus PEP 604 unions in annotations, so
  3.13 excluded interpreters that run it fine — most of the installed base, for
  no benefit. `abi3-py310` (wheels are `cp310-abi3`), `requires-python >=3.10`,
  the 3.10–3.12 classifiers are back, and every `python-X.Y+` badge follows.
  The mypy `python_version` is now `3.10` and CI's test job runs on 3.10, so a
  3.11+ construct fails in review rather than at a user's import.

### Fixed

- **A failed `with` block no longer produces output.** `Writer.__exit__` called
  `close()` unconditionally, so an exception inside the block still finalised the
  file — a failed run left one that opens cleanly. For the in-place
  `recreate(dst=None)` it also ran `os.replace(temp, final)`, **overwriting the
  source with a partial result**. And when `close()` itself raised, that
  exception replaced the user's. It now aborts, removes the partial output, and
  never masks the original error.
- **`filtered()` composes.** Each call built its filter from its own arguments
  alone, so `f.filtered(require=…).filtered(event_tag=…)` silently dropped the
  `require` — and the record-tag clause was *widened* rather than dropped,
  because the core unioned record tags while replacing every other clause.
  Chaining now narrows: `require` unions, `record_tag` and `event_tag`
  intersect, and two `event_tag_any` clauses raise, because "any of A" and "any
  of B" is not expressible as one bitmask.
- **`entries=` no longer answers wrongly on a filtered chain.** It resolves each
  index through the random-access path, which addresses the file's event stream
  and never consulted the filter — so it returned events the filter excludes,
  and the indices did not mean what a range read means. It now raises; making
  the two agree needs a decision about which index space `entries=` speaks.
- **The key namespace no longer depends on how many banks matched.** `single` —
  did the caller name one bank as a bare string? — reached only the Awkward
  assembler, so `arrays(["REC::Particle"], library="np")` returned bare
  `pid`/`px` keys while the same call with `library="ak"` returned a record
  namespaced by bank. A loop keyed on `"BANK/col"` worked until it met a file
  where the glob matched a single bank. All four backends now key off the
  request.
- **`banks=` together with `filter_name=` is refused.** `filter_name` replaces
  the bank selection outright, so `arrays("REC::Particle",
  filter_name="REC::Event*")` silently returned `REC::Event`. Now a `TypeError`.
- **`skim(tags=…)` no longer leaves a mis-tagged file behind.** The length check
  ran after the skim had finished, and the short `tags` was padded with zero per
  event — so the caller got an exception *and* a complete, silently mis-tagged
  file that opened cleanly. The partial output is removed.
- **The Arrow schema is declared non-nullable.** It was inferred, leaving every
  field nullable, so a Parquet round-trip returned `option[var * ?float32]`
  instead of `var * float32`. The docs blamed Arrow for this; the schema was
  ours. Values were never affected.
- **`composite(library="np")` keeps a consistent shape.** `np.array(…,
  dtype=object)` over equal-length slices collapses to a rank-2 array of boxed
  scalars, so the result's shape depended on whether the file happened to have a
  constant number of rows per event.
- A URL passed to `open()` now reports that remote sources are unsupported,
  instead of falling through to the glob branch and reporting "no such file or
  directory".

### Added

- **`to_dask()` is a real dask-awkward source.** It was `from_map` over a plain
  function, which dask-awkward cannot introspect, so the array was lazy in name
  only: constructing it **read partition 0** just to learn the type, `len()` and
  entry slices raised on unknown divisions, and every partition read every column
  of every selected bank however little of it the graph touched.

  It now carries the form (from a zero-event read, which decompresses no record),
  reports the batch boundaries it had already computed as `divisions`, and
  implements dask-awkward's `ColumnProjectionMixin` — so `dak.sum(p.px)` reads
  `px` alone, across banks as well as within one, and
  `dak.report_necessary_columns` answers instead of returning `{}`. Under `cut=`
  divisions are deliberately withheld: a per-event cut drops events, and
  boundaries that later prove wrong are worse than absent ones.
- CI actually exercises what it builds: the test job matrixes over interpreters
  (the 3.10 floor on all three OSes, plus 3.14 on Linux) instead of pinning one;
  every wheel job installs the wheel it just built and reads a file with it; the
  sdist job rebuilds the tarball into a wheel and asserts its metadata; and the
  13 `py/examples/*.py` are smoke-run, as the Rust examples already were.
- `filter_name=` accepts a **sequence of globs** as a union, on `arrays`,
  `iterate` and `keys` — asking for `REC::*` plus `RUN::config` needed two calls.
- `library="pd"` frames carry `attrs["num_entries"]`. An event with no rows is
  absent from the `(entry, subentry)` index entirely, so a frame cannot be
  positionally joined against `event_tags()`. The frame is deliberately **not**
  reindexed to the full range — that would insert a row per empty event, i.e.
  invent a particle, and make `pd` disagree with `ak`/`np` on row counts — so the
  true count travels alongside instead.
- Module-level `iterate()` gained `cut=`, which `Chain.iterate` already had.
- `CITATION.cff`, and the wheel now ships the licence text (PEP 639
  `license` + `license-files`) rather than only naming MIT in metadata. The
  file `license-files` points at is `py/LICENSE.txt`, a **symlink** to the real
  `LICENSE` — a glob cannot escape the project directory, a copy could drift,
  and the name cannot be `LICENSE` because maturin already places the repo-root
  one at the sdist root.
- `scripts/release.py check` also verifies `CITATION.cff`'s version and that
  `py/LICENSE` has not drifted from `LICENSE`.

### Changed

- `record_decompressed_sizes()` reads its record headers in parallel. It runs
  before `iterate(step_size="200 MB")` can plan a single batch, so on a
  many-record chain the serial version was a visible stall before any data moved.

- **The version badges on the GitHub README**, which showed `v0.1.1` and
  `python 3.10 | … | 3.14` while 0.3.0 was current — stale across four releases.

  They were dynamic, on the reasoning that a live page wants "latest". That
  reasoning was wrong: a shields.io badge sits behind its own Cloudflare edge
  (`max-age=10800`) *and*, on GitHub, behind `camo.githubusercontent.com`, and a
  dynamic URL never changes, so neither cache ever refetches. A static
  `pypi-vX.Y.Z` badge puts the version in the URL, so each release mints a URL no
  cache has seen and the correct image appears immediately.

  All four badge sites are now static. `scripts/release.py prepare` rewrites
  three of them and the generated docs page reads the version from
  `py/pyproject.toml`, so none can be forgotten. `check` also now asserts the
  `python-3.13+` badges match `requires-python` — the mismatch that shipped in
  0.2.0.

- **A release no longer publishes without reaching the docs site.** The
  release-notes page is generated from `CHANGELOG.md` at build time, but the docs
  workflow was path-filtered to `website/**` — and a release commit touches the
  changelog and manifests, not `website/`. So 0.3.0 shipped to PyPI while the
  site still showed 0.2.2. `CHANGELOG.md` is now in the filter.

## [0.3.0] - 2026-07-24

### Added

- **`scripts/release.py`** — one command per phase of a release, because a
  release has to keep six files plus the changelog in step and only one of them
  was ever checked. `prepare` rewrites every version site (both `Cargo.toml`s,
  `pyproject.toml`, the static PyPI badge, both lockfiles), moves `[Unreleased]`
  into a dated section, fixes the compare links, and runs the full check suite.
  `tag` is gated on the tree being clean, `HEAD` matching the remote, **every
  workflow green on that exact commit**, and the version not already existing on
  PyPI — then asks you to type the version, since publishing cannot be undone.
- A **`version-consistency` CI job** running `scripts/release.py check` on every
  PR, so a forgotten manifest or badge is caught long before a release rather
  than shipping, as it did in 0.2.1.

## [0.2.2] - 2026-07-24

A packaging fix. No library code changed, so behaviour is identical to 0.2.1.

### Fixed

- **The version badge on the PyPI page.** It read `v0.1.1` on the 0.2.1 page.
  `py/README.md` is the PyPI long description, which PyPI **freezes at upload**
  and also serves on every older version's page — so a dynamic `pypi/v` badge
  there can only be right by luck: it reports whatever is newest (wrong on an
  older version's page), and on the project page it lags up to three hours,
  because shields.io caches with `max-age=10800` no matter what `cacheSeconds`
  you pass.

  The PyPI description now carries **static** `pypi-vX.Y.Z` / `python-3.13+`
  badges, which are exactly right for a frozen page and depend on no cache.
  `README.md` and the docs site keep dynamic badges — those pages are live, so
  "latest" is the correct meaning. Bumping the static badge is now a step in
  `RELEASING.md`.

## [0.2.1] - 2026-07-24

A packaging fix. No library code changed, so behaviour is identical to
0.2.0.

### Changed

- **The Python floor is 3.13**, as the documentation has always said. The build
  disagreed: `pyo3` was on `abi3-py310` and `pyproject.toml` on
  `requires-python = ">=3.10"`, so 0.2.0 shipped `cp310-abi3` wheels advertising
  3.10 support the project did not claim. Now `abi3-py313` /
  `requires-python = ">=3.13"`, matching the mypy `python_version` and the docs,
  and the stale 3.10–3.12 classifiers are gone.

  This narrows support, so pip on 3.10–3.12 will resolve to 0.2.0 instead —
  which does work there. The floor is a support decision, not a syntax one: the
  code itself still only needs 3.10.

### Added

- A **supported-Python badge** (`pypi/pyversions`) on both READMEs, so the floor
  is visible where people look rather than only in the metadata. It reads from
  the published classifiers, which is how the 3.10 mismatch stayed invisible.

## [0.2.0] - 2026-07-24

Eighteen merged changes. The headline is that the **Python binding stopped being
a lossy view of the crate** — composite banks, cuts, index replay, Parquet and
dask are all reachable now — alongside a reader hardened against malformed input
and the first cross-implementation benchmark against the reference C++ and Java
readers.

Three changes need a decision on upgrade, all listed under *Changed*: the MSRV
moves to 1.95, `pip install oxihipo` now brings every backend, and two
exceptions change class.

### Added

#### Python

- **`cut=` expressions** on `arrays` / `iterate`, evaluated with the bank's
  columns bound to jagged `ak.Array` values. One keyword covers both
  granularities, decided by what the expression evaluates to: `cut="pid == 11"`
  filters **rows** (every event survives), `cut="ak.any(pid == 11, axis=1)"`
  filters **events**. A column named only in the cut is read for it and then
  dropped. Array columns (`T#N`) keep their inner width. The expression is
  `eval`'d with builtins removed — that blocks mistakes, not attackers, so build
  cuts from your own source.
- **`arrays(entries=[...])`** — read an explicit list of global event indices
  instead of a contiguous range, for replaying events a previous pass flagged.
  Output is aligned 1:1 with the list, so order and duplicates are preserved and
  an out-of-range index yields an empty entry. Sort the list: lookups go through
  the record cache, so a run inside one record costs a single decode.
- **`to_parquet()`** — write a selection straight to Parquet, the handover to
  polars / duckdb / pandas. `step_size=` streams one row group per chunk, so
  inputs far larger than RAM work in about one chunk of memory.
- **`to_dask()`** — a lazy [dask-awkward](https://dask-awkward.readthedocs.io)
  array, one partition per `step_size` batch (the counterpart to `uproot.dask`).
  Needs the new `oxihipo[dask]` extra. Reach for `iterate()` / `workers=` first
  on a single machine.
- **Composite banks** — `composite(bank)` reads the CLAS12 structures that carry
  an inline format string instead of a schema, columnar, with positional fields
  (`f0`, `f1`, …), in `library="ak"` or `"np"`.
- **`bank_ids`** — `{bank: (group, item)}`, the wire identifiers other HIPO
  tools address banks by.
- **`max_record_bytes`** on `create()` / `recreate()`, and **`record_count`** to
  read back how many records a file has. These belong together: a reader
  parallelises over whole records, so `record_count` bounds how many cores a scan
  can use, and `max_record_bytes` is the knob that sets it.
- **`file_header`** — the HIPO file header as a `FileHeader` named tuple.
- **Writer-side event tags** — stamp a per-event tag from `extend(tags=…)`.
- Module-level equivalents for the remaining methods, so one-off reads need no
  `open()`.

#### Rust

- **Big-endian file reading.** A big-endian record is byte-swapped once per
  record after decompression, so every downstream zero-copy path stays
  native-endian and no read gets slower.
- **User CONFIG section, read and write** — the key/value run config the C++
  (`addUserConfig`) and Java writers emit, at `(32555,1)` / `(32555,2)`.
  Interoperates both ways.
- **JSON dictionary support** — schemas are read from the `(120,1)` JSON
  structure when the compact `(120,2)` text is absent.
- **A record cache for random access.** `Chain::event(i)` kept re-decoding the
  containing record on every call. The last decoded record is now held on the
  chain, so a run of lookups inside one record costs a slice rather than a fresh
  inflate — sorted lookups drop from milliseconds to microseconds. All three
  record layouts are cached. **Sequential iteration never consults the cache**,
  so `events()` / `for_each` throughput is unchanged.
- **`Chain::read_columns_at`** — the columnar read behind `entries=`.
- `Endianness` and `FileHeader` are exported. `Chain::file_header` returned a
  `&FileHeader` whose `endianness` field had an unnameable type, so downstream
  code could read the integer fields but could not write a signature over the
  return or `match` the enum at all.

#### Documentation and testing

- **A cross-implementation benchmark** against the reference C++ and Java
  readers, plus Python — five read scenarios, every implementation printing a
  checksum so a run only counts if they all agree. Published with the method and
  the commands to reproduce it.
- **Record size and parallel scaling** — measured guidance that 32 MB records
  are the wrong default for a parallel scan of anything under ~1 GB.
- Fuzzing (`cargo-fuzz`, three targets), a deterministic corpus harness, broader
  property tests, a golden file written by the reference C++ writer, and
  `criterion` benches so a read regression is caught rather than shipped.
- CI now runs on Linux, macOS and Windows, and gates the declared MSRV, every
  feature combination, `cargo-deny`, the doctests, and a build of the fuzz
  targets.

### Changed

- **MSRV is now 1.95** (was 1.87), and the Python crate moves to edition 2024.
  CI builds and tests against exactly the declared MSRV, so it cannot drift
  silently.
- **`pip install oxihipo` installs every backend** — `awkward`, `pandas` and
  `pyarrow` alongside `numpy`. Previously only `numpy` came by default, so
  `arrays()`, whose `library` defaults to `"ak"`, raised `ImportError` on a fresh
  install until the user found the right extra. The imports stay lazy, so
  `import oxihipo` still pulls in none of them and an unused backend costs only
  disk. The `[awkward]` / `[pandas]` / `[arrow]` / `[all]` extras still resolve,
  as no-ops. pip cannot express an opt-out: use `pip install --no-deps oxihipo
  numpy` for the old footprint.
- **Two exceptions change class.** `ColumnLengthMismatch` now raises
  `ValueError` rather than `TypeError` (a length disagreement is a shape problem,
  not a type one), and thread-pool or internal failures raise the base
  `OxihipoError` instead of `CorruptFileError` — reporting them as file
  corruption sent you hunting a bad file that was not there.
- `HipoError` gained `ThreadPool` and `Internal` variants, which were previously
  overloaded onto `Compression`. The enum is `#[non_exhaustive]`.
- Dependencies refreshed; `criterion` moves to 0.8.

### Fixed

- **Reader hardening against malformed and untrusted input.** The crate builds
  with `panic = "abort"`, so a reachable panic is a denial of service. Event
  offsets are bounds-checked, decompression allocations are capped against a
  plausible bound, out-of-range descriptors error instead of panicking, and
  by-bank synthesis zero-fills rather than aborting. Verified read-neutral by an
  interleaved best-of-8 A/B: +0.0% on `None`/`Lz4`.
- **An empty record no longer truncates a scan.** `build_index_by_scanning` gave
  up at the first record with `event_count == 0` — legal, and what a skim that
  kept nothing from a batch produces — so every later record silently vanished.
  The regression test was verified to fail without the fix (1 event instead of
  all).
- **The descriptor sort is verified, not `debug_assert`ed.** `bank_index` binary
  searches that slice, so in **release** an unsorted directory silently missed
  banks that were present — wrong data, no error. Now `CorruptRecord`.
- Composite accessors and `BankBuilder::set_*_at` were unchecked, so a bad index
  aborted the process; they now behave like the lenient `Bank::get`.
- **gzip decode read the stream twice**, and `lz4-sys` failed to build on
  Linux-aarch64 because `c_char` is `u8` there — eight hardcoded `i8` casts are
  now `core::ffi::c_char`.
- `composite()` reported "no composite bank X in this file" for a bank that *is*
  in the file but has a schema — on real data every bank took that branch. The
  two causes now read differently.

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

[Unreleased]: https://github.com/mathieuouillon/oxihipo/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/mathieuouillon/oxihipo/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/mathieuouillon/oxihipo/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/mathieuouillon/oxihipo/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/mathieuouillon/oxihipo/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/mathieuouillon/oxihipo/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/mathieuouillon/oxihipo/releases/tag/v0.1.0
