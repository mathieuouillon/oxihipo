# Cross-implementation read benchmark

`cargo bench` tracks oxihipo against *itself* over time. This measures it
against the **other HIPO implementations** — the C++ `hipo4` reader, the Java
`jnp-hipo4` reader, and oxihipo's own Python bindings — because the top-level
README makes a throughput claim, and a claim like that should be reproducible.

Not run by CI: it needs a C++ build and a JVM, and timings from a shared runner
would be noise.

## The validity gate

Every program prints a **checksum** of everything it read. A run is only valid
if all four agree — that is what proves they did the same work rather than one
of them quietly skipping a bank. The results below all passed:

| scenario | checksum |
| --- | --- |
| `count` | `0.000` (touches no bank) |
| `bank1` | `19999900000.000` |
| `col1` | `8400000.000` |
| `scan2` | `69200102.851` |
| `wide` | `2170341596.656` |

Floating-point columns must be accumulated in `f64` in every implementation. An
`f32` accumulator drifts (`69199880` vs `69200102.851`) and trips the gate —
which is the gate doing its job.

## Scenarios

| scenario | work | what it isolates |
| --- | --- | --- |
| `count` | iterate every event, touch no bank | iteration + record decode floor |
| `bank1` | read `REC::Event.evno` only | cost of reaching a *small* bank past a big one |
| `col1` | read `REC::Particle.pid` only | one column out of eight |
| `scan2` | read `pid` + `px` | the common two-column analysis read |
| `wide` | read all 8 `REC::Particle` columns | per-column overhead at scale |

## The two API shapes

This is the single most important thing to understand before reading the
numbers — most of the spread is API shape, not language.

| | how you read | implementations |
| --- | --- | --- |
| **per-event** | loop events, ask a bank for values | Rust, C++, Java |
| **columnar** | one bulk pass materialises whole columns | Rust (`read_columns`), Python |

Within per-event, the accessors differ too: Rust's `bank.read(handle)` returns a
**typed column slice**, while C++ and Java expose **per-element** getters. That
difference cuts both ways — see `wide` below.

## Fixture

200 000 events, three banks (`REC::Particle` ×8 columns, `REC::Event` ×2,
`REC::Calorimeter` ×3 present on 1/3 of events), 1–5 particles per event.

| format | size | vs uncompressed |
| --- | --- | --- |
| `none` | 27.1 MB | 1.00× |
| `lz4` | 16.3 MB | 1.66× |
| `lz4best` | 13.2 MB | 2.05× |
| `lz4percolumn` | 7.7 MB | **3.51×** |

## Results

Apple Silicon, macOS, LZ4 unless stated. `warm` = best of 10 in-process passes;
`cold` = the first pass in a fresh process. Serial (`threads=1`) unless stated.

### Per-event API — warm ms (cold ms)

| scenario | Rust | C++ | Java |
| --- | --- | --- | --- |
| `count` | **6.0** (8.8) | 16.3 (17.0) | 6.7 (25.2) |
| `bank1` | 11.8 (14.1) | 16.9 (17.9) | **8.6** (33.7) |
| `col1` | 13.0 (13.8) | 16.9 (18.0) | **10.1** (43.6) |
| `scan2` | 15.8 (17.2) | 17.3 (18.3) | **9.7** (40.0) |
| `wide` | 33.0 (35.2) | 19.9 (20.7) | **16.4** (63.5) |

### Columnar API — warm ms

| scenario | Rust 1 thread | Python 1 thread | Rust all cores | Python all cores |
| --- | --- | --- | --- | --- |
| `bank1` | 7.1 | 6.8 | **2.9** | 3.5 |
| `col1` | 6.7 | 8.3 | 6.6 | **3.6** |
| `scan2` | 9.5 | 9.2 | **4.4** | 4.6 |
| `wide` | 16.9 | 14.8 | **7.1** | 7.3 |

### Compression — `scan2`, per-event, warm ms

| format | Rust | C++ | Java |
| --- | --- | --- | --- |
| `none` | 13.1 | 16.5 | **8.3** |
| `lz4` | 16.4 | 17.6 | **11.2** |
| `lz4best` | 15.6 | 16.5 | **10.2** |
| `lz4percolumn` | **12.8** | n/a | n/a |

`lz4percolumn` is a Rust-only format extension; the released C++ and Java
readers reject it (correctly — they have no decoder for tag 7). It is both the
smallest on disk **and** the fastest to read here.

## What the numbers actually say

