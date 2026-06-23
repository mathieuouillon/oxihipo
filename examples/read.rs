//! Read a HIPO file with a plain `for` loop — showing the simplest paths:
//! a typed `bank_row!` struct, and one-call `ev.get` / `ev.col` straight
//! off the event (no `ev.bank(...)` step).
//!
//! ```sh
//! cargo run --release --example read -- /tmp/demo.hipo
//! ```

use std::env;

// A ready-made typed row for REC::Particle. Roll your own for any bank in
// three lines with `oxihipo::bank_row!`.
use oxihipo::clas12::RecParticle;
use oxihipo::{Chain, Result};

fn main() -> Result<()> {
    let path = env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/demo.hipo".to_string());

    // `Chain::open` opens the file, parses the header, and reads the
    // dictionary; record payloads stream in on demand. A single file is
    // just a chain of length 1 — directories and globs work too.
    let chain = Chain::open(&path)?;

    println!("file: {path}");
    println!("  events:  {}", chain.event_count());
    println!("  records: {}", chain.record_count());
    println!("  schemas: {}", chain.schemas().len());

    let mut events_seen = 0u64;
    let mut total_particles = 0u64;
    for ev in chain.events() {
        let ev = ev?;
        events_seen += 1;

        // Typed rows: named fields, no per-column name juggling.
        let particles: Vec<RecParticle> = ev.rows::<RecParticle>().collect();
        total_particles += particles.len() as u64;

        if events_seen <= 3 {
            // One-call scalar access straight off the event — no `ev.bank`.
            let event_id: i32 = ev.get("REC::Event", "event", 0);
            for p in &particles {
                println!(
                    "  ev {events_seen} #{event_id} pid={:>4} p=({:+.3}, {:+.3}, {:+.3})",
                    p.pid, p.px, p.py, p.pz,
                );
            }
        }
    }
    println!("scanned {events_seen} events, {total_particles} particles");

    // Bulk column straight off the event, via random access by index.
    if let Some(ev) = chain.event(0) {
        let px = ev.col::<f32>("REC::Particle", "px")?;
        println!("event 0: {} REC::Particle px value(s)", px.len());
    }

    Ok(())
}
