# drt-browser

DRT in a page, as one package. Each entry is a plain ES module or a
`.wasm` the module beside it loads.

| Import | What it is |
| --- | --- |
| `drt-browser` | `drt_web.js`, the browser build of drt: `DrtTerm` (and `DrtTerm.repl`, the REPL as a byte stream), the swarm table, the socket and the SSH server (doc/Browser.md). Loads `drt-browser/wasm`. |
| `drt-browser/term` | `attach(DrtTerm, terminal, …)`: an xterm.js terminal as the page's shell and REPL. |
| `drt-browser/access` | The browser access client: `offer`, `direct`, `answer`, `listen`, `canonicalPeer`, with types (doc/BrowserAccess.md, doc/P2P.md). |
| `drt-browser/ssh` | `drt ssh` and `:ssh` in a page, over `drt-browser/ssh-client`. |
| `drt-browser/bundle`, `drt-browser/bundle.min` | `drt.js` and `drt.min.js`: the browser build, the browser access client and the SSH client as one ES module with both `.wasm` inlined. Also release assets of their own. |
| `drt-browser/config.schema.json` | The `drt start` config file's JSON Schema, for a profile editor. |

The version is the drt release it was built from; `BUILDINFO.txt` on that
release says what the build inside carries.

## drt.js

One file a page imports from wherever it is served, with nothing beside
it to fetch. The server needs only `content-type: application/javascript`
and, from another origin, `access-control-allow-origin`.

```js
import { call, connect, ssh, kv, session } from 'https://cdn.example/drt@0.8.0/drt.js';

const peer = await call('drt+ssh://signal.example/v1/home/box', { token });
const shell = await ssh(connect(peer), 'SHA256:…');       // pinned host key
await shell.authKey('me', privateOpenssh);

const store = kv('drt://signal.example', 'home', { token });
await store.put('notes/a', 'one');

const { token: jwt } = await session().token();          // stdlib:session, same origin
```

- `call(peer, {token, headers, iceServers, fingerprint})` adds no ICE
  server of its own: none unless the page passes them, and never a relay.
- `loadSsh()` and `loadRuntime()` unpack each wasm module on first use.
- A page with a Content-Security-Policy needs `'wasm-unsafe-eval'` in
  `script-src` for either module.
- `session()` sends the browser's cookie, so the session's `/api/*` must
  be on the page's origin, as a proxy mounts it.

`crates/drt-web/cdn/drt.mjs` lists every export; `e2e.mjs` beside it is
the check every release passes.
