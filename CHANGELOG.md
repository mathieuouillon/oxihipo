# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While the
version is below `1.0.0`, minor releases may contain breaking changes.

## [Unreleased]

### Added

- **`Compression` is a (codec, layout) pair**, not a flat list of six named
  combinations: `Codec::{None,Lz4,Lz4Hc,Gzip,Zstd}` x
  `Layout::{PerChunk,PerBank,PerColumn}`. All **15** pairs have a wire tag and
  round-trip. The six historical names survive as associated constants, so
  `Compression::Lz4PerColumn` is still valid source at all 229 call sites and
  still means exactly what it meant (`Lz4Hc` x `PerColumn`).
- **Zstandard**, levels 1-6 via `Compression::with_zstd_level`. The level is a
  writer-side knob and never reaches the wire — one tag decodes them all,
  unlike LZ4/LZ4-HC which burn two. On a 248 MB real CLAS12 file,
  `Zstd x PerColumn` is **2.22x** smaller and scans in **20.9 ms**, against
  2.03x/28.2 ms for `Lz4Hc x PerColumn` and 2.32x/21.9 ms for
  `Gzip x PerColumn` — but writes in 0.69 s where gzip takes 2.52 s and LZ4-HC
  7.66 s.

  Tags **4 and 5**, left poisoned when `Lz4Chunked` and `Lz4ByBank` v1 were
  removed in 0.x, are reused for Zstd. That is safe specifically because a
  zstd frame begins with the magic `0xFD2FB528`: a stale file carrying one of
  those tags fails the frame check rather than decoding as something
  plausible. Tag 15 is the only one left unassigned.

  Only the six pairs that predate the matrix are readable by `hipo-cpp` and
  `hipo-java`; the other nine are oxihipo extensions those readers reject as
  an unknown tag. The split-record *directory* stays LZ4 for every layout so
  tags 6 and 7 remain byte-compatible.
- **Python: `compression=` takes the pair too** — `"<codec>+<layout>"`, e.g.
  `"zstd+percolumn"` or `"zstd6+perbank"`. A bare codec means `perchunk`, and
  the six older names still work and still mean the same thing
  (`"lz4percolumn"` is `lz4hc+percolumn`, not `lz4+percolumn` — the split
  codecs were always high-compression). An unknown name lists what is valid;
  a zstd level outside 1-6 is an error rather than a silent clamp.

### Fixed

- **`read_columns` handed back buffers carrying their growth slack.** Assembly
  grew each column with `extend_from_slice` per record, so the result kept the
  last doubling's headroom — measured at **1.664x the payload** on a real
  CLAS12 DST, retained for as long as the caller held it. That is the lifetime
  of the NumPy array for the Python binding, where `into_pyarray` moves the
  `Vec` across with the slack included. Now **1.000x**.

  Every chunk is already in hand when the buffers are assembled, so the final
  length is known before any appending and the buffers are sized exactly up
  front. That is faster than the naive fix as well as smaller: a first version
  grew and then `shrink_to_fit`, which left best-of-15 unchanged but median ~5%
  worse on a 9-column read (the end-of-assembly realloc). Sizing up front
  removes both the doubling and the realloc — measured against the pre-session
  baseline, best-of **1.283 -> 1.229 s** and median **1.606 -> 1.551 s**.


## [0.8.0] - 2026-07-31

**Breaking: this is `0.8.0` rather than `0.7.2`.** Removing the `Debug` bound
from `BankRow::Handles` relaxes what an implementor must provide, but it is
source-breaking for any *consumer* that relied on `T::Handles: Debug` in a
generic context — a `^0.7` dependent would have broken on a plain
`cargo update`. `scripts/release.py` now refuses a patch bump on a changelog
section that says "breaking", so this cannot be shipped as a patch by mistake.

### Added

- **`Chain::par_fold(threads, id, fold, reduce)`** — per-worker accumulators
  joined by a reduce, instead of every result crossing a shared atomic. How
  much that buys depends entirely on how expensive your events are, and both
  regimes are documented: on 4M one-row events, `for_each` going from one
  thread to twelve measures **0.32x** — three times *slower* than serial —
  while `par_fold` measures 7.98x. On a real 599k-event CLAS12 DST, where LZ4
  decode of 47 banks dominates, `for_each` already scales 3.27x and the two
  land within run-to-run noise. `examples/bench_par` reports all four numbers.
- **`Chain::fold(init, f)`** — the sequential case with no `Send`/`Sync` bound,
  so a `Cell` or `Rc` accumulator compiles on a path that never spawns a thread.
- **`Chain::open_with(src, len, label)`** and **`pub trait ReadAt`** — read a
  chain from any byte source. XRootD, S3, HTTP-range and in-memory testing
  become third-party code; `read_exact_at` takes `&self`, so a rayon scan
  already issues concurrent positioned reads against it.
- **`Bank::iter(handle)`** and **`Bank::read_into(handle, &mut Vec<T>)`** —
  allocation-free column access on byte-packed rows.
- **`Display` for `Schema`, `Dict` and `Bank`.** `{}` prints the useful thing
  and truncates; `{:#}` prints everything. `Dict` sorts by `(group, item)` and
  caps at 8 schemas — the alternative was `{:?}`, which on the shipped
  274-schema CLAS12 dictionary is over 41 million characters. `Bank` renders a
  row table capped at 10 rows, with each cell formatted by *its own* type, so
  an `f32` keeps its shortest round-trip rather than becoming 17 digits of
  widened noise. `examples/show` prints all three.
- **`Bank::value` / `value_i64` / `value_by_name` / `array_values`** — read a
  cell without naming its type, for printers, statistics and expression
  evaluators that walk `schema().entries()`. Indexed by schema position, not
  name: the hash costs ~5.8 ns per cell against ~0.4 ns for the type dispatch.
  Returns `Option`, and refuses array columns rather than silently handing back
  element 0 of a covariance matrix.
