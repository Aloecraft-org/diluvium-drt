# The browser suite

Real Chromium, the real release `.wasm`, a mock JS host.

```sh
npm install                 # playwright (the browser is preinstalled)
npm run build               # cargo build --target wasm32 + wasm-bindgen
npm test                    # drives it in Chromium
```

`CHROMIUM_PATH` overrides the browser binary; the default is this
environment's preinstalled one, because the npm package's pinned build and
what is on disk do not always match and downloading a second browser to run
seven assertions is not worth it.

## Why a browser and not node

`doc/Release.md`'s stated trigger for the wasm32 release leg is a **node**
smoke step asserting `abiVersion() === 1`. That bar is too low, and this
suite is the evidence: **two of its seven assertions were wrong before they
were run**, in opposite directions, and only a browser settled them. Both
concerned what a Rust panic does at the wasm boundary — see the comment on
the last test, which is kept deliberately long.

The project has also already been bitten once by a divergence node cannot
see: `doc/HostBaseline.md` records that Lab's REPL cannot answer
`host.time()` at all, because it evaluates on a thread that cannot park, so
the queue round-trip has nowhere to yield. Node's event loop is not the
browser's. The browser is the thing being shipped, so the browser is what
runs the test.

## What it does and does not prove

**Does:** that the export layer marshals, that a JS host object drives a
real `Swarm` through a full root-program lifecycle, that a throwing JS host
becomes a Rust `Err` rather than a crash, that caps and budgets cross
intact, and what a panic actually does.

**Does not:** that diluvium interprets anything. `host.mjs` is a mock, the
browser twin of `tests/bridge.rs`'s `MockBridge`, and for the same reason —
what needs proving in a page is the boundary, not the interpreter. Swapping
it for diluvium's real JS binding (`bindings/js` in that repo) is a
one-line change at the call site and is the next slice.

**Also does not:** run a guest hostcall. The connector/pump layer is the
third piece of task #31 and does not exist, so a program can run, park and
be driven in a page but cannot reach `host.fs` or `host.time`.
