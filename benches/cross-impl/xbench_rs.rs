// Cross-implementation read benchmark — Rust (oxihipo).
//   xbench_rs <file.hipo> <scenario> [iters]
// Scenarios (identical work in every implementation; each prints a checksum
// so a mismatch proves the runs are not comparable):
//   count  iterate events, touch no bank
//   scan2  REC::Particle pid + px
//   wide   all 8 REC::Particle columns
//   col1   REC::Particle pid only
//   bank1  REC::Event evno only (skips the big particle bank)
use std::hint::black_box;
use oxihipo::{Chain, Result};

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let path = a.next().expect("path");
    let scen = a.next().unwrap_or_else(|| "scan2".into());
    let iters: usize = a.next().map(|s| s.parse().unwrap()).unwrap_or(10);
    // Explicit thread count: 1 = serial, 0 = every core. The Python binding
    // defaults to 0, so leaving this implicit would compare a parallel read
    // against a serial one.
    let threads: usize = a.next().map(|s| s.parse().unwrap()).unwrap_or(1);

    let (mut best, mut first) = (f64::MAX, f64::NAN);
    let (mut events, mut csum) = (0u64, 0f64);
    for _ in 0..iters {
        let chain = Chain::open(&path)?;
        let ps = chain.schemas().require("REC::Particle")?;
        let es = chain.schemas().require("REC::Event")?;
        let h_pid = ps.handle::<i32>("pid")?;
        let h_px = ps.handle::<f32>("px")?;
        let h_py = ps.handle::<f32>("py")?;
        let h_pz = ps.handle::<f32>("pz")?;
        let h_vz = ps.handle::<f32>("vz")?;
        let h_ch = ps.handle::<i8>("charge")?;
        let h_st = ps.handle::<i16>("status")?;
        let h_c2 = ps.handle::<f32>("chi2pid")?;
        let h_ev = es.handle::<i64>("evno")?;

        // Columnar scenarios (`c_*`) use `read_columns` -- the same bulk API the
        // Python binding drives -- instead of per-event iteration.
        if let Some(rest) = scen.strip_prefix("c_") {
            let sel: Vec<(&str, &[&str])> = match rest {
                "bank1" => vec![("REC::Event", &["evno"][..])],
                "col1" => vec![("REC::Particle", &["pid"][..])],
                "scan2" => vec![("REC::Particle", &["pid", "px"][..])],
                "wide" => vec![(
                    "REC::Particle",
                    &["pid", "px", "py", "pz", "vz", "charge", "status", "chi2pid"][..],
                )],
                o => panic!("unknown columnar scenario {o}"),
            };
            let t0 = std::time::Instant::now();
            let bufs = chain.read_columns(&sel, None, threads)?;
            let mut sum = 0f64;
            let mut ev = 0u64;
            for b in &bufs {
                ev = (b.offsets.len() - 1) as u64;
                for c in &b.columns {
                    // Integers reduce in i64 (exact, and the loop vectorises).
                    // Floats must accumulate in f64 to match the other three
                    // implementations bit-for-bit -- an f32 accumulator drifts
                    // (69199880 vs 69200102.851) and would break the checksum
                    // gate. That sequential f64 add cannot vectorise, which is
                    // the same constraint every implementation here is under.
                    sum += match &c.data {
                        oxihipo::ColumnData::I8(v) => v.iter().map(|&x| x as i64).sum::<i64>() as f64,
                        oxihipo::ColumnData::I16(v) => v.iter().map(|&x| x as i64).sum::<i64>() as f64,
                        oxihipo::ColumnData::I32(v) => v.iter().map(|&x| x as i64).sum::<i64>() as f64,
                        oxihipo::ColumnData::I64(v) => v.iter().sum::<i64>() as f64,
                        oxihipo::ColumnData::F32(v) => v.iter().map(|&x| x as f64).sum::<f64>(),
                        oxihipo::ColumnData::F64(v) => v.iter().sum::<f64>(),
                    };
                }
            }
            let dt = t0.elapsed().as_secs_f64();
            black_box(sum);
            events = ev; csum = sum;
            if first.is_nan() { first = dt; }
            if dt < best { best = dt; }
            continue;
        }

        let t0 = std::time::Instant::now();
        let mut sum = 0f64;
        let mut ev = 0u64;
        for e in chain.events() {
            let e = e?;
            ev += 1;
            match scen.as_str() {
                "count" => {}
                "bank1" => {
                    if let Some(b) = e.bank("REC::Event") {
                        for &v in b.read(h_ev).iter() { sum += v as f64; }
                    }
                }
                _ => {
                    if let Some(b) = e.bank("REC::Particle") {
                        match scen.as_str() {
                            "col1" => { for &v in b.read(h_pid).iter() { sum += v as f64; } }
                            "scan2" => {
                                for &v in b.read(h_pid).iter() { sum += v as f64; }
                                for &v in b.read(h_px).iter() { sum += v as f64; }
                            }
                            "wide" => {
                                for &v in b.read(h_pid).iter() { sum += v as f64; }
                                for &v in b.read(h_px).iter() { sum += v as f64; }
                                for &v in b.read(h_py).iter() { sum += v as f64; }
                                for &v in b.read(h_pz).iter() { sum += v as f64; }
                                for &v in b.read(h_vz).iter() { sum += v as f64; }
                                for &v in b.read(h_ch).iter() { sum += v as f64; }
                                for &v in b.read(h_st).iter() { sum += v as f64; }
                                for &v in b.read(h_c2).iter() { sum += v as f64; }
                            }
                            s => panic!("unknown scenario {s}"),
                        }
                    }
                }
            }
        }
        let dt = t0.elapsed().as_secs_f64();
        black_box(sum);
        events = ev; csum = sum;
        if first.is_nan() { first = dt; }
        if dt < best { best = dt; }
    }
    println!("rust\t{scen}\t{first:.5}\t{best:.5}\t{events}\t{csum:.3}");
    Ok(())
}
