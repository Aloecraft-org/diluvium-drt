# FM-2, upstream: a data race in the named-continuation registries

**Audience: whoever picks this up in `Aloecraft-org/diluvium`.** This is a
DRT document about a diluvium bug, written here because DRT is where the
evidence was collected and where FM-2 has been tracked for four days. Copy
it into that repo or read it from this one; nothing in it depends on DRT.

**Status: fixed upstream in diluvium 5.5.1_build12, 2026-09-01.** That
release adds `src/dsync.h` and guards both registries, and its changelog
names `f137b30` and earlier as affected. Everything below is the brief
that asked for it, kept as written: the diagnosis, the evidence and the
constraints are still the record of *why* the fix is shaped the way it
is, and the reproduction recipe is still how you would check a build.

Read the rest as history. **The pin bump is taken**: this tree carries
`515160f` (5.5.1_build12p1), so nothing here is outstanding. The
`drt-swarm` mitigation is kept for one more release rather than because it
is needed; the last section says why.

Verified facts and reconstruction are kept apart throughout, because the
history of this bug is four days of confident wrong answers built on
evidence that could not have distinguished them.

---

## The bug in one paragraph

`diluvium_shim_addcont` (`src/dshim.c:750`) appends to a process-global
array with no synchronisation of any kind. Two threads calling `dv_new`
concurrently can both claim the same slot index, leaving a slot whose
`name` is still `NULL`; the next scan calls `strcmp(NULL, ...)` and the
process dies with SIGSEGV. `diluvium_snap_addcont` (`src/dsnap.c:1312`)
is the same function over a second array with the same defect. The
registries are **process-global**, so `dv.h`'s "one instance, one thread"
contract does not cover them — a host obeying that contract to the letter
still hits this.

## Mission

Make concurrent `dv_new` safe. Two arrays, one shared root cause, plus a
racy `static int done` guard on the same path. **Do not** fix it by
telling hosts not to do this: they are already within contract.

---

## The evidence

### The core

