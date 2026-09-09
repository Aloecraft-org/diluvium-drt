# Diluvium syntax proposals

Status: brainstorm for review. Everything here is on the table; the tiers are a recommendation, not a decision. Already-shipped and already-planned items are listed for completeness so the sigil budget is visible in one place.

## 1. Ground rules

**Freeness test.** Every new form must be a syntax error in stock Lua 5.5. Not "unlikely in practice," a hard parse error. The CI gate: a corpus of every new form is fed to stock `luac -p` and the build fails if any of it parses. The reverse corpus (the Lua test suite plus real-world code) must parse and run identically under diluvium.

**Desugar only.** New syntax compiles to existing opcodes. No new opcodes, no dump/undump changes, `luac -l` output indistinguishable from hand-written Lua. This is how `??` landed (branch compilation, `OP_2Q` removed) and it is what keeps the bytecode identical across targets and the analyzer honest. Where a desugar needs a helper, it calls a registry function the way literal suffixes do (looked up outside `_ENV`, so it cannot be shadowed).

**Contextual keywords carry the risk.** Lua is whitespace-insensitive, so `NAME` followed by something must be unambiguous by token alone, never by line. Each contextual keyword below states its disambiguation rule. The pattern: the keyword is only special when the token after it makes stock Lua fail.

**One sigil, one meaning.** Section 9 is the allocation table. A sigil is spent once.

**Fewer ceremony tokens, not a different language.** Keep `if`/`then`/`end` where they are; add forms that remove nesting and boilerplate. The VHDL feeling comes from `end` density, and the biggest wins against it are expression-bodied functions and lambdas, not braces (see 6.2).

## 2. Shipped and planned

Shipped: `$"..."` interpolation, `??` null coalescing, literal-suffix registry (`1.23d`), secure functions.

Planned and recorded: `switch` (contextual, no fallthrough, multi-value arms, no destructuring match), compound assignment, `defer`, "decorator features" (see 5.6 for the sigil decision this forces), LuaCATS comment annotations.

## 3. Tier A: small, clear, ship together

### 3.1 Compound assignment (planned)

```lua
x += 1        t[k] ..= "s"        n *= 2
```

Free: `NAME +` cannot start a statement. Semantics worth pinning: the target's prefix and key are evaluated once (`t[f()] += 1` calls `f` once). Operators: `+= -= *= /= //= %= ^= ..= &= |= ~= << >>`. Note `~=` is already "not equal"; use `~~=` for bitwise-xor-assign or skip xor.

### 3.2 Default parameters

```lua
function connect(host, port = 8080, tls = true)
```

Free: `=` inside a parameter list is an error. Desugar: `if port == nil then port = 8080 end` as a prologue. Explicit `nil` triggers the default (Lua-consistent). Default expressions evaluate per call and may reference earlier parameters.

### 3.3 `continue`

Contextual: `continue` as a whole statement, i.e. followed by a token that cannot continue an expression (`end`, `else`, `elseif`, `until`, another statement, EOF). `continue(...)`, `continue = ...`, `continue.x` stay identifier uses. Codegen jumps straight to the loop's condition, which sidesteps the `repeat ... until` label-scope problem that bites `goto continue` in stock Lua.

### 3.4 Null-safe navigation

```lua
port = cfg?.server?.port ?? 8080
name = users?[id]?:display()
```

Free: `?` is not a Lua token. Tokens: `?.` `?[` `?:` `?(`. Semantics as JavaScript optional chaining: the first nil short-circuits the whole chain to nil (`a?.b.c` is nil when `a` is nil, no error on `.c`). Compiled with branches, same technique as `??`. Pairs with `??` naturally.

### 3.5 Numeric literal separators and binary literals

```lua
1_000_000     0b1010_0110     0xFF_FF
```

Free: `_` inside a numeral and `0b` are both malformed numbers today. Underscore is ignored; it must sit between digits. Finance code reads much better with it.

### 3.6 `const`

```lua
const RATE = 0.05d
```

