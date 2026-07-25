"""Tests for the oxihipo Python binding.

Runs against small committed fixtures (``tests/data/*.hipo``, written by
``src/bin/gen_sample.rs``) so no Rust build is needed to run them. The data
model: 8 events; event ``i`` has ``i % 4`` particles (so 0, 4 are empty) with
``pid = i*100 + r``, an array column ``cov`` (``float32[3]``), one
``REC::Event`` row per event with ``evno = 1000 + i``, and a per-event tag
``1 << (i % 3)`` (so tags cycle 1, 2, 4). The file also carries a tag-name
registry ``{dvcs: 0, sidis: 1, elastic: 2}``.
"""

import os
import shutil

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
    # tuple indexing: (bank, col) → one column; (bank, [cols]) → a record
    assert chain["REC::Particle", "pid"][3].tolist() == [300, 301, 302]
    assert set(chain["REC::Particle", ["pid", "px"]].fields) == {"pid", "px"}
    with pytest.raises(KeyError):
        chain["REC::Particle", "pid", "px"]  # wrong-arity tuple
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
    assert isinstance(summary, oxihipo.SkimSummary)
    assert summary.events == len(SURV)  # attribute access…
    assert tuple(summary) == (summary.events, summary.records, summary.bytes)  # …and positional
    reopened = oxihipo.open(out)
    assert reopened.num_entries == len(SURV)
    assert ak.to_list(reopened.array("REC::Event", "evno")) == [[1000 + i] for i in SURV]


def test_open_missing_file_raises():
    # A wildcard-free path that isn't a file/dir is a mistake (typo, wrong cwd)
    # and must raise, not silently open an empty chain (that footgun made a
    # typo'd filename look like "0 events").
    with pytest.raises(OSError):
        oxihipo.open("definitely-not-a-real-file.hipo")


def test_open_glob_no_match_is_empty_chain():
    # A pattern with glob metacharacters that matches nothing is still a
    # legitimately empty chain, not an error.
    c = oxihipo.open("definitely-not-a-real-dir-*/*.hipo")
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


# --- threads knob + arrow interop ------------------------------------------
def test_threads_knob_equivalence(chain):
    ak = pytest.importorskip("awkward")
    ref = ak.to_list(chain.arrays("REC::Particle", ["pid", "cov"], threads=0))
    for t in (1, 2, 4):
        got = ak.to_list(chain.arrays("REC::Particle", ["pid", "cov"], threads=t))
        assert got == ref, f"threads={t}"


def test_iterate_threads(chain):
    ak = pytest.importorskip("awkward")
    full = chain.arrays("REC::Particle", ["pid"])
    chunks = list(chain.iterate("REC::Particle", ["pid"], step_size=3, threads=2))
    assert ak.to_list(ak.concatenate(chunks)) == ak.to_list(full)


def test_library_arrow(chain):
    pytest.importorskip("awkward")
    pytest.importorskip("pyarrow")
    tbl = chain.arrays("REC::Particle", ["pid", "px"], library="arrow")
    assert tbl.num_rows == 8  # one row per event; columns are jagged lists
    assert {"pid", "px"} <= set(tbl.column_names)


# --- multi-process reading (workers=), for parallel-filesystem I/O ----------
def test_parallel_arrays_matches_single(chain):
    ak = pytest.importorskip("awkward")
    single = chain.arrays("REC::Particle", ["pid", "px", "cov"])
    par = chain.arrays("REC::Particle", ["pid", "px", "cov"], workers=2)
    assert ak.to_list(par) == ak.to_list(single)


def test_parallel_arrays_multibank(chain):
    ak = pytest.importorskip("awkward")
    single = chain.arrays(["REC::Particle", "REC::Event"])
    par = chain.arrays(["REC::Particle", "REC::Event"], workers=3)
    assert set(par.fields) == set(single.fields)
    assert ak.to_list(par["REC::Event"].evno) == ak.to_list(single["REC::Event"].evno)


def test_parallel_iterate_matches_single(chain):
    ak = pytest.importorskip("awkward")
    full = chain.arrays("REC::Particle", ["pid"])
    chunks = list(chain.iterate("REC::Particle", ["pid"], step_size=1, workers=2))
    assert ak.to_list(ak.concatenate(chunks)) == ak.to_list(full)


def test_parallel_respects_filter(chain):
    ak = pytest.importorskip("awkward")
    g = chain.filtered(require=["REC::Particle"])  # workers must reapply the filter
    single = g.arrays("REC::Event", ["evno"])
    par = g.arrays("REC::Event", ["evno"], workers=2)
    assert ak.to_list(par.evno) == ak.to_list(single.evno)
    assert len(par) == len(SURV)


def test_module_arrays_workers():
    ak = pytest.importorskip("awkward")
    single = oxihipo.arrays(os.path.join(DATA, "sample.hipo"), "REC::Particle", ["pid"])
    par = oxihipo.arrays(os.path.join(DATA, "sample.hipo"), "REC::Particle", ["pid"], workers=2)
    assert ak.to_list(par) == ak.to_list(single)


# --- correctness fixes (empty selections, guards, degenerate ranges) --------
def test_empty_selection_returns_empty_not_crash(chain):
    """A non-matching filter_name / empty bank list yields an empty (length-0)
    array on every library instead of throwing from the assembler."""
    ak = pytest.importorskip("awkward")
    empty = chain.arrays(filter_name="REC::NoSuchBank*")  # default library="ak"
    assert len(empty) == 0 and list(empty.fields) == []
    assert ak.to_list(chain.arrays([], library="ak")) == ak.to_list(empty)
    assert chain.arrays(filter_name="REC::NoSuchBank*", library="np") == {}


