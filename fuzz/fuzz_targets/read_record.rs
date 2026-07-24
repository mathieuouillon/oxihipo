//! Fuzz the record decoder directly, skipping the file-header layer.
//!
//! `open_file` has to get past the file header before it reaches record
//! decoding, so most random inputs die early. This target feeds bytes straight
//! to the record decoder — the code that decompresses a payload, builds the
//! event-offset table, and slices per-event byte ranges — so the fuzzer spends
//! its budget on the parts that handle attacker-controlled lengths.
//!
//! Run with:  cargo +nightly fuzz run read_record
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxihipo::fuzz_api;

fuzz_target!(|data: &[u8]| {
    // Decode as a standalone record (header at offset 0). Any malformed input
    // must return `Err`, never panic/abort/UB.
    if let Ok(rec) = fuzz_api::decode_record(data) {
        // Touch every event slice the decoder reported, bounded so a huge
        // event_count can't turn a crash-hunt into a timeout report.
        let n = rec.event_count().min(256);
        for i in 0..n {
            if let Some(ev) = rec.event(i) {
                // Walk the event's structures — header parse + bounds checks.
                fuzz_api::walk_structures(ev);
            }
        }
    }
});
