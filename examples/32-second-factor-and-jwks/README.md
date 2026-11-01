# 32-second-factor-and-jwks

A second factor and a token another service can check, with the program
naming keys and never holding one.

## Run it

```
cd examples/32-second-factor-and-jwks
drt run --config app.json
```

## What you should see

```
scan    otpauth://totp/Example:alice?secret=QAJ2VBMUC7RQLXKP54REGMZSEGQY6VQ7&issuer=Example&algorithm=SHA1&digits=6&period=30
verify  true
again   false replayed
token   true for alice
jwk     OKP Ed25519 kid=issuer:1 x=UDrOiG8_EmCw-Ka7xucvIWMtxalQ-18b_G8WQMedOAc
```

The URI and the key are the same on every run and every host with this
master: a label's keys are derived from the master and the label's name, so
there is nothing to store. Change the master and both change.

## What it shows

- **`crypto/derive` makes a name, not a key.** The program gets back
  nothing, then passes `totp:alice` or `issuer:1` to the calls that use it.
- **`crypto/totp_enroll` is the one call that hands out a seed**, because an
  authenticator needs it. It is its own grant, so the program that checks
  codes can hold `totp_verify` without it.
- **A code is single-use because the caller says so.** `totp_verify` answers
  the step the code matched; passing that back as `after` refuses it again.
  Store it with the user's session.
- **The label picks the algorithm.** `issuer:1` was derived with
  `alg = "EdDSA"`, so `jwt_verify` checks its tokens as EdDSA whatever their
  header says. `crypto/public_key` answers the JWK; put it in
  `{"keys": [...]}` and any JWT library verifies the token from it.

## What it does not show

It does not serve the JWKS over HTTP; `17-serving-http` is how a program
answers a request. And it runs under `drt` only for now: the plain
Diluvium host's crypto connector does not answer `derive`, the TOTP calls or
EdDSA.