def test_empty_selection_arrow(chain):
    pytest.importorskip("pyarrow")
    tbl = chain.arrays(filter_name="REC::NoSuchBank*", library="arrow")
    assert tbl.num_columns == 0  # empty table, not a crash


def test_empty_selection_workers_matches_single(chain):
    """Empty selection + workers>1 must not IndexError (the parallel stitch runs
    before the assembler's empty guard) — it must match the workers=1 result."""
    ak = pytest.importorskip("awkward")
    ak_s = chain.arrays(filter_name="REC::NoSuchBank*")
    ak_p = chain.arrays(filter_name="REC::NoSuchBank*", workers=2)
    assert len(ak_p) == len(ak_s) == 0
    assert chain.arrays([], library="np", workers=2) == {}


def test_concat_raw_handles_empty_results():
    """The stitch returns [] for all-empty worker results, so the assembler's
    empty guard runs instead of an IndexError."""
    from oxihipo import _parallel

    assert _parallel._concat_raw([]) == []
    assert _parallel._concat_raw([[], []]) == []


def test_np_zero_event_matches_ak_length(chain):
    """0-event reads agree between np and ak (np.split used to give length-1)."""
    ak = pytest.importorskip("awkward")
    d = chain.arrays("REC::Particle", ["pid"], library="np", entry_start=0, entry_stop=0)
    a = chain.arrays("REC::Particle", ["pid"], entry_start=0, entry_stop=0)
    assert len(d["pid"]) == len(a) == 0


def test_np_values_match_awkward(chain):
    ak = pytest.importorskip("awkward")
    d = chain.arrays("REC::Particle", ["pid", "cov"], library="np")
    a = chain.arrays("REC::Particle", ["pid", "cov"])
    assert [list(x) for x in d["pid"]] == ak.to_list(a.pid)
    assert [x.tolist() for x in d["cov"]] == ak.to_list(a.cov)


def test_columns_with_multiple_banks_raises(chain):
    with pytest.raises(TypeError):
        chain.arrays(["REC::Particle", "REC::Event"], ["pid"])
    with pytest.raises(TypeError):
        chain.arrays(filter_name="REC::*", columns=["pid"])


def test_step_size_rejects_bool_and_zero_bytes(chain):
    with pytest.raises(TypeError):
        list(chain.iterate("REC::Particle", step_size=True))
    with pytest.raises(ValueError):
        list(chain.iterate("REC::Particle", step_size="0 MB"))


def test_parallel_empty_range_matches_single(chain):
    """workers>1 on an empty/degenerate range returns empty like workers=1
    (used to IndexError in the stitch)."""
    ak = pytest.importorskip("awkward")
    par = chain.arrays("REC::Particle", ["pid"], workers=4, entry_start=99, entry_stop=99)
    assert len(par) == 0


# --- Pythonic surface -------------------------------------------------------
def test_numpy_returns_named_tuple(chain):
    col = chain.numpy("REC::Particle", "pid")
    v, o, i = col  # still unpacks positionally
    assert col.values is v and col.offsets is o and col.inner_len == i


def test_context_manager_and_close(chain):
    with oxihipo.open(os.path.join(DATA, "sample.hipo")) as f:
        assert f.num_entries == 8
    assert f._c is None  # released on exit
    assert repr(f) == "<oxihipo.Chain: closed>"  # repr never throws
    with pytest.raises(ValueError):  # clean error, not opaque NoneType
        f.num_entries
    with pytest.raises(ValueError):
        len(f)
    with pytest.raises(ValueError):
        f.arrays("REC::Particle")


def test_arrow_array_column_and_workers(chain):
    pa = pytest.importorskip("pyarrow")
    ak = pytest.importorskip("awkward")
    # T#N array column → FixedSizeList (the inner>1 branch)
    cov_ak = chain.arrays("REC::Particle", ["cov"]).cov
    cov_tb = chain.arrays("REC::Particle", ["cov"], library="arrow").column("cov")
    assert cov_tb.to_pylist() == ak.to_list(cov_ak)
    # arrow + workers>1 stitches to the same table as workers=1
    a = chain.arrays("REC::Particle", ["pid", "px"], library="arrow")
    b = chain.arrays("REC::Particle", ["pid", "px"], library="arrow", workers=2)
    assert a.column("pid").to_pylist() == b.column("pid").to_pylist()
    assert a.column("px").to_pylist() == b.column("px").to_pylist()


def test_chain_and_proxy_are_iterable(chain):
    assert list(chain) == chain.keys()  # __iter__ over bank names
    proxy = chain["REC::Particle"]
    assert list(proxy) == proxy.keys() and len(proxy) == len(proxy.keys())


def test_dir_surfaces_delegated_reader_methods(chain):
    d = dir(chain)
    assert "num_entries" in d and "record_spans" in d and "arrays" in d


def test_copy_does_not_recurse(chain):
    import copy

    shallow = copy.copy(chain)  # used to RecursionError
    assert shallow._c is chain._c
    with pytest.raises(TypeError):  # frozen reader isn't deep-copyable — clean error
        copy.deepcopy(chain)


