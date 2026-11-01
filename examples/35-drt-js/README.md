# 35-drt-js

`drt.js` in a page: a terminal that runs `drt`, and a panel that watches
the instances it starts. Everything runs in the browser; nothing here
talks to a server.

```sh
cp <the release's drt.min.js> .        # or crates/drt-web/cdn/dist/drt.min.js after script/drt-js.sh
python3 -m http.server 8000            # a page's modules need http, not file://
```

Open `http://localhost:8000` and type `drt start --config app.json`. The
root spawns three workers, and the panel lists all four with their parent,
whether they are resident, the instructions each has spent and their
capabilities. **park**, **wake** and **kill** are the swarm's own verbs.
^C stops the deployment.

| File | What it is |
|---|---|
| `main.js` | Imports `drt.min.js`, starts the terminal, seeds `app/` into the page's filesystem, and gives the panel to Alpine. |
| `terminal.js` | xterm.js with drt's line editor: each `drt ...` line is ticked to its end in the page. |
| `inspector.js` | The panel's state: the running session's instances, twice a second. |
| `index.html`, `style.css` | The layout, and the panel's table as Alpine markup. |
| `app/` | The program `drt start` runs. |

xterm.js and Alpine come from jsdelivr. `crates/drt-web/cdn/e2e.mjs` runs
this page in Chromium on every release.