Contextual: `const` followed by `NAME`. Pure sugar for `local RATE <const> = ...`. Reads better and hides the attribute syntax that nobody remembers.

### 3.7 Spread

```lua
f(a, ...args)          {...defaults, ...overrides}
```

Free: `...` followed by `NAME` is an error. In calls and constructors, last position desugars to `table.unpack(args)`. Multiple spreads in a constructor go through a registry helper (Lua only expands the last expression).

### 3.8 Expression-bodied functions

```lua
function area(w, h) = w * h
local function sq(x) = x * x
function Point:len() = math.sqrt(self.x^2 + self.y^2)
```

Free: after `)` a block begins and `=` cannot start a statement. Desugar to `return expr end`. Removes one `end` per one-liner, which in practice is most small functions.

### 3.9 `defer` (planned)

```lua
defer conn:close()
```

Contextual: `defer` followed by a call or `function`. Desugar to a to-be-closed variable: `local _ <close> = dv.defer(function() conn:close() end)` where the helper returns a value with `__close`. Correct on error and on every scope exit, zero VM change.

## 4. Tier B: medium, high value

### 4.1 Compact lambdas

```lua
map(xs, |x| x * 2)
sort(t, |a, b| a.score > b.score)
local thunk = || compute()
```

Free: `|` has no unary form, so `|` at an expression start is an error today; `||` is not a token. Body is one expression at lowest precedence; ends where an expression ends (`,` `)` `}` keyword). Implicit single return. `|x| x | y` is a lambda returning `x | y`, as expected. Multi-statement bodies keep using `function`; a `|x| do ... end` form is possible but reintroduces what the lambda is meant to remove.

Rejected alternatives: `fn(x) body` (any program with a function named `fn` becomes ambiguous); `(x) => body` (needs arbitrary lookahead to tell a parameter list from a parenthesized expression). `\x -> body` is also free and fine if `|` is wanted elsewhere.

### 4.2 Destructuring

```lua
local {host, port} = cfg          -- keyed:      cfg.host, cfg.port
local [first, second] = pair      -- positional: pair[1], pair[2]
for {name, age} in each(people) do
[a, b] = [b, a]                   -- swap via constructor spread
```

Free: `local {`, `local [`, `for {`, and `[` or `{` at statement start are all errors. Braces mean keyed, brackets positional, mirroring JS. Nested patterns and parameter-list destructuring are possible later through the same parser path; not in v1. Pairs well with `local {a, b} = require "mod"`.

### 4.3 `if` and `switch` as expressions

```lua
local sign = if x < 0 then -1 elseif x > 0 then 1 else 0
local label = switch code case 200: "ok" case 404: "missing" else "?"
```

Free: `if` and `switch` at an expression position are errors. Ends the `a and b or c` trap where `b` is false or nil. Every branch is an expression; `else` is mandatory in expression form.

### 4.4 Slicing

```lua
xs[2:5]     xs[3:]     xs[:n]     s[1:3]
```

Free: `[ exp :` is an error. Desugar to a registry call that looks up `__slice` on the metatable in C. Strings get `__slice` mapped to `string.sub`, plain tables to a `table.move` copy, and the numeric spec's array type implements it as a view. No new metamethod slot in `ltm.c` is needed because the lookup happens in the helper.

### 4.5 `export`

```lua
export function connect(url) ... end
export DEFAULT_PORT = 8080
```

Contextual: `export` followed by `function`, or by `NAME =`. A chunk that uses `export` gets a hidden module table and an implicit `return` of it; an explicit top-level `return` in such a chunk is a compile error. Makes dollup packages read like modules instead of `local M = {} ... return M` ceremony.

### 4.6 Function attributes

```lua
function price(qty, rate) <deterministic>
  return qty * rate
end
function step(state) <pure> ... end
```