def test_event_tag_filter(chain):
    ak = pytest.importorskip("awkward")
    # tag = 1 << (i % 3), evno = 1000 + i.  tag == 1 → i in {0, 3, 6}.
    got = ak.to_list(chain.filtered(event_tag=[1]).array("REC::Event", "evno"))
    assert got == [[1000], [1003], [1006]]

    # event_tag_any is a bitmask: bit 0 or bit 2 → tags {1, 4} → i in {0,2,3,5,6}.
    n = len(ak.to_list(chain.filtered(event_tag_any=0b101).array("REC::Event", "evno")))
    assert n == 5

    # ANDs with require: tag == 2 AND carries REC::Particle (i % 4 > 0) → i in {1, 7}.
    both = chain.filtered(require=["REC::Particle"], event_tag=[2])
    assert ak.to_list(both.array("REC::Event", "evno")) == [[1001], [1007]]

    # a tag no event carries → an empty result, not an error.
    assert ak.to_list(chain.filtered(event_tag=[999]).array("REC::Event", "evno")) == []


def test_event_tag_filter_survives_workers(chain):
    # The event-tag filter must reach the worker processes (workers=).
    ak = pytest.importorskip("awkward")
    g = chain.filtered(event_tag=[1])
    single = ak.to_list(g.arrays("REC::Event", ["evno"]).evno)
    par = ak.to_list(g.arrays("REC::Event", ["evno"], workers=2).evno)
    assert par == single == [[1000], [1003], [1006]]


def test_event_tags_column(chain):
    # per-event tag as a flat uint32 column, aligned with arrays().
    ak = pytest.importorskip("awkward")
    tags = chain.event_tags()
    assert tags.dtype == np.uint32
    assert tags.tolist() == [1, 2, 4, 1, 2, 4, 1, 2]  # 1 << (i % 3)

    # lines up 1:1 with the event axis of arrays(), so it can cut on it.
    p = chain.arrays("REC::Event", ["evno"])
    assert len(tags) == len(p)
    evno = ak.to_numpy(ak.flatten(p.evno))
    assert evno[(tags & 1) != 0].tolist() == [1000, 1003, 1006]  # tag bit 0 set

    # tracks the chain filter, and honors entry_start/entry_stop.
    assert chain.filtered(event_tag=[4]).event_tags().tolist() == [4, 4]
    assert chain.event_tags(entry_start=2, entry_stop=5).tolist() == [4, 1, 2]


def test_tag_names(chain):
    # The persisted name↔bit registry rides in the file (gen_sample records it).
    assert chain.tag_names == {"dvcs": 0, "sidis": 1, "elastic": 2}


def test_event_tag_filter_by_name(chain):
    ak = pytest.importorskip("awkward")
    # A name resolves to its bit (any-mask): "dvcs" = bit 0 → tag 1 → i in {0,3,6}.
    got = ak.to_list(chain.filtered(event_tag="dvcs").array("REC::Event", "evno"))
    assert got == [[1000], [1003], [1006]]

    # Multiple names OR their bits: dvcs|elastic = bits {0,2} → tags {1,4} → 5 events,
    # exactly the raw-bitmask spelling.
    by_name = chain.filtered(event_tag=["dvcs", "elastic"]).array("REC::Event", "evno")
    by_mask = chain.filtered(event_tag_any=0b101).array("REC::Event", "evno")
    assert ak.to_list(by_name) == ak.to_list(by_mask)
    assert len(ak.to_list(by_name)) == 5

    # event_tag_any accepts names too: "sidis" = bit 1 → tag 2 → i in {1,4,7}.
    sidis = ak.to_list(chain.filtered(event_tag_any="sidis").array("REC::Event", "evno"))
    assert sidis == [[1001], [1004], [1007]]

    # An unknown name fails loudly rather than silently matching nothing.
    with pytest.raises(KeyError):
        chain.filtered(event_tag="nope")


def test_event_tag_filter_by_name_survives_workers(chain):
    # Names resolve in the parent to numeric masks, so worker processes (which
    # re-open the chain) need no registry of their own.
    ak = pytest.importorskip("awkward")
    g = chain.filtered(event_tag="dvcs")
    single = ak.to_list(g.arrays("REC::Event", ["evno"]).evno)
    par = ak.to_list(g.arrays("REC::Event", ["evno"], workers=2).evno)
    assert par == single == [[1000], [1003], [1006]]


def test_skim_tagged(chain, tmp_path):
    # Retag each event with a fresh scheme (even → dvcs, odd → sidis), record a
    # registry, and reread the tagged DST by name — the whole loop.
    ak = pytest.importorskip("awkward")
    n = chain.num_entries
    tags = np.array([1 if i % 2 == 0 else 2 for i in range(n)], dtype=np.uint32)
    dst = str(tmp_path / "tagged.hipo")
    summary = chain.skim(dst, tags=tags, tag_names={"dvcs": 0, "sidis": 1})
    assert summary.events == n

    out = oxihipo.open(dst)
    assert out.tag_names == {"dvcs": 0, "sidis": 1}          # self-describing
    assert out.event_tags().tolist() == tags.tolist()         # retagged
    # banks copied through intact
    assert ak.to_list(out.arrays("REC::Event", ["evno"]).evno) == [[1000 + i] for i in range(n)]
    # rereads by name: dvcs bit → even events
    dvcs = ak.to_list(out.filtered(event_tag="dvcs").array("REC::Event", "evno"))
    assert dvcs == [[1000 + i] for i in range(n) if i % 2 == 0]

    # a tags length that doesn't match the events written is a ValueError
    with pytest.raises(ValueError):
        chain.skim(str(tmp_path / "bad.hipo"), tags=np.array([1, 2], dtype=np.uint32))


