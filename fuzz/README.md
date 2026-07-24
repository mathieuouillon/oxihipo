# Fuzzing

The reader parses untrusted binary, and the crate is built with
`panic = "abort"`, so any reachable panic is a process abort. These targets hunt
for inputs that panic, abort, hang, or trip UB.

Requires nightly:

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run open_file      # whole open + read path
cargo +nightly fuzz run read_record    # record decoder, below the file header
cargo +nightly fuzz run schema_text    # schema text / JSON parser
```

Targets:

| target | entry point | what it covers |
| --- | --- | --- |
| `open_file` | `Chain::open` + full read | file header, dictionary, trailer index (and scan fallback), decompression, per-event slicing, bank/column access |
| `read_record` | `fuzz_api::decode_record` | payload decompression, event-offset table, per-event byte ranges, structure walking — the code handling attacker-controlled lengths |
| `schema_text` | `Schema::parse_text` / `parse_json` | the hand-written schema parser (array lengths, malformed tokens, huge counts) |

`read_record` and `schema_text` reach internals through the crate's `fuzz-api`
feature (`oxihipo::fuzz_api`), which is not part of the public API.

A crash reproduces with:

```sh
cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<crash-file>
```

**Findings should be minimized and added to `tests/fuzz_corpus.rs`**, which
replays known-bad inputs through the same entry points as an ordinary
`cargo test` — so a fixed crash stays fixed on stable CI without nightly.
