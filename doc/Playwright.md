# The browser capability, backed by Playwright: an assessment

**Status:** assessment, written 2026-09-03 against v0.4.2 on
`claude/hostcall-plugin-design-wci0fu`, with the deferred pump read off
`claude/drt-wasm-port-planning-4ua6qk` (M3, `7cb85b5`). Nothing here is
built. Every claim about the tree names the file it rests on; the sizes
are estimates and say so.

**The ask.** A hostcall family that lets a program drive a real browser:
open a page, read it, click and type in it, take a screenshot. "Playwright"
names the backing a deployment would run, not the capability -- the same
split as `rest` (the capability) over a TLS stack (the backing), and the
family is named `browser/*` below for that reason.

---

## 1. The verdict, in three sentences

Build it as the first **plugin**, not as a connector compiled into `drt`:
a Node program that owns a Playwright browser and answers `browser/*`
frames over the channel the C host already specified, wired by a
manifest and a `plugins` block. The plugin channel is the piece DRT does
not have yet, and this capability is the argument for building it: a
browser wants Node, Playwright and a Chromium install on the box, none of
which belongs in a static binary. Under the deferred pump the call shape
is already right; under today's pump a browser call would stall every
guest for the length of a page load, exactly as `rest` and `exec` do.

## 2. What is already in the tree to build on

Verified by reading, not inferred.

- **The connector seam needs nothing new.** `Connector::call` takes the
  call name, `args` as msgpack, and the scope, and answers a value or a
  sentence (`crates/drt-connector/src/lib.rs`). A plugin backing is one
  more `impl Connector`; the dispatcher's gating, token echo and
  answered-always guarantee cover it unchanged.
- **The deferred pump exists, on the wasm branch.** `Dispatcher::route`
  hands back a `PendingCall` that owns everything the connector needs,
  and `drt_swarm::pump::Pump` polls its future once per pump with a no-op
  waker, parking it in an in-flight table until it is ready. A reply owed
  to a dead instance is dropped; one owed to a hibernated instance is
  held. A future that checks a channel and answers `Pending` when nothing
  has arrived is all a subprocess-backed connector needs, because the loop
  itself is the waker.
- **The subprocess discipline exists: `connectors/exec`.** Spawn into its
  own process group, deadline the call, cap the bytes, sweep the group on
  every exit path, answer 127 for a program that is not there. A plugin
  host process wants the same bounds, once, at its own spawn.
- **The wire protocol exists, upstream.** `doc/BUILD8.md` §2 in
  diluvium: exec an absolute path, hand it a socketpair as fd 3, speak
  `u32` big-endian length-prefixed msgpack frames. A request is
  `{version, id, target, args}`; a reply is `{version, id, final, value}`
  or `{version, id, final, error: {class, code, message}}`. The manifest
  is `<name>.plugin.json`: flat metadata read, schemas skipped, a `wake`
  policy per capability, `max_inflight`, `call_timeout_ms`.
  `plugins/rest/rest_plugin.mjs` is a Node plugin speaking exactly this
  on fd 3 with a dependency-free msgpack in about a hundred lines, and a
  browser plugin starts by copying that file.
- **Chromium is already in CI.** The wasm branch's `browser` job installs
  node 22 and runs `npx playwright install --with-deps chromium`;
  `playwright-core` 1.62.1 sits under `crates/drt-web/browser-test`. A
  browser plugin's test has the same ingredients.
- **The verb set has a reference.** playwright-core 1.62.1 ships
  `playwright-cli` (`lib/tools/cli-client/help.json`): `open`, `goto`,
  `snapshot`, `find`, `click`, `fill`, `type`, `press`, `select`, `hover`,
  `eval`, `screenshot`, `pdf`, `tab-*`, `state-load`, `state-save`,
  `cookie-*`, `requests`, `route`, `console`. That is the surface an
  automation agent uses today, and `snapshot` -- the ARIA tree as text
  with element refs -- is the primitive that makes a page readable to a
  program without shipping it the HTML.
- **`deny` grants exist.** `drt-caps` carries `Effect::Deny`, so
  "everything in the family except `eval`" is one config line today:
  grant `host:browser/*`, deny `host:browser/eval`.

## 3. The shape

### 3.1 The family

`browser/*`, one plugin process per deployment, sessions inside it.

