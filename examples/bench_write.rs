//! Write-path benchmark: event assembly + record framing.
//!
//! Usage: `bench_write <out.hipo> [events] [banks] [cols] [rows]`
//!
//! Prints wall time and the output's SHA-agnostic size, so two builds can be
//! compared for both speed and byte-identical output. Interleave the builds
//! when comparing — running one to completion then the other lets filesystem
//! and page-cache drift land on whichever went last.
use oxihipo::{Compression, DataType, Dict, Result, Schema, Writer};
use std::env;
use std::time::Instant;

fn main() -> Result<()> {
    let mut a = env::args().skip(1);
    let out = a
        .next()
        .expect("usage: bench_write <out> [events] [banks] [cols] [rows]");
    let events: usize = a.next().map(|s| s.parse().unwrap()).unwrap_or(50_000);
    let banks: usize = a.next().map(|s| s.parse().unwrap()).unwrap_or(47);
    let cols: usize = a.next().map(|s| s.parse().unwrap()).unwrap_or(8);
    let rows: u32 = a.next().map(|s| s.parse().unwrap()).unwrap_or(3);

    let mut dict = Dict::new();
    for b in 0..banks {
        let entries: Vec<(String, DataType, u32)> = (0..cols)
            .map(|c| {
                let ty = match c % 4 {
                    0 => DataType::Int,
                    1 => DataType::Float,
                    2 => DataType::Short,
                    _ => DataType::Byte,
                };
                (format!("c{c}"), ty, 1)
            })
            .collect();
        dict.add(Schema::from_columns(
            format!("B{b}::x").as_str(),
            300 + b as u16,
            1,
            entries,
        ));
    }
    let names: Vec<String> = (0..cols).map(|c| format!("c{c}")).collect();
    let bank_names: Vec<String> = (0..banks).map(|b| format!("B{b}::x")).collect();

    let t = Instant::now();
    let mut w = Writer::create(&out)
        .schemas(&dict)
        .compression(Compression::Lz4)
        .build()?;
    for e in 0..events {
        w.event(|ev| {
            for bn in &bank_names {
                ev.bank(bn, |b| {
                    for r in 0..rows {
                        b.row(|x| {
                            for (c, name) in names.iter().enumerate() {
                                match c % 4 {
                                    0 => x.set(name, (e as i32) + r as i32)?,
                                    1 => x.set(name, r as f32 * 0.5)?,
                                    2 => x.set(name, r as i16)?,
                                    _ => x.set(name, (r % 128) as i8)?,
                                };
                            }
                            Ok(())
                        })?;
                    }
                    Ok(())
                })?;
            }
            Ok(())
        })?;
    }
    let s = w.finish()?;
    let el = t.elapsed();
    println!(
        "{:>7.3}s  {:>9.0} ev/s  {} bytes  {} records",
        el.as_secs_f64(),
        events as f64 / el.as_secs_f64(),
        s.bytes,
        s.records
    );
    Ok(())
}