- **`Chain::skim_banks` / `skim_banks_with`** — bank projection, the
  `hipoutils -filter` equivalent. Keeping `REC::Particle` and `REC::Event` from
  a 9.1 GB CLAS12 DST leaves 126 MB: **1.389% of the source, 72x smaller**,
  with the kept values identical. `SkimSummary` reports what was kept, how many
  structures were dropped, and any `pindex` left pointing at a dropped bank.
- **`BankPatterns`** — glob patterns over bank names, shared by anything that
  needs to name a set of banks. `REC::*` is a prefix, `*::Particle` spans the
  `REC::`/`RECHB::`/`RECAI::` families, and a misspelled literal is an error
  rather than an empty output.
- **`StructureHeader::to_bytes`** — the inverse of `parse`. Needed to copy a
  structure between events without hardcoding the 8-byte layout; on the split
  codecs a header and its payload are never contiguous, so there is nothing to
  copy verbatim.
- **`BankBuilder::finish_into` / `with_buffers` / `into_buffers`,
  `EventBuilder::reset` / `add_bank` / `finish_into`** — assemble events
  through reusable buffers. `finish(self)` consuming the builder was why
  `BankBuilder::reset` had no possible caller.
- **`examples/bench_write`** — the write-path benchmark, which did not exist.
- **`HipoError::at_offset`, `HipoError::AtOffset`, `HipoError::InvalidUsage`**
  — decode errors now name the record they came from and the file they came
  from. Writer-API misuse (`set_*` before `push_row`, an out-of-range row
  index) reports as `InvalidUsage` rather than masquerading as a corrupt
  record.
- **`EventCtx::try_bank`** — the fallible sibling of `bank()`, separating
  "this event has no such bank" (`Ok(None)`) from "this bank is corrupt"
  (`Err`) and from "the dictionary has no such bank" (`Err(UnknownSchema)`).
  `bank()` collapses all three into `None`, so a corrupt `Lz4PerBank` record
  read as an event with no particles and an analysis under-counted silently.
- **`Dict::try_add`** — non-panicking `add`.
- **`examples/bench_random`** — concurrent random-access throughput, the
  benchmark for the record cache.

### Documentation

- The reading guide now **leads with `bank_row!`**: a Java or C++ port lands on
  `ev.bank(name)` hoisted out of a loop, which in oxihipo re-resolves the name
  every event. Typed rows resolve handles once per bank.
- A new **Recovering a damaged file** section — `open_salvage` + `skim` is the
  equivalent of Java's `hipoutils -doctor`, and appeared in no guide before.
- **C++ `hipo4` cannot read `T#N` array columns.** Its schema parser splits on
  `,` and `/` only, so the type token is the literal `"F#6"` and resolves to
  type -1. Worse than the missing column: every column declared *after* it is
  silently mis-offset. Java reads them correctly, so this is a C++ gap, not an
  oxihipo one — but it needed saying next to the feature.
- The `lz4-apple` "~7% faster" claim is now backed by the measurement:
  6.0-7.7% on decoder-isolated timings over 188 real DST records, 4.5% off a
  full single-threaded scan of a 9.1 GB DST.
- `Chain::for_each` documents that its parallel modes already overlap I/O with
  compute — each worker `pread`s independently — which is why there is no
  prefetcher and why oversubscribing is the lever on a slow filesystem.

### Fixed

- **The split codecs copied and re-copied on every record.** `parse` copied
  the compressed section out of the caller's buffer (4 MB per record on a real
  DST), and the lazy stream inflate allocated a `Vec`, let `decompress`
  reallocate it, then reallocated *again* to shrink it back to a `Box<[u8]>`.
  New `parse_owned` takes the buffer where the caller is done with it, and the
  inflate decompresses straight into an exactly-sized box. Measured on real
  re-encoded CLAS12 data: full scans **176.5 -> 148.1 ns/event on `Lz4PerBank`
  (-16%)** and 72.4 -> 69.5 on `Lz4PerColumn` (-4%); random access +13% and
  +15%. Plain `Lz4` — what a stock DST uses — is unchanged.
- **Decode errors were unattributable.** 58 of 73 construction sites pass
  `offset: 0`, so a corrupt record in the middle of a multi-gigabyte chain
  reported `corrupt record at offset 0x0` — pointing at the file header — with
  no indication of which file. All four record-processing entry points now
  attach both: `file "run_b.hipo": record at offset 0x768: compression error:
  lz4 decompress failed`. No measurable read-path cost (142.8 -> 142.7 ns/event,
  interleaved).
- **The write path allocated per bank and per column, on every event.**
  Measured, not estimated: 15 allocations/event for a 2-bank event, 151 for
  1x28x40, 266 for 10 banks, and **666 for a 47-bank CLAS12-shaped event** —
  the earlier estimate was ~28. `Writer` now recycles column buffers and the
  event builder across events; the marginal cost is **0.015 allocations per
  additional event**, all of it the record buffer's geometric growth, with the
  assembly share at exactly 0. Writing 50,000 such events goes from 31,148 to
  49,836 ev/s (**1.59x**) with byte-identical output.
- **Concurrent `Chain::event` did not scale at all.** The record cache held
  its mutex *across* the record decode, so every miss serialised behind one
  ~8 MB decompression. Measured on a 9.1 GB CLAS12 DST: random access was flat
  at ~380 ev/s from 1 to 12 threads (12t/1t = 0.97x). Decoding outside the lock
  takes 12 threads to 2721 ev/s — **7.1x** — with single-threaded throughput
  unchanged and identical checksums.
- **`OwnedEvent::size()` decompressed the whole event to measure it.** On the
  split codecs it reassembled via `bytes()`, inflating every bank stream in the
  record to answer a question the record directory already holds. 5.7 -> 19.0
  Mev/s on `Lz4PerBank` (3.3x) and 5.6 -> 19.6 on `Lz4PerColumn` (3.5x).