| call | args | answer | wake |
|---|---|---|---|
| `browser/open` | `{url}` | `{session, url, title}` | reissue |
| `browser/goto` | `{session, url}` | `{url, title}` | error |
| `browser/snapshot` | `{session, target?}` | `{text}` -- the ARIA snapshot with refs | reissue |
| `browser/find` | `{session, text}` | `{refs}` | reissue |
| `browser/click` | `{session, ref \| selector}` | `{}` | error |
| `browser/fill` | `{session, ref \| selector, text}` | `{}` | error |
| `browser/type` / `browser/press` | `{session, text}` / `{session, key}` | `{}` | error |
| `browser/select` | `{session, ref \| selector, value}` | `{}` | error |
| `browser/text` | `{session, selector?}` | `{text}` | reissue |
| `browser/screenshot` | `{session, full_page?, format?}` | `{bytes}` as msgpack `bin` | reissue |
| `browser/eval` | `{session, js}` | `{value}` | error |
| `browser/wait` | `{session, selector \| text \| ms}` | `{}` | reissue |
| `browser/close` | `{session}` | `{}` | reissue |

`session` is a small integer the plugin mints, valid for the process's
life and for the instance that opened it. `ref` is the element handle a
snapshot named. `wake` is the manifest's policy for a call in flight when
its instance hibernates: reads are safe to ask twice, actions are not,
and a program is told an action was lost rather than having it repeated.

Not in the first cut, named so the omission is a decision: `pdf`,
`upload`, `drag`, downloads, tabs beyond one page per session, video and
tracing, the cookie and storage verbs (the scope owns those, §3.2), and
`route` (the scope owns that too).

### 3.2 The scope, which is the whole of the safety story

Every bound is the deployment's. The plugin has nowhere to put a scope
under the C protocol, so the scope travels in the `plugins` block and the
plugin reads it once at startup (the hello frame `doc/Plugins.md`'s
design would add; until then, arguments on the exec line).

```json
"plugins": {
  "browser": {
    "manifest": "browser.plugin.json",
    "scope": {
      "allow": ["https://example.com", "https://*.example.org"],
      "state_file": "auth.json",
      "max_timeout_ms": 30000,
      "max_bytes": 524288,
      "max_sessions": 4,
      "headless": true
    }
  }
}
```

- **`allow` is an origin allowlist enforced per request, not per call.**
  `rest` checks the URL a program named; a browser fetches subresources,
  follows redirects and loads iframes the program never named, so the
  check has to sit where every request passes: `context.route('**/*')`,
  aborting anything outside the list. That is a stronger control than
  `rest`'s, and one honest gap: the plugin sees names, not resolved
  addresses, so a DNS rebinding into private space is not caught the way
  `rest`'s second check catches it. The mitigation is the deployment's
  network, not the plugin's: run the plugin where private ranges are
  unreachable, or behind a proxy that refuses them.
- **`state_file` is the credential.** Playwright's storage state --
  cookies, local storage, an authenticated session -- loaded by the plugin
  and never readable by the guest, the way `rest`'s injected headers and
  `ssmtp`'s relay password are. A program acts as the deployment's login
  without holding it. No `cookie-set` verb for the guest, for the same
  reason `Reply-To` is the `ssmtp` scope's.
- **`max_timeout_ms`** is a ceiling on every call's wall clock; a call may
  ask for less. **`max_bytes`** caps a screenshot, a snapshot and a text
  read, refusing rather than truncating past it -- and it must sit well
  under the instance's `memory_kb`, because a reply lands on the guest's
  heap (§4.3). **`max_sessions`** bounds pages per plugin. **`headless`**
  defaults to true and is the only mode the first cut supports.
- **`eval` is its own grant.** Arbitrary JavaScript in the page is a
  different power from clicking in it; a deployment grants
  `host:browser/*` and denies `host:browser/eval` unless it means it.

### 3.3 Where it runs, per target

Native only, like `exec`. The plugin is a process; wasip2 has no
subprocess and no threads, and a page cannot drive a second browser. The
`web` and `wasi` profiles say the family is absent, and a program asking
gets `denied` by name, which is the baseline rule.

## 4. What it costs, and where the unknowns are

### 4.1 The two routes

| | plugin: Node + Playwright, out of process | connector: Rust over the DevTools protocol, in process |
|---|---|---|
| what runs the browser | Playwright 1.62, the real thing: auto-waiting, the ARIA snapshot, three browser engines | `chromiumoxide` 0.9.1 (2026-02) or `headless_chrome` 1.0.22 (2026-06); CDP only, Chromium only; the snapshot formatting is yours to write |
| what the box needs | node, `npx playwright install --with-deps chromium`, the plugin file at an absolute path | a Chromium binary, and a CDP client plus its async runtime linked into `full` |
| what DRT needs first | the plugin channel (`doc/Plugins.md`) | nothing new |
| the `playwright` crate | not a route: `playwright` 0.0.20, last published 2022-08, unmaintained | -- |
| verdict | **this one** | a fallback if Node on the box is unacceptable; a smaller, worse browser and a week more work to reach the same surface |

