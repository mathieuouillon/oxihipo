//! Integration tests for the `hipo-analysis` framework: build a small
//! `.hipo` file, run a multi-algorithm analysis over it, and check the
//! histograms, cut-flow, and skim.

use hipo::{Chain, DataType, Dict, Schema, Writer};
use hipo_analysis::prelude::*;

/// A typed product shared through the per-event `Context`.
struct Electron(LorentzVector);

#[derive(Clone)]
struct RequireParticles;
impl Algorithm for RequireParticles {
    fn name(&self) -> &str {
        "require"
    }
    fn process(&mut self, ctx: &mut Context<'_>, _out: &mut Output) -> Flow {
        match ctx.event().bank("REC::Particle") {
            Some(p) if p.rows() > 0 => Flow::Continue,
            _ => Flow::Skip,
        }
    }
}

#[derive(Clone)]
struct FindElectron;
impl Algorithm for FindElectron {
    fn name(&self) -> &str {
        "find-electron"
    }
    fn process(&mut self, ctx: &mut Context<'_>, _out: &mut Output) -> Flow {
        let Some(p) = ctx.event().bank("REC::Particle") else {
            return Flow::Skip;
        };
        let Some(row) = (0..p.rows()).find(|&r| p.get::<i32>("pid", r) == 11) else {
            return Flow::Skip;
        };
        let Some(e) = LorentzVector::from_row(&p, row, M_ELECTRON) else {
            return Flow::Skip;
        };
        ctx.put(Electron(e));
        Flow::Continue
    }
}

#[derive(Clone)]
struct FillMomentum;
impl Algorithm for FillMomentum {
    fn name(&self) -> &str {
        "fill"
    }
    fn process(&mut self, ctx: &mut Context<'_>, out: &mut Output) -> Flow {
        let Some(Electron(e)) = ctx.get::<Electron>() else {
            return Flow::Skip;
        };
        out.h1("p", 100, 0.0, 10.0).fill(e.p());
        *out.count("electrons") += 1;
        Flow::Continue
    }
}

fn dict() -> Dict {
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "REC::Particle",
        300,
        1,
        [
            ("pid".into(), DataType::Int),
            ("px".into(), DataType::Float),
            ("py".into(), DataType::Float),
            ("pz".into(), DataType::Float),
        ],
    ));
    d
}

/// Write `n` events, each with a single pid=11 electron at a fixed momentum.
fn write_file(path: &std::path::Path, d: &Dict, n: i32) {
    let mut w = Writer::create(path).schemas(d).build().unwrap();
    for _ in 0..n {
        w.event(|ev| {
            ev.bank("REC::Particle", |b| {
                b.row(|r| {
                    r.set("pid", 11_i32)?;
                    r.set("px", 0.30_f32)?;
                    r.set("py", 0.40_f32)?;
                    r.set("pz", 2.0_f32)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            Ok(())
        })
        .unwrap();
    }
    w.finish().unwrap();
}

fn analysis() -> Analysis {
    Analysis::new()
        .then(RequireParticles)
        .then(FindElectron)
        .then(FillMomentum)
}

#[test]
fn run_fills_histograms_and_cutflow() {
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.hipo");
    let p2 = dir.path().join("b.hipo");
    let d = dict();
    write_file(&p1, &d, 120);
    write_file(&p2, &d, 80);
    let chain = Chain::open_all([&p1, &p2]).unwrap();

    let report = analysis().run(&chain, 0).unwrap();

    assert_eq!(report.cutflow.passed_count("require"), Some(200));
    assert_eq!(report.cutflow.passed_count("find-electron"), Some(200));
    assert_eq!(report.cutflow.passed_count("fill"), Some(200));
    assert_eq!(report.cutflow.passed_count("missing"), None);
    assert_eq!(report.output.counter("fill", "electrons"), 200);

    let h = report.output.h1_ref("fill", "p").expect("histogram booked");
    assert_eq!(h.sum(), 200.0);
}

#[test]
fn sequential_matches_parallel() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.hipo");
    write_file(&p, &dict(), 150);
    let chain = Chain::open(&p).unwrap();

    let par = analysis().run(&chain, 0).unwrap();
    let seq = analysis().run_sequential(&chain).unwrap();

    assert_eq!(
        par.output.counter("fill", "electrons"),
        seq.output.counter("fill", "electrons"),
    );
    assert_eq!(par.cutflow.passed_count("fill"), Some(150));
    assert_eq!(seq.cutflow.passed_count("fill"), Some(150));
}

#[test]
fn skim_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.hipo");
    let dst = dir.path().join("skim.hipo");
    write_file(&src, &dict(), 100);
    let chain = Chain::open(&src).unwrap();

    let stats = hipo_analysis::skim(&chain, &dst, |ev| ev.has("REC::Particle")).unwrap();
    assert_eq!(stats.events_in, 100);
    assert_eq!(stats.events_kept, 100);

    let skimmed = Chain::open(&dst).unwrap();
    assert_eq!(skimmed.event_count(), 100);
}
