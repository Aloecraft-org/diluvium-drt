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
//! - `ws`: a byte stream over the page's socket, for protocols that want
//!   one. The page keeps the socket; this keeps channel ends, which is
//!   what makes the stream `Send`.
//! - `ssh`: `drt-sshd`, the SSH server over that stream -- pubkey only,
//!   keys named in advance -- handing out a shell's two ends. The same one
//!   the native REPL server runs.
//! - `wasi_shim` (browser only): wasi-libc's seventeen syscalls.
//! - `js` (browser only): connectors the page hands in -- a `net`
//!   transport, or any family as a `{ call }` object (SPEC.md §7's
//!   `JsConnector`) -- by kernel, through `Term::with_backings`.
//! - `pagefs` (browser only): a terminal's filesystem answered by a page
//!   object, handed in as `fs` (cdn/fs.mjs: an aloelite volume), one per
//!   terminal or shared by several.
//! - `sqljs` (browser only): `sql/*` over a sql.js module the page hands
//!   in, with the native connector's contract.

pub mod term;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub mod bindings;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub mod editor;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub mod js;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub mod pagefs;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub mod raw;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub mod sqljs;
/// The SSH server, `drt-sshd`, under the name this crate has always used.
pub use drt_sshd as ssh;
pub mod swarm;
pub mod ws;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod wasi_shim;

pub use term::{Backing, Entered, PageFs, Session, Sink, Step, Term};
