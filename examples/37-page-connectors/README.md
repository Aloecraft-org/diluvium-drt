# 37-page-connectors

A connector family that drt does not carry, answered by the page. The page
hands `kernel()` an object with a `call(name, args)` function under a family
name, and a program reaches it with `host.call` like any other family. Here
the family is `storage` and the object is the browser's `localStorage`
(`storage.js`).

## Run it

```
script/drt-js.sh                 # from the repo root: builds drt.min.js
python3 -m http.server 8000
```

Open <http://localhost:8000/examples/37-page-connectors/>. `main.js` makes a
kernel with `{ storage }`, puts the program and both configs in its files,
and runs `drt run` under each config.

## What you should see

```
$ drt run --config app.json
  storage/set greeting   ok      true
  storage/get greeting   ok      hello from the page
  storage/get missing    error   no key 'missing'

$ drt run --config locked.json
  storage/set greeting   denied  'storage/set' is outside this instance's grants
  ...
```

What `call` returns is the reply, and a promise works too. What it throws,
or a promise it rejects, is an `error`. The config gates a family the page
handed in the same way it gates `fs`: `locked.json` wires `storage` and
grants only `host:time/*`, so every call is denied before it reaches the
page.

A page cannot hand in a family drt carries itself (`fs`, `time`, `crypto`),
and `net` takes a transport rather than `call` (doc/doc_old/Browser.md).
