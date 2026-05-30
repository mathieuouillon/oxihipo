//! Read a HIPO file with a plain `for` loop.
//!
//! ```sh
//! cargo run -p hipo --release --example read -- /tmp/demo.hipo
//! ```

use std::env;

use oxihipo::{Chain, Result};

fn main() -> Result<()> {
    let path = env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/demo.hipo".to_string());

    // 1. `Chain::open` mmaps the file, parses the file header, and reads
    //    the dictionary record into a `Dict`. Single-file is just a
    //    chain of length 1; for multi-file, use `Chain::open_all`.
    let chain = Chain::open(&path)?;

    // 2. Inspect file-level metadata.
    let header = chain.file_header().expect("non-empty chain");
    println!("file: {path}");
    println!("  events:  {}", chain.event_count());
    println!("  records: {}", chain.record_count());
    println!("  schemas: {}", chain.schemas().len());
    println!("  trailer: {} bytes from start", header.trailer_position);

    // 3. Plain `for` loop. The yielded `OwnedEvent` is a slice into a
    //    shared, ref-counted record buffer; no per-event allocation.
    let mut events_seen = 0u64;
    let mut total_particles = 0u64;
    for ev in chain.events() {
        events_seen += 1;
        let p = oxihipo::or_continue!(ev.bank("REC::Particle"));
        let e = oxihipo::or_continue!(ev.bank("REC::Event"));
        total_particles += p.rows() as u64;
        if events_seen <= 3 {
            let event_id: i32 = e.get("event", 0);
            for r in 0..p.rows() {
                let pid: i32 = p.get("pid", r);
                let px: f32 = p.get("px", r);
                let py: f32 = p.get("py", r);
                let pz: f32 = p.get("pz", r);
                println!(
                    "  ev {events_seen} #{event_id} part pid={pid:>4} p=({px:+.3}, {py:+.3}, {pz:+.3})"
                );
            }
        }
    }

    println!("scanned {events_seen} events, {total_particles} particles");

    // 4. Random access still works through `chain.event(idx)`.
    if chain.event_count() >= 100
        && let Some(owned) = chain.event(99)
        && let Some(p) = owned.bank("REC::Particle")
    {
        let pid: i32 = p.get("pid", 0);
        println!("event 99 -> first pid = {pid}");
    }

    Ok(())
}