`diluvium-drt` CI [run 33444175875](https://github.com/Aloecraft-org/diluvium-drt/actions/runs/33444175875),
job `test`, step `Test (all features)`, 2026-08-31:

```
process didn't exit successfully: .../deps/host_lua-db40667d4c8c7927
  (signal: 11, SIGSEGV: invalid memory reference)
```

Core dumps were armed, so this one was symbolized in-run
(`/tmp/core.a_relay_block_r.11593`):

```
Program terminated with signal SIGSEGV, Segmentation fault.
#0  __strcmp_evex () at ../sysdeps/x86_64/multiarch/strcmp-evex.S:314
[Current thread is 1 (Thread 0x7fe9857fd6c0 (LWP 11596))]

Thread 1 (faulting):
#0  __strcmp_evex ()
#1  diluvium_shim_addcont ()
#2  diluvium_openlibs ()
#3  dv_new ()
#4  diluvium::Instance::fresh (cfg=...) at src/lib.rs:400
...
#10 host_lua::a_relay_block_refuses_a_half_configured_label (host_lua.rs:217)
```

The decisive part is the *other* threads. Two more were inside the same
function at the same moment:

```
Thread 2:  luaopen_dhostlib <- luaL_requiref <- diluvium_openlibs <- dv_new
           ... host_lua::a_config_that_is_not_data_is_refused (host_lua.rs:144)
Thread 4:  luaopen_dhostlib <- luaL_requiref <- diluvium_openlibs <- dv_new
           ... host_lua::a_rendezvous_fetchpoint_configures_its_relay_in_host_lua
```

Three threads in `diluvium_openlibs` concurrently; the one that faulted
faulted in a `strcmp` inside the registry scan.

diluvium revision under test: **`f137b308c4dce917b24c71ab41add61606945e58`**
(the `Cargo.lock` pin). rustc stable, `ubuntu-latest`, debug build.

### Prior occurrences, now explained

| when | run | binary |
|---|---|---|
| 2026-08-28 | [33157346043](https://github.com/Aloecraft-org/diluvium-drt/actions/runs/33157346043) | `host_lua`, ~87 ms in, before any test reported |
| 2026-08-28 | [33192177326](https://github.com/Aloecraft-org/diluvium-drt/actions/runs/33192177326) | `stun` |
| 2026-08-31 | 33444175875 | `host_lua`, with the core above |

"~87 ms in, before any test reported" is process start, which is exactly
and only when this can fire. See *Why it never reproduced* below.

Note the `stun` crash of 2026-08-28 has a **separate, already-fixed**
cause recorded in `doc/Failure-Modes.md` as FM-1 — a use-after-free in
tokio 1.53.1 runtime teardown. FM-1 is real, is not this, and is not
diluvium's. Do not merge the two.

---

## The mechanism

`src/dshim.c:745-763`, quoted whole because the whole thing is the bug:

```c
#define DSHIM_MAXCONT	64

typedef struct dshim_cont { const char *name; lua_KFunction k; } dshim_cont;
static dshim_cont dshim_conts[DSHIM_MAXCONT];
static int dshim_ncont = 0;

LUA_API int diluvium_shim_addcont (const char *name, lua_KFunction k) {
  int i;
  if (name == NULL || k == NULL)
    return 0;
  for (i = 0; i < dshim_ncont; i++) {
    if (strcmp(dshim_conts[i].name, name) == 0)
      return (dshim_conts[i].k == k);  /* idempotent, never a silent rebind */
  }
  if (dshim_ncont >= DSHIM_MAXCONT)
    return 0;
  dshim_conts[dshim_ncont].name = name;
  dshim_conts[dshim_ncont].k = k;
  dshim_ncont++;
  return 1;
}
```

No mutex, no atomic, no once-guard — `grep -n "pthread\|mutex\|atomic\|_Atomic\|once" src/dshim.c src/dlibs.c` returns nothing.

`diluvium_openlibs` (`src/dlibs.c:37`) calls into it on **every** `dv_new`:

```c
LUA_API void diluvium_openlibs (lua_State *L) {
  diluvium_task_registerconts();          /* -> addcont("dtask.driver", ...) */
  const luaL_Reg *lib;
  for (lib = diluvium_libs; lib->name != NULL; lib++) {
    luaL_requiref(L, lib->name, lib->func, 1);   /* luaopen_dqueue ->
                                                    addcont("dqueue.wait", ...) */
    lua_pop(L, 1);
  }
}
```

### The interleaving that produces the observed core

Reconstruction — consistent with the core, not independently observed:

1. Cold process. `dshim_ncont == 0`; `dshim_conts` is a static array, so
   every slot is zero, i.e. `.name == NULL`.
2. Threads A and B both enter `addcont("dtask.driver", ...)`. Both read
   `dshim_ncont == 0`, so neither executes the scan loop.
3. Both write slot 0 and both do `dshim_ncont++`. The increment is a
   non-atomic read-modify-write, but even when both increments land,
   `dshim_ncont` becomes 2 while **slot 1 was never written**.
4. Any subsequent call — `addcont("dqueue.wait", ...)`, from either
   thread — scans `i = 0..1` and reaches `strcmp(dshim_conts[1].name, name)`
   with `.name == NULL`.
5. SIGSEGV in `strcmp`, which is frame #0 of the core.

A second, independent route to the same crash needs no lost slot at all:
there is no barrier between the `.name` store and the `dshim_ncont++`, so
another thread may observe the incremented count before the name store is
visible and scan a slot holding `NULL` or a torn pointer.

Both routes are the same defect. Do not spend time deciding which one
fired — the fix must close both, and the core cannot distinguish them.

### The same bug, second copy

`src/dsnap.c:1309-1325`:

```c
static ds_cont ds_conts[DS_MAXCONT];
static int ds_ncont = 0;

LUA_API int diluvium_snap_addcont (const char *name, lua_KFunction k) { ... }
```

Byte-for-byte the same shape. It has not been seen to crash, which means
nothing: it is on the snapshot path rather than the `dv_new` path, so it
is reached less often, not more safely.

### And a racy once-guard on the same path

`src/dsnap.c:1024`:

```c
static void ds_learnconts (lua_State *L) {
  static int done = 0;
  if (done) return;
  done = 1;
  ds_learncont(L, "pcall(...)",  "baselib.pcall");
  ds_learncont(L, "xpcall(...)", "baselib.xpcall");
}
```

Two threads can both see `done == 0` and both run the body, which calls
`diluvium_shim_addcont` — so this widens the window in §*The mechanism*
rather than being a separate concern. It also means the registry is **not**
capped at the two compile-time names: snapshot-capable builds discover
more at runtime.

### Readers are exposed too

`diluvium_shim_contname`, `diluvium_shim_contfunc`, `ds_contname` and
`ds_contfunc` all walk the same arrays with the same absence of
synchronisation, and `ds_contfunc` calls `strlen(ds_conts[i].name)`. A fix
that guards only the writers leaves a reader dereferencing `NULL` on a
concurrently-growing array.

---

## Why it never reproduced, and why the previous conclusions were wrong

This is the part worth internalising, because it invalidates a lot of
apparently strong evidence recorded in DRT's `doc/Release.md`.

**`addcont` only writes when the name is not already present.** Once every
name is registered, every later call matches in the scan loop and returns
early. The array is then read-only for the life of the process.

So the race window is the first few microseconds of the first concurrent
`dv_new` calls in a fresh process, and then it closes permanently.

Consequences, each of which retires a previous finding:

- **"Not concurrent instance lifecycle — 64,800 instances across 24 threads,
  clean."** That stress could not have found this. It samples the window
  exactly once, at startup, and then hammers an immutable array for the
  rest of the run. The conclusion was drawn from evidence with no power to
  reject the hypothesis.
- **"~2,600 runs of that exact test binary, clean."** Each run is one
  sample of a narrow, scheduling-dependent window. Low probability per
  process, and iterations *within* a process contribute nothing.
- **"500 runs of the same binary hash on the runner that crashed, 0
  failures."** Same. More runs of the same shape is the wrong axis; the
  right one is more threads racing into a *cold* process.
- **"Under valgrind, 0 errors from 0 contexts."** Valgrind's default tool
  is memcheck, which does not detect data races. This needed helgrind/DRD
  or ThreadSanitizer. The clean memcheck run was true and irrelevant.
- **"It is not `host_lua`-specific."** Correct, and now explained: any
  test binary that constructs instances from several threads qualifies.

**The general lesson for reproducing this: more iterations do not help,
only more fresh processes with more threads entering `dv_new` at once.**

---

## Constraints the fix has to respect

Established by reading the build, not assumed:

- **C99.** `Makefile:25,30,36` set `-std=c99` for Linux, macOS and Windows.
  `_Atomic`, `<stdatomic.h>` and C11 `threads.h` are therefore off the
  table without a standard bump. A few targets use `-std=gnu99`; the core
  ones do not.
- **pthread is not currently linked.** `PLATFORM_CFLAGS` carries
  `-DLUA_USE_LINUX -Wl,-E -ldl` and no `-lpthread`. On glibc ≥ 2.34 the
  pthread symbols live in libc so this is usually a non-issue, but it is
  not free on every target and must be checked, not assumed.
- **wasm is a real target.** `build_wasm`, `_wasi_static_lib` and
  `_wasm_unknown_build` exist, and `src/wasm_stubs.c:34` calls
  `diluvium_openlibs`. Whatever lands must compile where there are no
  threads at all.
- **Windows is a declared platform** (`-DLUA_USE_WINDOWS`), so a
  POSIX-only fix needs a Windows arm.
- **`dv.h:15-19` is doctrine, and it is about instances:**

  > One instance, one thread. The host must not call any function here for
  > a given instance from more than one thread at a time. This is not a
  > lock we forgot to add; it is the contract.

  Read it carefully before proposing to widen it. It is a **per-instance**
  contract. The registries are **per-process**. Whatever you decide, the
  header currently does not say anything true about process-global state,
  and that gap is how this bug survived review.

---

## Candidate fixes

Ordered by my recommendation. The owner may reasonably pick differently;
what matters is that the choice is made deliberately, since option C is a
contract change and the others are not.

### A. A platform once-guard around registration — recommended

Wrap the registration performed by `diluvium_openlibs` (and
`ds_learnconts`) in a real one-time initialisation primitive:
`pthread_once` on POSIX, `InitOnceExecuteOnce` on Windows, a plain flag
where there are no threads (wasm). Roughly twenty lines behind one macro.

- No ABI change, no burden on any host, fixes every host at once.
- Registration genuinely happens once, so the readers become safe by
  construction after it: the arrays are immutable before any second thread
  can observe them.
- Costs: a small platform-`#ifdef` block in a codebase that has kept the
  core free of them, and possibly `-lpthread` on some targets.

If you take this, also make the readers correct in the pathological case
by skipping `NULL` names in every scan loop — cheap, and it means a future
dynamic registration cannot resurrect the crash silently.

### B. Lock-free append

Reserve a slot with an atomic fetch-add, publish `.name` last with a
release barrier, and have scanners skip slots whose `.name` is `NULL`.

- No platform threading library.
- But it needs atomics, which needs C11 or compiler builtins
  (`__atomic_*` on GCC/Clang, `_Interlocked*` on MSVC) — the same
  portability problem as A, wearing different clothes, and harder to get
  right. Duplicate entries for the same name become possible; benign here
  since lookups take the first match, but it is one more thing to reason
  about.

### C. An explicit `diluvium_init()` the host calls once before threads

Move all process-global registration into a new entry point, and document
that it must be called once before any `dv_new` on any thread.

- Most in keeping with `dv.h`'s stated doctrine, and zero synchronisation
  in the core.
- But it is an **ABI and contract change**: every host, DRT included, must
  be updated, and a host that forgets gets exactly today's crash with no
  diagnostic. It also does not help a host that legitimately creates
  instances concurrently *after* init — it only moves the requirement.
- If chosen, `dv_new` should detect the missing init and refuse by name
  rather than crash.

### What not to do

- **Do not** paper over it by making the scan tolerate `NULL` and stopping
  there. That removes the crash and leaves the lost-update race, so the
  registry can end up missing a name — which surfaces later as a snapshot
  refused for a continuation the process could perfectly well have known.
  That is a much worse bug: silent, and far from its cause.
- **Do not** serialise `dv_new` wholesale with a global lock in the core.
  It would work, and it would quietly make instance creation a contention
  point for every multi-tenant host forever.
- **Do not** raise `DSHIM_MAXCONT`. The array is not full; the count is
  wrong.

---

## Reproducing it

Nothing in the tree reproduces this today, and the acceptance gate should
be a test that does. Both halves are needed: the first proves the fix, the
second proves the test would have caught the bug.

**1. A threaded cold-start check.** New `test/dshim_race_check.c`, in the
style of the existing `test/dshim_check.c` and wired into the `Makefile`
alongside the `dshim_check` target (`Makefile:523`): spawn N threads (N =
cores, minimum 4) that each immediately call `dv_new` on a fresh state,
join, assert every expected continuation name resolves. The value is in
process startup, so the harness must **run the binary many times** rather
than loop inside one process — a shell loop of a few hundred fresh
executions is the shape that matches the failure.

**2. ThreadSanitizer, which is the real gate.** `sanitize_checks`
(`Makefile:417`) builds `-fsanitize=address,undefined` and no more, which
is why four days of sanitizer runs said nothing: **neither ASan nor UBSan
detects data races.** Add a `-fsanitize=thread` lane over the new check.
TSan should report the race on `dshim_ncont` on the *unfixed* tree on
essentially the first run, and nothing after the fix.

Verify in the order this repo's own test files describe: confirm the new
check fails on the current tree first, then fix, then confirm it passes.
A race test that has never been seen red proves nothing.

---

## Definition of done

1. `src/dshim.c` and `src/dsnap.c` registries are safe under concurrent
   `dv_new` — both writers and all four readers.
2. `ds_learnconts`'s `static int done` no longer races.
3. A TSan lane over a threaded cold-start check, demonstrated red before
   the fix and green after.
4. Still builds and passes for wasm, macOS and Windows, since the fix
   touches a path all of them run.
5. `dv.h`'s contract section says something true about process-global
   state — either "the core handles it" (A/B) or "call `diluvium_init`
   first" (C). Today it says nothing, and a host reading it carefully
   still walks into this.
6. `CHANGELOG.md` entry naming the crash, so anyone holding a DRT binary
   built against `f137b30` can tell whether their copy carries the fix.

## What DRT is doing meanwhile

DRT serialises instance *creation* behind a mutex in
`crates/drt-swarm/src/engine.rs` (see `doc/Failure-Modes.md`, FM-2). That
is a mitigation in one host, not a fix: it costs nothing because creation
is rare, and it makes DRT's test suite honest again. It does not protect
any other host.

It is still there, and the condition for removing it is now met: `grep -A2
'name = "diluvium"' Cargo.lock` shows build12p1. It is kept for the release
that moves the pin and goes in the one after, deliberately — removing a
mitigation in the same change that moves the thing it mitigates leaves
nothing to compare against if the crash returns. The comment at the lock's
use site says the same, so a future reader hits it wherever they start.

DRT's shipped exposure was nil either way — `drt run`, `drt start` and
`drt repl` create instances only on the drive-loop thread — so this was a
test-harness crash for us. It would not have been for a multi-threaded
host, and drt-web is one of those waiting to happen.
