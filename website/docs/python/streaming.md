---
id: streaming
title: Streaming
sidebar_position: 2
---

# Streaming (bigger than RAM)

`iterate` yields the chain in fully-materialized chunks. Each is dropped before
the next is read, so resident memory stays at about one chunk — which is what
lets a 10–100 GB input read in roughly constant RAM.

```python
for chunk in f.iterate("REC::Particle", ["px"], step_size="200 MB"):
    hist.fill(ak.flatten(chunk.px))
```

Each chunk has the same shape `arrays()` would give you for that slice of
events, so anything that works on the whole array works on a chunk.

## `step_size`

Either an event count or a byte budget:

```python
f.iterate("REC::Particle", step_size=1_000_000)     # events
f.iterate("REC::Particle", step_size="200 MB")      # decompressed bytes
f.iterate("REC::Particle", step_size="1 GB")
```

Chunks are aligned to **record and file boundaries**, so a single oversized
record is never split across chunks. Byte budgets are measured against
decompressed record payloads, which is why `"200 MB"` is a memory statement
rather than a statement about bytes read off disk.

Units: `B`, `KB`/`MB`/`GB`/`TB` (powers of 10) and `KiB`/`MiB`/`GiB`/`TiB`
(powers of 2). A non-positive budget raises.

## Progress reporting

`report=True` pairs each chunk with a `Report`:

```python
for chunk, report in f.iterate("REC::Particle", step_size=1_000_000, report=True):
    print(report.entry_start, report.entry_stop, report.file_path)
```

`entry_start` / `entry_stop` are global event indices; `file_path` is the file
the chunk's records came from (chunks are file-aligned, so it's unambiguous).

## Multi-file, without opening everything

The module-level `iterate` takes the same sources as `open`:

```python
for chunk in ox.iterate("/data/run5042/*.hipo", "REC::Particle", step_size="1 GB"):
    ...
```

It's a generator, so a multi-file chain never opens more than it needs at once.

## Combining with the other knobs

`filter_name`, `entry_start` / `entry_stop`, `library=`, `threads=`, and
`workers=` all apply to `iterate` exactly as they do to `arrays`:

```python
for chunk in f.iterate(filter_name="REC::*", library="np", step_size="500 MB"):
    ...
```

For the I/O-bound farm case, add `workers=N` — see
[Parallel reading](./parallel.md).
