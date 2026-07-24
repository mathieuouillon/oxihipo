// Cross-implementation read benchmark (Rust / oxihipo).
// Identical workload to xbench_cpp.cc and XBenchJava.java:
//   scan every event, read REC::Particle pid(i32) + px(f32), accumulate.
use std::hint::black_box;
use oxihipo::{Chain, Result};

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let path = a.next().expect("path");
    let iters: usize = a.next().map(|s| s.parse().unwrap()).unwrap_or(5);

    let mut best = f64::MAX;
    let mut first = f64::NAN;
    let mut events = 0u64;
    let mut rows = 0u64;
    let (mut csp, mut csx) = (0i64, 0f64);
    for _ in 0..iters {
        // Reopen every iteration so all three implementations start each pass
        // with cold scratch buffers; the timer excludes open + dictionary, as
        // it does in the C++ and Java benchmarks.
        let chain = Chain::open(&path)?;
        let schema = chain.schemas().require("REC::Particle")?;
        let h_pid = schema.handle::<i32>("pid")?;
        let h_px = schema.handle::<f32>("px")?;
        let t0 = std::time::Instant::now();
        let (mut sp, mut sx) = (0i64, 0f64);
        let (mut ev, mut rw) = (0u64, 0u64);
        for e in chain.events() {
            let e = e?;
            ev += 1;
            if let Some(b) = e.bank("REC::Particle") {
                for &v in b.read(h_pid).iter() { sp += v as i64; }
                for &v in b.read(h_px).iter() { sx += v as f64; }
                rw += b.rows() as u64;
            }
        }
        let dt = t0.elapsed().as_secs_f64();
        black_box((sp, sx));
        events = ev; rows = rw; csp = sp; csx = sx;
        if first.is_nan() { first = dt; }
        if dt < best { best = dt; }
    }
    println!("rust\t{:.4}\t{:.4}\t{}\t{}\t{}\t{:.3}", first, best, events, rows, csp, csx);
    Ok(())
}
