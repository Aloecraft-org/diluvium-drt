# 05-calling-a-rest-api

Outbound HTTP from a program, and the two things that make the grant
interesting: the scope is an **origin allowlist**, and an allowed origin can
carry **headers the connector injects and the program can neither set nor
read**.

That second one is the part worth the example. It means a drt app can call
an authenticated API without the program ever holding the credential.

## Run it

```
cd examples/05-calling-a-rest-api
drt run --config allowlist.json
```

Paths inside a config resolve against the directory you run from, so start
with the `cd`. This command needs no network — every call in `app.dlua` is
refused before anything is connected. The one call that does need a network
is in `live.dlua`, and there is a section about it at the end.

## What you should see

```
  http://api.github.com/zen                error  'http://api.github.com:80' is outside this instance's granted origins
  https://api.github.com:8443/zen          error  'https://api.github.com:8443' is outside this instance's granted origins
  https://api.github.com.example.net/zen   error  'https://api.github.com.example.net:443' is outside this instance's granted origins
  https://api.github.com@evil.example/zen  error  the url carries userinfo, which this connector refuses
  http://169.254.169.254/latest/meta-data/ error  'http://169.254.169.254:80' is outside this instance's granted origins
  rest/put (not rest/get or rest/post)     error  'rest/put' is not a rest call
```

Exits 0, writes nothing, and prints the same thing every time.

## What it teaches

**An unscoped outbound-HTTP grant is not a small thing to hand out.** A
program that holds one can reach the cloud metadata endpoint at
`169.254.169.254`, your database on `10.x`, or the host's own control plane.
That is why `host:rest/*` alone buys nothing here: with no scope the
connector permits no origin at all, rather than every origin.

**The scope is an origin allowlist, and an origin is scheme + host + port.**
`allowlist.json` grants `https://api.github.com`. The first two rows above
are that exact host over `http`, and that exact host and scheme on another
port — two different places, neither of them the one that was granted.

**The refusal is on the name, before anything is resolved.** No DNS lookup
happens for `api.github.com.example.net`, and no socket is opened to
`169.254.169.254`. There is a second check the allowlist cannot make, on the
address a permitted name actually resolves to, because an allowed name that
points into private space is the DNS rebinding shape and it arrives looking
exactly like a legitimate request.

**A URL that reads as the granted origin is not the granted origin.** Three
of the rows are ways of writing a URL that a human skimming a diff, or a log
line, would take for the allowed API: a suffix that ends with it, a userinfo
part that puts it left of the `@`, a different port. Userinfo is refused
rather than quietly dropped, because dropping it would authorise
`evil.example` while the string still said `api.github.com`.

**A refusal is a reply.** `host.try` hands back `value, status, detail`, so
all six rows are printed by the same two lines of code. Which origins exist
is the deployment's decision, not the program's, so a refused URL is not an
exception — it is an answer.

**The program does not know the allowlist.** Nothing in `app.dlua` names
`api.github.com` as granted or names the config at all. It names URLs; the
deployment decides which of them exist. That is what lets the same program be
pointed at a staging API by editing one file.

## The header the program cannot see

Open `allowlist.json` and `live.dlua` side by side. The allow entry carries:

```json
"headers": { "user-agent": "drt-example/0.4.0" },
"allow_headers": ["accept"]
```

`headers` are set by the connector on every request to that origin. The
program cannot set them — it is refused by name if it tries, rather than
having its value silently dropped — and they are not echoed back in the
reply, so it cannot read them either. `allow_headers` is then the exhaustive
list of what the program *may* set.

GitHub's API refuses a request that carries no user-agent, so this is not
decoration: the call does not work without that header, and `live.dlua` does
not contain it. Swap `user-agent` for an `authorization` and nothing in the
program changes. The credential lives in the deployment, on the other side
of the boundary from the code that spends it.

## The half that needs a network

```
drt run live.dlua --config allowlist.json
```

**This does not work in v0.4.0.** A URL the allowlist permits reaches the
part of the connector that opens a socket, and `drt run` drives connectors
without the async runtime that part needs, so the process aborts instead of
answering. Nothing you can put in the program or the config works around it.

It is worth saying which half of this example that leaves standing: all of
it above. Every refusal is decided before a connection is attempted, so the
allowlist, the origin comparison and the injected-header terms in the config
are exactly as shown. What you cannot see today is a successful body coming
back.
