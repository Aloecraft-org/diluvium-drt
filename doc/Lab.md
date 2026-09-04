# The Lab, and where DRT fits into it

*(Not diluvium's `doc/Lab.md`, which is a different document in a
different repository. This one is DRT's view outward.)*

**Status:** assessment, 2026-09-04, from reading
`Aloecraft-org/diluvium-lab` at `aa0b39c` (build10 vocabulary, pushed
2026-09-03). Written from DRT's side about a repository DRT does not own:
the Lab's own `ROADMAP.md` is the authority on the Lab, and where this
document and that one disagree, that one is right. Every claim here is
from the tree at that commit, and each is cited so the next reader can
check whether it still holds.

The occasion was SSH into a browser landing end to end
(`doc/SshInBrowser.md`), and the question it raised: what does that give
the Lab, as the Lab actually is today?

---

## 1. The verdict

**Additive, not a migration.** A terminal tool beside the Lab's notebook
kernel is worth building now and touches nothing load-bearing. Replacing
the Lab's kernel with DRT is worth building *later* and would be a
capability regression today (§3).

The two are independent and should stay that way. Coupling them makes a
demo that is ready now wait on work measured in months, and buys nothing
in return.

## 2. What the Lab is, as of this reading

Not what DRT's older documents describe, so this is worth stating plainly.

The Lab is a notebook front end that runs **the C core directly**:
`libdiluvium_wasi.wasm` and `diluvium_swarm_wasi.wasm` from the release
mirror, in the tab, through a WASI shim the page supplies
(`src/kernel/wasi.js`). It pins `v5.5.1_build10`, commit `7dfe1d59`
(`vendor/PINNED_TAG`, `vendor/BUILDINFO.txt`).

**The host is JavaScript.** `src/kernel/swarm.js` is 1351 lines and its
own header calls it what it is: *"`doc/Host.md`'s seven duties, in a
browser... One of them exists in C and this is the other one."* Around it,
`connectors.js` (698 lines), `lua-harness.js` (1024), `wasm-kernel.js`
(1039), `sqlite.js` (553), `instance.js` (314), `topology.js` (243) —
about 7,100 lines of `src/kernel` in total. `src/dvs_shim.c` inverts the
vtable problem so the whole `dvs_` surface is callable from JS.

**There is a seam for a second backend, and it is deliberate.**
`src/kernel/kernel.js`: *"Everything reaches the kernel through this, even
though exactly one implementation exists today. That is deliberate and it
is the single highest-leverage structural choice in the project."* Its
`capabilities` object already carries `instances` and `swarm` as
feature-detected facts, and `swarmCapable(exports)` is where a build is
recognised.

**There is a rail for a second tool.** `src/notebook/panel.js` is a
`ToolPanel` with registered tools; `src/app.js` registers two, `outline`
and `swarm`. A third is the existing extension point rather than a new
mechanism.

**There is no terminal.** `src/notebook/console.js` is a notebook console.
Nothing in the tree references xterm.js.

## 3. The inversion: the Lab is ahead on connectors

This is the finding that decides the shape of everything below.

| | Lab (`connectors.js`) | DRT `web` |
|---|---|---|
| `time` | yes | yes |
| `crypto` | yes | yes |
| `fs` | — | yes (`MemFs`) |
| `sql` | yes, over vendored sql.js, with a granted scope | no |
| `rest` | yes, over `fetch`, prefix allowlist | no |
| `js/invoke` | **yes** | **no** |
| random | `rng/int`, `rng/bytes` | `crypto/random` |
| listener | yes (`Listener`) | no (a page has no socket to bind) |

`jsConnector(invoke)` answers `js/invoke` — a guest calling a JavaScript
function the deployment registered. That is the question this work started
from ("does `exec` mean anything in the browser; can we run JavaScript"),
and the Lab answered it some time ago, in its own host. DRT's `web`
profile has no equivalent, and `GUARANTEES.md` now says so in as many
words: *no connector in this build can execute script or reach the
document*.

So the Lab's JavaScript host is not a stopgap waiting for DRT. On the
connector axis it is ahead, and **swapping the Lab's kernel for DRT today
would lose `sql`, `rest` and `js/invoke`.** The path that closes that gap
is `doc/Plugins.md`, not this document.

The `random` row is a different kind of finding and the more important
one. A guest wanting random bytes writes `rng/bytes` on one host and
`crypto/random` on the other: the same program does not run on both. That
is the *"sorry, that's different in Lab"* failure the browser-first plan
existed to prevent, it has already happened in miniature, and it went
unnoticed because nothing compares the two hosts. Which spelling is
canonical is `doc/Host.md`'s to say and is not settled here; what is
settled is that a divergence of exactly this shape survived in both trees
until somebody read them side by side.

## 4. Three corrections to DRT's own documents

Named here because planning around a stale fact is how a project spends a
month on the wrong thing.

- **`doc/Wasm.md`: "the Lab has not moved since…"** — it has, a great
  deal. Build10 alone added deferred hostcall answers, response headers,
  a granted `sql` scope, the `host` library in notebooks, and outbound
  HTTP.
- **`doc/HostBaseline.md`: Lab's REPL cannot answer `host.time()`.** That
  was recorded as *the* browser-side failure, and the mechanism it blamed
  — nothing to yield to — is exactly what build10's deferred seam
  provides (`swarm.js` carries `DEFERRED`, `_defer`, and
  `{status: 'pending'}`). **This needs re-measuring before it is cited
  again.** Much of the browser-first urgency rested on it.
- **`README.md` in the Lab says build7; `vendor/PINNED_TAG` says
  build10.** Not DRT's to fix, and a useful illustration of DRT's own
  rule: read the compatibility fact off the artifact, never off prose.

## 5. Track A: a terminal tool, now

The rail's third tool, over `drt_web.tar.gz`. It sits beside the notebook
kernel, shares nothing with it, and needs no change to `kernel.js`.

What it takes: vendor the tarball the way `sql-wasm.js` is vendored, add
xterm.js, and call `attach(DrtTerm, terminal, { DrtEditor })`. The Lab's
"no framework, no bundler, no CDN" rule is satisfied unchanged — the
tarball is plain ES modules and a `.wasm`.

What it gives: a `$ ` prompt running `drt run`, `drt repl` and `drt
buildinfo`, with history, word motions, undo and Tab completing from the
running instance's own namespace (`doc/Wasm.md` D8, M8).

Then the toggle. `relay-leg.js` parks a leg on a rendezvous relay and
`DrtSshServer` serves the claim, so a standard `ssh` client reaches the
tab through `drt tunnel` (`doc/SshInBrowser.md` §5). The panel shows the
host key's fingerprint, takes the authorized keys, and hands back the
`ssh -o ProxyCommand=…` line to copy.

**The Lab's own doctrine already shapes this correctly**, which is the
argument for building it there rather than inventing a posture. From
`src/notebook/remote.js`: *"The Lab makes no request at load — a hard
constraint... Every fetch here is something somebody pressed."* A
Reachable toggle inherits that: off by default, one press, naming the
relay host it would talk to, with the same confirmation pattern the Open
URL flow already uses. That is the whole of "make it hard to do something
stupid by accident", already written down and already enforced.

## 6. The best reason to do it is not the demo

Two independent hosts, in one page, running the same program.

Guest indistinguishability is the property both projects rest on —
`doc/Hostcall.md` calls it a feature the lab workflow depends on — and
today it is asserted rather than checked. The Lab's host is JavaScript;
DRT's is Rust; they were written from the same `doc/Host.md` by different
hands. A **"run this cell in the terminal too"** button compares them on
every cell somebody writes, and costs almost nothing once the terminal is
there.

That is a stronger conformance test than either repository currently has,
and it is only available because the two hosts are different. It is a
reason to keep them both for a while rather than to hurry the merge.

§3's `rng/bytes` versus `crypto/random` is the argument in one line: a
divergence in the guest-visible surface, sitting in two repositories,
found by reading rather than by running. The button would have caught it
the first time anybody asked a guest for a random number.

## 7. Versions: two axes, and the fix is to show both

The question that prompted this work ("how do we handle versions between
diluvium and DRT") has a concrete answer once both are in one page.

The Lab's Runtime dropdown picks a **diluvium** build from the mirror
(`releases.js`, `vendor/PINNED_TAG`). DRT pins a diluvium revision and
records it in `BUILDINFO` (`doc/Release.md`: the compatibility fact
travels with the bytes). Those are two axes and they *will* differ — as of
this reading the Lab runs `7dfe1d59` and DRT embeds `f4d52516`.

Do not reconcile them yet. **Show both**, so a divergence is a thing on
the screen rather than a thing in an argument: the terminal reports `drt
0.5.0 · diluvium f4d5251`, the notebook reports `5.5.1_build10`, and
they are allowed to disagree. Reconciling is a decision that gets easier
after it is visible, and impossible to size before.

## 8. Track B: `DrtKernel`, later

A second `Kernel` implementation, which is what `kernel.js` was built for.
It is the end state — one host, not two — and it is blocked on things
neither cheap nor close:

- **The connector gap in §3.** `sql`, `rest`, and a `js/invoke`
  equivalent. `doc/Plugins.md` is the path for the last one.
- **The version axes in §7**, which stop being separable the moment DRT
  *is* the kernel.
- **Main thread versus worker.** The Lab has a worker kernel mode
  (`worker-kernel.js`, `kernel-worker.js`); `drt_web` runs on whichever
  thread it is given, with `wasm_bindgen_futures`. A worker has
  `WebSocket`, so both should work. Neither has been tried.

Open Track B when §3 closes, and not before: until then it trades
capability for uniformity, which is the wrong direction.

## 9. Costs, measured where they could be

- **`drt_web_bg.wasm` is 3,436,485 bytes** (`release-small`, 2026-09-04),
  of which the SSH server is 1,521,413 — it was 1,915,072 before. Gzipped,
  1,089,348.
- **The Lab's `npm run bake`** produces one self-contained file the README
  puts at ~1.5 MB. Adding the terminal takes that to roughly 5 MB. Either
  exclude the terminal from the bake or accept the number; it is a
  decision, not a discovery.
- **xterm.js is a new vendored dependency** (~250 KB minified). The Lab
  has no terminal emulator today.
- **"Run commands" over SSH means `drt` commands**, not shell commands. A
  page cannot exec, and `GUARANTEES.md` says the session's reach is
  whatever the page's shell exposes. Worth saying before a demo rather
  than after.

## 10. What I would do, in order

1. **Settle `rng/*` versus `crypto/random`** (§3). It is one call, it is
   a real divergence today, and it is the cheapest possible proof that
   the comparison in §6 is worth having.
2. **Re-measure `doc/HostBaseline.md`'s claim** against build10. One
   afternoon, and it decides how much of the browser-first argument still
   applies.
3. **The terminal tool** (§5, first half). Additive, demoable, no kernel
   changes.
4. **The Reachable toggle** (§5, second half). The demo that was named at
   the start of the wasm work.
5. **The cross-check button** (§6). The cheapest strong test either
   project can buy.
6. **Track B** (§8), when and only when §3 closes.
