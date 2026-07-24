# Cross-implementation read benchmark

`cargo bench` tracks oxihipo against *itself* over time. This measures it
against the **other two HIPO implementations** — the C++ `hipo4` reader and the
Java `jnp-hipo4` reader — because that is the claim in the top-level README, and
a claim like that should be reproducible rather than folklore.

It is not run by CI: it needs a C++ build and a JVM, and benchmark timings from
a shared CI runner would be noise.

## What it measures

All three programs do **identical work**: open the file, scan every event, read
`REC::Particle`'s `pid` (int32) and `px` (float32), and accumulate both into
checksums. Each prints its checksums, and **the run is only valid if all three
match** — that is what proves they did the same work rather than, say, one of
them skipping a bank.

Each uses its own library's documented fast path, with **column indices
resolved once** outside the loop:

| | read loop | column access |
| --- | --- | --- |
| Rust | `chain.events()` | `bank.read(handle)` → typed slice |
| C++ | `reader.next(banks)` | `bank.getInt(item, row)` |
| Java | `reader.nextEvent(ev)` + `ev.read(bank)` | `bank.getInt(item, row)` |

Note the asymmetry, because it explains part of the result: the Rust API hands
back a **typed column slice** the loop can walk (and the compiler can
vectorise), while the C++ and Java APIs expose **per-element accessors** that
recompute an offset per value. That is an API-shape difference, not just a
codegen one. Using the *name*-taking accessors instead (`getInt("pid", row)`)
costs C++ roughly **3.5x** — the first version of this benchmark did exactly
that and produced a badly misleading result.

## Running it

```sh
# 1. fixture: 200k events, LZ4 (the only compression all three read)
#    gen_fixture.rs is a bin in a scratch crate that depends on oxihipo
cargo run --release --bin gen_fixture -- /tmp/xbench.hipo lz4 200000

# 2. Rust
cargo run --release --bin xbench_rs -- /tmp/xbench.hipo 10

# 3. C++ (against a built hipo-cpp)
clang++ -std=c++17 -O3 -DNDEBUG -I$HIPO_CPP xbench_cpp.cc -o xbench_cpp \
    -L$HIPO_CPP/build/hipo4 -lhipo4 -Wl,-rpath,$HIPO_CPP/build/hipo4
./xbench_cpp /tmp/xbench.hipo 10

# 4. Java (against the jnp-hipo4 jar)
javac -cp $JAR -d . XBenchJava.java && java -cp "$JAR:." XBenchJava /tmp/xbench.hipo 10
```

Each prints `impl<TAB>cold<TAB>warm<TAB>events<TAB>rows<TAB>sum_pid<TAB>sum_px`.
Run them **serially** — concurrent runs contend for memory bandwidth and the
numbers become meaningless.

## Results

Apple Silicon, macOS; 200k events / 600k particle rows; 9.7 MB on disk, 18.4 MB
decompressed; LZ4. `cold` is the first pass in a fresh process, `warm` is the
best of 10 in-process passes. Checksums matched across all three
(`sum_pid = 8400000`, `sum_px = 60800102.851`).

| | cold (ms) | warm (ms) | warm Mevent/s | warm GB/s | vs C++ warm |
| --- | --- | --- | --- | --- | --- |
| **Rust** (oxihipo) | **9.2** | 8.3 | 24.1 | 2.16 | **1.19x** |
| C++ (`hipo4`) | 11.5 | 9.9 | 20.2 | 1.82 | 1.00x |
| Java (`jnp-hipo4`) | 40.0 | **8.2** | 24.4 | 2.19 | 1.21x |

### Reading this honestly

- **Steady state, Rust ≈ Java, both ~20% ahead of C++.** oxihipo is faster than
  the C++ reader, but by about a fifth — not a step change. The top-level
  README's "meaningfully exceeds" is fair; anything stronger is not.
- **Java is not slow once warm.** After JIT warm-up it matches Rust on this
  workload. Any claim that the Rust reader is dramatically faster than Java is
  wrong for steady-state scanning.
- **Cold start is where they separate.** A single pass in a fresh process — what
  a short analysis job or a per-file batch worker actually pays — costs Java
  **~4.3x** more than Rust (40 ms vs 9 ms) because the JIT has not warmed up.
  That, not peak throughput, is oxihipo's real advantage over the JVM reader.
- **Scope.** One workload (sequential scan, two scalar columns), one file shape,
  one machine. It does not cover partial/selective reads — where oxihipo's
  per-column format has an advantage no whole-record reader can match — nor
  writing, nor multi-file chains.
