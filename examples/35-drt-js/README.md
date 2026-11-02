# 35-drt-js

`drt.js` in a page: a terminal that runs the browser build of `drt` as if
you had called the binary with no config and no files, and a panel that
lists the instances of what it runs.

The page loads the `drt.min.js` this checkout builds, nothing fetched.
From the repo root:

```sh
WASI_SDK_PATH=<wasi-sdk 27> script/drt-web.sh           # the browser build; needs wasm-bindgen-cli and node too
script/drt-ssh-page.sh && script/drt-js.sh              # the SSH client, then both into drt.min.js
cp crates/drt-web/cdn/dist/drt.min.js examples/35-drt-js/   # again after each rebuild
cd examples/35-drt-js && python3 -m http.server 8000    # a page's modules need http, not file://
```

Open `http://localhost:8000`. The page runs `drt` once, which prints its
usage, then gives you a `$` prompt at the bottom. Only `drt` is on its
path, and Tab completes its subcommands. `drt repl`
opens a Diluvium REPL with the native one's editor: history, Tab over the
guest's names, and the line coloured as you type. Ctrl+D leaves it.

The panel shows instances only for `drt start`, the one command with a
swarm, and `drt start` needs a config and a program file. This page has
no files, so the panel says nothing is running.

| File | What it is |
|---|---|
| `main.js` | Imports the copied `drt.min.js`, puts `terminal()` in `#terminal`, and gives the panel to Alpine. |
| `inspector.js` | The panel's state: the running command's instances, twice a second. |
| `index.html`, `style.css` | The layout, and the panel's table as Alpine markup. |

The terminal (xterm.js with drt's line editor) is part of that
`drt.min.js`. Only Alpine, for the panel, comes from jsdelivr.
`crates/drt-web/cdn/e2e.mjs` runs this page in Chromium on every release.
