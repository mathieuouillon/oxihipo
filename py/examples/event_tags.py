"""Per-event tags — read them, filter by name, label a skim, retag in place.

    python event_tags.py [FILE.hipo]

Every HIPO event carries a 32-bit tag in its header. oxihipo reads it without
inflating any bank, and filters on it with *pushdown* — non-matching events are
dropped before their banks are decoded. A file can also carry a persisted
name-to-bit registry, so you filter by `"dvcs"` instead of remembering `1 << 0`.

The bundled sample tags event `i` with `1 << (i % 3)` and ships the registry
`{dvcs: 0, sidis: 1, elastic: 2}`.
"""

import os
import shutil
import sys
import tempfile

import awkward as ak
import numpy as np

import oxihipo as ox

DATA = os.path.join(os.path.dirname(__file__), "..", "tests", "data")
src = sys.argv[1] if len(sys.argv) > 1 else os.path.join(DATA, "sample.hipo")
tmp = tempfile.mkdtemp()

f = ox.open(src)

# --- 1. read the tags and the file's registry ------------------------------
tags = f.event_tags()          # uint32, one per event, aligned 1:1 with arrays()
print(f"{f.num_entries} events")
print("event_tags():", tags.tolist())
print("tag_names   :", f.tag_names)      # {} if the file carries no registry

# The tag column lines up with the data, so it composes with normal Awkward:
evno = ak.to_numpy(ak.flatten(f.array("REC::Event", "evno")))
print("evno of tag==1 events:", evno[tags == 1].tolist())

# --- 2. filter by tag — by bit, by mask, or by name ------------------------
if f.tag_names:
    by_name = f.filtered(event_tag="dvcs")                 # bit 0
    either = f.filtered(event_tag=["dvcs", "elastic"])     # dvcs OR elastic
    print("\nfiltered(event_tag='dvcs')          ->",
          ak.to_list(ak.flatten(by_name.array("REC::Event", "evno"))))
    print("filtered(event_tag=['dvcs','elastic'])->",
          ak.to_list(ak.flatten(either.array("REC::Event", "evno"))))

# Numeric forms always work, registry or not:
print("filtered(event_tag=[2])             ->",
      ak.to_list(ak.flatten(f.filtered(event_tag=[2]).array("REC::Event", "evno"))))
print("filtered(event_tag_any=0b101)       ->",
      len(f.filtered(event_tag_any=0b101).array("REC::Event", "evno")), "events")

# --- 3. tag-and-skim: compute a label, write it into a new DST -------------
# Vectorized over all events — no Python loop. `tags=` must align 1:1 with the
# events the chain yields; `tag_names=` records the registry so the output is
# self-describing.
p = f.arrays("REC::Particle", ["pid"])
mult = ak.to_numpy(ak.num(p.pid))
new_tags = np.where(mult >= 2, 1 << 0, 0).astype(np.uint32)   # bit 0 = "busy"

tagged = os.path.join(tmp, "tagged.hipo")
summary = f.skim(tagged, tags=new_tags, tag_names={"busy": 0})
print(f"\ntag-and-skim: {summary.events} events -> {tagged}")

g = ox.open(tagged)
print("re-read registry:", g.tag_names)
busy = g.filtered(event_tag="busy")
print("busy events (mult >= 2):",
      ak.to_list(ak.flatten(busy.array("REC::Event", "evno"))))

# --- 4. retag one event in place, no rewrite -------------------------------
# Only uncompressed files can be patched (the tag lives inside a compressed
# block otherwise) — this is a single 4-byte pwrite, so the file size is
# unchanged and no bank is touched.
plain = os.path.join(tmp, "plain.hipo")
shutil.copy(os.path.join(DATA, "sample_none.hipo"), plain)   # written with compression="none"
before = os.path.getsize(plain)

h = ox.open(plain)
h.set_event_tag(2, 0xBEEF)                 # one event
h.set_event_tags({4: 7, 6: 8})             # a batch, all-or-nothing

after = os.path.getsize(plain)
print(f"\nin-place retag: size {before} -> {after} bytes (unchanged: {before == after})")
print("tags now:", [hex(t) for t in ox.open(plain).event_tags().tolist()])

# A compressed file refuses, leaving the bytes untouched:
try:
    ox.open(os.path.join(DATA, "sample.hipo")).set_event_tag(1, 5)
except ValueError as e:
    print("compressed file rejected as expected:", str(e)[:70], "...")
