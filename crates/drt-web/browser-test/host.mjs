// A JS host implementing doc/Browser.md's fifteen functions, faking the
// diluvium instance.
//
// This is the browser twin of `tests/bridge.rs`'s MockBridge and it exists
// for the same reason: what needs proving in a page is the BOUNDARY -- that
// wasm-bindgen marshals these arguments, that a thrown JS error becomes a
// Rust Err, that a Rust panic becomes a catchable JS throw -- not that the
// interpreter interprets. Swapping this for diluvium's real JS binding is a
// one-line change at the call site, and is what the next slice does.
//
// The behaviour it fakes is deliberately the smallest thing that exercises
// a full lifecycle: a program runs once, parks on nothing, and is done.
export function makeHost({ failLoad = false } = {}) {
  let next = 1;
  const live = new Map();
  const released = [];
  return {
    released,
    // --- Engine ---
    abiVersion: () => 1,
    load(program, name, budget, unsafeStdlib) {
      if (failLoad) throw new Error("the program was rejected");
      const h = next++;
      live.set(h, { program, name, budget, unsafeStdlib, ran: false });
      return h;
    },
    restore(bytes, hostStamp, budget, unsafeStdlib) {
      const h = next++;
      live.set(h, { restored: true, budget, unsafeStdlib, ran: false });
      return h;
    },
    release(h) {
      released.push(h);
      live.delete(h);
    },
    // --- Instance ---
    queue: (h, name) => (name === "in" ? 1 : name === "out" ? 2 : null),
    queueInfo: (h, q) => ({ len: 0, capacity: 8, enabled: true, exported: true }),
    push: (h, q, bytes) => "accepted",
    pop: (h, q) => null,
    run(h) {
      const inst = live.get(h);
      if (inst) inst.ran = true;
      return { done: true };
    },
    resume: (h, fired) => ({ done: true }),
    resumeTimeout: (h) => ({ done: true }),
    currentWait: (h) => null,
    usage: (h) => ({ instructions: 42, memoryKbPeak: 7, bytesNow: 128 }),
    exceeded: (h) => false,
    snapshot: (h, hostStamp) => new Uint8Array([1, 2, 3]),
    // --- Host ---
    drive(id, h) {
      const inst = live.get(h);
      if (!inst) return "no such instance";
      if (!inst.ran) {
        inst.ran = true;
        return "alive";
      }
      return "exited";
    },
  };
}
