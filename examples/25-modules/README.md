# 25-modules

A program in three files. `require` here opens nothing: before the program
started, the host walked this directory, compiled every `.dlua` and `.lua` in
it, and put `require` in scope over the result. The guest still cannot read a file —
that is the whole point — and it can still have libraries.

## Run it

```
cd examples/25-modules
drt run app.dlua
```

## What you should see

```
$ drt run app.dlua
The Unit That Moves
One File, Then Another, Then A Third
one table:	true
require(text.missing): no such module in .
```

## What it teaches

**A module is a file that returns something.** Nothing declares it. Nothing
registers it. `text/case.dlua` is the module `text.case` because it is guest
source beside the entry — `.dlua` or `.lua`, dots are the path separator, and
the entry itself is not one of its own modules.

**The node's directory is the whole of the search path.** There is no
parent traversal, no project `lib/`, no `DRT_PATH`, and nothing reachable
outside this folder. That is what makes a module part of the node it ships
with: two nodes that need the same library each carry a copy, and each can
run under a different ceiling.

**Modules see each other.** `text/join.dlua` requires `text.case` exactly as
`app.dlua` does, because every chunk is loaded against the same globals.

**A module runs once.** The third line is `require("text.case") == case` —
one table, cached after the first call, as in Lua.

**A miss is a named failure that says where it looked.** `no such module in .`
is the directory that was walked; under `drt start` it is `live/<name>`, which
is the answer to "why can it not find my file" most of the time.

**Requiring granted nothing.** A module runs in the node that required it,
with that node's caps — there is no grant for "may require", because there is
nothing to grant. A hostcall the node does not hold is refused inside a module
exactly as it is refused in the entry.

`doc/Modules.md` is the mechanism, including why this is not
`package.preload` and what a `.dluac` would need first.
