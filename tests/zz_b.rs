use oxihipo::{Chain, Compression, DataType, Dict, Schema, Writer};

#[test]
fn bitinfo_on_a_real_file() {
    let d0 = std::env::temp_dir().join("oxi_b");
    let _ = std::fs::remove_dir_all(&d0);
    std::fs::create_dir_all(&d0).unwrap();
    let p = d0.join("a.hipo");
    let mut d = Dict::new();
    d.add(Schema::from_columns(
        "A::b",
        300,
        1,
        [("x".into(), DataType::Int, 1)],
    ));
    let mut w = Writer::create(&p)
        .schemas(&d)
        .compression(Compression::Lz4)
        .max_record_events(2)
        .build()
        .unwrap();
    for i in 0..6 {
        w.event(|ev| {
            ev.bank("A::b", |b| {
                b.row(|r| {
                    r.set("x", i)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            Ok(())
        })
        .unwrap();
    }
    w.finish().unwrap();

    // Walk every record header by hand and print its bit_info word.
    let bytes = std::fs::read(&p).unwrap();
    const MAGIC: [u8; 4] = [0x00, 0x01, 0xda, 0xc0];
    let mut i = 0usize;
    let mut n = 0;
    while i + 4 <= bytes.len() {
        if bytes[i..i + 4] == MAGIC && i >= 28 {
            let h = i - 28;
            if h + 24 <= bytes.len() {
                let hlw = u32::from_le_bytes(bytes[h + 8..h + 12].try_into().unwrap());
                if hlw == 14 {
                    let bi = u32::from_le_bytes(bytes[h + 20..h + 24].try_into().unwrap());
                    let ec = u32::from_le_bytes(bytes[h + 12..h + 16].try_into().unwrap());
                    println!(
                        "rec {n}: off={h} events={ec} bit_info=0x{bi:08x}  bit8={} bit9={} bit10={} bit11={}",
                        (bi >> 8) & 1,
                        (bi >> 9) & 1,
                        (bi >> 10) & 1,
                        (bi >> 11) & 1
                    );
                    n += 1;
                }
            }
        }
        i += 1;
    }
    println!("(rec 0 = file header, 1 = dictionary, then data, last = trailer)");
    let _ = Chain::open(&p);
    let _ = std::fs::remove_dir_all(&d0);
}
