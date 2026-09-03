//! The browser tier (doc/Wasm.md D3): the same `drt` the binary is, with
//! the C core linked in, behind a terminal contract a page attaches
//! xterm.js to.
//!
//! Nothing here is a second runtime. [`term::Term::exec`] parses a command
//! line with the binary's own `Cli`, assembles the same config and
//! dispatcher, and drives the same `Solo`, `Repl` and `DeployDriver` the
//! native loops drive -- the one thing a page cannot share is the loop
//! itself, which may not sleep, so [`term::Session::tick`] hands the sleep
//! to the page. The platform underneath is `drt-platform`'s browser half:
//! a `MemFs` the page seeds, `web-time` for the clock, `getrandom`'s
//! `wasm_js` backend, and a stdio sink the page installs -- which is also
//! where wasi-libc's `fd_write` lands (`wasi_shim`), so the C core's
//! `print` and the runtime's own text reach the same terminal in order.
//!
//! The crate is built so that almost none of it is browser-only: `term`
//! compiles and is tested natively (`tests/term.rs`), and the two
//! browser-only files are marshalling (`bindings`) and syscalls
//! (`wasi_shim`).
//!
//! ## surface block
//!
//! - [`term::Term`], [`term::Session`], [`term::Step`]: the contract.
//! - `bindings` (browser only): the wasm-bindgen exports -- `DrtTerm`,
//!   `DrtSession`, `DrtEditor`, `abiVersion`, `buildInfo`, `setPanicHook`.
//! - `editor` (browser only): the one line editor, over the page's
//!   xterm.js object.
//! - `swarm`: the instances table -- `dvs.c`'s sixteen, over a
//!   `Deployment`.
//! - `wasi_shim` (browser only): wasi-libc's seventeen syscalls.

pub mod term;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub mod bindings;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub mod editor;
pub mod swarm;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod wasi_shim;

pub use term::{Session, Step, Term};
