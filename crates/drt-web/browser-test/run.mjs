// The browser half of the suite. Loads the real wasm, drives it through the
// mock host, and posts results where Playwright can read them.
//
// Every assertion here is one a native `cargo test` CANNOT make: they are
// all about the wasm boundary. doc/HostBaseline.md records the one real
// browser-vs-native divergence this project has hit -- Lab's REPL cannot
// answer host.time() because it evaluates on a thread that cannot park --
// and that class of thing is only ever visible from inside a page.
import init, { abiVersion, buildInfo, setPanicHook, Swarm } from "./pkg/drt_web.js";
import { makeHost } from "./host.mjs";

const results = [];
function check(name, fn) {
  try {
    fn();
    results.push({ name, ok: true });
  } catch (e) {
    results.push({ name, ok: false, why: String(e && e.message || e) });
  }
}
function assert(cond, msg) {
  if (!cond) throw new Error(msg || "assertion failed");
}

await init();
setPanicHook();

check("abiVersion answers without a host, and must not throw", () => {
  assert(abiVersion() === 1, `abiVersion() === ${abiVersion()}`);
});

check("buildInfo names what this artifact carries", () => {
  const b = buildInfo();
  assert(b.profile === "web", `profile ${b.profile}`);
  assert(String(b.dvAbi) === "1", `dvAbi ${b.dvAbi}`);
  assert(b.exports.includes("Swarm.new"), "exports should name Swarm.new");
  assert(b.version.length > 0, "version should be set");
});

check("a swarm constructed with no host refuses by name", () => {
  let threw = null;
  try { new Swarm(null, 8, 4); } catch (e) { threw = String(e); }
  assert(threw && threw.includes("host object"), `got ${threw}`);
});

check("a root program runs to completion through the JS host", () => {
  const host = makeHost();
  const sw = new Swarm(host, 8, 4);
  const id = sw.root("print('hi')", [], {});
  assert(typeof id === "number", "root should return an id");
  assert(sw.alive() === 1, `alive ${sw.alive()}`);
  assert(sw.ids().length === 1, "one on the roster");
  let guard = 0;
  while (sw.alive() > 0 && guard++ < 50) sw.step();
  assert(sw.alive() === 0, "the program should finish");
  assert(host.released.length >= 1, "the JS table should have been released");
});

check("caps and holds cross the boundary", () => {
  const host = makeHost();
  const sw = new Swarm(host, 8, 4);
  const id = sw.root("x", [{ capability: "host:time" }], { instructions: 1000 });
  assert(sw.holds(id, "host:time"), "should hold host:time");
  assert(!sw.holds(id, "host:fs/read"), "should not hold host:fs/read");
  const b = sw.budget(id);
  assert(b && Number(b.instructions) === 1000, `budget ${JSON.stringify(b)}`);
});

check("a JS host that throws becomes a Rust error, not a crash", () => {
  const sw = new Swarm(makeHost({ failLoad: true }), 8, 4);
  let threw = null;
  try { sw.root("x", [], {}); } catch (e) { threw = String(e); }
  assert(threw && threw.includes("rejected"), `got ${threw}`);
  // The module must still be usable: a thrown host error is the guest's
  // problem, not the engine's.
  assert(abiVersion() === 1, "the module should still answer");
});

// KEEP THIS LAST, and read the comment before changing it.
//
// This test has now been wrong TWICE, in opposite directions, and each time
// running it in a browser is what settled it. That history is the argument
// for the browser suite existing, so it is written down rather than tidied
// away:
//
//   1. It first asserted doc/Browser.md's claim -- that `guard` converts a
//      panic into a thrown JS error. It failed. wasm32-unknown-unknown is
//      panic="abort" (the TARGET's default on stable, not a profile
//      choice), and catch_unwind cannot catch an abort, so `guard` never
//      runs on a panic.
//   2. It was then rewritten to assert the module dies. It failed too. A
//      wasm trap terminates the current CALL and throws `RuntimeError:
//      unreachable` into JS; the instance is still there and later calls
//      still answer.
//
// So what is actually true, and what matters: a panic IS catchable from JS,
// but nothing ran on the way out -- no unwinding, no Drop, no cleanup. The
// module keeps answering while Rust's invariants may be broken, and a JS
// caller who catches and carries on is using state nothing repaired. That
// is more dangerous than a clean death, because it looks fine.
//
// Hence the discipline in exports.rs: exports must not panic. `guard` is
// not what keeps a page alive, and no export may rely on it.
check("a panic traps into JS, and the module keeps answering (which is the hazard)", () => {
  const sw = new Swarm(makeHost(), 8, 4);
  let threw = null;
  try { sw.__panicForTests(); } catch (e) { threw = String(e); }
  assert(threw, "a panicking export must surface something to JS");
  // The trap, not guard's message. If this ever reads as guard's wording,
  // unwinding has arrived on this target and the note above needs revisiting.
  assert(
    threw.includes("unreachable"),
    `expected a wasm trap ("unreachable"), got: ${threw}`
  );
  assert(
    !threw.includes("no longer trustworthy"),
    "guard appears to have caught a panic; exports.rs's panic note is now stale"
  );
  // Pinned because it is the surprising half: the instance survives, so a
  // caller cannot tell from the outside that anything is wrong.
  assert(abiVersion() === 1, "the module stopped answering after a trap");
});

document.getElementById("out").textContent = JSON.stringify(results, null, 2);
window.__drtResults = results;
