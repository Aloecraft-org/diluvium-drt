# Load-time modules

A `require` for guests that never touches a filesystem. The host resolves
modules before the program runs; the guest looks them up.

```
   live/fp/                          the chunk that gets loaded
   ├── entry.dlua      ──walk──▶     module table, compiled
   ├── util/enc.dlua                 require, over that table
   └── db/claims.dlua                entry, compiled under its own name
```

No new hostcall, no new capability, no format change, and no ABI change.
Every failure is a named failure, and every one of them happens before the
node's own code runs except the one that cannot: a name the guest passes to
`require`, which the host never sees.

## The mechanism, and why it is not `package.preload`

The obvious shape is Lua's own — register each module in `package.preload`,
let stock `require` find it. That table does not exist here. A sealed guest
is opened without `LUA_LOADLIBK` (`dlibs.c`, `diluvium_openguestlibs`), so
there is no `package`, no `package.preload`, no `package.loaded`, and no
`require`. Those arrive only with `--unsafe`, where `require` is the real
filesystem one this mechanism exists to avoid.

The ABI has no preload call either. `dv_register_code` looks like one and is
not: it is snapshot dedup, letting a snapshot carry a 32-byte hash instead of
a chunk's bytecode, and registering a chunk there does not make it reachable
by name.

What the seal does keep is `load`, deliberately, with the reason written
beside it in `dlibs.c`:

> Not 'load'. It compiles bytes the program already holds and reaches nothing.

So: the host reads the modules, generates a chunk holding their source,
compiles each with `load`, installs a `require` that looks up the result, and
compiles and calls the entry. `crates/drt/src/modules.rs` is all of it.

Each module is its own `load`, which buys two things. Tracebacks name the
module's own file and line, because its chunk name is its path:

```
drt run: util/enc.dlua:3: from inside the module
stack traceback:
	util/enc.dlua:3: in field 'boom'
	[string "entry.dlua"]:1: in main chunk
```

And Lua's 200-local ceiling is per function (`lparser.c`, `MAXVARS`), so a
module spends its own rather than the entry's. That is the relief this was
wanted for.

**The entry is `load`ed too**, rather than the generated chunk being
prepended to it. Prepending would shift every line of the entry by the length
of the preamble, and every traceback and error message with it. Loaded
separately it keeps its own line numbers — and its own chunk name, bare where
a module's is `@`-prefixed, because the entry had a name before modules
existed and adding one to a node must not change the shape of that node's
errors.

## Resolution

- **Name to path.** Dots become separators, plus the extension:
  `require("db.claims")` is `db/claims.dlua`.
- **Scope is the node's own directory and nothing else.** Not `init/`, not a
  sibling node, not the project root. The node's directory is the program's
  own directory — `live/<name>/` for a deployed entry, and the config's
  directory for a `--config` run, which resolves its program against itself
  for the same reason. Both are the directory that moves as a unit, which is
  what makes a module part of the node it ships with.
- **The entry is not one of its own modules.** It is about to run as the
  entry; a `require` of it would run it twice.
- **A component holds letters, digits and `_`.** The dot is the separator, so
  a component may not contain one: with `my.helper.dlua` allowed, `my.helper`
  would name both it and `my/helper.dlua` and one would win silently.
- **No parent traversal, no absolute paths, no leading separators.** A name
  with `..`, one starting with `/`, or one holding anything outside
  `[A-Za-z0-9_.]` is refused.
- **`stdlib` is reserved** as a first component. A stdlib program is reached
  by the `stdlib:` entry spelling and never by `require`.
- **`.dlua` only.** A `.lua` beside the entry is a program somebody runs
  directly; promoting it to a module because it parses would make every
  existing node's directory mean something new.
- **The walk happens once, at load.** A file added afterwards is not visible
  until the node is loaded again — what `diluvium analyze` sees over the
  directory is what the loader saw.
- **A module runs once**; later `require`s return the cached value, and a
  module that returns nothing is `true`, as in Lua.
- **A cycle is a named failure**, not a stack overflow, which would name
  neither module in it.

## Capabilities

None. A module runs in the node that required it, with that node's caps.
There is no grant for "may require", because requiring adds nothing a node
did not already hold, and attenuation is untouched.
`a_module_holds_exactly_the_caps_the_node_holds` in
`crates/drt/tests/modules.rs` is that property: the same denied hostcall is
denied identically whether it sits in a module or in the entry.

## Three corrections to the design note

**`.dluac` is deferred, not cut for taste.** `engine.rs` loads source with
`text_only(true)` — *"Source only unless bytecode was explicit:
GUARANTEES.md, the verifier that does not exist yet"* — and guest-side `load`
accepts a binary chunk even under that flag, which upstream records as a real
defect. Reaching bytecode modules through it would put unverified bytecode
inside the one sandbox the entry is protected from. So a `.dluac` beside the
entry is refused by name, saying why, rather than quietly ignored. It lands
the day the verifier does.

**A bad name is refused at the call, not at load.** The design's acceptance
list asks for `require("..secret")` to fail at load. The host only ever sees
the directory; it never sees a call site, so a name that reaches `require`
cannot be refused before the call that passes it. It is refused there, by
name, with the reason — which is the property that was wanted. (Scanning the
entry for literal `require("…")` at load would half-deliver it and miss every
computed name, so it is not done.)

**Every module is its own chunk, not its own budget.** The design says both.
The chunk is right and is what relieves the 200-local ceiling. The budget is
not: `dv_set_budget` is per *instance*, so every chunk in a node shares one
budget, and compiling the modules spends a little of it before the entry
starts.

## One rule, written twice

The name rule runs in two places: over the files the host finds, and over
whatever string the guest hands `require`. The host cannot be in the loop for
the second — that is what a preload table *is* — so the rule is written twice
and cannot be written once.

What can be done is make the two testably identical, and that is done. The
reserved word and the character class are interpolated into the generated Lua
from the Rust constants above it, the refusal strings are
character-for-character the same, and
`the_two_copies_of_the_name_rule_agree` runs one table of cases through both
and fails if they ever differ.

## Sharing across repos

A library is a package. `dollup pull <ref>` delivers it into `init/`, deploy
copies it into the node's directory, and the loader finds it there. There is
no second delivery path and no library search path outside the node.

Two nodes that both need a library each carry a copy. That is correct: they
may run under different ceilings, ship in different roots, and be committed
separately. The cost is bytes on disk, which is the right thing to spend.

## What this is not

- **Not a text loader.** Lua modules only. `.sql` migrations stay a build
  step that emits `migrations/001.dlua` returning a string.
- **Not a shared library dir.** No `lib/` at the project root, no `DRT_PATH`.
- **Not a queue.** Pure functions are modules; anything that owns state is a
  node.
- **Not a connector.** Host-shaped things stay in DRT.

## Named but not built

- A module manifest pinning which files are modules. Today it is every
  `.dlua` in the directory except the entry.
- Lazy compilation. Today every module is compiled at load, which is what
  makes a syntax error in one a failure before any of the node's code runs.
- `.dluac`, above.
