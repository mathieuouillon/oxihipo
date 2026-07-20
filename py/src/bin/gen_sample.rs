//! Test helper: write a small, known HIPO sample so the Python smoke test has
//! a real file to read. Usage: `gen_sample <out.hipo> [none|lz4|bybank|percolumn]`.
//! The data model mirrors `tests/columns.rs`.

use oxihipo::{Compression, DataType, Dict, Schema, Writer};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: gen_sample <out.hipo> [format]");
    let fmt = args.next().unwrap_or_else(|| "percolumn".into());
    let compression = match fmt.as_str() {
        "none" => Compression::None,
        "lz4" => Compression::Lz4,
        "bybank" => Compression::Lz4PerBank,
        "percolumn" => Compression::Lz4PerColumn,
        other => panic!("unknown format {other}"),
    };

    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        1,
        [
            ("pid".into(), DataType::Int, 1),
            ("px".into(), DataType::Float, 1),
            ("cov".into(), DataType::Float, 3),
        ],
    ));
    d.add(Schema::from_columns(
        "REC::Event",
        300,
        30,
        [("evno".into(), DataType::Long, 1)],
    ));

    let mut w = Writer::create(&path)
        .schemas(&d)
        .compression(compression)
        .max_record_events(3)
        .build()
        .unwrap();

    for i in 0..8i64 {
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| {
                    r.set("evno", 1000 + i)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            let rows = (i % 4) as i32;
            if rows > 0 {
                ev.bank("REC::Particle", |b| {
                    for r in 0..rows {
                        b.row(|rw| {
                            rw.set("pid", (i as i32) * 100 + r)?;
                            rw.set("px", i as f32 + r as f32 * 0.1)?;
                            let base = ((i as i32) * 100 + r) as f32;
                            rw.set("cov", [base, base + 0.5, base + 0.25])?;
                            Ok(())
                        })?;
                    }
                    Ok(())
                })?;
            }
            Ok(())
        })
        .unwrap();
    }
    w.finish().unwrap();
    eprintln!("wrote {path} ({fmt})");
}