def test_set_event_tag_in_place(tmp_path):
    # Patch tags on disk without a rewrite. Copy the *uncompressed* fixture so
    # we mutate a throwaway file, never the committed one.
    dst = str(tmp_path / "patch.hipo")
    shutil.copy(os.path.join(DATA, "sample_none.hipo"), dst)
    size_before = os.path.getsize(dst)

    f = oxihipo.open(dst)
    f.set_event_tag(2, 0xBEEF)
    assert f.set_event_tags({4: 7, 6: 8}) == 2

    # No rewrite: the file is exactly the same size.
    assert os.path.getsize(dst) == size_before
    # Visible through a fresh open (tags cycle 1, 2, 4 before patching).
    tags = oxihipo.open(dst).event_tags().tolist()
    assert tags[2] == 0xBEEF
    assert tags[4] == 7
    assert tags[6] == 8
    assert tags[0] == 1 and tags[1] == 2 and tags[3] == 1  # untouched

    # Out-of-range entry → IndexError.
    with pytest.raises(IndexError):
        oxihipo.open(dst).set_event_tag(999, 1)


def test_set_event_tag_rejects_compressed(tmp_path):
    # A compressed file can't be patched in place; the call raises ValueError
    # and leaves the file byte-for-byte unchanged.
    dst = str(tmp_path / "compressed.hipo")
    shutil.copy(os.path.join(DATA, "sample.hipo"), dst)  # Lz4PerColumn
    with open(dst, "rb") as fh:
        before = fh.read()
    with pytest.raises(ValueError):
        oxihipo.open(dst).set_event_tag(1, 5)
    with open(dst, "rb") as fh:
        assert fh.read() == before


# -------------------------------------------------------------------------
# ROOT RDataFrame bridge
# -------------------------------------------------------------------------
def _skip_without_root():
    """Skip unless both awkward and ROOT import; returns the awkward module
    (what the value-checking tests need)."""
    ak = pytest.importorskip("awkward")
    pytest.importorskip("ROOT")
    return ak


def _colnames(df) -> set[str]:
    """RDataFrame column names as plain ``str`` (they come back as cppyy
    ``std::string``, whose hashing/``bytes`` comparison is fragile in sets)."""
    return {str(n) for n in df.GetColumnNames()}


def test_rdataframe_single_bank(chain):
    ak = _skip_without_root()
    df = chain.rdataframe("REC::Particle", ["pid", "px"])
    # Names are sanitized to C++ identifiers (`::` and `/` → `_`).
    assert _colnames(df) == {"REC_Particle_pid", "REC_Particle_px"}
    assert df.Count().GetValue() == 8  # one RDF entry per event

    # The jagged column is an RVec: per-event .size() reproduces the counts.
    h = df.Define("n", "(int) REC_Particle_pid.size()").Histo1D(
        ("n", "", 4, 0, 4), "n"
    ).GetValue()
    assert h.GetEntries() == 8
    assert h.GetMean() == pytest.approx(1.5)  # mean of 0,1,2,3,0,1,2,3

    # Values flow through unchanged: Σ px over all particles matches Awkward.
    expected = float(ak.sum(chain.array("REC::Particle", "px")))
    got = df.Define("s", "Sum(REC_Particle_px)").Sum("s").GetValue()
    assert got == pytest.approx(expected)


def test_rdataframe_array_column_survives(chain):
    # A T#N array column (cov/F#3) crosses the bridge as a nested RVec.
    _skip_without_root()
    df = chain.rdataframe("REC::Particle", ["cov"])
    assert "REC_Particle_cov" in _colnames(df)
    assert df.Count().GetValue() == 8
    assert "RegularArray" in df.GetColumnType("REC_Particle_cov")


def test_rdataframe_multi_bank(chain):
    _skip_without_root()
    df = chain.rdataframe(["REC::Particle", "REC::Event"])
    names = _colnames(df)
    assert "REC_Event_evno" in names
    assert any(n.startswith("REC_Particle_") for n in names)
    assert df.Count().GetValue() == 8


def test_rdataframe_filtered_and_filter_name(chain):
    _skip_without_root()
    # A chain filter carries through: only events with REC::Particle survive.
    g = chain.filtered(require=["REC::Particle"])
    assert g.rdataframe("REC::Particle", ["px"]).Count().GetValue() == len(SURV)
    # filter_name selects columns across banks (bank/col keys, sanitized).
    df = chain.rdataframe(filter_name="REC::Particle/p*")
    assert _colnames(df) == {"REC_Particle_pid", "REC_Particle_px"}


def test_rdataframe_empty_selection_raises(chain):
    _skip_without_root()
    # Unlike arrays() (which yields an empty array), an RDataFrame with no
    # columns is useless — so a non-matching selection is a hard error.
    with pytest.raises(ValueError):
        chain.rdataframe(filter_name="NOPE::*")


def test_rdataframe_module_level():
    _skip_without_root()
    df = oxihipo.rdataframe(os.path.join(DATA, "sample.hipo"), "REC::Event", ["evno"])
    assert df.Count().GetValue() == 8
    assert "REC_Event_evno" in _colnames(df)


