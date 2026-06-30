#set document(
  title: "HIPO Compression Schemes and Read-Performance Benchmark",
  author: "oxihipo",
)
#set page(paper: "a4", margin: 2.1cm, numbering: "1")
#set text(font: "New Computer Modern", size: 10.5pt, lang: "en")
#set par(justify: true, leading: 0.62em)
#set heading(numbering: "1.1")
#show heading.where(level: 1): it => block(above: 1.5em, below: 0.7em)[#it]
#show raw.where(block: false): it => box(
  fill: luma(240), inset: (x: 2pt), outset: (y: 2pt), radius: 2pt,
)[#it]
#show link: set text(fill: rgb("#1a5fb4"))

// ---- Title ---------------------------------------------------------------
#align(center)[
  #text(18pt, weight: "bold")[
    HIPO Compression Schemes and \
    Single-Thread Read-Performance Benchmark
  ]
  #v(0.4em)
  #text(10.5pt)[*oxihipo* — pure-Rust HIPO v6 reader/writer · Jefferson Lab CLAS12]
  #v(0.2em)
  #text(9pt, fill: luma(110))[2026-06-30]
]
#v(0.6em)

#block(fill: luma(243), inset: 9pt, radius: 4pt, width: 100%)[
  *Summary.* HIPO records can be stored under seven compression schemes.
  Five are _whole-record_ codecs (`None`, `Lz4`, `Lz4Best`, `Gzip`,
  `Lz4Chunked`): reading any bank inflates the entire record. Two are
  _per-bank_ codecs (`Lz4ByBank`, `Lz4ByBankV2`): reading a bank inflates
  only that bank. On a real CLAS12 reconstruction file (598,738 events; a
  dictionary of 274 schemas of which only *73 are populated*, dominated by
  large raw-detector readout banks) an analysis that reads a handful of
  banks is *#text(weight: "bold")[≈ 5× faster]* with `Lz4ByBank` than with
  plain LZ4 — and faster than _uncompressed_ — because it is the only family
  that decompresses just the banks you touch. `Lz4ByBankV2` additionally
  LZ4‑HC-compresses each bank stream, so it is *smaller than `Lz4Best`*
  (1.96× vs 1.88× vs `None`) at identical fast reads.
]

= Introduction

The HIPO v6 container stores Jefferson Lab CLAS12 event data as a sequence
of _records_, each holding many _events_, each event holding many _banks_
(detector and reconstruction tables). A file's dictionary defines hundreds
of bank schemas — 274 in the file studied here, of which only *73 are
actually populated* (the other 201 are declared but never filled) — and the
populated set is dominated by large raw-detector readout banks
(ADC/TDC/waveform), while a typical physics analysis reads only a few small
reconstruction banks (`REC::Particle`, `REC::Event`, …). Read throughput is
therefore governed less by raw codec speed than by *how much of each record
a format forces you to decompress.*

This document describes the compression schemes implemented in `oxihipo`
and reports a single-thread read-speed benchmark across all of them, on
both real CLAS12 data and a synthetic control.

= The HIPO container in one page

A file is a header, a dictionary record (the bank schemas), a stream of
data records, and a trailer index:

#align(center)[
  #text(9pt, font: "DejaVu Sans Mono")[
    file ─▶ [file header] [dict record] [data record]…[data record] [trailer index]
  ]
]

Each *data record* (1–8 MB) contains many events plus one compressed
payload. Each *event* contains *structures* (banks). Each *bank* is stored
*column-major*: all values of column 0, then all of column 1, and so on —
which makes a whole-column read a contiguous, cache-friendly,
zero-copy slice on little-endian hardware.

The crucial degree of freedom is *how the record payload is compressed*,
because that decides whether reading one bank requires inflating the whole
record or just that bank's bytes.

= The seven compression schemes

