# Sessions: `stdlib:session`

A login portal and forward-auth endpoint, written in dlua on four
connectors: `ldap` checks the password, `crypto` checks a TOTP code and
signs tokens, `rest` asks a webhook, and `kv` keeps the sessions in one
topic of a `drt p2p --match --kv --topics` server. Passwords stay in the
directory (LLDAP, OpenLDAP); nothing here stores or hashes one. It runs
under `drt` only: the plain `diluvium` binary answers none of those
connectors.

```sh
drt p2p --match 9100 --kv /var/lib/drt-kv --topics topics.json   # sessions live here
drt start --config session.json                                  # entry: stdlib:session
```

`crates/drt/tests/session-browser/session.json` is a whole config, with
`@PORT@` names its script fills in: the grants, the four connectors, and the listener's header lists, which must
include `cookie` and the `x-forwarded-*` names in, and `set-cookie`,
`location` and the `remote-*` names out. The kv scope's token needs
`read` and `write` in the topic.

## Signing in

`POST /login` binds the user. A wrong password and an unknown user look
alike to the browser. The password opens a *pending*
session (`pending_ttl`), and the second factor turns it into a signed-in
one under a new cookie (`session_ttl`, from sign-in; there is no idle
timeout). The cookie is `HttpOnly; SameSite=Lax`, `Secure` unless
`cookie_secure` is false, and `Domain=cookie_domain` when set. `rd` is
followed only to a relative path or to a host in `cookie_domain`.

- **TOTP.** A user with TOTP on file enters a code. Each code works once,
  across replicas, because the last step used is written back to kv by
  compare-and-set. Enrolment, for a user with none on file, follows
  `totp_enroll`: `off` (never here), `first_login` (whoever has the
  password, once), or `webhook` (after the webhook approves). The page
  shows the secret and the `otpauth://` URI. There is no QR code. To
  reset a user, delete `totp/<user>` from the topic. The next enrolment
  makes a new seed.
- **Webhook.** The program POSTs JSON `{event, challenge, purpose, user,
  name, email, groups, remote_addr, user_agent, code?}` to `webhook_url`
  and reads back `{"decision": "approve"|"deny"|"pending"}`. Anything else
  approves nothing. `event` is `challenge` first, then `poll` each time
  the waiting page reloads (every 2 s, until `webhook_wait`), or `code`
  when the user types one the webhook sent some other way. A secret
  header for the webhook goes in the `rest` scope's `headers`, so the
  program never holds it.
- **Bans.** `max_retries` failures (passwords, codes, denials) within
  `find_time` ban the user for `ban_time`, in kv, so every replica agrees.

## For the proxy

`/verify` reads the cookie and answers:

| Answer | When |
|---|---|
| 200, `Remote-User`, `Remote-Groups` (comma-separated), `Remote-Name`, `Remote-Email` | signed in, and in one of `?group=a,b` if given |
| 403 | signed in, not in those groups |
| 302 to `login_url?rd=<the original URL>` | not signed in, `login_url` set, and `X-Forwarded-Method` is GET or HEAD |
| 401 | otherwise |

Caddy, as `script/session-browser.sh` runs it in Chromium:

```
forward_auth 127.0.0.1:9091 {
	uri /verify?group=admins
	copy_headers Remote-User Remote-Groups
}
```

Traefik's `forwardAuth` sends the same headers. That is not tested here.

## Other requests

- `GET /api/session`: the session as JSON (`user`, `groups`, `amr`,
  `signed_in`, `expires_in`), or 401.
- `POST /api/token`: a JWT signed EdDSA under the label `jwt_key`, with
  `iss` (`issuer`), `aud` (`jwt_audience`, when set), `sub` (the
  username), `groups`, `amr`, `name` and `email`. It lives `jwt_ttl` and
  never longer than the session. `stdlib:jwks` (`examples/34-jwks`), run
  with the same crypto master and `keys` set to `jwt_key`, publishes the
  public key to a topic's kv and serves the set, so a match server's
  topics (`doc/P2P.md` §2.4) and any JWKS client verify the token.
- `POST /logout`.

## Arguments

`issuer`, `cookie_name`, `cookie_domain`, `cookie_secure`,
`session_ttl`, `pending_ttl`, `login_url`, `default_redirect`,
`second_factor` (`totp`, `webhook`, both, or `none` for a password
alone), `totp_enroll`, `webhook_url`, `webhook_timeout_ms`,
`webhook_wait`, `jwt_key`, `jwt_ttl`, `jwt_audience`, `max_retries`, `find_time`,
`ban_time`, `username_attr`, `name_attr`, `email_attr`. The defaults and
what each means are `DEFAULTS` at the top of
`crates/drt/src/stdlib/session.dlua`. An unknown argument stops the
program.

## Not built

WebAuthn, OIDC, access rules by domain or path (beyond `?group=`), an idle
timeout, a QR code, and serving more than one request at
a time. Each request makes several host calls in turn (directory, kv,
webhook), so a slow webhook delays everyone by up to `webhook_timeout_ms`.
