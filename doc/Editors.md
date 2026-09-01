# Getting `.dlua` recognised

Diluvium is Lua plus a handful of additions (string interpolation, `??`,
`switch`, `defer`, `with`, safe navigation), so **every surface below has a
cheap answer — "treat it as Lua" — and one good answer, the real grammar.**
The cheap answer is a one-liner and gets you comments, strings, numbers and
keywords; the additions render unhighlighted rather than wrong. The good
answer exists in exactly one place today and is worth spreading.

The single most useful thing to know: **almost none of this is global.**
Each repository, and each editor, needs telling separately.

---

## GitHub, and why linguist is not the play

Per repository, in `.gitattributes`:

```
*.dlua linguist-language=Lua
```

That is it, and it must be repeated in **every repo holding `.dlua`** —
`diluvium`, `diluvium-drt`, `discofetch`, `disco-fetchpoint`, Lab. There is
no org-level setting. A repo without the line renders `.dlua` as plain
text.

Getting Diluvium into linguist *itself* — which would make `.dlua`
highlight everywhere on GitHub with no per-repo line — has a published bar,
and it is worth knowing the number before anyone spends time on it. From
linguist's CONTRIBUTING:

- **at least 2000 files with the extension, indexed in the last year**, for
  an extension expected more than once per repository;
- a *"reasonable distribution across unique `:user/:repo` combinations"* —
  so 2000 files in five repos does not count;
- a syntax grammar under an acceptable licence, added as a submodule;
- real-world samples — *"Hello world and other examples found in tutorials
  will not be accepted."*

That is an adoption threshold, not an engineering one. Nothing in our
control shortens it, and `.gitattributes` is the answer until Diluvium is
being written by strangers at that scale.

**GitLab**, if it ever matters, uses the same file with a different key:
`*.dlua gitlab-language=lua`.

---

## Editors

The real grammar lives in
[`editors/vscode`](https://github.com/Aloecraft-org/diluvium/tree/main/editors/vscode)
in the diluvium repo: a `dlua` language id, a `source.dlua` TextMate
grammar that knows the additions, and bracket/comment configuration. It
stays there rather than being copied into consumers — a grammar describes
the *language*, so it belongs with the language, and a second copy would
fall behind the first time a keyword lands in `dhostlib.h`.

### The highest-leverage single action

**Publish that extension to Open VSX as well as the VS Code Marketplace.**

Cursor, Windsurf, VSCodium, Gitpod and Eclipse Theia cannot install from
Microsoft's marketplace — its terms restrict it to Microsoft products — so
they all read [open-vsx.org](https://open-vsx.org) instead. Publishing to
only one registry means roughly half the editors people actually use get
nothing. Both take the same `.vsix`:

```sh
cd diluvium/editors/vscode
npx @vscode/vsce package                 # -> diluvium-0.1.0.vsix
npx @vscode/vsce publish                 # VS Code Marketplace
npx ovsx publish diluvium-0.1.0.vsix     # Open VSX
```

Until then, from source:

```sh
code --install-extension diluvium-0.1.0.vsix
```

`sample/showcase.dlua` in the extension is the fastest check that it works.

### The cheap answer, per editor

Each of these says "this is Lua" and needs no grammar.

| editor | where | line |
|---|---|---|
| VS Code and forks | `settings.json` | `"files.associations": { "*.dlua": "lua" }` |
| Neovim | `init.lua` | `vim.filetype.add({ extension = { dlua = "lua" } })` |
| Neovim (tree-sitter too) | `init.lua` | also `vim.treesitter.language.register("lua", "dlua")` |
| Vim | `.vimrc` | `au BufRead,BufNewFile *.dlua setfiletype lua` |
| Helix | `languages.toml` | add `"dlua"` to the `lua` language's `file-types` |
| Zed | `settings.json` | `"file_types": { "Lua": ["dlua"] }` |
| Emacs | `init.el` | `(add-to-list 'auto-mode-alist '("\\.dlua\\'" . lua-mode))` |
| JetBrains | Settings → Editor → File Types → Lua | add the pattern `*.dlua` |
| Sublime Text | with a `.dlua` open: View → Syntax → Open all with current extension as… → Lua | |
| `bat` / `delta` | `~/.config/bat/config` | `--map-syntax='*.dlua:Lua'` |

Worth putting the VS Code line in each repo's `.vscode/settings.json` so it
applies to everyone who opens the repo without anyone configuring anything
— that is the one place a per-repo setting reaches other people, and it
costs two lines.

---

## When `.host.lua` goes away

It is only still here because code written for `diluvium-host` depends on
it, and it is meant to be removed rather than kept. Nothing above blocks
that, and only two things will need touching when it happens:

- the extension's `filenamePatterns: ["*.host.lua"]` in
  `editors/vscode/package.json`, which claims it as Diluvium;
- the note in this repo's `.gitattributes` explaining why `*.host.lua` gets
  no rule.

Neither is load-bearing. On GitHub the file has always been highlighted as
Lua simply because it ends in `.lua`, so its removal changes nothing there.