def test_iterate_rdataframe_streams_and_merges(chain):
    _skip_without_root()

    def size_hist(df):
        return df.Define("n", "(int) REC_Particle_px.size()").Histo1D(
            ("n", "", 4, 0, 4), "n"
        )

    ref = size_hist(chain.rdataframe("REC::Particle", ["px"])).GetValue()

    total = None
    nchunks = 0
    for df in chain.iterate_rdataframe("REC::Particle", ["px"], step_size=2):
        nchunks += 1
        h = size_hist(df).GetValue()
        if total is None:
            total = h.Clone()
            total.SetDirectory(0)  # detach so it outlives the chunk's RDF
        else:
            total.Add(h)

    assert nchunks == 3  # 3 records → 3 record-aligned chunks
    assert total.GetEntries() == ref.GetEntries() == 8
    assert total.GetMean() == pytest.approx(ref.GetMean())


def test_iterate_rdataframe_report(chain):
    _skip_without_root()
    seen = 0
    for df, rep in chain.iterate_rdataframe(
        "REC::Event", ["evno"], step_size=3, report=True
    ):
        assert isinstance(rep, oxihipo.Report)
        assert rep.file_path.endswith(".hipo")
        seen += df.Count().GetValue()
    assert seen == 8


def test_rdf_name_sanitization():
    # The naming rule is a pure function — check it directly (no ROOT needed).
    from oxihipo import _rdf_name

    assert _rdf_name("REC::Particle/px") == "REC_Particle_px"
    assert _rdf_name("RUN::config/torus") == "RUN_config_torus"
    assert _rdf_name("9bank/x").startswith("_")  # leading digit gets a prefix


# -------------------------------------------------------------------------
# Writing
# -------------------------------------------------------------------------
def test_writer_roundtrip_jagged(tmp_path):
    # Write a fresh file with a jagged bank, read it back.
    ak = pytest.importorskip("awkward")
    dst = str(tmp_path / "w.hipo")
    w = oxihipo.create(dst, compression="lz4percolumn")
    w.new_bank("NEW::bank", {"px": "F", "pid": "I"})
    px = ak.Array([[1.0, 2.0], [], [3.0, 4.0, 5.0]])
    pid = ak.Array([[11, 22], [], [11, -11, 211]])
    w.extend({"NEW::bank": {"px": px, "pid": pid}})
    summary = w.close()
    assert summary.events == 3

    f = oxihipo.open(dst)
    assert f.num_entries == 3
    assert ak.to_list(f.array("NEW::bank", "px")) == [[1.0, 2.0], [], [3.0, 4.0, 5.0]]
    assert ak.to_list(f.array("NEW::bank", "pid")) == [[11, 22], [], [11, -11, 211]]


def test_writer_scalar_bank_and_context(tmp_path):
    # A scalar-per-event bank (1-D NumPy columns), via the context manager.
    ak = pytest.importorskip("awkward")
    dst = str(tmp_path / "w2.hipo")
    with oxihipo.create(dst) as w:
        w.new_bank("RUN::config", {"run": "I", "energy": "F"})
        w.extend({
            "RUN::config": {
                "run": np.array([5, 5, 6], dtype=np.int32),
                "energy": np.array([10.6, 10.6, 10.2], dtype=np.float32),
            }
        })
    f = oxihipo.open(dst)
    assert f.num_entries == 3
    assert ak.to_list(f.array("RUN::config", "run")) == [[5], [5], [6]]  # one row/event


def test_writer_array_columns(tmp_path):
    # Fixed-length array columns (T#N) round-trip. sample.hipo's REC::Particle
    # has cov/F#3 (a jagged array column) — read it, write it, read it back.
    ak = pytest.importorskip("awkward")
    p = oxihipo.open(os.path.join(DATA, "sample.hipo")).arrays("REC::Particle")
    dst = str(tmp_path / "arr.hipo")
    with oxihipo.create(dst) as w:
        w.new_bank("REC::Particle", {"pid": "I", "px": "F", "cov": "F#3"})
        w.extend({"REC::Particle": p})
    q = oxihipo.open(dst).arrays("REC::Particle")
    assert ak.to_list(q.pid) == ak.to_list(p.pid)
    assert ak.to_list(q.px) == ak.to_list(p.px)
    assert ak.to_list(q.cov) == ak.to_list(p.cov)  # the array column survives
    assert "3 * float32" in str(q.type)

    # A scalar-per-event array bank from a plain (n_events, N) NumPy array.
    dst2 = str(tmp_path / "arr2.hipo")
    with oxihipo.create(dst2) as w:
        w.new_bank("VEC::mom", {"p": "F#3"})
        w.extend({"VEC::mom": {"p": np.array([[1, 2, 3], [4, 5, 6]], dtype=np.float32)}})
    g = oxihipo.open(dst2).array("VEC::mom", "p")
    assert ak.to_list(g) == [[[1, 2, 3]], [[4, 5, 6]]]  # one 3-vector row per event

    # A bad #N length is still rejected.
    with pytest.raises(ValueError):
        oxihipo.create(str(tmp_path / "bad.hipo")).new_bank("B::b", {"x": "F#0"})


def test_decorate_adds_bank(tmp_path):
    # Copy a fixture, decorate it with a per-event ML-score bank, read back.
    ak = pytest.importorskip("awkward")
    src = str(tmp_path / "src.hipo")
    shutil.copy(os.path.join(DATA, "sample.hipo"), src)  # Lz4PerColumn, 8 events
    dst = str(tmp_path / "dec.hipo")
    n = oxihipo.open(src).num_entries
    scores = (np.arange(n, dtype=np.float32) * 0.5)

    w = oxihipo.recreate(src, dst)
    w.new_bank("ML::pred", {"score": "F"})
    w.extend({"ML::pred": {"score": scores}})
    assert w.close().events == n

    f = oxihipo.open(dst)
    assert f.num_entries == n
    assert "REC::Particle" in f and "ML::pred" in f
    # existing banks copied verbatim…
    assert ak.to_list(f.arrays("REC::Event", ["evno"]).evno) == [[1000 + i] for i in range(n)]
    # …and the new bank is there, one score per event.
    assert np.allclose(ak.to_numpy(ak.flatten(f.array("ML::pred", "score"))), scores)


