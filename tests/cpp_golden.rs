//! Golden-file test against the **reference C++ writer**.
//!
//! Every other round-trip test in this suite writes *and* reads with this
//! crate, so a shared mis-encoding of the HIPO wire format would pass green in
//! both directions. This test reads a file produced by the reference C++
//! `hipo4` writer (`hallb/clas12/hipo-cpp`, master) and asserts exact values —
//! the only test that can catch "we agree with ourselves but not with HIPO".
//!
//! ## Regenerating the fixture
//!
//! `tests/data/cpp/golden_lz4.hipo` was produced by `tests/data/cpp/gen_golden.cc`
//! built against C++ `hipo4` master (commit `84592d4`):
//!
//! ```sh
//! git clone https://code.jlab.org/hallb/clas12/hipo-cpp && cd hipo-cpp
//! meson setup build . -Dbuildtype=release -D build_tests=false \
//!     -D build_examples=false -D dataframes=false && ninja -C build
//! clang++ -std=c++17 -O2 -I. path/to/gen_golden.cc -o gen_golden \
//!     -Lbuild/hipo4 -lhipo4 -Wl,-rpath,build/hipo4
//! ./gen_golden tests/data/cpp/golden_lz4.hipo
//! ```
//!
//! The reference master writer has no compression setter — it always emits LZ4
//! (type 1), which is also what real CLAS12 files use — so LZ4 is the format a
//! golden can cover.
//!
//! ## The dataset (mirrored from `gen_golden.cc`)
//!
//! 16 events, dictionary + user config, two banks:
//!
//! | bank | id | columns | rows/event |
//! | --- | --- | --- | --- |
//! | `REC::Event` | 300/30 | `evno/L`, `beamE/F` | 1 |
//! | `REC::Particle` | 300/31 | `pid/I`, `px/F`, `py/F`, `pz/D`, `status/S`, `charge/B` | `e % 4` (so events 0, 4, 8, 12 have none) |

use std::path::PathBuf;

use oxihipo::Chain;

fn golden() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/cpp/golden_lz4.hipo")
}

const N_EVENTS: u64 = 16;

fn n_rows(e: i64) -> u32 {
    (e % 4) as u32
}

#[test]
fn cpp_written_file_opens_with_the_expected_dictionary() {
    let chain = Chain::open(golden()).unwrap();
    assert_eq!(chain.event_count(), N_EVENTS, "event count");

    let dict = chain.schemas();
    let ev = dict.require("REC::Event").expect("REC::Event in dict");
    assert_eq!((ev.group(), ev.item()), (300, 30));
    assert_eq!(
        ev.entries()
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>(),
        ["evno", "beamE"]
    );

    let p = dict
        .require("REC::Particle")
        .expect("REC::Particle in dict");
    assert_eq!((p.group(), p.item()), (300, 31));
    assert_eq!(
        p.entries()
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>(),
        ["pid", "px", "py", "pz", "status", "charge"]
    );
    // Widths must agree with the C++ schema, or every bank would mis-slice.
    // pid/I(4) + px/F(4) + py/F(4) + pz/D(8) + status/S(2) + charge/B(1) = 23
    assert_eq!(p.row_size(), 23, "row size of the C++ schema");
}

#[test]
fn cpp_written_values_read_back_exactly() {
    let chain = Chain::open(golden()).unwrap();
    let mut seen = 0i64;
    for res in chain.events() {
        let ev = res.expect("event decodes");
        let e = seen;

        let evb = ev.bank("REC::Event").expect("REC::Event present");
        assert_eq!(evb.rows(), 1);
        assert_eq!(evb.get::<i64>("evno", 0), 1000 + e, "evno at event {e}");
        assert_eq!(evb.get::<f32>("beamE", 0), 10.6_f32, "beamE at event {e}");

        let n = n_rows(e);
        match ev.bank("REC::Particle") {
            Some(p) => assert_eq!(p.rows(), n, "particle rows at event {e}"),
            None => assert_eq!(n, 0, "event {e} should have had {n} particle rows"),
        }
        if let Some(p) = ev.bank("REC::Particle") {
            for r in 0..n {
                let rf = r as f32;
                let rd = r as f64;
                assert_eq!(p.get::<i32>("pid", r), 11 + (e as i32) * 10 + r as i32);
                assert_eq!(p.get::<f32>("px", r), e as f32 * 0.5 + rf);
                assert_eq!(p.get::<f32>("py", r), e as f32 * -0.25 + rf);
                assert_eq!(p.get::<f64>("pz", r), e as f64 * 0.125 + rd);
                assert_eq!(p.get::<i16>("status", r), (e as i16) * 4 + r as i16);
                assert_eq!(p.get::<i8>("charge", r), r as i8 - 1);
            }
        }
        seen += 1;
    }
    assert_eq!(seen as u64, N_EVENTS);
}

#[test]
fn cpp_written_user_config_reads_back() {
    // The C++ writer stored two entries via `addUserConfig`; reading them
    // proves the (32555,1)/(32555,2) layout matches.
    let chain = Chain::open(golden()).unwrap();
    assert_eq!(chain.config("generator"), Some("cpp-hipo4-master"));
    assert_eq!(chain.config("dataset"), Some("golden-v1"));
}

#[test]
fn cpp_written_file_reads_the_same_every_way() {
    // Sequential, random access, and the columnar materializer must agree on a
    // C++-written file — they take three different decode routes.
    let chain = Chain::open(golden()).unwrap();

    let sequential: Vec<i64> = chain
        .events()
        .map(|ev| {
            ev.unwrap()
                .bank("REC::Event")
                .unwrap()
                .get::<i64>("evno", 0)
        })
        .collect();
    assert_eq!(sequential.len() as u64, N_EVENTS);

    for (i, expect) in sequential.iter().enumerate() {
        let ev = chain.event(i as u64).expect("index in range");
        assert_eq!(
            ev.bank("REC::Event").unwrap().get::<i64>("evno", 0),
            *expect,
            "random access disagrees at {i}"
        );
    }

    let cols = chain
        .read_columns(&[("REC::Event", &["evno"][..])], None, 1)
        .unwrap();
    assert_eq!(cols[0].offsets.len() as u64, N_EVENTS + 1);
    assert_eq!(cols[0].columns.len(), 1);
}