Free: after `)` a block begins and `<` cannot start a statement; the form mirrors Lua's own `<const>`/`<close>`. Semantics are compile-time: the analyzer must prove the attribute or compilation fails. This turns the determinism verdict from a report into an opt-in assertion per function, without touching the "report, don't enforce" default anywhere else. It also gives the earlier `await`-as-assertion idea a home. Attributes that need runtime behavior (memoize, trace) are a separate decision; see 5.6.

## 5. Tier C: large, decide deliberately

### 5.1 Classes

```lua
class Account extends Base
  balance = 0d                      -- per-instance default

  function new(owner)               -- constructor, implicit self
    super(owner)
    @owner = owner
  end

  function deposit(amt)
    @balance += amt
    return self
  end

  static function empty() = Account("nobody")

  function __tostring() = $"Account({@owner}: {@balance})"
end
```

Contextual keywords: `class` followed by `NAME` (statement start; `class = ...`, `class(...)`, `class.x` stay identifiers). `extends`, `static`, and `super` are only special inside a class body, which is already non-Lua territory. `@name` is `self.name`; `@:m()` is `self:m()`. `@` is not a Lua token, so it is free everywhere.

Desugar, all plain metatables so it interoperates with hand-written Lua OOP and existing class libraries:

```lua
local Account = setmetatable({}, {
  __index = Base,
  __call  = function(cls, ...) local o = setmetatable({}, cls); cls.new(o, ...); return o end,
})
Account.__index = Account
Account.__name  = "Account"
function Account.new(self, owner) self.balance = 0d; Base.new(self, owner); self.owner = owner end
function Account.deposit(self, amt) ... end
function Account.empty() ... end
function Account.__tostring(self) ... end
```

Rules to pin:

- Methods declared with `function` inside the body get implicit `self`; `static` opts out.
- Field defaults are evaluated per instance in the constructor prologue (no shared table-valued defaults, the Python trap). Parent defaults apply when `super(...)` runs.
- `super(...)` calls the parent constructor with `self`; `super.m(...)` calls the parent method with `self`.
- Metamethods declared in the body land in the class table, so instances get operator overloading for free. Because Lua finds metamethods with a raw lookup and does not follow `__index`, class creation copies the parent's `__`-prefixed entries into the child (the standard inheritance gotcha, handled once in the desugar).
- Instance test: `dv.isa(obj, Account)` registry helper walking the chain. An `is` operator is Tier D.

This is the largest item and the one most likely to grow. Ship the minimal form above and let demand pull mixins, properties, or private fields.

### 5.2 Braces instead of `end`

The honest analysis, because this one looks free and is not.

`if cond then { ... }`, `while cond do { ... }`, `function f() { ... }`: **free.** After `then`, `do`, or `)`, a block begins, and `{` cannot start a statement. Closing `}` pairs with the opening `{`; `end` pairs with `then`/`do`.

`if cond { ... }` (dropping `then`): **not free.** `f(x) {a=1}` is a valid Lua call with a table argument, so `if f(x) {a=1} then ... end` is a valid Lua program today and a brace parser would reinterpret it. The same holds for `while f(x) {` and `for k in pairs(t) {`. Vanishingly rare in real code, but it means the 100% claim gets an asterisk.

Recommendation: do not do braces. The `then {` form is compatible but saves almost nothing over `then ... end`, and the `then`-less form breaks the guarantee. Expression-bodied functions (3.8), lambdas (4.1), `if`-expressions (4.3), and `?.` (3.4) remove more `end`s and more nesting from real code than braces would, with no asterisk.

### 5.3 Inline type annotations

```lua
local rate: decimal = 0.05d
function total(items: {Item}, tax: number): decimal
```

Free: `local NAME :`, `NAME :` inside a parameter list, and `) :` before a body are all errors. Erased at compile time, Luau-style. LuaCATS is already decided, which covers editors and LLM tooling without new syntax; the case for inline annotations is the bytecode analyzer, which cannot see comments. If the verdict ever wants type information (e.g. "this value only flows through decimal"), inline annotations dumped into a side section of the chunk would be the way to carry it. Defer until the analyzer asks.

## 6. Tier D: speculative, worth a sketch

