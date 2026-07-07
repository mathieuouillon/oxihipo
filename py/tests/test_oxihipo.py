"""Tests for the oxihipo Python binding.

Runs against small committed fixtures (``tests/data/*.hipo``, written by
``src/bin/gen_sample.rs``) so no Rust build is needed to run them. The data
model: 8 events; event ``i`` has ``i % 4`` particles (so 0, 4 are empty) with
``pid = i*100 + r``, an array column ``cov`` (``float32[3]``), and one
``REC::Event`` row per event with ``evno = 1000 + i``.
"""

import os

import numpy as np
import pytest

import oxihipo

DATA = os.path.join(os.path.dirname(__file__), "data")
FIXTURES = ["sample.hipo", "sample_none.hipo"]  # Lz4PerColumn and None

SURV = [i for i in range(8) if i % 4 > 0]  # events carrying REC::Particle
PART_OFFSETS = [0, 0, 1, 3, 6, 6, 7, 9, 12]
PID = [100, 200, 201, 300, 301, 302, 500, 600, 601, 700, 701, 702]


@pytest.fixture(params=FIXTURES)
def chain(request):
    return oxihipo.open(os.path.join(DATA, request.param))


# --- metadata / discovery --------------------------------------------------
def test_len_and_counts(chain):
    assert chain.num_entries == 8
    assert len(chain) == 8
    assert chain.file_count == 1
    assert len(chain.files) == 1


def test_keys_typenames_contains(chain):
    assert chain.keys() == ["REC::Particle", "REC::Event"]
    assert chain.keys(recursive=True) == [
        "REC::Particle/pid",
        "REC::Particle/px",
        "REC::Particle/cov",
        "REC::Event/evno",
    ]
    assert chain.keys(recursive=True, filter_name="*/px") == ["REC::Particle/px"]
    assert dict(chain.typenames())["REC::Particle/cov"] == "float32[3]"
    assert "REC::Particle" in chain and "NOPE" not in chain


# --- the NumPy path (no awkward needed) ------------------------------------
def test_numpy_path(chain):
    values, offsets, inner = chain.numpy("REC::Particle", "pid")
    assert offsets.dtype == np.int64 and offsets.tolist() == PART_OFFSETS
    assert values.dtype == np.int32 and values.tolist() == PID
    assert inner == 1
    cov_vals, _, cov_inner = chain.numpy("REC::Particle", "cov")
    assert cov_inner == 3 and cov_vals.shape == (12 * 3,)


def test_read_columns_multi_bank(chain):
    res = chain.read_columns(
        [("REC::Particle", ["pid"]), ("REC::Event", ["evno"])]
    )
    (pb, poff, pcols), (eb, eoff, ecols) = res
    assert pb == "REC::Particle" and poff.tolist() == PART_OFFSETS
    assert pcols[0][1].tolist() == PID
    assert eb == "REC::Event" and eoff.tolist() == list(range(9))
    assert ecols[0][1].tolist() == [1000 + i for i in range(8)]


# --- the Awkward path ------------------------------------------------------
def test_arrays_single_bank(chain):
    ak = pytest.importorskip("awkward")
    p = chain.arrays("REC::Particle", ["pid", "px", "cov"])
    assert str(p.type) == "8 * var * {pid: int32, px: float32, cov: 3 * float32}"
    assert ak.to_list(p.pid) == [[], [100], [200, 201], [300, 301, 302], [], [500], [600, 601], [700, 701, 702]]
    assert p[2].pid.tolist() == [200, 201]  # event → particle
    assert p[3].cov[0].tolist() == [300.0, 300.5, 300.25]  # array cell


def test_array_single_column(chain):
    ak = pytest.importorskip("awkward")
    px = chain.array("REC::Particle", "px")
    assert len(px) == 8 and px[2].tolist()[0] == pytest.approx(2.0)


def test_arrays_multi_bank_record(chain):
    ak = pytest.importorskip("awkward")
    ev = chain.arrays(["REC::Particle", "REC::Event"])
    assert set(ev.fields) == {"REC::Particle", "REC::Event"}
    assert ak.to_list(ev["REC::Event"].evno) == [[1000 + i] for i in range(8)]


def test_getitem_and_bank_proxy(chain):
    ak = pytest.importorskip("awkward")
    assert chain["REC::Particle/pid"][3].tolist() == [300, 301, 302]
    part = chain["REC::Particle"]
    assert part.keys() == ["pid", "px", "cov"]
    assert part.typenames()["cov"] == "float32[3]"
    assert ak.to_list(part["pid"])[2] == [200, 201]
    with pytest.raises(KeyError):
        chain["NOPE"]


def test_filter_name(chain):
    ak = pytest.importorskip("awkward")
    a = chain.arrays(filter_name="REC::Particle/p*")
    assert set(a["REC::Particle"].fields) == {"pid", "px"}


# --- library dispatch ------------------------------------------------------
def test_library_np(chain):
    d = chain.arrays("REC::Particle", ["pid", "cov"], library="np")
    assert d["pid"].dtype == object
    assert d["pid"][2].tolist() == [200, 201]
    assert d["cov"][3].shape == (3, 3)  # 3 rows × inner 3
    assert d["pid"][0].tolist() == []  # empty event


