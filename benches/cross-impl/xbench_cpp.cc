// Cross-implementation read benchmark (C++ / hipo4 master).
// Identical workload to xbench_rs.rs and XBenchJava.java.
#include <chrono>
#include <cstdio>
#include <string>
#include <vector>
#include "hipo4/reader.h"
#include "hipo4/dictionary.h"
#include "hipo4/bank.h"

int main(int argc, char** argv) {
  if (argc < 2) { std::fprintf(stderr, "usage: xbench_cpp <file> [iters]\n"); return 2; }
  const std::string path = argv[1];
  const int iters = argc > 2 ? std::atoi(argv[2]) : 5;

  double best = 1e300, first = -1;
  long long events = 0, rows = 0, csp = 0; double csx = 0;
  for (int it = 0; it < iters; it++) {
    // Reopen per iteration: the Rust and Java loops re-scan the same open file,
    // but the C++ reader has no rewind, so this is the closest equivalent. The
    // open cost (header + dictionary) is a few hundred microseconds against a
    // multi-millisecond scan.
    hipo::reader r;
    r.open(path.c_str());
    hipo::dictionary dict;
    r.readDictionary(dict);
    std::vector<hipo::bank> banks;
    banks.emplace_back(dict.getSchema("REC::Particle"));
    // Resolve the column indices once, mirroring the pre-resolved handles the
    // Rust loop uses; the name-taking accessors hash per call.
    const int i_pid = banks[0].getSchema().getEntryOrder("pid");
    const int i_px  = banks[0].getSchema().getEntryOrder("px");

    auto t0 = std::chrono::steady_clock::now();
    long long sp = 0; double sx = 0; long long ev = 0, rw = 0;
    while (r.next(banks)) {
      ev++;
      hipo::bank& b = banks[0];
      const int n = b.getRows();
      for (int i = 0; i < n; i++) sp += b.getInt(i_pid, i);
      for (int i = 0; i < n; i++) sx += b.getFloat(i_px, i);
      rw += n;
    }
    auto t1 = std::chrono::steady_clock::now();
    double dt = std::chrono::duration<double>(t1 - t0).count();
    asm volatile("" :: "r"(sp), "r"(sx) : "memory");   // defeat elimination
    events = ev; rows = rw; csp = sp; csx = sx;
    if (first < 0) first = dt;
    if (dt < best) best = dt;
  }
  std::printf("cpp\t%.4f\t%.4f\t%lld\t%lld\t%lld\t%.3f\n", first, best, events, rows, csp, csx);
  return 0;
}