### 6.1 Pipe

```lua
prices |> returns() |> rolling(20) |> std()
```

Free: `| >` is an error. `x |> f(a)` desugars to `f(x, a)` (first-argument insertion). Method chaining with `:` already covers objects, so this earns its place only for free-function pipelines; the dlua stats library is the likely customer.

### 6.2 `in` operator

```lua
if key in allowed then
```

Free: `in` outside a `for` header is an error. Desugar to a registry `__contains` lookup, falling back to `t[key] ~= nil`.

### 6.3 `?` try postfix

```lua
local conn = open(url)?
```

Free: `?` followed by a statement boundary. Desugar for Lua's `nil, err` convention: `local conn, __e = open(url); if conn == nil then return nil, __e end`. Rust's `?`, adapted. Tokenizer distinguishes it from `?.`/`?[`/`?:`/`??` because those are single tokens.

### 6.4 `:=` local declaration

```lua
total := 0
```

Free: `: =` is an error. Pure sugar for `local total = 0`. Tempting, but `local` is short and `const` (3.6) already covers the common case where the keyword feels heavy.

### 6.5 `with`

```lua
with req do
  .method = "POST"
  .body = payload
end
```

Contextual `with` followed by an expression then `do`; leading `.x` inside the body is `req.x`. Free inside the `with` block only. MoonScript precedent. Niche; include only if builder-style code shows up in the swarm agents.

## 7. Rejected

- **Indentation-delimited blocks.** Whitespace sensitivity breaks arbitrary valid Lua.
- **`then`-less braces.** See 5.2.
- **`fn` lambda keyword.** Identifier ambiguity; see 4.1.
- **`cond ? a : b` ternary.** `b : c(...)` is a method call. Use `if`-expressions.
- **Leading `.x` as `self.x` outside `with`.** `f()\n.x = 5` is already `f().x = 5` in Lua.
- **`match` / destructuring switch.** Already decided against.

## 8. Decisions this forces

**`@`: self sugar or decorators.** Both want it and it can only mean one thing at statement start. Recommendation: `@` is `self` (MoonScript precedent, used constantly inside class bodies), and decorators use the attribute slot from 4.6: `function f(x) <memoize> ... end` desugars to `f = memoize(f)` when the attribute resolves to a runtime function rather than an analyzer check. One syntax, two resolution paths, no second sigil.

**Where sugar lives in the tree.** Every item here is parser and codegen. Keep them in a separate source file the patch series applies on top of `lparser.c`/`lcode.c` hooks, so a rebase carries one set of hook points rather than a diff through the whole parser.

**Positioning.** Once lambdas, `?.`, classes, and expression-bodied functions are in, `.dlua` reads as its own language while `.lua` files keep running unchanged. That is the "don't lead with Lua" outcome without ever breaking a Lua user.

## 9. Sigil allocation

| Form | Meaning | Status |
|---|---|---|
| `$"..."` | interpolation | shipped |
| `??` | null coalesce | shipped |
| `1.23d` (letters after a numeral) | literal suffix registry | shipped |
| `?.` `?[` `?:` `?(` | null-safe navigation | proposed |
| `?` postfix | try | speculative |
| `@` | `self` | proposed (see 8) |
| `\|...\|` at expression start | lambda | proposed |
| `<...>` after `)` | function attributes and decorators | proposed |
| `...x` | spread | proposed |
| `[i:j]` | slice | proposed |
| `= expr` after `)` | expression-bodied function | proposed |
| `{...}` / `[...]` in binding position | destructuring | proposed |
| `\|>` | pipe | speculative |
| `:=` | local | speculative |

## 10. Suggested order

1. Tier A in one pass: they are each an afternoon and together they change how the language feels.
2. Lambdas, expression-bodied functions, `?.`, `if`-expressions: the `end`-reducers.
3. Destructuring, slicing, `export`, function attributes: the ones the numeric spec and dollup packages will lean on.
4. Classes.
5. Tier D by demand only.
