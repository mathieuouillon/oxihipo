// Generate golden HIPO files with the REFERENCE C++ hipo4 writer (master).
// Usage: gen_golden <out.hipo>    (reference C++ master always writes LZ4)
//
// Dataset is deterministic and documented in tests/cpp_golden.rs:
//   REC::Event    (300,30): evno/L, beamE/F           -- 1 row per event
//   REC::Particle (300,31): pid/I, px/F, py/F, pz/D, status/S, charge/B
//                                                     -- (e % 4) rows
//   16 events, several records (max 4 events/record).
#include <cstdio>
#include <string>
#include "hipo4/writer.h"
#include "hipo4/dictionary.h"
#include "hipo4/event.h"
#include "hipo4/bank.h"

int main(int argc, char** argv) {
  if (argc < 2) { std::fprintf(stderr, "usage: gen_golden <out>\n"); return 2; }
  // The reference C++ master writer always emits LZ4 (compression type 1);
  // there is no setter. That is also what real CLAS12 files use.

  hipo::schema ev_s("REC::Event", 300, 30);   ev_s.parse("evno/L,beamE/F");
  hipo::schema p_s("REC::Particle", 300, 31); p_s.parse("pid/I,px/F,py/F,pz/D,status/S,charge/B");

  hipo::writer w;
  w.getDictionary().addSchema(ev_s);
  w.getDictionary().addSchema(p_s);
  w.addUserConfig("generator", "cpp-hipo4-master");
  w.addUserConfig("dataset", "golden-v1");
  w.open(argv[1]);

  hipo::event event;
  for (int e = 0; e < 16; e++) {
    event.reset();
    { hipo::bank b(ev_s, 1);
      b.putLong("evno", 0, 1000 + e);
      b.putFloat("beamE", 0, 10.6f);
      event.addStructure(b); }
    int n = e % 4;
    if (n > 0) {
      hipo::bank b(p_s, n);
      for (int r = 0; r < n; r++) {
        b.putInt("pid", r, 11 + e * 10 + r);
        b.putFloat("px", r, (float)(e) * 0.5f + (float)r);
        b.putFloat("py", r, (float)(e) * -0.25f + (float)r);
        b.putDouble("pz", r, (double)e * 0.125 + (double)r);
        b.putShort("status", r, (short)(e * 4 + r));
        b.putByte("charge", r, (int8_t)(r - 1));
      }
      event.addStructure(b);
    }
    w.addEvent(event);
  }
  w.close();
  std::printf("wrote %s (lz4)\n", argv[1]);
  return 0;
}
