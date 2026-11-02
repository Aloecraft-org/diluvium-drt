# 35-drt-js

`drt.js` in a page: a terminal that runs the browser build of `drt` as if
you had called the binary with no config and no files, and a panel that
lists the instances of what it runs.

The page loads the `drt.min.js` this checkout builds, nothing fetched.
From the repo root:

```sh
script/drt-web.sh                                       # the browser build; fetches a wasi-sdk and wasm-bindgen, needs node
script/drt-ssh-page.sh && script/drt-js.sh              # the SSH client, then both into drt.min.js
cp crates/drt-web/cdn/dist/drt.min.js examples/35-drt-js/   # again after each rebuild
cd examples/35-drt-js && python3 -m http.server 8000    # a page's modules need http, not file://
```

For the Files panel, aloelite-wasm and its explorer, built from an
aloelite checkout at the commit this page was checked with:

```sh
git clone https://github.com/Aloecraft-org/aloelite && git -C aloelite checkout 0a23703
cd aloelite/rust && cargo build -p aloelite-wasm --target wasm32-unknown-unknown --release
wasm-bindgen --target web --out-dir pkg target/wasm32-unknown-unknown/release/aloelite_wasm.wasm   # wasm-bindgen-cli 0.2.127, the version drt pins too
cp pkg/aloelite_wasm.js pkg/aloelite_wasm_bg.wasm aloelite-wasm/explorer/aloelite-explorer.js <drt>/examples/35-drt-js/
```

Open `http://localhost:8000`. The page runs `drt` once, which prints its
usage, then gives you a `$` prompt at the bottom. Only `drt` is on its
path, and Tab completes its subcommands. `drt repl`
opens a Diluvium REPL with the native one's editor: history, Tab over the
guest's names, and the line coloured as you type. Ctrl+D leaves it.

Variables lists what the REPL has defined. Instances is the swarm of
the command that is running: `drt start`'s, which needs a config and a
program file, or the REPL's own, which is the REPL and whatever a line
spawned (`p = host.spawn{code = [[...]]}`), with the swarm's verbs as
buttons. Files is the kernel's aloelite volume, in memory: a file you
add there is one `drt run` sees, and what a program writes shows up
there.

| File | What it is |
|---|---|
| `main.js` | Imports the copied `drt.min.js`, puts `terminal()` in `#terminal`, and gives the panel to Alpine. |
| `inspector.js` | Variables and Instances: the running command's REPL names and instances, twice a second. |
| `files.js` | Files: one aloelite volume handed to `terminal()` as its filesystem and to aloelite's explorer. |
| `index.html`, `style.css` | The layout, and the panel's table as Alpine markup. |

The terminal (xterm.js with drt's line editor) is part of that
`drt.min.js`. Only Alpine, for the panel, comes from jsdelivr.
`crates/drt-web/cdn/e2e.mjs` runs this page in Chromium on every release.