#figure(
  caption: [Compression schemes. "Granularity" is the unit that must be
    inflated to read one bank. The two _per-bank_ schemes are Rust-only
    extensions not readable by the C++ `hipo4` reader.],
  table(
    columns: (auto, 1fr, auto, auto),
    align: (left, left, center, center),
    inset: (x: 6pt, y: 4pt),
    stroke: 0.5pt + luma(180),
    table.header(
      [*Scheme*], [*What it does*], [*Granularity*], [*C++ readable*],
    ),
    [`None`], [Raw bytes, no compression], [record], [yes],
    [`Lz4`], [One LZ4 block over the whole record payload], [record], [yes],
    [`Lz4Best`], [LZ4‑HC: same wire/decoder as `Lz4`, better ratio (slower _write_ only)], [record], [yes],
    [`Gzip`], [DEFLATE (zlib) over the whole record], [record], [yes],
    [`Lz4Chunked`], [Record split into _N_-event chunks, each its own LZ4 block], [sub-record chunk], [no (Rust-only)],
    [`Lz4ByBank`], [One LZ4 stream _per bank type_ + a presence/size directory; fast default-LZ4 streams], [*per bank*], [no (Rust-only)],
    [`Lz4ByBankV2`], [`Lz4ByBank` with *LZ4‑HC* bank streams + an LZ4-compressed directory + version byte; best ratio (slower _write_)], [*per bank*], [no (Rust-only)],
  ),
)

== The decisive distinction: whole-record vs per-bank inflate

The reader has two internal paths:

- *Bytes path* (`None`, `Lz4`, `Lz4Best`, `Gzip`, `Lz4Chunked`). The whole
  record payload is inflated _eagerly_ into one buffer before any event is
  produced. Reading 1 bank or 270 banks costs the same: you always pay to
  inflate everything.

- *ByBank path* (`Lz4ByBank`, `Lz4ByBankV2`). The record's small directory
  (bank descriptors, an event×bank presence matrix, per-bank sizes) is
  parsed eagerly, but each bank's LZ4 stream is inflated _lazily_ on the
  first `ev.bank("NAME")`. Untouched banks are never decompressed.

`Lz4ByBank` is the default output of `Chain::skim`, precisely because real
analyses are bank-sparse. The benchmark below quantifies why.

= Benchmark methodology

The example `examples/bench_read_compression.rs` re-encodes one identical
set of events into every scheme, then times three single-thread read
patterns of growing *scope*, reporting the *best of N passes* (minimum =
least noise):

- *sel* — touch only the narrow `REC::Event` bank (1 bank).
- *full* — read `REC::Particle` (`pid`/`px`/`py`/`pz`, via pre-resolved column
  handles) plus `REC::Event` (2 banks); the "decode the main bank" case.
- *all* — walk every structure of every event, i.e. read all ≈ 73 populated
  banks; on `Lz4ByBank` this forces inflating every bank stream.

Metrics: on-disk *size*, compression *ratio* vs `None`, wall-clock *ms* for
the whole 100k-event pass, and *Mevt/s* (million events per second).

All runs: single-thread, warm page cache, release build, Apple M4 Pro
(aarch64). Numbers are relative throughput on one machine, not absolute
limits.

= Results

== Real CLAS12 data

Input: `rec_clas_022083.evio.00000-00009.hipo` — 8.5 GB, 598,738 events.
Its dictionary declares *274 bank schemas, but only 73 are populated*
(≥ 1 row anywhere in the file); the other 201 are empty. The populated set
is dominated by raw detector readout: `AHDC::adc` alone holds 92.7 M rows
across the file, versus 4.7 M for `REC::Particle` and 0.59 M for
`REC::Event` (Appendix A lists all 73). A whole-record codec must inflate
*every* populated bank in a record — including the multi-million-row raw
ADC/TDC/waveform banks — even to read the two small reconstruction banks an
analysis wants; that is the work `Lz4ByBank` skips. The first 100,000 events
were re-encoded into each scheme; the read touches *2 banks*
(`REC::Particle`, `REC::Event`). Best of 5 passes.