def test_library_pd(chain):
    pytest.importorskip("awkward")
    pd = pytest.importorskip("pandas")
    df = chain.arrays("REC::Particle", ["pid", "px"], library="pd")
    assert list(df.columns) == ["pid", "px"] and len(df) == 12


def test_library_unknown_raises(chain):
    with pytest.raises(ValueError):
        chain.arrays("REC::Particle", library="xml")


# --- range -----------------------------------------------------------------
def test_entry_range(chain):
    ak = pytest.importorskip("awkward")
    p = chain.arrays("REC::Particle", ["pid"], entry_start=4, entry_stop=8)
    # global events [4,8): rows 0,1,2,3 → pids [], [500], [600,601], [700,701,702]
    assert ak.to_list(p.pid) == [[], [500], [600, 601], [700, 701, 702]]


# --- filter + skim ---------------------------------------------------------
def test_filtered(chain):
    ak = pytest.importorskip("awkward")
    g = chain.filtered(require=["REC::Particle"])
    assert g.num_entries == 8  # pre-filter total, uproot semantics
    ev = g.arrays("REC::Event", ["evno"])
    assert len(ev) == len(SURV)
    assert ak.to_list(ev.evno) == [[1000 + i] for i in SURV]


def test_filtered_unknown_bank_raises(chain):
    with pytest.raises(KeyError):
        chain.filtered(require=["NOPE"])


def test_skim(chain, tmp_path):
    ak = pytest.importorskip("awkward")
    out = str(tmp_path / "skim.hipo")
    summary = chain.filtered(require=["REC::Particle"]).skim(out, compression="lz4percolumn")
    assert summary["events"] == len(SURV)
    reopened = oxihipo.open(out)
    assert reopened.num_entries == len(SURV)
    assert ak.to_list(reopened.array("REC::Event", "evno")) == [[1000 + i] for i in SURV]


def test_open_missing_is_empty_chain():
    # A bare name that isn't a file/dir is treated as a no-match glob → empty.
    c = oxihipo.open("definitely-not-a-real-file.hipo")
    assert c.num_entries == 0 and c.file_count == 0


# A SINGLE path auto-detects (file/dir/glob); a LIST is taken verbatim. Both
# fixtures share the same dictionary, so the chain of the two is 16 events.
def test_open_directory():
    c = oxihipo.open(DATA)
    assert c.file_count == 2 and c.num_entries == 16


def test_open_glob():
    c = oxihipo.open(os.path.join(DATA, "*.hipo"))
    assert c.file_count == 2 and c.num_entries == 16


def test_open_list_verbatim():
    c = oxihipo.open([os.path.join(DATA, "sample.hipo"), os.path.join(DATA, "sample_none.hipo")])
    assert c.file_count == 2 and c.num_entries == 16


# --- streaming (bounded-memory iterate) ------------------------------------
def test_record_spans_and_sizes(chain):
    spans = chain.record_spans()  # (file_idx, record_idx, global_start, event_count)
    assert [s[3] for s in spans] == [3, 3, 2]  # max_record_events=3 over 8 events
    assert sum(s[3] for s in spans) == 8
    sizes = chain.record_decompressed_sizes()
    assert len(sizes) == len(spans) and all(s > 0 for s in sizes)


def test_iterate_reassembles(chain):
    ak = pytest.importorskip("awkward")
    full = chain.arrays("REC::Particle", ["pid"])
    chunks = list(chain.iterate("REC::Particle", ["pid"], step_size=2))
    assert len(chunks) == 3  # record-aligned: 3 records
    assert ak.to_list(ak.concatenate(chunks)) == ak.to_list(full)


def test_iterate_byte_step(chain):
    ak = pytest.importorskip("awkward")
    full = chain.arrays("REC::Particle", ["pid"])
    chunks = list(chain.iterate("REC::Particle", ["pid"], step_size="1 KB"))
    assert ak.to_list(ak.concatenate(chunks)) == ak.to_list(full)


def test_iterate_report(chain):
    pytest.importorskip("awkward")
    seen = 0
    for chunk, rep in chain.iterate("REC::Particle", ["pid"], step_size=3, report=True):
        assert isinstance(rep, oxihipo.Report)
        assert rep.entry_stop > rep.entry_start
        assert rep.file_path.endswith(".hipo")
        seen += len(chunk)
    assert seen == 8


def test_iterate_multifile_is_file_aligned():
    ak = pytest.importorskip("awkward")
    cc = oxihipo.open(DATA)  # 2 files
    report = list(cc.iterate("REC::Event", ["evno"], step_size=1000, report=True))
    assert len({r.file_path for _, r in report}) == 2  # a chunk never spans files
    assert sum(len(x) for x, _ in report) == 16


def test_module_level_iterate():
    total = sum(
        len(x)
        for x in oxihipo.iterate(os.path.join(DATA, "*.hipo"), "REC::Event", ["evno"], step_size=5)
    )
    assert total == 16


def test_iterate_bad_step_size(chain):
    with pytest.raises(ValueError):
        list(chain.iterate("REC::Particle", step_size="200 furlongs"))
    with pytest.raises(ValueError):
        list(chain.iterate("REC::Particle", step_size=0))