### 4.2 Sizing, plugin route

| piece | size | rests on |
|---|---|---|
| the plugin channel: manifest types, frame codec, `ProcessChannel`, `PluginConnector` | ~3 days | `doc/Plugins.md`; the `exec` spawn discipline; BUILD8 §2 |
| its in-flight integration: wire id to `(instance, tok)`, `max_inflight`, deadlines, the `wake` policy | ~1-2 days, **after M3 merges** | `Pump`'s in-flight table on the wasm branch |
| the Node plugin: `browser/*` over Playwright, the session table, `route`-enforced allowlist, storage state, caps, teardown | ~2-3 days | `rest_plugin.mjs`'s framing, copied |
| conformance: the same guest against a mock backing and the real plugin; a Chromium run in CI | ~1 day | the `browser` job's node and Chromium |
| an example (`17-browser`, against a page the example serves itself, so it needs no network), docs, `capabilities/list` entries | ~1-2 days | `examples/run-all.sh` |

Roughly two weeks to a shipped capability, of which the channel is the
half that every later plugin reuses. With the channel already built, a
browser plugin is a week.

### 4.3 The hazards, stated rather than discovered

- **It leaves the sandbox, twice.** A browser runs any site's JavaScript
  and holds hundreds of megabytes; the instruction budget reaches none of
  it. The bounds are the scope's and the wall clock's, and GUARANTEES.md's
  `exec` paragraph applies to it verbatim.
- **A reply lands on the guest's heap, and the queue has no byte cap of
  its own.** `capacity` counts messages (`src/dqueue.c`); a message is
  bytes on the instance's heap under `memory_kb`. A screenshot bigger than
  what is left cannot be pushed, and the deferred pump keeps a reply it
  cannot land, retrying every tick -- forever, for a reply that will never
  fit. So `max_bytes` is not a convenience: it must default low
  (512 KiB), screenshots default to JPEG, and the pump wants to tell a
  push that *cannot* fit from one that does not fit *yet*, which is a
  small change worth making before this ships.
- **Sessions outlive calls.** `sql` was the first connector with state
  across hostcalls and it needed `Connector::finish` to say what was lost
  at teardown. A browser session held by an instance that died should be
  closed when the instance dies, not at process end; the wasm branch adds
  `SwarmHost::released`, and the plugin channel is where that fact reaches
  a plugin. A session that survives a hibernate-and-restore in another
  process is stale by construction and is refused by name.
- **A call takes seconds.** Under the current pump every guest stalls for
  it (`doc/Failure-Modes.md` names the pattern for `rest`). Under M3's
  pump the call is parked and the swarm keeps stepping. This capability
  should not ship on the blocking pump.
- **Replay does not re-drive the browser.** The reply is a message, logged
  and replayed, as with `exec` and `time`.
- **It is not deterministic.** Two runs of the same program see two pages.
  `snapshot` is the most stable read a program can make; `html` and
  `screenshot` are the least, and the example should teach the former.

## 5. Decisions this needs

1. **The plugin channel first, or a purpose-built sidecar?** A
   `BrowserConnector` that spawns Node and speaks frames is four-fifths of
   the generic channel with the reuse removed. Recommendation: the
   channel, then this as its first plugin and its acceptance test.
2. **Scope delivery.** The C protocol has no scope; the plan in
   `doc/Plugins.md` proposes a hello frame. Until it exists, the scope goes
   to the plugin as an argument on its exec line, which is honest and ugly.
3. **`eval`.** Ship it as its own grant, or leave it out of the first cut
   and add it when a program needs it. Recommendation: ship it denied by
   example, so the config line that grants it is a deliberate act.
4. **The allowlist's DNS gap.** Accept it with the deployment-network
   mitigation stated, or refuse private-range destinations at the plugin
   by resolving before each navigation. The second is partial (a page's
   own subresources resolve in the browser) and should not be sold as
   more than it is.

## 6. Not this document's

The plugin channel's own design is `doc/Plugins.md`. The Lab's browser
tier (`doc/Browser.md`) is a different thing with a colliding name: that
is DRT running *in* a page, this is DRT driving one.