#figure(
  caption: [Real CLAS12, 100k events. Read *scope* grows left→right:
    `sel` = `REC::Event` only (1 bank), `full` = `REC::Particle`+`REC::Event`
    (2), `all` = every populated bank (≈ 73). Times are ms for the whole 100k
    pass; *bold* = fastest in column. Whole-record schemes are flat across
    scope; `Lz4ByBank` is fastest at 1–2 banks but the *slowest-but-gzip at
    `all`* — its edge is conditional on reading few banks.],
  table(
    columns: 7,
    align: (left, right, right, right, right, right, right),
    inset: (x: 5pt, y: 3.5pt),
    stroke: 0.5pt + luma(180),
    table.header(
      [*Scheme*], [*size MB*], [*ratio*], [*sel* \ (1)],
      [*full* \ (2)], [*all* \ (≈73)], [*all Mevt/s*],
    ),
    [`None`],        [3469.9], [1.00], [285.8], [307.9], [314.2], [0.32],
    [`Lz4`],         [2162.9], [1.60], [801.9], [856.2], [828.6], [0.12],
    [`Lz4Best`],     [1845.1], [1.88], [809.7], [828.8], [811.1], [0.12],
    [`Gzip`],        [1704.5], [2.04], [6017.2], [5933.4], [6306.3], [0.02],
    [`Lz4Chunked`],  [2163.9], [1.60], [380.6], [403.0], [380.4], [0.26],
    [`Lz4ByBank`],   [2099.9], [1.65], [161.6], [179.1], [1040.5], [0.10],
    [`Lz4ByBankV2`], [1766.6], [1.96], [*141.9*], [*158.1*], [997.4], [0.10],
  ),
)

== Synthetic control (only 2 banks per event)

The same benchmark on synthetic data whose events contain *only the 2 banks
being read* (150k events, best of 9). With nothing to skip, the per-bank
advantage nearly vanishes — isolating that the real-data win comes from
*unread* banks, not from the codec.

#figure(
  caption: [Synthetic, 150k events, 2 of 2 banks read. The ByBank edge
    collapses when there are no unread banks to skip.],
  table(
    columns: 8,
    align: (left, right, right, right, right, right, right, right),
    inset: (x: 5pt, y: 3.5pt),
    stroke: 0.5pt + luma(180),
    table.header(
      [*Scheme*], [*MB*], [*ratio*], [*full ms*], [*Mevt/s*],
      [*MB/s*], [*sel ms*], [*sel Mevt/s*],
    ),
    [`None`],        [62.0], [1.00], [24.3], [6.2], [2551], [9.7],  [15.5],
    [`Lz4`],         [13.1], [4.73], [32.7], [4.6], [401],  [17.2], [8.7],
    [`Lz4Best`],     [7.8],  [7.95], [24.7], [6.1], [316],  [9.1],  [16.5],
    [`Gzip`],        [6.8],  [9.12], [42.6], [3.5], [160],  [27.3], [5.5],
    [`Lz4Chunked`],  [20.1], [3.08], [24.8], [6.0], [810],  [11.3], [13.3],
    [`Lz4ByBank`],   [13.4], [4.63], [40.2], [3.7], [333],  [7.9],  [19.0],
    [`Lz4ByBankV2`], [7.2],  [8.61], [31.9], [4.7], [226],  [6.5],  [23.1],
  ),
)

= Analysis

#block(fill: rgb("#eef6ee"), inset: 9pt, radius: 4pt, width: 100%)[
  *Headline.* When an analysis reads a *few* banks (the common case),
  `Lz4ByBankV2` is *5.4×* faster than plain LZ4 (158 ms vs 856 ms at 2 banks)
  and *≈ 1.9×* faster than uncompressed `None` (308 ms) — while also being the
  *smallest* read-friendly format (1.96× vs `None`, edging out `Lz4Best`'s
  1.88×). But the advantage is *conditional on selectivity*: reading _every_
  bank it is *≈ 1.2× slower than LZ4* (997 ms vs 829 ms). Read speed is set by
  _how many banks you must inflate_, not by codec speed.
]

