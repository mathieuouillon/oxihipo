//! Fuzz the whole open + read path with arbitrary bytes as a "HIPO file".
//!
//! The reader parses untrusted binary, so no input may panic, abort, hang, or
//! trigger UB — every malformed file must surface as an `Err` (or an empty
//! chain). This target exercises: file-header parse, dictionary parse, trailer
//! index decode (and the scan fallback), record decompression, per-event slice
//! bounds, and bank/column access on whatever schemas the dictionary yields.
//!
//! Run with:  cargo +nightly fuzz run open_file
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // A temp file per input: `Chain::open` takes a path. Keep it in the OS temp
    // dir and let the guard remove it.
    let Ok(mut f) = tempfile::Builder::new().suffix(".hipo").tempfile() else {
        return;
    };
    use std::io::Write;
    if f.write_all(data).is_err() {
        return;
    }
    if f.flush().is_err() {
        return;
    }

    let Ok(chain) = oxihipo::Chain::open(f.path()) else {
        return; // rejected at open — the expected outcome for most inputs
    };

    // Walk every event, touching each bank and column the dictionary declares.
    // Bounded so a header claiming a huge event count can't make the fuzzer
    // spin forever (a hang is a finding, but a slow-unit report is noise).
    for (n, ev) in chain.events().enumerate() {
        if n >= 64 {
            break;
        }
        let Ok(ev) = ev else { break };
        let _ = ev.tag();
        for schema in chain.schemas().iter() {
            let Some(bank) = ev.bank(schema.name()) else {
                continue;
            };
            let rows = bank.rows().min(64);
            for e in schema.entries() {
                for r in 0..rows {
                    // Read through the typed accessor for this column's type.
                    match e.ty {
                        oxihipo::DataType::Byte => {
                            let _ = bank.get::<i8>(&e.name, r);
                        }
                        oxihipo::DataType::Short => {
                            let _ = bank.get::<i16>(&e.name, r);
                        }
                        oxihipo::DataType::Int => {
                            let _ = bank.get::<i32>(&e.name, r);
                        }
                        oxihipo::DataType::Long => {
                            let _ = bank.get::<i64>(&e.name, r);
                        }
                        oxihipo::DataType::Float => {
                            let _ = bank.get::<f32>(&e.name, r);
                        }
                        oxihipo::DataType::Double => {
                            let _ = bank.get::<f64>(&e.name, r);
                        }
                    }
                }
            }
        }
    }

    // Random access and the columnar path take different code routes.
    let _ = chain.event(0);
    let _ = chain.event(chain.event_count().saturating_sub(1));
    let _ = chain.event_count();
});
