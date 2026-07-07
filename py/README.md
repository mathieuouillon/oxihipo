# oxihipo (Python)

Fast, **columnar** reading of HIPO (CLAS12) files, powered by the Rust
`oxihipo` core. A HIPO bank reads like a [uproot](https://uproot.readthedocs.io)
jagged branch:

```python
import oxihipo as ox

f = ox.open("run5042.hipo")          # file | dir | glob | list of paths
f.num_entries                        # event count
f.keys()                             # ['REC::Particle', 'REC::Event', ...]

p = f.arrays("REC::Particle", ["pid", "px", "py", "pz"])
p.px                                 # jagged: p[event].px indexes particles
px = f.array("REC::Particle", "px")  # one column, type: N * var * float32

# NumPy-only (no Awkward import needed):
values, offsets, inner_len = f.numpy("REC::Particle", "px")
```

## How it works

The whole per-event loop runs in **Rust with the GIL released**. One pass over
the file materializes each requested column into a flat NumPy buffer plus one
shared `int64` offsets buffer per bank — exactly an Awkward `ListOffsetArray` /
`Index64` layout — moved into NumPy zero-copy. `array` / `arrays` wrap those
buffers into an `ak.Array`; nothing is copied past decompression, and Python
never iterates events.

## Build

Requires the Rust toolchain and [maturin](https://www.maturin.rs).

```sh
cd py
maturin develop --release        # build + install into the active venv
# or: maturin build --release     # produce an abi3 wheel under target/wheels
```

The extension is `abi3` (one wheel per OS/arch works across CPython ≥ 3.9).

## Dependencies

- `numpy >= 1.24` (required)
- `awkward >= 2.6` (optional — only for `array` / `arrays`; install with
  `pip install oxihipo[awkward]`)