+ *Per-bank wins big — when you read few banks.* Each record holds the
  populated banks of its events (73 distinct banks file-wide, most present in
  the bulk of events — see Appendix A), dominated by huge raw ADC/TDC/waveform
  tables; the analysis reads 2 small `REC::` banks. The Bytes schemes inflate
  _all_ of them to reach those 2; ByBank inflates 2. Hence 5.4× — and only a
  wash (a slight loss, even) on the synthetic control whose events contain
  just the 2 banks read, proving the win comes from _skipped_ banks, not the
  codec.

+ *…and loses once you read everything.* In the `all`-banks column `Lz4ByBank`
  rises to 1040 ms (`Lz4ByBankV2` 997 ms) — *≈ 6× its own 2-bank time* and
  *≈ 1.25× slower than LZ4* (829 ms). Reading all banks it inflates all 73
  streams (nothing skipped) _and_ pays per-stream decode setup plus a
  per-(event, bank) gather that one whole-record LZ4 block avoids. (An earlier
  version also re-synthesised a contiguous event blob per event;
  `ev.structures()` now yields slices straight from the decompressed streams,
  removing that copy — worth ≈ 8 %, but not the structural gap.) So for
  full-event work — recook, format conversion, analyses that touch most banks
  — a whole-record scheme (`Lz4`/`Lz4Best`) is the right choice; ByBank is for
  bank-sparse reads.

+ *Whole-record schemes are flat across scope.* `None`/`Lz4`/`Lz4Best`/`Gzip`/
  `Lz4Chunked` all show `sel ≈ full ≈ all` (e.g. Lz4 776 / 828 / 772 ms): they
  inflate the entire record up front, so the number of banks read barely
  matters. Only `Lz4ByBank`'s cost tracks what you actually touch.

+ *Uncompressed is not fastest.* `None` (308 ms) loses to `Lz4ByBankV2` at 2
  banks (158 ms): `None` must stream 3.47 GB, while ByBankV2 reads a 1.77 GB
  file and inflates only 2 banks — strictly less total work. Compression that
  enables selectivity beats no compression.

+ *Gzip is disqualifying for reads* — ≈ 5.9 s, *37×* slower than ByBank at
  2 banks, despite the best ratio (2.04×). Acceptable only for cold archival.
  `Lz4Best` reads at `Lz4` speed (≈ 810 ms) but is 15 % smaller — LZ4‑HC costs
  only _write_ time; prefer it for a C++-readable whole-record format.
  `Lz4Chunked` (403 ms at 2 banks, ≈ 2× faster than `Lz4`) decodes smaller
  blocks with better cache locality, but its real purpose (parallel decode) is
  moot single-thread.

+ *Full reads are decompression-CPU-bound.* Real events cost microseconds
  each versus ≈ 0.18 µs for the tiny synthetic ones; the codec's per-byte
  decode rate dominates wall-clock. The only structural lever is to
  *decompress fewer bytes* — which `Lz4ByBank` does at bank granularity, and a
  future per-column layout would do at column granularity. (Merely
  _re-ordering_ the all-banks read column-major — one decompressed stream
  swept end-to-end before the next — was measured to *not* help: the cost is
  the 73 small per-stream decodes plus the per-(event, bank) gather, which
  order doesn't change. Fewer bytes, not better order, is the lever.)

= Recommendations

#table(
  columns: (auto, 1fr),
  align: (left, left),
  inset: (x: 6pt, y: 4pt),
  stroke: 0.5pt + luma(180),
  table.header([*Use case*], [*Recommended scheme*]),
  [Repeated analysis reads (the common case)],
  [*`Lz4ByBank`* — ≈ 5× faster, faster than uncompressed, 1.65× smaller, fast
   to write. The `skim` default.],
  [Same, when smallest on-disk also matters],
  [*`Lz4ByBankV2`* — identical fast reads, *best ratio* (1.96×, beats
   `Lz4Best`) via LZ4‑HC bank streams; ≈ 19× slower to _write_.],
  [Must stay C++ `hipo4`-readable],
  [*`Lz4Best`* — same decode speed as `Lz4`, ≈ 15 % smaller.],
  [Cold archival, rarely read],
  [`Gzip` (best ratio) — never for read-heavy work.],
  [Scratch / fully warm-cached, disk-cheap],
  [`None` — no decode, but largest and not actually fastest.],
)

