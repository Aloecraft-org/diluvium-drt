# 36-post-quantum

Post-quantum signatures and encryption from aloecrypt_core. The program
names keys and never holds one.

## Run it

```
cd examples/36-post-quantum
drt run --config app.json
```

## What you should see

```
-----BEGIN ALOECRYPT SIGNATURE-----
verify  true
altered false mismatch
-----BEGIN ALOECRYPT ENCRYPTED-----
opened  hello, alice
error   aloecrypt/sign: 'inbox:alice' is outside this instance's host:aloecrypt/sign grant, which allows: release:*
hash    f60ab6a916da4915f30847b601e7cb213d3a4efdbbcf987d69381f92b84feeaf
```

The program prints only the first line of each document. The full text is
armored base64 that another aloecrypt reader can open.

## What it shows

- **A label is a keypair.** `release:1` under `ml-dsa-65` and `inbox:alice`
  under `ml-kem-768` are derived from the master and the label, so the same
  config gives the same keys on every host and nothing is stored.
- **The public half is text.** `aloecrypt/public_key` answers it as an
  armored `ALOECRYPT SIGNER` or `RECIPIENT` document, for a program to
  publish, and `verify` and `encrypt` take it back.
- **A failed signature is an answer.** `verify` replies `valid = false`
  with a reason (`mismatch` or `malformed`). A malformed key is an error.
- **The grant names the labels.** `sign` and `decrypt` use a private key,
  so their grants carry label patterns. This program may sign as
  `release:*` and decrypt as `inbox:*`, and nothing else.

## What it does not show

Whether to trust a public key: the document moves a key intact and is not
signed, and trust is aloecrypt's certificate layer, which is not built. This
also runs under `drt` only. The plain Diluvium host has no aloecrypt connector.