- **`Composite::f64` and `Composite::f32` returned 0.0 for every integer
  field.** Both matched only `Float`/`Double`, so reading an `Int` field came
  back 0.0 — indistinguishable from a stored zero, in an accessor whose whole
  purpose is to erase the type.
- **`BankBuilder::with_row_capacity` under-reserved array columns** by exactly
  the per-row element count, so a `cov/F#16` column got 1/16th of what it
  needed and regrew from there. Affects the Python columnar writer, which uses
  `with_row_capacity` + `push_rows` directly.
- **`ColumnBuffers::event_count` wrapped to `usize::MAX`** on an
  externally-constructed value with empty `offsets` (release; it panicked in
  debug). It saturates, matching `total_rows` beside it.
- **`Dict::parse_text` could abort the process.** It is public, in the prelude,
  and takes arbitrary text; more than 65,536 schemas overflowed the `u16` id
  space and panicked, and release builds set `panic = "abort"`, so
  `catch_unwind` never returned. `parse_text` and the chain's dictionary merge
  now use `Dict::try_add`.
- **`Chain::for_each_column` silently ignored an active filter**, returning
  every value in the file. It now returns
  `HipoError::FilterIgnoredByColumnSweep` naming the alternative.
  `Chain::event` and `Chain::event_count` document that they are pre-filter.
- **The split-codec parsers reserved a file-controlled directory length before
  bounding it**, so a crafted 24-byte section could force a
  `Vec::with_capacity(3_000_000_008)` before `decompress`'s own amplification
  check ran.
- **`compress()` fabricated a `&mut [u8]` over uninitialized spare capacity.**
  The default `lz4-c` build no longer creates the reference at all. (Hardening:
  Miri does not flag the old shape, only an actual uninitialized *read*.)
- Split codecs now **reject big-endian records** explicitly rather than being
  stopped incidentally by a later `event_count` mismatch.

### Changed

- **`BankRow::Handles` no longer requires `Debug`**, lifting the 12-column cap
  on `bank_row!` — std implements `Debug` for tuples only up to 12 elements,
  and 16 of the 30 CLAS12 bank definitions are wider. `REC::Calorimeter`'s 28
  columns now map to one struct.
- The split-codec writer doc blocks stated `ext_format_version` 3 and 2 while
  the code has emitted 2 and 1 since 0.7.1 — the revert updated the constants,
  tests and changelog but missed the prose. The version numbers now live in
  `wire::constants` and are shared by writer and reader.

## [0.7.1] - 2026-07-29

A single-purpose release: **undo 0.7.0's split-codec format-version bump**,
which broke the C++ and Java implementations of those codecs. The composite fix
0.7.0 shipped is kept in full.

### Fixed