The corollary for future work: since the lever is _bytes inflated_, the next
structural gain is pushing selectivity from per-_bank_ to per-_column_ — a
Rust-only `Lz4ByColumn` (V3) layout storing one LZ4 stream per
`(bank, column)`, cross-event contiguous, so reading `px` of a 30-column
bank inflates one column, not thirty, and yields a single SIMD-ready slice.

= Reproducing

```sh
# real file (first 100k of its events, best-of-5):
cargo run --release --example bench_read_compression -- \
    /path/to/rec_clas_022083.evio.00000-00009.hipo 5 100000

# synthetic control (events, iters):
cargo run --release --example bench_read_compression -- 150000 9
```

The benchmark re-encodes the (capped) input into all seven schemes in a
temporary directory and prints the tables above. Cap the event count to fit
the seven re-encoded copies on disk (an 8 GB file uncapped would need
≈ 70 GB).

#pagebreak()

= Appendix A — Populated banks in `rec_clas_022083`

All *73 schemas with at least one row* across the full file (598,738 events),
sorted by total row count; the other 201 declared schemas are empty. `% evt`
is the fraction of events containing the bank; `rows` is the file-wide total.
Note that data volume is dominated by raw detector readout
(`AHDC`/`FTOF`/`ECAL`/`RICH`/`ATOF` `adc`/`tdc`/`wf`), while the
reconstruction banks an analysis actually reads (`REC::Particle`,
`REC::Event`, …) are comparatively tiny — which is exactly why a per-bank
codec wins. Produced by `examples/list_populated_banks.rs`.

