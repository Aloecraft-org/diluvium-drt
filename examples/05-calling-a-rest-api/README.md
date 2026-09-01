# 05-calling-a-rest-api

Outbound HTTP, and the grant that permits it. The `rest` connector's scope is
an **origin allowlist**: the deployment names the origins that exist for this
program, and every other URL is refused by name, before an address is looked
up or a socket is opened.

## Run it

```
cd examples/05-calling-a-rest-api
drt run --config allowlist.json
```

Paths inside a config resolve against the directory you run from, so start
with the `cd`. Nothing here touches the network. The call that does connect is
next door, in `05-calling-a-rest-api-live`.

## What you should see

```
  http://api.github.com/zen                error  'http://api.github.com:80' is outside this instance's granted origins
  https://api.github.com:8443/zen          error  'https://api.github.com:8443' is outside this instance's granted origins
  https://api.github.com.example.net/zen   error  'https://api.github.com.example.net:443' is outside this instance's granted origins
  https://api.github.com@evil.example/zen  error  the url carries userinfo, which this connector refuses
  http://169.254.169.254/latest/meta-data/ error  'http://169.254.169.254:80' is outside this instance's granted origins
```

Exits 0, writes nothing, prints the same thing every time.

## What it teaches

**An origin is scheme + host + port, all three.** `allowlist.json` grants
`https://api.github.com`. The first two rows are that exact host over `http`,
then that exact host and scheme on another port — two other places.

**A URL that reads as the granted origin is not the granted origin.** A name
that merely ends with it, and a userinfo part that puts it left of the `@`,
both skim as the allowed API in a diff or a log line. Userinfo is refused
rather than silently dropped: dropping it would send the request to
`evil.example` while the string still said `api.github.com`.

**Which is why an unscoped outbound grant is not a small thing to hand out.**
A program holding one reaches the cloud metadata endpoint on the last row,
your database on `10.x`, your own control plane. So the capability alone buys
nothing here: a `rest` entry with no scope is refused when the app starts,
rather than read as every origin.

**A refusal is a reply, not an exception.** `host.try` hands back
`value, status, detail`, so all five rows go through the same two lines — the
two that would have printed a reply.

**The program does not know the allowlist.** Nothing in `app.dlua` names
`api.github.com` as granted, or names the config at all. It names URLs; the
deployment decides which of them exist. That is what lets the same program be
aimed at a staging API by editing one file.

`05-calling-a-rest-api-live` is the call that is permitted, and the header the
deployment injects into it.
