//! wasi-libc's seventeen syscalls, defined here so the browser module
//! imports none of them (doc/Wasm.md D4).
//!
//! wasi-libc declares each syscall as an import from
//! `wasi_snapshot_preview1` under the C symbol
//! `__imported_wasi_snapshot_preview1_<name>`; a definition of that symbol
//! in this module wins at link time, and the import section ends up
//! holding nothing but wasm-bindgen's own glue. Measured on the spike that
//! became this file: 56 `env` imports with no libc, 17 with wasi-libc, none
//! with these.
//!
//! What each does is what C's own semantics say a machine with no such
//! facility does, never a pretence: `fd_write` is the only one that
//! carries data, and it carries it to `drt_platform::stdio` -- the same
//! sink the runtime's own text goes to, so the C core's `print` and a
//! `drt run:` refusal reach the page in order. The clocks are the page's.
//! Files, environment and the process answer `EBADF`/`ENOTSUP`: the fs
//! connector never reaches libc (it reads the page's `MemFs`), and a
//! sealed instance has no `io` or `os` to ask with.
//!
//! In this crate rather than `drt-platform` because these are `#[no_mangle]`
//! symbols resolved by wasi-libc's objects at the final link, and the final
//! crate is where they are certain to be linked.

use drt_platform::clock::{Instant, SystemTime, UNIX_EPOCH};
use drt_platform::stdio::{self, Fd};

const EBADF: i32 = 8;
const ENOTSUP: i32 = 58;
/// wasi's `clockid`: 0 is realtime, everything else is a monotonic flavour.
const CLOCK_REALTIME: i32 = 0;

thread_local! {
    static MONOTONIC_ORIGIN: Instant = Instant::now();
}

#[no_mangle]
pub extern "C" fn __imported_wasi_snapshot_preview1_fd_write(
    fd: i32,
    iov: i32,
    iovs: i32,
    written: i32,
) -> i32 {
    let stream = match fd {
        1 => Fd::Stdout,
        2 => Fd::Stderr,
        _ => return EBADF,
    };
    let mut total: u32 = 0;
    for i in 0..iovs.max(0) as usize {
        // An iovec is two u32s: base, then length.
        let entry = iov as usize + i * 8;
        let (base, len) = unsafe { (*(entry as *const u32), *((entry + 4) as *const u32)) };
        let bytes = unsafe { std::slice::from_raw_parts(base as usize as *const u8, len as usize) };
        stdio::write(stream, bytes);
        total = total.saturating_add(len);
    }
    unsafe { *(written as usize as *mut u32) = total };
    0
}

#[no_mangle]
pub extern "C" fn __imported_wasi_snapshot_preview1_clock_time_get(
    id: i32,
    _precision: i64,
    out: i32,
) -> i32 {
    let ns: u64 = if id == CLOCK_REALTIME {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    } else {
        MONOTONIC_ORIGIN.with(|origin| origin.elapsed().as_nanos() as u64)
    };
    unsafe { *(out as usize as *mut u64) = ns };
    0
}

#[no_mangle]
pub extern "C" fn __imported_wasi_snapshot_preview1_environ_sizes_get(
    count: i32,
    size: i32,
) -> i32 {
    unsafe {
        *(count as usize as *mut u32) = 0;
        *(size as usize as *mut u32) = 0;
    }
    0
}

#[no_mangle]
pub extern "C" fn __imported_wasi_snapshot_preview1_environ_get(_environ: i32, _buf: i32) -> i32 {
    0
}

#[no_mangle]
pub extern "C" fn __imported_wasi_snapshot_preview1_fd_close(_fd: i32) -> i32 {
    EBADF
}

#[no_mangle]
pub extern "C" fn __imported_wasi_snapshot_preview1_fd_fdstat_get(_fd: i32, _out: i32) -> i32 {
    EBADF
}

#[no_mangle]
pub extern "C" fn __imported_wasi_snapshot_preview1_fd_fdstat_set_flags(
    _fd: i32,
    _flags: i32,
) -> i32 {
    EBADF
}

#[no_mangle]
pub extern "C" fn __imported_wasi_snapshot_preview1_fd_prestat_dir_name(
    _fd: i32,
    _path: i32,
    _len: i32,
) -> i32 {
    EBADF
}

#[no_mangle]
pub extern "C" fn __imported_wasi_snapshot_preview1_fd_prestat_get(_fd: i32, _out: i32) -> i32 {
    EBADF
}

#[no_mangle]
pub extern "C" fn __imported_wasi_snapshot_preview1_fd_read(
    _fd: i32,
    _iov: i32,
    _iovs: i32,
    _read: i32,
) -> i32 {
    EBADF
}

#[no_mangle]
pub extern "C" fn __imported_wasi_snapshot_preview1_fd_renumber(_from: i32, _to: i32) -> i32 {
    EBADF
}

#[no_mangle]
pub extern "C" fn __imported_wasi_snapshot_preview1_fd_seek(
    _fd: i32,
    _offset: i64,
    _whence: i32,
    _out: i32,
) -> i32 {
    EBADF
}

#[no_mangle]
pub extern "C" fn __imported_wasi_snapshot_preview1_path_open(
    _fd: i32,
    _dirflags: i32,
    _path: i32,
    _path_len: i32,
    _oflags: i32,
    _rights_base: i64,
    _rights_inheriting: i64,
    _fdflags: i32,
    _out: i32,
) -> i32 {
    ENOTSUP
}

#[no_mangle]
pub extern "C" fn __imported_wasi_snapshot_preview1_path_remove_directory(
    _fd: i32,
    _path: i32,
    _path_len: i32,
) -> i32 {
    ENOTSUP
}

#[no_mangle]
pub extern "C" fn __imported_wasi_snapshot_preview1_path_rename(
    _fd: i32,
    _path: i32,
    _path_len: i32,
    _new_fd: i32,
    _new_path: i32,
    _new_path_len: i32,
) -> i32 {
    ENOTSUP
}

#[no_mangle]
pub extern "C" fn __imported_wasi_snapshot_preview1_path_unlink_file(
    _fd: i32,
    _path: i32,
    _path_len: i32,
) -> i32 {
    ENOTSUP
}

#[no_mangle]
pub extern "C" fn __imported_wasi_snapshot_preview1_proc_exit(_code: i32) -> ! {
    // A sealed instance has no `os.exit`; anything reaching this is a libc
    // abort, and a trap is the honest shape of one in a page.
    std::process::abort()
}
