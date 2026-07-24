// Fixture for the cross-implementation benchmark: three banks so that
// selective reads (one column, one bank) are meaningful, and a wide bank so
// "read everything" differs from "read two columns".
//   xfixture <out.hipo> <none|lz4|lz4best|percolumn> [events]
use oxihipo::{Compression, Dict, Result, Schema, Writer};

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let path = a.next().expect("out");
    let fmt = a.next().unwrap_or_else(|| "lz4".into());
    let nev: usize = a.next().map(|s| s.parse().unwrap()).unwrap_or(200_000);
    let comp = match fmt.as_str() {
        "none" => Compression::None,
        "lz4" => Compression::Lz4,
        "lz4best" => Compression::Lz4Best,
        "percolumn" => Compression::Lz4PerColumn,
        o => panic!("fmt {o}"),
    };
    let mut d = Dict::new();
    d.add(Schema::parse_text(
        "{REC::Particle/300/1}{pid/I,px/F,py/F,pz/F,vz/F,charge/B,status/S,chi2pid/F}",
    ).unwrap());
    d.add(Schema::parse_text("{REC::Event/300/30}{evno/L,beamE/F}").unwrap());
    d.add(Schema::parse_text("{REC::Calorimeter/300/40}{energy/F,sector/B,layer/B}").unwrap());

    let mut w = Writer::create(&path).schemas(&d).compression(comp).build()?;
    for e in 0..nev {
        w.event(|ev| {
            ev.bank("REC::Event", |b| {
                b.row(|r| { r.set("evno", e as i64)?; r.set("beamE", 10.6f32)?; Ok(()) })?;
                Ok(())
            })?;
            let n = 1 + e % 5;
            ev.bank("REC::Particle", |b| {
                for k in 0..n {
                    b.row(|r| {
                        r.set("pid", (11 + (e + k) % 7) as i32)?;
                        r.set("px", e as f32 * 0.001 + k as f32)?;
                        r.set("py", e as f32 * -0.002 + k as f32)?;
                        r.set("pz", e as f32 * 0.003 + k as f32)?;
                        r.set("vz", e as f32 * 0.004 - k as f32)?;
                        r.set("charge", ((k as i32 % 3) - 1) as i8)?;
                        r.set("status", ((e + k) % 4000) as i16)?;
                        r.set("chi2pid", e as f32 * 0.01 + k as f32)?;
                        Ok(())
                    })?;
                }
                Ok(())
            })?;
            // present on ~1/3 of events, so "one bank" reads skip work
            if e % 3 == 0 {
                ev.bank("REC::Calorimeter", |b| {
                    for k in 0..2 {
                        b.row(|r| {
                            r.set("energy", e as f32 * 0.5 + k as f32)?;
                            r.set("sector", (k as i32 + 1) as i8)?;
                            r.set("layer", ((e + k) % 9) as i8)?;
                            Ok(())
                        })?;
                    }
                    Ok(())
                })?;
            }
            Ok(())
        })?;
    }
    w.finish()?;
    let sz = std::fs::metadata(&path).unwrap().len();
    eprintln!("{path}: {nev} events, {:.1} MB ({fmt})", sz as f64 / 1e6);
    Ok(())
}
