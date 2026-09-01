# Editors, and why GitHub cannot match one

Two different mechanisms highlight the same files, and they are not
interchangeable. Worth twenty lines here so nobody tries to make one do the
other's job.

## GitHub: `.gitattributes`, and its ceiling

GitHub highlights by *language*, from a fixed list compiled into its own
tooling. There is no Diluvium in that list and a repository cannot add one:
`.gitattributes` can only map a path to a language that already exists. So
DRT's rule says the true-enough thing —

```
*.dlua linguist-language=Lua
```

— and Diluvium's additions (string interpolation, `??`, `switch`, `defer`,
`with`, safe navigation) come out unhighlighted rather than wrong, because
Lua's grammar still parses everything around them.

**Parity with the editor extension is therefore not reachable from this
repository**, and that is a property of GitHub rather than a thing left
undone. Shipping a TextMate grammar here would change nothing on
github.com.

## Editors: the extension, which is upstream

`editors/vscode` in [`Aloecraft-org/diluvium`](https://github.com/Aloecraft-org/diluvium/tree/main/editors/vscode)
is the real thing: a `dlua` language id, a `source.dlua` TextMate grammar
that knows the additions, and a bracket/comment configuration. It claims
`.dlua` **and `*.host.lua`** — the second because the host-config dialect
is Diluvium too, which GitHub cannot express, since `.host.lua` is already
`.lua` to it.

It lives there rather than being copied here on purpose. A grammar is a
description of the *language*, so it belongs with the language; a second
copy in this repository would be a second thing to keep in step with
`dhostlib.h`, and it would fall behind the first time a keyword lands
upstream.

Install it from source (it was not confirmed published to the marketplace
when this was written, so check before recommending an extension id):

```sh
git clone https://github.com/Aloecraft-org/diluvium
cd diluvium/editors/vscode && npx @vscode/vsce package
code --install-extension diluvium-*.vsix
```

Its `sample/showcase.dlua` is the fastest way to see whether it is working.
