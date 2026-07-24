// Generate a realistically-sized HIPO file for read benchmarking.
use oxihipo::{Compression, Dict, Result, Schema, Writer};

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let path = a.next().expect("path");
    let fmt = a.next().unwrap_or_else(|| "lz4".into());
    let nev: usize = a.next().map(|s| s.parse().unwrap()).unwrap_or(200_000);
    let comp = match fmt.as_str() {
        "none" => Compression::None,
        "lz4" => Compression::Lz4,
        "lz4best" => Compression::Lz4Best,
        "gzip" => Compression::Gzip,
        "perbank" => Compression::Lz4PerBank,
        "percolumn" => Compression::Lz4PerColumn,
        o => panic!("fmt {o}"),
    };
    let mut d = Dict::new();
    d.add(Schema::parse_text("{REC::Particle/300/1}{pid/I,px/F,py/F,pz/F}").unwrap());
    d.add(Schema::parse_text("{REC::Event/300/30}{evno/L}").unwrap());
    let mut w = Writer::create(&path).schemas(&d).compression(comp).build()?;
    for e in 0..nev {
        w.event(|ev| {
            ev.bank("REC::Event", |b| { b.row(|r| { r.set("evno", e as i64)?; Ok(()) })?; Ok(()) })?;
            let n = 1 + (e % 5);
            ev.bank("REC::Particle", |b| {
                for k in 0..n {
                    b.row(|r| {
                        r.set("pid", (11 + (e + k) % 7) as i32)?;
                        r.set("px", (e as f32 * 0.001 + k as f32) )?;
                        r.set("py", (e as f32 * -0.002 + k as f32))?;
                        r.set("pz", (e as f32 * 0.003 + k as f32))?;
                        Ok(())
                    })?;
                }
                Ok(())
            })?;
            Ok(())
        })?;
    }
    w.finish()?;
    let sz = std::fs::metadata(&path).unwrap().len();
    eprintln!("wrote {nev} events -> {path} ({:.1} MB, {fmt})", sz as f64 / 1e6);
    Ok(())
}
