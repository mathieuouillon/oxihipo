//! Fuzz the schema parsers with arbitrary text.
//!
//! `Schema::parse_text` / `parse_json` are hand-written parsers fed straight
//! from a file's dictionary record, so their input is untrusted: malformed
//! tokens, absurd `T#N` array lengths, and huge column counts must all return
//! `Err` rather than panic or overflow.
//!
//! Run with:  cargo +nightly fuzz run schema_text
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxihipo::Schema;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    // Both on-disk forms. A parse that succeeds must yield a self-consistent
    // schema: the row size has to equal the sum of its columns' widths, or a
    // downstream reader would mis-slice banks.
    for schema in [Schema::parse_text(text), Schema::parse_json(text)]
        .into_iter()
        .flatten()
    {
        let sum: u64 = schema
            .entries()
            .iter()
            .map(|e| e.ty.size() as u64 * e.length as u64)
            .sum();
        assert_eq!(
            sum,
            schema.row_size() as u64,
            "row_size disagrees with the sum of column widths for {text:?}"
        );
        // Every column must be resolvable by its own name.
        for e in schema.entries() {
            assert!(
                schema.column_index(&e.name).is_some(),
                "column {:?} not resolvable in {text:?}",
                e.name
            );
        }
    }
});
