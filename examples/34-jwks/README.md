# 34-jwks

A JSON Web Key Set for tokens DRT signs. `stdlib:jwks` writes each key's
public half to a topic's kv and serves the topic's set at
`/.well-known/jwks.json`. The private keys never leave the crypto connector.

## Run it

```
cd examples/34-jwks
drt p2p --match 18498 --kv ./data
```

and in two more terminals:

```
drt start --config app.json
```

```
curl -s http://127.0.0.1:18499/.well-known/jwks.json | python3 -m json.tool --sort-keys
drt kv ls drt://127.0.0.1:18498/v1/auth/kv
drt run --config sign.json > token
python3 verify.py token
```

`verify.py` needs PyJWT (`pip install 'pyjwt[crypto]'`). `./demo.sh` runs
all of it, which is how the gate runs it.

## What you should see

The set holds `issuer:1` and `issuer:2`, each `{"alg": "EdDSA", "crv":
"Ed25519", "kid", "kty": "OKP", "use": "sig", "x"}` and nothing else. kv
holds them as `jwks/issuer~3A1` and `jwks/issuer~3A2`, and the whole set as
one value under `jwks`, each expiring in a day.
Then:

```
kid issuer:2 sub alice
```

## What it teaches

**Old and new keys are served together.** `args.keys` lists both, and a
token names its key in `kid`, so a verifier picks the right one: PyJWT's
`PyJWKClient` does exactly that from the URL alone.

**A key leaves by its TTL.** Each listed key is written again every
`ttl / 3` seconds. Take `issuer:1` out of `args.keys` and it drops out of
the set within `ttl` of its last write once the deployment restarts, so set
`ttl` past your longest token lifetime.

**The set is the topic's.** Several issuers writing to one topic serve one
set. On a server with `--topics`, give the kv scope a token holding `read`
and `write`. The surface block of `crates/drt/src/stdlib/jwks.dlua` is the
whole reference.

It runs under `drt` only: the plain `diluvium` host has no `kv` connector
and no EdDSA.
