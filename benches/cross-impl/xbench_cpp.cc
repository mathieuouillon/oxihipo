// Cross-implementation read benchmark — C++ (hipo4).
//   xbench_cpp <file.hipo> <scenario> [iters]
// Same scenarios and checksums as xbench_rs.rs / XBenchJava.java / xbench_py.py.
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <string>
#include <vector>
#include "hipo4/reader.h"
#include "hipo4/dictionary.h"
#include "hipo4/bank.h"

int main(int argc, char** argv) {
  if (argc < 3) { std::fprintf(stderr, "usage: xbench_cpp <file> <scenario> [iters]\n"); return 2; }
  const std::string path = argv[1], scen = argv[2];
  const int iters = argc > 3 ? std::atoi(argv[3]) : 10;

  double best = 1e300, first = -1;
  long long events = 0; double csum = 0;
  for (int it = 0; it < iters; it++) {
    hipo::reader r;
    r.open(path.c_str());
    hipo::dictionary dict;
    r.readDictionary(dict);
    std::vector<hipo::bank> banks;
    banks.emplace_back(dict.getSchema("REC::Particle"));   // 0
    banks.emplace_back(dict.getSchema("REC::Event"));      // 1
    hipo::bank& P = banks[0];
    hipo::bank& E = banks[1];
    // Resolve column indices once (the name-taking accessors hash per call).
    const int i_pid = P.getSchema().getEntryOrder("pid");
    const int i_px  = P.getSchema().getEntryOrder("px");
    const int i_py  = P.getSchema().getEntryOrder("py");
    const int i_pz  = P.getSchema().getEntryOrder("pz");
    const int i_vz  = P.getSchema().getEntryOrder("vz");
    const int i_ch  = P.getSchema().getEntryOrder("charge");
    const int i_st  = P.getSchema().getEntryOrder("status");
    const int i_c2  = P.getSchema().getEntryOrder("chi2pid");
    const int i_ev  = E.getSchema().getEntryOrder("evno");

    auto t0 = std::chrono::steady_clock::now();
    double sum = 0; long long ev = 0;
    while (r.next(banks)) {
      ev++;
      if (scen == "count") continue;
      if (scen == "bank1") {
        const int n = E.getRows();
        for (int i = 0; i < n; i++) sum += (double)E.getLong(i_ev, i);
        continue;
      }
      const int n = P.getRows();
      if (scen == "col1") {
        for (int i = 0; i < n; i++) sum += P.getInt(i_pid, i);
      } else if (scen == "scan2") {
        for (int i = 0; i < n; i++) sum += P.getInt(i_pid, i);
        for (int i = 0; i < n; i++) sum += P.getFloat(i_px, i);
      } else if (scen == "wide") {
        for (int i = 0; i < n; i++) sum += P.getInt(i_pid, i);
        for (int i = 0; i < n; i++) sum += P.getFloat(i_px, i);
        for (int i = 0; i < n; i++) sum += P.getFloat(i_py, i);
        for (int i = 0; i < n; i++) sum += P.getFloat(i_pz, i);
        for (int i = 0; i < n; i++) sum += P.getFloat(i_vz, i);
        for (int i = 0; i < n; i++) sum += P.getByte(i_ch, i);
        for (int i = 0; i < n; i++) sum += P.getShort(i_st, i);
        for (int i = 0; i < n; i++) sum += P.getFloat(i_c2, i);
      } else { std::fprintf(stderr, "unknown scenario\n"); return 2; }
    }
    auto t1 = std::chrono::steady_clock::now();
    double dt = std::chrono::duration<double>(t1 - t0).count();
    asm volatile("" :: "r"(&sum) : "memory");
    events = ev; csum = sum;
    if (first < 0) first = dt;
    if (dt < best) best = dt;
  }
  std::printf("cpp\t%s\t%.5f\t%.5f\t%lld\t%.3f\n", scen.c_str(), first, best, events, csum);
  return 0;
}
