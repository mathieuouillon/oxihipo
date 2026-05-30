//! Build a small HIPO file using the new closure-based writer API.
//!
//! ```sh
//! cargo run -p hipo --release --example write -- /tmp/demo.hipo
//! cargo run -p hipo --release --example read  -- /tmp/demo.hipo
//! ```

use std::env;

use oxihipo::{Compression, DataType, Dict, Result, Schema, Writer};

fn main() -> Result<()> {
    let path = env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/demo.hipo".to_string());

    let mut dict = Dict::new();
    dict.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [
            ("evno".into(), DataType::Long),
            ("beamE".into(), DataType::Float),
        ],
    ));
    dict.add(Schema::from_columns(
        "REC::Particle",
        300,
        1,
        [
            ("pid".into(), DataType::Int),
            ("px".into(), DataType::Float),
            ("py".into(), DataType::Float),
            ("pz".into(), DataType::Float),
            ("charge".into(), DataType::Byte),
        ],
    ));

    let mut w = Writer::create(&path)
        .schemas(&dict)
        .compression(Compression::Lz4)
        .build()?;

    for evno in 0..1000_i64 {
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| {
                    r.set("evno", evno + 1)?;
                    r.set("beamE", 10.604_f32)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            ev.bank("REC::Particle", |b| {
                for i in 0..(evno % 7 + 1) as i32 {
                    b.row(|r| {
                        r.set("pid", 11 + i)?;
                        r.set("px", i as f32 * 0.1)?;
                        r.set("py", -i as f32 * 0.05)?;
                        r.set("pz", (i as f32 + 1.0) * 0.5)?;
                        r.set("charge", (i as i8) - 2)?;
                        Ok(())
                    })?;
                }
                Ok(())
            })?;
            Ok(())
        })?;
    }
    w.finish()?;

    println!("wrote {}", path);
    Ok(())
}