**Java is fast once warm, and slow cold.** After JIT warm-up it wins most
per-event scenarios. But `cold` — one pass in a fresh process, which is what a
short analysis job or a per-file batch worker pays — costs it **2.6–4.3×** more
than Rust (e.g. 40.0 ms vs 17.2 ms on `scan2`). That, not peak throughput, is
where the Rust reader is decisively ahead of the JVM one.

**C++ is the flattest.** It is the slowest on the cheap scenarios (`count`,
`col1`) because `reader.next(banks)` materialises the requested banks whether or
not you read them, but it barely moves as you add columns — 17.3 → 19.9 ms from
`scan2` to `wide` — so it *wins `wide` outright* against Rust.

**Rust's per-event `wide` is its weak spot** (33.0 ms, 1.7× slower than C++).
`bank.read(handle)` sets up a column view per call; with 1–5 rows per event that
setup never amortises, and eight of them per event dominate. The library's
answer is the columnar API: `wide` drops 33.0 → 16.9 ms serial, 7.1 ms across
cores. **If you are reading many columns, do not loop events.**

**The Python binding costs ~0–10%, not a multiple.** Serial columnar Python is
0.87–1.10× native Rust, because the decode happens in Rust with the GIL
released and the columns move into NumPy zero-copy. On `wide` Python is
*faster* (14.8 vs 16.9 ms) — NumPy's vectorised reduce beats the plain `f64`
accumulation loop the Rust program uses.

**Headline claim, checked:** oxihipo beats the C++ reader on 4 of 5 per-event
scenarios (1.1–2.7×) and loses `wide`. "Meaningfully exceeds" is fair for
typical analysis reads; it is not a uniform win, and this file says so.

## Two measurement traps

Both of these produced badly wrong numbers before being caught. If you re-run
this, check them first.

1. **Name-taking accessors.** An early draft had C++ use `getInt("pid", row)`,
   which hashes the name *per value*. That alone made C++ look 3.5× slower
   (38.8 → 11.2 ms). All implementations now resolve column indices once,
   outside the loop.
2. **Implicit threading.** The Python binding defaults to `threads=0`, meaning
   *every core*, while the Rust call was passing `1`. That made Python look 2×
   faster than the Rust code it calls. Both now take an explicit thread count,
   and serial and parallel are reported separately.

Also: Rust originally reused one open file across iterations while C++ and Java
reopened, giving it warm scratch buffers the others lacked. All four now reopen
per iteration, with open + dictionary parsing outside the timed region.

## Running it

```sh
SCRATCH=/tmp/xbench
HIPO_CPP=/path/to/hipo-cpp          # built: meson setup build . && ninja -C build
JAR=/path/to/jnp-hipo4-4.3-SNAPSHOT-all.jar
```

**1. Fixtures** (`gen_fixture.rs` / `xbench_rs.rs` are bins in a scratch crate
that depends on `oxihipo`):

```sh
for f in none lz4 lz4best percolumn; do
  cargo run --release --bin xfixture -- $SCRATCH/xb_$f.hipo $f 200000
done
```

**2. Rust** — `<file> <scenario> [iters] [threads]`; prefix a scenario with
`c_` for the columnar API, `threads=0` means every core:

```sh
cargo run --release --bin xbench_rs -- $SCRATCH/xb_lz4.hipo scan2 10 1
cargo run --release --bin xbench_rs -- $SCRATCH/xb_lz4.hipo c_wide 10 0
```

**3. C++**:

```sh
clang++ -std=c++17 -O3 -DNDEBUG -I$HIPO_CPP xbench_cpp.cc -o xbench_cpp \
    -L$HIPO_CPP/build/hipo4 -lhipo4 -Wl,-rpath,$HIPO_CPP/build/hipo4
./xbench_cpp $SCRATCH/xb_lz4.hipo scan2 10
```

**4. Java**:

```sh
javac -cp $JAR -d . XBenchJava.java
java -cp "$JAR:." XBenchJava $SCRATCH/xb_lz4.hipo scan2 10
```

**5. Python** — `<file> <scenario> [iters] [threads]`:

```sh
pip install oxihipo
python xbench_py.py $SCRATCH/xb_lz4.hipo scan2 10 1
```

Each prints `impl<TAB>scenario<TAB>cold<TAB>warm<TAB>events<TAB>checksum`
(seconds). **Run them serially** — concurrent runs contend for memory bandwidth
and the numbers stop meaning anything. Then check every implementation printed
the same checksum for a given scenario before believing any of it.

## Scope

One machine, one file shape, LZ4-family compression, read-only. It does not
cover writing, multi-file chains, filtered/tagged reads, or array (`T#N`)
columns — the released C++ reader has no `T#N` support, so that comparison
cannot be made against it today.
