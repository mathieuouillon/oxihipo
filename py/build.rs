//! Emit the macOS linker flags an `extension-module` cdylib needs, so the crate
//! builds correctly under *any* tool or working directory.
//!
//! A pyo3 `extension-module` deliberately leaves the CPython API symbols
//! (`_Py_*`) undefined — the host interpreter resolves them at import time — so
//! on macOS the link needs `-undefined dynamic_lookup`. A `.cargo/config.toml`
//! only supplies that when cargo's *current directory* is this crate, which
//! breaks `maturin sdist` source installs (they build from the tarball root
//! with `--manifest-path py/Cargo.toml`). A build script travels with the
//! crate into the sdist and runs regardless of CWD.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // Applies only to the cdylib artifact (not the rlib or the bin).
        println!("cargo:rustc-cdylib-link-arg=-undefined");
        println!("cargo:rustc-cdylib-link-arg=dynamic_lookup");
    }
}
