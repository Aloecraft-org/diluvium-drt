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
- `session()` is for a page behind `stdlib:session`; see below.

`crates/drt-web/cdn/drt.mjs` lists every export; `e2e.mjs` beside it is
the check every release passes.

## session(): a page behind stdlib:session

`session()` reads the signed-in user and asks for a JWT, with the cookie
`stdlib:session` set at sign-in. The cookie is `HttpOnly` (a page cannot
read it) and `SameSite=Lax`, and `stdlib:session` sends no CORS headers,
so the browser only sends it, and only lets the page read the answer,
when the request goes to **the page's own origin**. `stdlib:session` is
usually on another host (`auth.example.com`), so the proxy in front of
the app mounts its JSON API and `/logout` on the app's origin:

```
app.example.com {
	@session path /api/* /logout
	handle @session {
		reverse_proxy 127.0.0.1:9091      # stdlib:session
	}
	handle {
		forward_auth 127.0.0.1:9091 {
			uri /verify
		}
		reverse_proxy 127.0.0.1:8080      # the app
	}
}
```

| Call | Request | Answer |
|---|---|---|
| `session().info()` | `GET /api/session` | `{user, groups, amr, signed_in, expires_in, ...}`, or `null` when signed out |
| `session().token()` | `POST /api/token` | `{token, expires_in, token_type}`: an EdDSA JWT, also a topic token for `call` and `kv` |
| `session().logout()` | `POST /logout` | ends the session and revokes the user's JWTs |

`session({ base })` sends the requests to `base` instead of the page's
origin. That works only when `base` is the same site as the page and
answers CORS with credentials, which `stdlib:session` does not, so leave
`base` out unless something in front of it adds them. A refusal is a
`SessionError` carrying `status` (401: not signed in; 404 from `token()`:
the session issues no JWTs, `jwt_key` is unset).

`script/session-browser.sh` runs exactly this setup in Chromium, with
`crates/drt/tests/session-browser/Caddyfile` as the proxy.

