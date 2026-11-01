# 35-drt-js

`drt.js` in a page: a terminal that runs the browser build of `drt` as if
you had called the binary with no config and no files, and a panel that
lists the instances of what it runs.

```sh
script/drt-js.sh                       # from the repo root: builds crates/drt-web/cdn/dist/drt.min.js
python3 -m http.server 8000            # also from the root; a page's modules need http, not file://
```

Open `http://localhost:8000/examples/35-drt-js/`. The page runs `drt`
once, which prints its usage, then gives you a `$` prompt. Only `drt`
is on its path. `drt repl` opens a Diluvium REPL (Ctrl+D leaves it).

The panel shows instances only for `drt start`, the one command with a
swarm, and `drt start` needs a config and a program file. This page has
no files, so the panel says nothing is running.

| File | What it is |
|---|---|
| `main.js` | Imports the repo's `drt.min.js`, starts the terminal, and gives the panel to Alpine. |
| `terminal.js` | xterm.js with drt's line editor: each `drt ...` line runs in the page until it ends or Ctrl+C. |
| `inspector.js` | The panel's state: the running command's instances, twice a second. |
| `index.html`, `style.css` | The layout, and the panel's table as Alpine markup. |

xterm.js and Alpine come from jsdelivr. `crates/drt-web/cdn/e2e.mjs` runs
this page in Chromium on every release.
