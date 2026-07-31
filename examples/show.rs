//! Print a file's dictionary, one schema, and one bank — the `Display` impls.
//! Usage: cargo run --release --example show -- <file> [bank]
use oxihipo::{Chain, Result};
use std::env;

fn main() -> Result<()> {
    let mut a = env::args().skip(1);
    let path = a.next().expect("usage: show <file> [bank]");
    let want = a.next().unwrap_or_else(|| "REC::Particle".into());
    let chain = Chain::open(&path)?;

    println!("{}\n", chain.schemas());
    if let Some(s) = chain.schemas().get(&want) {
        println!("{s}\n");
    }
    for ev in chain.events().take(200) {
        let ev = ev?;
        if let Some(b) = ev.ctx().bank(&want)
            && b.rows() >= 3
        {
            println!("{b}");
            break;
        }
    }
    Ok(())
}
