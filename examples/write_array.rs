//! Write a small HIPO file with an array column, then read it back to
//! confirm round-trip. Demonstrates the `name/T#N` schema syntax.
//!
//! ```sh
//! cargo run -p hipo --release --example write_array -- /tmp/arr.hipo
//! ```

use std::env;

use oxhipo::{Chain, Compression, Dict, Result, Schema, Writer};

fn main() -> Result<()> {
    let path = env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/arr.hipo".to_string());

    // Build a dict containing a schema with two array columns:
    //   trk_id/I       — scalar i32
    //   cov/F#6        — 6 floats per row (Kalman covariance row, say)
    //   hits/S#3       — 3 shorts per row
    let mut dict = Dict::new();
    dict.add(Schema::parse_text("{REC::Traj/100/1}{trk_id/I,cov/F#6,hits/S#3}").unwrap());

    // ----- write -----
    {
        let mut w = Writer::create(&path)
            .schemas(&dict)
            .compression(Compression::Lz4)
            .build()?;
        for evno in 0..10_i32 {
            w.event(|ev| {
                ev.bank("REC::Traj", |b| {
                    for tr in 0..2_i32 {
                        b.row(|r| {
                            r.set("trk_id", evno * 10 + tr)?;
                            r.set(
                                "cov",
                                [
                                    evno as f32 * 0.10 + tr as f32 * 0.01,
                                    evno as f32 * 0.10 + tr as f32 * 0.01 + 0.001,
                                    evno as f32 * 0.10 + tr as f32 * 0.01 + 0.002,
                                    evno as f32 * 0.10 + tr as f32 * 0.01 + 0.003,
                                    evno as f32 * 0.10 + tr as f32 * 0.01 + 0.004,
                                    evno as f32 * 0.10 + tr as f32 * 0.01 + 0.005,
                                ],
                            )?;
                            r.set(
                                "hits",
                                [
                                    (evno + tr) as i16,
                                    (evno + tr + 1) as i16,
                                    (evno + tr + 2) as i16,
                                ],
                            )?;
                            Ok(())
                        })?;
                    }
                    Ok(())
                })?;
                Ok(())
            })?;
        }
        w.finish()?;
    }
    println!("wrote {path}");

    // ----- read back -----
    let chain = Chain::open(&path)?;
    let s = chain.schemas().get("REC::Traj").expect("schema in dict");
    println!("schema as text: {}", s.to_text());

    let mut events_seen = 0u64;
    for ev in chain.events() {
        events_seen += 1;
        let bank = ev.bank("REC::Traj").expect("REC::Traj present");

        // Typed (const-generic) array reads — zero-copy when aligned.
        let cov = bank.col::<[f32; 6]>("cov").unwrap();
        let hits = bank.col::<[i16; 3]>("hits").unwrap();
        let trk_ids = bank.col::<i32>("trk_id").unwrap();

        if events_seen <= 2 {
            for r in 0..bank.rows() as usize {
                println!(
                    "  ev {events_seen} row {r}: trk_id={} cov={:?} hits={:?}",
                    trk_ids[r], cov[r], hits[r],
                );
            }
        }
    }
    println!("scanned {events_seen} events");

    // Demonstrate the runtime escape hatch (array_at with dynamic length).
    if let Some(owned) = chain.event(0) {
        let bank = owned.bank("REC::Traj").unwrap();
        let row0_cov = bank.array_at::<f32>("cov", 0).unwrap();
        println!("event 0 row 0 cov via array_at: {:?}", &*row0_cov);
    }

    Ok(())
}