- **`Lz4PerBank` and `Lz4PerColumn` files written by 0.7.0 are unreadable by
  `hipo-cpp` and `hipo-java`.** 0.7.0 raised the on-disk
  `ext_format_version` from 2 to 3 (by-bank) and 1 to 2 (by-column) when it
  appended the composite `header_size` table. Those version numbers turn out to
  be a **cross-implementation contract**: the
  [`hipo-cpp`](https://code.jlab.org/hallb/clas12/hipo-cpp) and
  [`hipo-java`](https://code.jlab.org/hallb/clas12/hipo-java)
  `feature/bybank-bycolumn-compression` branches document and implement exactly
  versions 2 and 1. Measured on a JLab farm node against both:

  | file | oxihipo | `hipo-java` | `hipo-cpp` |
  |---|---|---|---|
  | by-bank / by-column @ 0.6.0 | ✅ | ✅ | ✅ |
  | by-bank / by-column @ 0.7.0 | ✅ | ❌ `failed to decode ByBank record section` | ❌ **segfault** |
  | by-bank / by-column @ 0.7.1 | ✅ | ✅ | ✅ |

  All three now produce byte-identical checksums on the same files, array
  (`T#N`) columns included.

  **The bump was never necessary.** The `header_size` table is appended after
  every other directory table, so a reader that predates it never looks that
  far — proven by patching only the version byte back on a 0.7.0 file, after
  which both other implementations read it perfectly. The library now detects
  the table by **directory length** instead of by the version byte, which is
  what lets the version stay fixed while the format grows.

  0.7.1 still reads the version-3/2 files 0.7.0 wrote, so nothing already
  written is lost. Rewrite them with 0.7.1 if you need to share them.

### Added

- `tests/composite_codecs.rs` asserts the on-disk `ext_format_version` is 2
  (by-bank) and 1 (by-column), reading the byte back off the file. The contract
  is with other codebases, so it needed a test that fails if it drifts again.

### Documentation

- The claim that the split codecs are readable only by this library was wrong
  and is corrected everywhere it appeared. The released C++ `hipo4` and Java
  readers do not know wire tags 6 and 7, but the branches above do.

## [0.7.0] - 2026-07-29

### Breaking

- **`TagRegistry::insert` and `TagRegistry::from_names` return `Result`.** A tag
  name that cannot survive the on-disk `name=bit` text form is now refused
  instead of silently mangled (below). `WriterBuilder::tag_names` keeps its
  signature and surfaces the error from `build`.

### Changed

- **Split-codec record format bumped: `Lz4PerBank` 2 → 3, `Lz4PerColumn` 1 → 2.**
  The directory gained a `B × u8` composite `header_size` table, appended after
  the existing tables so every offset before it is byte-for-byte unchanged. One
  parser reads both versions, with the tail defaulting to 0 — which is exactly
  what the old versions meant. Any other version is refused rather than
  misread — measured: a 0.6.0 reader opens a 0.7.0 split-codec file and reports
  its event count, then fails the first `events()` item with `Lz4PerBank:
  unsupported extension-format version`. It never returns wrong data.

  Compatibility was checked in both directions, all four codecs, by building
  v0.6.0 and HEAD side by side and cross-reading:

  | | `None` | `Lz4` | `Lz4PerBank` | `Lz4PerColumn` |
  |---|---|---|---|---|
  | HEAD reads 0.6.0 | ✅ | ✅ | ✅ | ✅ |
  | 0.6.0 reads HEAD | ✅ | ✅ | clean error | clean error |

  The blob codecs are not merely compatible, they are **byte-identical**: the
  same inputs written by 0.6.0 and by HEAD hash to the same SHA-256. Since those
  are the only codecs C++ hipo4 and Java can read, interoperability with them
  cannot have changed. Neither split codec was ever readable outside this
  library.

### Fixed

- **Composite banks lost their format string under `Lz4PerBank` and
  `Lz4PerColumn`.** A bank's structure length word packs the data size into its
  low 24 bits and the composite `header_size` — the format string's length —
  into its top byte. Both split codecs take a record apart and store bank
  payloads separately, discarding the structure headers, and rebuilt that byte
  as zero. A composite bank therefore came back looking like an ordinary one:
  `header_size` read as 0 and `composite()` returned `None`. The two blob
  codecs, `None` and `Lz4`, were unaffected, so the same file written two ways
  disagreed about whether its banks were composite. Fixed by the format change
  above, plus carrying the byte through the by-bank structure iterator and both
  event synthesisers.

  Validated on real data as well as fixtures: 2,000 events of an 8.5 GB CLAS12
  DST (71 distinct banks) and of a simulation file (106 banks) were rewritten
  through all four codecs and compared bank for bank — every `header_size` and
  payload identical to the source. Neither file carries a composite bank, so
  this bug was not corrupting CLAS12 reconstruction output; it was corrupting
  composites, which the format allows anywhere.

- **`OwnedEvent::composite` returned `None` on every split-codec event.** It
  delegated to `EventCtx::composite`, which needs original structure bytes and
  documents itself as returning `None` for by-bank backends — advising callers
  to up-convert to `OwnedEvent`, the very thing that did not work. It now goes
  through the synthesised event blob, which (since the fix above) carries the
  composite `header_size`.

- **`Lz4PerColumn` stored a composite bank column-major** when a schema happened
  to describe its group/item and its payload divided evenly into rows — a layout
  that assumes fixed-width rows the bank does not have. Composites are now kept
  opaque regardless. Belt-and-braces: the split is a permutation the synthesiser
  inverts, so no round-trip through this library could observe it.

### Added

- `tests/composite_codecs.rs` — composite banks across all four codecs, checked
  both for the surviving `header_size` and for decoded field values. Two
  composites with different format-string padding, since only the 8-byte-padded
  one divides evenly into rows and so reaches the `Lz4PerColumn` guard.
- `Lz4PerBank` added to the read benchmark's format list. It reaches the reader
  through the by-bank structure iterator rather than the per-column synthesiser,
  so benching only `Lz4PerColumn` left that scan path unmeasured.
- `tests/salvage.rs` — four tests for the sequential scan: the trailer is not
  indexed as data, salvage resynchronises past a damaged header, an impossible
  `event_count` is rejected, and an intact file still indexes exactly on every
  codec.
- `tests/event_tag.rs` — tag names that cannot round-trip are refused, by the
  registry and by the writer; ordinary names still survive a real write and read.
- `tests/no_alloc.rs` — the per-event allocation contract is now checked on
  **every** codec, by comparing two files with the same record count and 4× the
  events rather than against a fixed budget.

### Performance

- **`Lz4PerBank` and `Lz4PerColumn` sequential reads are ~25 % faster.**
  Removing the per-event `Arc` allocation below: 959 µs → 724 µs and 998 µs →
  713 µs on the benchmark fixture. The blob codecs are unchanged — two
  interleaved A/B rounds disagreed on their sign (+4 % then −2.6 %), which is
  the machine's noise floor, and `OwnedEvent` is the same 56 bytes it was.

- **A tag name containing `=`, a line break, or edge whitespace was written and
  silently read back as something else.** The registry is stored as `name=bit`
  lines and the reader splits on the first `=`, splits lines on `\n`, and trims
  each name. Of five names written, only one survived unchanged:

  | written | read back |
  |---|---|
  | `plain` | `plain` |
  | `has=equals` | *dropped* |
  | `has\nnewline` | `newline` |
  | `␣␣padded␣␣` | `padded` |
  | `` (empty) | *dropped* |

  Two came back under a *different* name, so `mask("has\nnewline")` returned
  `None` and the flag quietly stopped matching. Such names are now refused when
  inserted, and `Writer::build` fails rather than writing one.

- **The scan indexed the trailer as a data record.** A trailer is an ordinary
  one-event record carrying the `file::index` bank — no header bit sets it
  apart — and the fallback scan walked straight into it. The comment said the
  normal path never met this; it does, whenever a trailer *exists but does not
  parse*, which is exactly when the fallback runs. A 12-event file with a
  corrupted trailer index reported 13 events.

- **One damaged record header cost the whole file, `open_salvage` included.**
  The scan propagated the parse error instead of resynchronising, so the salvage
  path — whose entire purpose is recovering from this — recovered nothing.
  Salvage now finds the next record and continues: on a 12-event file with one
  destroyed header it returns the 8 events on either side of the damage. The
  normal path still reports the corruption, deliberately.

- **`event_count` was taken on trust.** A corrupt header propagated straight
  into `Chain::event_count()`: flipping one record's count to 1,000,000 made a
  12-event file report 1,000,009, and the *first* `events()` item was an error.
  The record's index array bounds the real count at four bytes per event, and
  that is a header field, so no decompression is needed. Checked against real
  data before relying on it: across 1,951 records of an 8.5 GB CLAS12 DST, a
  simulation file, and C++ hipo4's own golden file, `index_array_length` is
  exactly `event_count * 4`.

- **Every split-codec event heap-allocated a cell it usually never filled.**
  `OwnedEvent` held the lazy whole-event blob as `Arc<OnceLock<Vec<u8>>>`, so
  constructing one allocated even when nothing ever asked for the blob: 852
  allocations for 800 events created and dropped untouched. The `Arc` moved
  inside the cell (`cell::OnceCell<Arc<Vec<u8>>>`), which allocates only on
  first use and keeps `OwnedEvent` at 56 bytes. `Chain::events` documents "no
  per-event allocation"; that is now true on every codec, and tested.

### Documentation

- `Chain::events`' memory contract now says what the split codecs actually cost
  (more per *record*, still nothing per event) and notes that the whole-event
  views synthesise a blob on first use.
- The split codecs sort banks by `(group, item)` — load-bearing, since the
  reader binary-searches that table — so they do **not** preserve the order
  banks were added in. `structures()` yields ascending `(group, item)` there and
  write order on the blob codecs. Nothing addresses a bank by position, so this
  is an iteration-order difference rather than a data one; it is now documented
  on `Compression` and at both sort sites rather than left to be discovered.

### Removed

- `tests/zz_b.rs` — a diagnostic that printed record `bit_info` words and
  asserted nothing, committed by accident in the record-header bit fix.

## [0.6.0] - 2026-07-29

A correctness release. Each entry is something that was **silently wrong** — the
library returned success and stored the wrong bytes, or aborted the process where
it should have returned an error — rather than something that failed visibly.

### Breaking

- **`BankBuilder::finish` and `EventBuilder::add` return `Result`.** Both are
  public, so this is a semver break, taken deliberately: the alternative is an
  API that silently corrupts data (below). Neither is used by `hipo-tools`, which
  builds and passes its 204 tests against this release unchanged.

### Fixed

- **A bank at or past 2^24 bytes was written truncated, with no error.** The
  structure length word carries the data size in its low 24 bits — the top byte
  is the composite `header_size` field — and `BankBuilder::finish` wrote a full
  `u32` into it. `Writer::finish` returned `Ok` regardless, so the loss was
  invisible until the file was read back. On a one-column `Int` bank:

  | rows written | data bytes | rows read back |
  |---|---:|---:|
  | 4,194,303 | 16,777,212 | 4,194,303 |
  | 4,194,304 | 16,777,216 | **0** |
  | 5,000,000 | 20,000,000 | **805,696** |

  At exactly 2^24 the size masks to zero *and* the overflowed `0x01` re-reads as
  `header_size = 1`, so the bank comes back looking composite. Every codec was
  affected — this is the structure header, not the compression. The boundary is
  now `HipoError::BankTooLarge`.

- **`Bank::read` was documented "Infallible" and panicked in release builds.**
  Every check in it was a `debug_assert`, while `ColumnHandle::placeholder()` is
  public and safe — and the `bank_row!`-generated `resolve_handles` hands one out
  for any column a runtime schema lacks. Reading through it fell into
  `schema.entries()[65535]` and aborted with "index out of bounds: the len is 1
  but the index is 65535", with `debug_assertions = false`.

  The documentation was the defect: it now states the real contract with a
  `# Panics` section, and the bounds check is unconditional with a message that
  names the mistake. Returning an empty column instead was rejected — `read` is
  the bulk accessor and callers zip parallel columns, so a zero-length result
  silently truncates the loop to no rows, trading a loud abort for quietly wrong
  physics. `read_handle_or_default` remains the per-row path that does accept
  placeholders.

- **`read_columns` could hand back buffers that contradicted their own
  offsets.** `merge_chunks` states the contract `ColumnBuffers` owes its caller —
  offsets starting at 0 and non-decreasing, each column holding exactly
  `total_rows * inner_len` values — but only as a `debug_assert`. A corrupted
  `Lz4PerColumn` record whose row counts and column payloads disagree produces
  exactly that violation, so a **release** build returned buffers whose data
  length did not match their offsets and slicing a row read the wrong values.
  The invariant is now enforced and returns `HipoError::CorruptRecord`.

  Found by the new sweep, and only in a debug build — the release runs it had
  been checked against compile the assertion out, which is precisely why it had
  survived. CI runs the debug profile and caught it on the first push.

- **Iterating onto a zero-event record panicked.** `EventIter::next_result`
  refilled the current record with `if` rather than `while`. `advance_record`
  resets the event cursor to 0, so the guard was tested against the record being
  left and never against the one arrived at; landing on an empty record indexed
  `event_offsets[1]` on a one-element table. A library may return `Err` for
  damaged input, but a panic leaves its caller — a CLI, a Python binding, a batch
  job — no way to handle the file at all.

  The empty record need not come from corruption: `Writer::flush_record` will not
  emit one, but the public `Writer::write_record` takes a prebuilt record and
  checks only that it is at least a header long.

### Internal

- **New `tests/mutation_sweep.rs`.** Mutates every byte of a real file, truncates
  at every length, and writes hostile values into every header word, then drives
  the whole public read path over each result asserting only that nothing panics.
  It is what found the iterator bug — six single-byte mutations of a 2 KB file
  reached it, each the low byte of a record's event count. `corruption.rs`
  already had an empty-record test that could not have caught it: that test also
  blanks the trailer to force the scan path, and the scan drops empty records
  before iteration ever sees one.

## [0.5.3] - 2026-07-28

### Added

- **`Chain::for_each_range` / `for_each_ranges`** — stream one or several global
  event ranges, reading only the records they touch. Reading part of a file
  previously meant `event(idx)` per index, which is why a per-record index could
  not be exploited: a downstream cut that correctly skipped 85% of events came
  out **4.5x slower** through that path than a full scan.

  Measured on a 3 GB CLAS12 file, 21,506 events over 89 ranges (warm, best of 3):

  | | `-j 1` | `-j 16` |
  |---|---|---|
  | all ranges, one call | 0.24 s | **0.11 s** |
  | one call per range | 0.62 s | 0.80 s |
  | `event(idx)` per index | 0.61 s | — |

  So 5.9x against per-index reading at 16 threads and 2.6x at one. The
  single-threaded margin is modest because `event` caches the record it last
  inflated — contiguous access was already reasonable; what this adds is
  parallelism across records.

  **Pass every range in one call.** Each call rebuilds the record task list and
  pays a rayon dispatch, so looping spends its time on bookkeeping — at 16
  threads that overhead makes the loop slower than one thread. A record
  straddling a boundary is read once and its out-of-range events dropped, so
  `events_in` counts what the ranges hold. Ranges may overlap and arrive
  unsorted; indices are the same pre-filter space as `read_columns(range)`.
- **`Chain::open_salvage`** — open a file whose 56-byte header is unusable, by
  finding the records themselves. The header is bookkeeping (magic, version,
  counts, where the dictionary and trailer are) and all of it is re-derivable,
  because every record carries its own header and magic. A file missing its
  first 56 bytes was not unreadable, only unopenable by a path that parses that
  header first.

  Two things the scan has to handle, both found by testing rather than by
  reading the format. **The trailer looks like a data record** — nothing in its
  header says otherwise (measured: `is_last_record` is 0 on both, and the
  `bit_info` difference is only padding), so a 120-event file came back with 121
  events; it is now recognised by content, the `file::index` bank. And **a
  truncated tail was indexed and then unreadable**, because the scan checked
  that a record's header fits in the file but not the record — fixed below.

  What it cannot recover is the dictionary, which lives in the record right
  after the header, so damage that took one usually took the other. The chain
  then has an empty dictionary rather than a guess; the events are still there
  and still copyable, but their banks have no names or column types.

### Fixed

- **A truncated tail no longer defeats `open_salvage`.** The record scan checked
  that a record's *header* fits in the file, not the record, so a killed
  writer's half-written last record produced an index entry that opened fine and
  then failed on read with "record extends past EOF".

  Salvage now stops there and keeps the intact prefix. **The normal path is
  unchanged and still raises**, deliberately: truncation is genuine corruption,
  and a reader that quietly returned a shorter file would give no way to tell.
  Making the stop unconditional was the first attempt, and the binding's
  `test_truncated_file_raises` caught it — the difference matters only because
  salvage's caller has already been told the file is damaged.

## [0.5.2] - 2026-07-26

### Fixed

- **Five more bank-stream slices guarded against a corrupt offset table.** 0.5.1
  fixed the four columnar sites; the same `&stream[record.bank_byte_range(e, b)]`
  pattern — a raw slice index with bounds read from the file — remained in
  `Event::iter_structures`, `OwnedEvent::bank` (twice) and `EventCtx::bank` (twice).
  These are the *whole-event* paths, so they are what a consumer reaches through
  `structures()`, `ev.bank()` and the `for_each` callback: a wider surface than the
  columnar ones. `None` is returned instead of panicking, which already means "no
  such bank in this event" at every call site.

  After the 0.5.1 fix a downstream fuzz test found the fifth site immediately, so
  the pattern was grepped for rather than the report chased, which found the other
  four at once.

## [0.5.1] - 2026-07-26

### Fixed

- **A corrupt bank offset table panicked instead of erroring.** Four places sliced
  a decompressed bank stream with a byte range read from the record's own offset
  table — `&stream[rec.bank_byte_range(e, b)]` — in `read_columns` (both the
  by-bank and per-column branches) and in `for_each_column` (likewise). A damaged
  table points past the end of the stream, and indexing a slice raw panics:
  `range end index 3400 out of range for slice of length 3379`. Every other kind of
  damage in this reader surfaces as an `Err`, so this was reachable from `scan`,
  `stats`, `hist` and `banks` in any downstream tool, on a file that opened cleanly.

  A bank whose extent does not fit is now treated as absent, which is how a bank
  that is not there was already reported.

  Found by property-testing a downstream CLI against byte-flipped files, not by
  reading the code: the offsets have to survive enough of the header to be used at
  all, which is a narrow enough window that no hand-written case had hit it. The
  regression test flips every byte of a by-bank file three ways and drives every
  columnar entry point — which is what turned up the fourth site, after the first
  three were fixed.

  One further raw slice in the same function, `&stream[..n * elem]`, was checked
  and left alone: `n` is derived by dividing by `elem`, so the bound holds by
  construction.

## [0.5.0] - 2026-07-26

### Added

- **`Chain::bank_occupancy`** — which banks carry data, in how many events, and
  how many rows, **without inflating a single bank or column**. Every number is a
  function of a bank's per-event byte extent, which both columnar layouts already
  record in their bank-offset tables, so on `Lz4PerBank` and `Lz4PerColumn`
  nothing beyond each record's header and offset tables is decompressed. Honors
  the chain filter, takes an optional global-index range, and follows the usual
  `threads` convention (`0` = all cores, `1` = sequential).

  This exists because the operation is easy to get wrong outside the library.
  Computing it from `Chain::events` costs an `OwnedEvent` — a copy of every
  event's bytes — and enumerating a per-column event's structures first
  *synthesises* a whole event out of separate column streams: measured at
  19 µs/event on `Lz4PerBank` and 26 µs/event on `Lz4PerColumn` in a downstream
  tool, against roughly 1.5 µs/event for reading the presence tables.

  `EventCtx` looks like the escape route and is not: it avoids the copy but
  cannot enumerate a per-column record's banks, because that needs exactly the
  synthesis it exists to avoid. A caller who tried it got **4 banks out of 71** —
  fast, plausible, and wrong with no error. Putting the operation here, with a
  cross-format equality test, is the fix for that class of mistake rather than
  for one instance of it.

  Banks declared but never populated are returned with zero counts, so "never
  written" stays distinguishable from "not in the dictionary". A bank opened with
  no rows counts as carrying no data, which is the question being asked.

  Classic layouts (`None`/`Lz4`/`Lz4Best`/`Gzip`) keep no per-bank table, so
  their records are decompressed and their events walked — but with no per-event
  allocation, so they gain too.

- **`BankOccupancy`** in the crate root, the per-bank result type.

## [0.4.1] - 2026-07-26

### Fixed

- **`for_each_column` failed outright on `Lz4PerBank` files.** Its fallback
  comment claimed to cover "Bytes / ByBank / chunked", but the fallback calls
  `decode_record_into`, which expects a single whole-record payload. A by-bank
  record is one LZ4 stream *per bank* plus a directory, so the call died with
  `lz4 decompress failed`. Every other format worked, and the one existing test
  only ever tried `Lz4PerColumn` and `Lz4`, so nothing caught it.

  There is now a by-bank branch that inflates just the requested bank's stream
  and reads the column per event, mirroring the per-column opaque path — and a
  test that sweeps a scalar *and* a jagged column on all six formats and
  requires them to agree with per-event reads.

  Found from downstream: a CLI built on this crate could not run `stats` on
  files its own `skim` had written, since `Lz4PerBank` is a common default.
- **PDG masses for light nuclei were the neutral atom, not the bare nucleus.**
  The table was generated from `particle`, which tabulates the atom for the
  `10LZZZAAAI` codes — the deuteron entry was 2.014101778 u exactly. A detector
  sees a stripped ion, so every nuclear mass was heavy by Z·m_e: 0.511 MeV for
  a deuteron or triton, 1.022 MeV for an alpha or He3. Small, systematic, and
  wrong for the one thing `pdg_mass` exists to do.

  All eight nuclear codes (Geant3 45/46/47/49 and their PDG spellings) are
  corrected; nothing else moves, since a free proton has no bound electron. The
  cross-check against `particle` now subtracts Z·m_e for those codes — comparing
  raw would have re-asserted the bug — and a second test pins the four values
  against the literature independently of the library.

### Changed

- **`for_each_column` documents that it ignores the chain filter.** It walks the
  record index directly, so `with_filter` and the record-tag pushdown are both
  skipped and the caller gets every value in the file. That is deliberate — the
  per-column fast path has no per-event predicate to apply — but silently
  returning a plausible number over the wrong event set is a trap, so the doc
  now says so and points at `read_columns`, which is also columnar and does
  honour the filter.

## [0.4.0] - 2026-07-26

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

- **A chain no longer requires every file to carry the same dictionary.** Opening
  one refused unless each file's `Dict` compared *equal* to file 0's, and that
  comparison was order-sensitive — `Dict` derives `PartialEq` over a positional
  `Vec<Schema>` plus index tables whose values are insertion indices — so files
  describing exactly the same banks in a different order were rejected, though
  nothing about the format makes that order meaningful.

  Real run periods are not dictionary-uniform either: a pass-2 cook adds a bank,
  an MC file carries `MC::Lund`. `ox.open("…/pass2/*/dst/*.hipo")` died on that,
  with no escape hatch. The chain now takes the **union**: `keys()` reports every
  bank any file declares, and a bank absent from a file yields empty entries for
  that file's events — which the read path already did for an absent bank.

  Two conflicts are still hard errors, because a reader cannot survive them: one
  name describing two layouts (columns would be decoded against the wrong
  schema), and one `(group, item)` used for two banks. The second is the
  dangerous one — the columnar path locates banks by id, so a collision would
  decode one file's bytes with another file's schema and return wrong numbers
  rather than fail. Both errors name the files and banks involved.
- **The read path goes through a `ReadAt` seam** instead of holding an
  `Arc<File>` directly, so a source that is not a local file — an in-memory
  image, eventually HTTP range requests — only has to supply bytes at an offset.
  Entirely internal: `SharedFile` never left `src/read/inner.rs`, every caller
  already went through `FileInner`'s two methods, and the in-place tag patch
  opens its own handle, so nothing outside that file changed.

  The trait fills a caller-owned buffer rather than returning one (the
  zero-allocation scan loop is pinned by a test) and has no `len()` — the file
  length is captured once at open, so no bounds check asks the source its size.
  Measured against the pre-change baseline, one virtual call per multi-MB record
  is not detectable: `scan` −1.2%/−2.2%/−6.0%, `columns` +1.5%/+1.2%/+3.5%,
  `open` −5.9%/0.0%/−0.5%, all inside this machine's run-to-run spread.
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

### Fixed

- **`to_dask` no longer builds a degenerate partition for an empty range.**
  `entry_start == entry_stop` landing *inside* a record produced a zero-width
  batch rather than none, so the array got a partition spanning no events and
  two equal divisions — which dask requires to be increasing. The same range
  past the end of the chain already raised; both now do. Found while giving
  `map_reduce` the same batch logic.
- **`read_columns_at` no longer re-reads a record per index.** Every lookup went
  through `Chain::event` and its single-slot record cache, so the cost depended
  entirely on the order of the list: 256 indices that happened to ascend cost
  13 µs, the same 256 scattered cost **7 ms**, because each one decompressed a
  whole record that the next call threw away.

  Entries are now resolved up front and grouped by the record holding them, so
  each record is read once whatever the order, and the groups run in parallel
  (`threads`, matching `read_columns`). Scattered reads are **~26× faster**
  (7.09 ms → 271 µs, `lz4`); ascending reads, which the cache already handled,
  stay put. This is what `arrays(entries=[...])` runs on, and a list of
  interesting events found by an earlier pass is rarely sorted.

  `Chain::read_columns_at` takes a trailing `threads` argument to match its
  siblings — a breaking change, called out here because the crate is pre-1.0.
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

- **`Chain.map_reduce(fn, ...)` — run the analysis in the workers.** `workers=`
  on `arrays`/`iterate` parallelises only the read: workers hand raw buffers
  back and the parent does the physics serially, which for a CLAS12 selection is
  where the time goes. `map_reduce` runs `fn` on each chunk *in* the worker and
  sends back only its return value — a filled `hist.Hist` pickles to a few
  hundred bytes against the hundreds of megabytes it was filled from.

  `reduce=` defaults to `operator.add`, which `hist.Hist`, `boost_histogram`,
  `np.ndarray` and numbers already implement. Results are folded in **event
  order** rather than completion order, so a non-commutative `reduce` is safe,
  and the parent holds one accumulator rather than every chunk's result.
  `initial=` seeds it and defines the empty-selection answer; without one an
  empty range raises instead of returning `None`.
- **`ox.link(banks)` — `pindex` cross-references across a whole read.** Wires
  both directions at once, so `ev["REC::Calorimeter"].particle.px` and
  `ev["REC::Particle"]["REC::Calorimeter"]` both work and the join becomes
  something you follow rather than something you write. Banks with no `pindex`
  pass through untouched.

  `directions=` exists because the two sides do not cost the same: the
  detector→particle side copies a particle record onto every detector row, which
  on a bank with ten times the rows is ten copies of each momentum. The other
  side regroups and copies nothing.

  An out-of-range `pindex` is `None` going forward and dropped going back —
  never attached to whichever particle happens to be there.
- **`ox.group_by_index(detector, counts)` — the `pindex` join.** Detector banks
  point at their particle by row number, and the project's own tutorial calls
  learning that join "the single most useful CLAS12-specific skill" — then does
  it by hand, `ak.sum(cal.energy[cal.pindex == 0], axis=1)`, which answers for
  one hardcoded particle.

  This regroups a detector bank into one sublist per particle, in particle
  order, so `ak.sum(by_particle.energy, axis=-1)` is a per-particle column that
  can be attached beside `px`. Selections compose before the reduction.

  A `pindex` outside its event's particle range is dropped rather than clamped:
  it names a particle that is not there, and folding it onto particle 0 would
  put that energy on a real track. Particles with no rows get an empty sublist,
  so the result always aligns with the particle array.
- **`ox.to_vector(array, mass=...)` — Lorentz-vector behaviours** over a
  momentum bank, via [vector](https://vector.readthedocs.io). `v.E`, `v.pt`,
  `v.eta`, `(v[:, 0] + v[:, 1]).mass` and `deltaR` on columns that were three
  flat arrays, with `mass="pdg"` taking each row's mass from its `pid`.

  Omitting `mass` gives a **3-vector**, not a massless 4-vector: an assumed-zero
  mass wearing a four-vector's interface is how a wrong invariant mass happens.
  `pid == 0` carries `nan` into `E` for the same reason. Columns not needed for
  the vector come through untouched, so cuts still work on the result.

  A function rather than a keyword on `arrays()`, so it composes with `iterate`
  chunks, `to_dask` partitions and post-`cut=` results instead of only the one
  call. `vector` is an optional extra (`pip install oxihipo[vector]`).
- **`ox.pdg_mass(pid)` — PDG masses in bulk**, plus `ox.pdg_name` and the
  `ox.PDG_MASS_GEV` table behind them. Every tutorial hardcoded the constants it
  needed (`M_PIP = 0.139570`, `M_P = 0.938272`) because there was nothing to
  call, and the obvious substitute fails on exactly the two things a CLAS12
  `REC::Particle` column is full of: `Particle.from_pdgid(0)` raises, though
  `pid == 0` is simply a track the reconstruction could not identify, and
  `from_pdgid(45)` raises, though CLAS12 writes **Geant3** codes for light
  nuclei (45/46/47/49 = D/T/He4/He3) and the project's own PID table documents
  45 as the deuteron.

  It keeps the shape it is given — scalar, NumPy, or the jagged `ak.Array` a
  column read returns — so a mass column lines up with the momenta beside it.
  Unknown codes give `nan` rather than raising. Masses are GeV, matching CLAS12
  momenta; `particle` reports MeV, and mixing them is a factor of 10³.

  No new dependency: the table is baked (generated from `particle`, and a test
  cross-checks all 44 codes against it when it is installed), so the answer does
  not change with the environment. One `searchsorted`, ~37 ms over 2M particles
  against ~3 s per-row — 61×, not the 250× the design note estimated. Its
  suggested `np.unique` step turned out to be slower than looking up directly,
  since it sorts the column to save a lookup over 44 entries.
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

[Unreleased]: https://github.com/mathieuouillon/oxihipo/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/mathieuouillon/oxihipo/compare/v0.7.1...v0.8.0
[0.7.1]: https://github.com/mathieuouillon/oxihipo/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/mathieuouillon/oxihipo/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/mathieuouillon/oxihipo/compare/v0.5.3...v0.6.0
[0.5.3]: https://github.com/mathieuouillon/oxihipo/compare/v0.5.2...v0.5.3
[0.5.2]: https://github.com/mathieuouillon/oxihipo/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/mathieuouillon/oxihipo/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/mathieuouillon/oxihipo/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/mathieuouillon/oxihipo/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/mathieuouillon/oxihipo/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/mathieuouillon/oxihipo/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/mathieuouillon/oxihipo/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/mathieuouillon/oxihipo/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/mathieuouillon/oxihipo/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/mathieuouillon/oxihipo/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/mathieuouillon/oxihipo/releases/tag/v0.1.0
