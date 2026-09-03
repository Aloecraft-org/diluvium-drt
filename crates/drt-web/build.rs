//! Links wasi-libc into the browser module (doc/Wasm.md D4).
//!
//! ## surface block
//!
//! - `main`: the only entry point. Does nothing unless the target is
//!   `wasm32-unknown-unknown`.
//! - [`SYSROOT_LIB_DIRS`]: where a wasi-sdk keeps `libc.a`, newest layout
//!   first.
//!
//! The C core needs a libc; a page has none. Rather than the 56-symbol
//! surface an embedder would otherwise hand-write, the wasi-sdk's own
//! `libc.a` is linked here and its seventeen syscalls -- the only thing in
//! it that touches a host -- are defined in `src/wasi_shim.rs`. The same
//! `WASI_SDK_PATH` diluvium-sys already requires for this target, so no
//! new environment variable. The archive is copied under a name that is
//! not `c`, so nothing else's `-lc` resolves to it by accident.

use std::path::PathBuf;

const SYSROOT_LIB_DIRS: [&str; 2] = ["lib/wasm32-wasip1", "lib/wasm32-wasi"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=WASI_SDK_PATH");
    if std::env::var("TARGET").as_deref() != Ok("wasm32-unknown-unknown") {
        return;
    }
    let sdk = PathBuf::from(std::env::var("WASI_SDK_PATH").unwrap_or_else(|_| {
        panic!(
            "\ndrt-web links wasi-libc into the browser module and needs a wasi-sdk (>= 24; \
             27 verified) named by WASI_SDK_PATH -- the same one diluvium-sys needs to \
             compile the C core for this target. See doc/Wasm.md §8.\n"
        )
    }));
    let sysroot = sdk.join("share/wasi-sysroot");
    let libc = SYSROOT_LIB_DIRS
        .iter()
        .map(|d| sysroot.join(d).join("libc.a"))
        .find(|p| p.exists())
        .unwrap_or_else(|| {
            panic!(
                "no libc.a under {} (looked in {:?})",
                sysroot.display(),
                SYSROOT_LIB_DIRS
            )
        });
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::copy(&libc, out.join("libwasilibc.a"))
        .unwrap_or_else(|e| panic!("cannot copy {}: {e}", libc.display()));
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=wasilibc");
}
