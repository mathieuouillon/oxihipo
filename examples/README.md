# Examples

Run any of these with `cargo run --release --example <name> -- [args]`.
Most take a `.hipo` path; `write` and `tag_and_skim` create their own input.

## Reading

| example | what it shows |
| --- | --- |
| `read` | the canonical `for ev in chain.events()` loop, a typed `bank_row!` struct, and one-call `ev.get` / `ev.col` |
| `columns` | the columnar reader (`read_columns`) — one flat buffer per column plus shared offsets |
| `clas12` | a ready-made `REC::*` row catalog for CLAS12 analysis |
| `parallel` | `for_each` fanning out across records with rayon |
| `list_populated_banks` | which banks actually carry rows in a file |
| `chain` | reading many files as one event stream |

## Writing / transforming

| example | what it shows |
| --- | --- |
| `write` | building a dictionary and writing events |
| `write_array` | fixed-length array columns (`name/T#N`) |
| `skim` | copying a filtered subset to a new file |
| `tag_and_skim` | per-event tags, then skimming by tag |
| `recook_by_bank` | re-encoding a file into the by-bank format |

## Benchmarks

These print best-of-N timings for quick manual checks. **For regression
tracking use `cargo bench`** (criterion, in `benches/`) — it compares against a
stored baseline; these do not.

| example | measures |
| --- | --- |
| `bench_scan` | steady-state sequential scan (good profiler target) |
| `bench_columns` | columnar read, pairs with `py/examples/bench_columns.py` |
| `bench_decoders` | raw record decode, no bank materialization |
| `bench_read_compression` | one dataset re-encoded into every format, then read |
| `bench_event_tags` | the per-event tag read paths |
| `bench_par` | single-threaded vs parallel scan |

## Layout diagnostics

Tools for deciding how a file should be written, rather than measuring the
reader. Both take a real file and answer a tuning question.

| example | answers |
| --- | --- |
| `profile_streams` | how concentrated is a record's payload? Largest bank / column stream as a share of its record, and the top streams overall |
| `record_size_scaling` | what does `max_record_bytes` buy? Record count, file size, and parallel speed-up across a sweep of flush targets |
