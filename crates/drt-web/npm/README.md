# drt-browser

DRT in a page, as one package. Each entry is a plain ES module or a
`.wasm` the module beside it loads.

| Import | What it is |
| --- | --- |
| `drt-browser` | `drt_web.js`, the browser build of drt: `DrtTerm` (and `DrtTerm.repl`, the REPL as a byte stream), the swarm table, the socket and the SSH server (doc/Browser.md). Loads `drt-browser/wasm`. |
| `drt-browser/term` | `attach(DrtTerm, terminal, …)`: an xterm.js terminal as the page's shell and REPL. |
| `drt-browser/access` | The browser access client: `offer`, `direct`, `answer`, `listen`, `canonicalPeer`, with types (doc/BrowserAccess.md, doc/P2P.md). |
| `drt-browser/ssh` | `drt ssh` and `:ssh` in a page, over `drt-browser/ssh-client`. |
| `drt-browser/config.schema.json` | The `drt start` config file's JSON Schema, for a profile editor. |

The version is the drt release it was built from; `BUILDINFO.txt` on that
release says what the build inside carries.