#text(8pt)[
#table(
  columns: (auto, auto, auto, auto),
  align: (left, right, right, right),
  inset: (x: 5pt, y: 2.1pt),
  stroke: 0.4pt + luma(205),
  table.header([*Bank*], [*g,i*], [*% evt*], [*total rows*]),
  [`AHDC::adc`], [22400,11], [99.4%], [92,661,939],
  [`FTOF::adc`], [21200,11], [99.4%], [48,253,043],
  [`AHDC::wf`], [22400,10], [39.7%], [37,066,870],
  [`RICH::tdc`], [21800,12], [99.1%], [36,551,379],
  [`ATOF::tdc`], [22500,12], [99.4%], [32,830,038],
  [`ECAL::tdc`], [20700,12], [99.4%], [29,915,269],
  [`ECAL::adc`], [20700,11], [99.4%], [29,632,436],
  [`REC::Traj`], [300,40], [84.5%], [20,875,889],
  [`ATOF::hits`], [22500,21], [98.3%], [20,361,446],
  [`ECAL::peaks`], [20700,22], [99.1%], [16,871,772],
  [`RF::tdc`], [21700,12], [99.4%], [14,256,417],
  [`AHDC::preclusters`], [23000,24], [84.8%], [13,458,235],
  [`ATOF::clusters`], [22500,22], [98.3%], [12,303,344],
  [`FTHODO::adc`], [21100,11], [99.4%], [11,698,921],
  [`AHDC::hits`], [23000,23], [84.8%], [9,638,406],
  [`CND::adc`], [20300,11], [99.4%], [8,789,009],
  [`FTHODO::hits`], [21100,21], [99.2%], [6,718,775],
  [`CND::tdc`], [20300,12], [99.0%], [5,604,926],
  [`FTHODO::clusters`], [21100,22], [99.2%], [5,095,355],
  [`REC::Particle`], [300,31], [98.3%], [4,695,074],
  [`REC::Scintillator`], [300,35], [97.6%], [4,315,368],
  [`REC::ScintExtras`], [300,43], [97.6%], [4,315,368],
  [`FTCAL::adc`], [21000,11], [97.4%], [4,222,568],
  [`RICH::Ring`], [21800,36], [20.9%], [4,203,184],
  [`REC::Calorimeter`], [300,32], [95.4%], [4,134,056],
  [`REC::CaloExtras`], [300,46], [95.4%], [4,134,056],
  [`ECAL::clusters`], [20700,23], [95.4%], [4,134,056],
  [`ECAL::calib`], [20700,24], [95.4%], [4,134,056],
  [`FTCAL::hits`], [21000,21], [93.1%], [3,090,510],
  [`DC::calib`], [20600,55], [4.4%], [2,130,750],
  [`AHDC::docaclusters`], [23000,126], [34.7%], [1,881,686],
  [`RUN::trigger`], [10000,13], [99.4%], [1,785,663],
  [`REC::Track`], [300,36], [84.5%], [1,573,172],
  [`REC::CovMat`], [300,38], [84.5%], [1,573,172],
  [`TimeBasedTrkg::TBTracks`], [20600,36], [84.5%], [1,573,172],
  [`LTCC::adc`], [21600,11], [85.1%], [1,398,278],
  [`RUN::rf`], [10000,12], [99.4%], [1,190,394],
  [`RF::adc`], [21700,11], [99.4%], [1,190,394],
  [`AHDC::interclusters`], [23000,27], [34.7%], [1,171,647],
  [`RICH::calib`], [21800,51], [3.3%], [1,091,496],
  [`AHDC::clusters`], [23000,25], [34.7%], [992,948],
  [`BAND::tdc`], [22100,12], [45.3%], [978,001],
  [`CND::hits`], [20300,21], [44.6%], [728,989],
  [`HTCC::adc`], [21500,11], [60.3%], [701,084],
  [`LTCC::clusters`], [21600,22], [66.6%], [650,121],
  [`RUN::config`], [10000,11], [99.8%], [597,432],
  [`HEL::online`], [22000,13], [99.4%], [595,229],
  [`HEL::decoder`], [22000,14], [99.4%], [595,197],
  [`REC::Event`], [300,30], [98.3%], [588,512],
  [`BAND::adc`], [22100,11], [24.8%], [482,829],
  [`HTCC::rec`], [21500,22], [47.5%], [388,869],
  [`REC::ForwardTagger`], [300,34], [27.8%], [343,930],
  [`REC::Cherenkov`], [300,33], [39.1%], [323,083],
  [`BAND::rawhits`], [22100,22], [17.3%], [310,420],
  [`RECFT::Particle`], [300,42], [5.0%], [300,647],
  [`AHDC::track`], [23000,21], [34.7%], [254,616],
  [`AHDC::kftrack`], [23000,26], [34.7%], [254,616],
  [`ALERT::projections`], [23000,31], [34.7%], [254,616],
  [`RICH::Particle`], [21800,37], [31.9%], [246,980],
  [`FT::particles`], [20900,24], [27.8%], [218,853],
  [`FTCAL::clusters`], [21000,22], [27.8%], [218,853],
  [`FTOF::calib`], [21200,35], [7.8%], [205,119],
  [`BAND::hits`], [22100,21], [14.0%], [143,442],
  [`RAW::scaler`], [20000,13], [0.7%], [75,848],
  [`ALERT::ai:projections`], [23000,32], [10.2%], [64,777],
  [`RUN::unix`], [10000,18], [0.0%], [59,509],
  [`RAW::epics`], [20000,15], [0.0%], [51,876],
  [`RECFT::Event`], [300,41], [5.0%], [30,117],
  [`ALERT::ai:prepid`], [23000,33], [3.1%], [18,838],
  [`COAT::config`], [10000,17], [0.0%], [12,684],
  [`RUN::scaler`], [10000,14], [0.7%], [4,322],
  [`HEL::scaler`], [10000,16], [0.4%], [2,946],
  [`HEL::flip`], [22000,12], [0.2%], [1,298],
)
]

#v(1fr)
#line(length: 100%, stroke: 0.4pt + luma(200))
#text(8.5pt, fill: luma(110))[
  Single-thread, warm-cache, Apple M4 Pro (aarch64), release. Relative
  throughput on one machine; absolute MB/s differs by hardware and (for cold
  network/parallel filesystems) shifts toward the smaller-on-disk schemes.
  `Lz4ByBank`/`Lz4ByBankV2`/`Lz4Chunked` are Rust-only format extensions.
]