def test_decorate_requires_all_events(tmp_path):
    # Providing fewer new-bank events than the source has is an error on close.
    src = str(tmp_path / "s.hipo")
    shutil.copy(os.path.join(DATA, "sample.hipo"), src)
    w = oxihipo.recreate(src, str(tmp_path / "d.hipo"))
    w.new_bank("ML::pred", {"score": "F"})
    w.extend({"ML::pred": {"score": np.array([1.0, 2.0], dtype=np.float32)}})  # 2 of 8
    with pytest.raises(ValueError):
        w.close()


def test_show(chain, capsys):
    chain.show()
    out = capsys.readouterr().out
    assert "REC::Particle" in out and "pid" in out
    chain.show("REC::Event")
    out2 = capsys.readouterr().out
    assert "REC::Event" in out2 and "evno" in out2


def test_user_config_round_trip(tmp_path):
    import awkward as ak

    p = str(tmp_path / "cfg.hipo")
    w = oxihipo.create(p, compression="lz4", config={"run": "5042", "torus": "-1.0"})
    w.new_bank("T::a", {"x": "I"})
    w.extend({"T::a": {"x": ak.Array([[1], [2]])}})
    w.close()
    f = oxihipo.open(p)
    assert f.config == {"run": "5042", "torus": "-1.0"}


def test_no_config_is_empty(tmp_path):
    import awkward as ak

    p = str(tmp_path / "nocfg.hipo")
    w = oxihipo.create(p, compression="lz4")
    w.new_bank("T::a", {"x": "I"})
    w.extend({"T::a": {"x": ak.Array([[1]])}})
    w.close()
    assert oxihipo.open(p).config == {}


def test_writer_event_tags_round_trip(tmp_path):
    import awkward as ak

    p = str(tmp_path / "tags.hipo")
    tags = np.array([1, 2, 4, 1, 2, 4], dtype=np.uint32)
    w = oxihipo.create(p, compression="lz4")
    w.new_bank("REC::Particle", {"pid": "I"})
    w.extend(
        {"REC::Particle": {"pid": ak.Array([[11], [22], [33], [44], [55], [66]])}},
        tags=tags,
    )
    w.close()
    f = oxihipo.open(p)
    assert f.event_tags().tolist() == tags.tolist()
    # tag-based filter selects the tag==1 events
    sub = f.filtered(event_tag=[1]).arrays("REC::Particle", ["pid"])
    assert ak.to_list(sub.pid) == [[11], [44]]


def test_writer_tags_length_mismatch_raises(tmp_path):
    import awkward as ak

    p = str(tmp_path / "bad.hipo")
    w = oxihipo.create(p, compression="lz4")
    w.new_bank("REC::Particle", {"pid": "I"})
    with pytest.raises(ValueError):
        w.extend(
            {"REC::Particle": {"pid": ak.Array([[11], [22]])}},
            tags=[1, 2, 3],  # 3 tags for 2 events
        )


# --- composite banks, bank ids, and the error mapping ----------------------
def test_bank_ids(chain):
    ids = chain.bank_ids
    assert ids["REC::Particle"] == (300, 1)
    assert ids["REC::Event"] == (300, 30)


def test_module_level_helpers_exist():
    # `numpy` / `event_tags` / `composite` / `skim` are methods *and* free
    # functions, matching `arrays` / `iterate` (they were methods only).
    for name in ("numpy", "event_tags", "composite", "skim", "arrays", "iterate"):
        assert hasattr(oxihipo, name), name
        assert name in oxihipo.__all__, name


def test_corrupt_file_raises_mapped_exception(tmp_path):
    # The exception tree was implemented but never exercised: a regression in
    # the mapping would have shipped silently.
    p = tmp_path / "garbage.hipo"
    p.write_bytes(b"this is not a hipo file, not even close")
    with pytest.raises(oxihipo.OxihipoError):
        oxihipo.open(str(p))
    assert issubclass(oxihipo.CorruptFileError, oxihipo.OxihipoError)


def test_truncated_file_raises(tmp_path):
    # A truncated record is genuine file corruption, so it must surface as
    # CorruptFileError (a subclass of OxihipoError) rather than a bare panic.
    import builtins

    src = os.path.join(DATA, "sample.hipo")
    data = builtins.open(src, "rb").read()
    p = tmp_path / "trunc.hipo"
    p.write_bytes(data[: len(data) // 3])
    with pytest.raises(oxihipo.OxihipoError):
        c = oxihipo.open(str(p))
        c.arrays("REC::Particle", ["pid"])


def test_unknown_bank_and_column_raise_keyerror(chain):
    with pytest.raises(KeyError):
        chain.arrays("NOT::abank", ["pid"])
    with pytest.raises(KeyError):
        chain.arrays("REC::Particle", ["not_a_column"])


def test_unknown_compression_is_value_error(tmp_path):
    with pytest.raises(ValueError):
        oxihipo.create(str(tmp_path / "x.hipo"), compression="not-a-codec")


# --- entries=, cut=, and max_record_bytes ---------------------------------


def test_entries_matches_the_equivalent_range(chain):
    ak = pytest.importorskip("awkward")
    n = min(20, chain.num_entries)
    rng = chain.arrays("REC::Particle", ["pid", "px"], entry_start=5, entry_stop=n)
    ent = chain.arrays("REC::Particle", ["pid", "px"], entries=list(range(5, n)))
    assert ak.almost_equal(rng, ent)


def test_entries_preserves_order_and_duplicates(chain):
    ak = pytest.importorskip("awkward")
    want = [3, 3, 1, 0]
    got = chain.arrays("REC::Particle", ["pid"], entries=want)
    assert len(got) == len(want)
    # Slot k must equal a standalone read of entries[k] — the alignment
    # guarantee the keyword rests on.
    for k, e in enumerate(want):
        one = chain.arrays("REC::Particle", ["pid"], entries=[e])
        assert ak.almost_equal(got[k : k + 1], one)


def test_entries_out_of_range_is_empty_not_an_error(chain):
    got = chain.arrays("REC::Particle", ["pid"], entries=[0, 10**9])
    # Still one entry per requested index, the bogus one simply empty.
    assert len(got) == 2
    assert len(got[1].pid) == 0


def test_entries_rejects_conflicting_and_negative(chain):
    with pytest.raises(ValueError):
        chain.arrays("REC::Particle", ["pid"], entries=[0], entry_start=0)
    with pytest.raises(ValueError):
        chain.arrays("REC::Particle", ["pid"], entries=[-1])


def test_cut_per_row_keeps_every_event(chain):
    ak = pytest.importorskip("awkward")
    base = chain.arrays("REC::Particle", ["pid", "px"])
    cut = chain.arrays("REC::Particle", ["pid", "px"], cut="pid == 11")
    # A row-level cut filters rows, never events.
    assert len(cut) == len(base)
    assert ak.almost_equal(cut, base[base.pid == 11])


def test_cut_per_event_drops_events(chain):
    ak = pytest.importorskip("awkward")
    base = chain.arrays("REC::Particle", ["pid"])
    cut = chain.arrays("REC::Particle", ["pid"], cut="ak.any(pid == 11, axis=1)")
    assert ak.almost_equal(cut, base[ak.any(base.pid == 11, axis=1)])


def test_cut_column_is_read_then_dropped(chain):
    ak = pytest.importorskip("awkward")
    # `pid` is only named by the cut, so it must not come back in the result.
    proj = chain.arrays("REC::Particle", ["px"], cut="pid == 11")
    assert proj.fields == ["px"]
    full = chain.arrays("REC::Particle", ["pid", "px"], cut="pid == 11")
    assert ak.almost_equal(proj.px, full.px)


def test_cut_preserves_array_column_width(tmp_path):
    ak = pytest.importorskip("awkward")
    p = tmp_path / "cutarr.hipo"
    w = oxihipo.create(str(p), compression="lz4")
    w.new_bank("T::B", {"pid": "I", "cov": "F#3"})
    w.extend({"T::B": {
        "pid": ak.Array([[11, 22, 11]]),
        "cov": ak.Array([[[1.0, 2.0, 3.0], [4.0, 5.0, 6.0], [7.0, 8.0, 9.0]]]),
    }})
    w.close()
    got = oxihipo.open(str(p)).arrays("T::B", cut="pid == 11")
    # Rows 0 and 2 survive and each stays 3 wide — the inner_len must not be
    # disturbed by filtering rows out from under it.
    assert got.cov.tolist() == [[[1.0, 2.0, 3.0], [7.0, 8.0, 9.0]]]


def test_cut_error_paths(chain):
    for bad in ("pid ==", "nosuchcolumn == 1", "px * 2", "__import__('os')"):
        with pytest.raises(ValueError):
            chain.arrays("REC::Particle", ["px"], cut=bad)
    # A cut spans one bank only; a multi-bank selection must say so.
    with pytest.raises(ValueError):
        chain.arrays(["REC::Particle", "REC::Event"], cut="pid == 11")


def test_max_record_bytes_changes_record_count(tmp_path):
    ak = pytest.importorskip("awkward")

    def write(name, mrb):
        p = tmp_path / name
        kw = {} if mrb is None else {"max_record_bytes": mrb}
        w = oxihipo.create(str(p), compression="lz4perbank", **kw)
        w.new_bank("T::B", {"x": "F"})
        for _ in range(20):
            w.extend({"T::B": {"x": ak.Array([[float(i)] * 40 for i in range(400)])}})
        w.close()
        return oxihipo.open(str(p))

    big = write("big.hipo", None)
    small = write("small.hipo", 256 * 1024)
    # Same events either way; more records is the point — a reader parallelises
    # over records, so this is the knob that decides how many cores it can use.
    assert small.num_entries == big.num_entries
    n_big = len(big._reader().record_spans())
    n_small = len(small._reader().record_spans())
    assert n_small > n_big, f"{n_small} !> {n_big}"


def test_max_record_bytes_rejects_zero(tmp_path):
    with pytest.raises(ValueError):
        oxihipo.create(str(tmp_path / "z.hipo"), max_record_bytes=0)


def test_composite_error_distinguishes_absent_from_not_composite(chain):
    # A schema'd bank is present but not composite. Saying "no composite bank X"
    # for it is false — X is in the file — and on real data every bank lands
    # here, so the two causes must read differently.
    with pytest.raises(KeyError, match="not a composite bank"):
        chain.composite("REC::Particle")
    with pytest.raises(KeyError, match="no bank"):
        chain.composite("NOT::areal::bank")


# --- file_header, record_count, to_parquet, to_dask -----------------------


def test_file_header_and_record_count(chain):
    h = chain.file_header
    assert h is not None
    # These are the dependable fields.
    assert h.version == 6
    assert h.endianness in ("little", "big")
    # record_count in the *header* is writer-dependent and in practice 0; the
    # index-derived count is the one to trust, so they must not be conflated.
    assert chain.record_count >= 1
    assert isinstance(h.record_count, int)
    # NamedTuple: positional unpacking still works.
    assert h[0] == h.version


def test_to_parquet_round_trips(chain, tmp_path):
    pq = pytest.importorskip("pyarrow.parquet")
    out = tmp_path / "p.parquet"
    groups = chain.to_parquet(str(out), "REC::Particle", ["pid", "px"])
    assert groups == 1
    got = pq.read_table(str(out))
    # Must equal exactly what the arrow backend produces for the same read.
    assert got.equals(chain.arrays("REC::Particle", ["pid", "px"], library="arrow"))


def test_to_parquet_streaming_matches_single_shot(chain, tmp_path):
    pq = pytest.importorskip("pyarrow.parquet")
    one, many = tmp_path / "one.parquet", tmp_path / "many.parquet"
    chain.to_parquet(str(one), "REC::Particle", ["pid"])
    groups = chain.to_parquet(str(many), "REC::Particle", ["pid"], step_size=2)
    assert groups > 1, "step_size must produce several row groups"
    assert pq.read_table(str(many)).equals(pq.read_table(str(one)))


def test_to_parquet_applies_cut_when_streaming(chain, tmp_path):
    # The cut must survive the streaming path, not just the single-shot one —
    # silently dropping it would write the wrong file.
    pq = pytest.importorskip("pyarrow.parquet")
    out = tmp_path / "cut.parquet"
    chain.to_parquet(str(out), "REC::Particle", ["px"], cut="pid == 11", step_size=2)
    want = chain.arrays("REC::Particle", ["px"], cut="pid == 11", library="arrow")
    assert pq.read_table(str(out)).equals(want)


def test_to_dask_matches_eager(chain):
    dak = pytest.importorskip("dask_awkward")
    ak = pytest.importorskip("awkward")
    lazy = chain.to_dask("REC::Particle", ["pid", "px"], step_size=2)
    assert lazy.npartitions >= 1
    assert ak.almost_equal(lazy.compute(), chain.arrays("REC::Particle", ["pid", "px"]))


def test_to_dask_preserves_a_chain_filter(chain):
    # Partitions are read in a reconstructed chain; forgetting the filter there
    # would silently widen every partition.
    dak = pytest.importorskip("dask_awkward")
    ak = pytest.importorskip("awkward")
    g = chain.filtered(require=["REC::Particle"])
    assert ak.almost_equal(
        g.to_dask("REC::Particle", ["pid"], step_size=2).compute(),
        g.arrays("REC::Particle", ["pid"]),
    )


# --- wave 0: argument handling that disagreed with itself -----------------


def test_key_namespace_follows_the_request_not_the_data(chain):
    """A list-of-one must namespace by bank on every backend.

    The namespace used to be decided by `len(res) > 1` on np/pd/arrow and by the
    caller's intent on ak, so a glob that happened to match one bank returned
    bare column keys under np and bank-qualified ones under ak — a loop keyed on
    "BANK/col" worked until it met such a file.
    """
    pytest.importorskip("awkward")

    def keys(obj, lib):
        if lib == "ak":
            return list(obj.fields)
        if lib == "np":
            return list(obj)
        if lib == "pd":
            return list(obj) if isinstance(obj, dict) else list(obj.columns)
        return list(obj.column_names)

    for lib in ("ak", "np", "pd", "arrow"):
        listed = keys(chain.arrays(["REC::Particle"], library=lib), lib)
        assert all(k.startswith("REC::Particle") for k in listed), (lib, listed)
        bare = keys(chain.arrays("REC::Particle", library=lib), lib)
        assert not any("/" in k for k in bare), (lib, bare)


def test_banks_and_filter_name_together_is_refused(chain):
    # `filter_name` selects across every bank, so it silently discarded `banks`:
    # arrays("REC::Particle", filter_name="REC::Event*") returned REC::Event.
    with pytest.raises(TypeError, match="not both"):
        chain.arrays("REC::Particle", filter_name="REC::Event*")


def test_arrow_schema_is_not_nullable(chain):
    pa = pytest.importorskip("pyarrow")
    t = chain.arrays("REC::Particle", ["px"], library="arrow")
    field = t.schema.field(0)
    assert not field.nullable
    assert not field.type.value_field.nullable


def test_module_iterate_forwards_cut():
    # oxihipo.iterate lacked `cut=` entirely while Chain.iterate had it, so a
    # typed caller failed mypy and an untyped one got TypeError.
    ak = pytest.importorskip("awkward")
    path = os.path.join(DATA, "sample.hipo")
    chunks = list(oxihipo.iterate(path, "REC::Particle", ["pid"], cut="pid == 100"))
    assert chunks
    for c in chunks:
        assert ak.all(c.pid == 100)


# NOTE: the `composite(library="np")` rank-1 fix has no test. Exercising it
# needs a file containing a composite bank, and none of the fixtures has one —
# nor does any real CLAS12 file checked so far. Writing one requires composite
# support in the writer, which does not exist. Flagged rather than faked.
