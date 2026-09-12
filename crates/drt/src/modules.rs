//! `require` for a sealed guest: the host resolves modules before the program
//! runs, and the guest looks them up. Nothing here opens a file on the guest's
//! behalf, at any point, ever.
//!
//! ## surface block
//!
//! - Entry points: [`program_load`], the whole of it — a path in, the source
//!   to load and the name to load it under back. [`discover`] and
//!   [`Modules::bootstrap`] are its two halves, public for the tests that hold
//!   each separately.
//! - Configurable values: [`EXTENSION`], [`BYTECODE_EXTENSION`],
//!   [`RESERVED_COMPONENT`], [`MAX_MODULES`], [`MAX_BYTES`], [`BOOTSTRAP_NAME`],
//!   [`BRACKET_CAP`].
//! - Fan-out: [`refuse_name`] is the name rule, one arm per refusal, and the
//!   Rust half of a rule that exists twice — see *Two copies of one rule*.
//!
//! ## Why a generated bootstrap and not `package.preload`
//!
//! The obvious shape is Lua's own: register each module in `package.preload`
//! and let stock `require` find it. That table does not exist here. A sealed
//! guest is opened without `LUA_LOADLIBK` (`dlibs.c`, `diluvium_openguestlibs`),
//! so there is no `package`, no `package.preload`, no `package.loaded`, and no
//! `require` — those arrive only with `--unsafe`, where `require` is the real
//! filesystem one this whole mechanism exists to avoid. The ABI has no preload
//! call either: `dv_register_code` looks like one and is not, being snapshot
//! dedup — it lets a snapshot carry a 32-byte hash instead of a chunk's
//! bytecode, and registering there does not make a chunk reachable by name.
//!
//! What the seal does keep is `load`, deliberately and with the reason written
//! down beside it: *"It compiles bytes the program already holds and reaches
//! nothing."* So the host reads the modules, generates a chunk that holds their
//! source, compiles each one with `load`, installs a `require` that looks up
//! the result, and finally compiles and calls the entry. No new hostcall, no
//! new capability, no ABI change, and no filesystem for the guest.
//!
//! Each module is its own `load`, which buys two things. Tracebacks name the
//! module's own file and line, because its chunk name is its path. And Lua's
//! 200-local ceiling is per function (`lparser.c`, `MAXVARS`), so a module
//! spends its own, not the entry's — the relief this was wanted for.
//!
//! **The entry is `load`ed too**, rather than the bootstrap being prepended to
//! it. Prepending would shift every line of the entry by the length of the
//! generated preamble, and every traceback and error message with it. Loaded
//! separately, the entry keeps its own name and its own line numbers, and the
//! bootstrap is invisible unless it is what failed.
//!
//! ## Two copies of one rule
//!
//! The name rule runs in two places: here, over the files found on disk, and
//! in the generated Lua, over whatever string the guest hands `require`. The
//! host cannot be in the loop for the second — that is the point of a preload
//! table — so the rule is written twice and cannot be written once.
//!
//! What can be done is make the two testably identical, and that is done:
//! [`RESERVED_COMPONENT`] and the character class are interpolated into the Lua
//! from the constants above it, and `the_two_copies_of_the_name_rule_agree`
//! runs the same table of cases through both and fails if they ever differ.
//!
//! ## What is not here
//!
//! `.dluac`. Bytecode modules are named in the design and deferred, because
//! `engine.rs` loads source with `text_only(true)` — "Source only unless
//! bytecode was explicit: GUARANTEES.md, the verifier that does not exist yet"
//! — and guest-side `load` accepts a binary chunk even under that flag, which
//! upstream records as a real defect. Reaching bytecode modules through it
//! would put unverified bytecode inside the one sandbox the entry is
//! protected from, so a `.dluac` beside the entry is refused by name rather
//! than quietly ignored. It lands when the verifier does.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The extension a module file has. One, not a list: a `.lua` beside the
/// entry is a program somebody runs directly, and promoting it to a module
/// because it parses would make every existing node's directory mean
/// something new.
pub const EXTENSION: &str = "dlua";

/// What a compiled module would be called. Refused, not ignored — see the
/// module header.
pub const BYTECODE_EXTENSION: &str = "dluac";

/// The first component no module may have. `stdlib:` is how a stdlib program
/// is named as an entry, and a module shadowing that spelling would make one
/// name mean two things.
pub const RESERVED_COMPONENT: &str = "stdlib";

/// The most modules one node may carry. Generous, and a named refusal rather
/// than a chunk so large the engine's own limits answer instead.
pub const MAX_MODULES: usize = 1_000;

/// The most module source one node may carry, in total.
pub const MAX_BYTES: u64 = 16 * 1024 * 1024;

/// The chunk name the generated bootstrap is loaded under. The leading `=` is
/// the engine's convention for a chunk name that is not a path, so a traceback
/// says `drt:modules:12:` rather than inventing a file nobody can open.
pub const BOOTSTRAP_NAME: &str = "=drt:modules";

/// How many `=` a long bracket may grow before the embedding gives up. A
/// source containing `]`, sixty-four `=`, `]` is not a thing that happens; the
/// cap is here so a bug upstream of it is a refusal and not a hang.
pub const BRACKET_CAP: usize = 64;

/// One module: what `require` calls it, where it came from, and its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    /// The dotted name `require` takes.
    pub name: String,
    /// The path relative to the node's directory — what a traceback shows,
    /// and what a refusal names.
    pub file: String,
    source: String,
}

/// Every module beside one program, and the directory they came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Modules {
    dir: String,
    modules: Vec<Module>,
}

/// What to load, and what to call it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub source: String,
    /// The chunk name. The program's own file name when there are no modules;
    /// [`BOOTSTRAP_NAME`] when there are, because then the chunk is generated
    /// and the program is loaded inside it under its own name.
    pub name: String,
    /// How many modules were found. Zero means nothing was generated and the
    /// program is loaded exactly as it was before any of this existed.
    pub modules: usize,
}

/// Read a program and whatever modules sit beside it.
///
/// The node's directory is the program's own directory: `live/<name>/` for a
/// deployed entry, and the config's directory for a `--config` run, which
/// resolves its program against itself for the same reason. Both are the
/// directory that moves as a unit, which is the property that makes a module
/// part of the node it ships with.
///
/// A program with no modules beside it is loaded as itself — same source, same
/// chunk name, nothing generated. That is the common case and it must stay
/// indistinguishable from what came before.
pub fn program_load(program: &Path) -> Result<Loaded, String> {
    let source = drt_platform::fs::read_to_string(program)
        .map_err(|e| format!("cannot read {}: {e}", program.display()))?;
    let name = program
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("program")
        .to_string();
    let modules = discover(program)?;
    if modules.is_empty() {
        return Ok(Loaded {
            source,
            name,
            modules: 0,
        });
    }
    let count = modules.len();
    Ok(Loaded {
        source: modules.bootstrap(&source, &name)?,
        name: BOOTSTRAP_NAME.to_string(),
        modules: count,
    })
}

/// Walk the directory `program` sits in and collect every module in it.
///
/// The program itself is not one. It is about to be run as the entry, and a
/// `require` of it would run it a second time — a footgun with no use, so the
/// rule is stated instead: every `.dlua` beside the entry is a module except
/// the entry.
pub fn discover(program: &Path) -> Result<Modules, String> {
    let dir = match program.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        // `--config app.json` with no directory on it: the node is the
        // working directory, the same answer `config::resolve_program` gives.
        _ => PathBuf::from("."),
    };
    if !drt_platform::fs::is_dir(&dir) {
        return Ok(Modules::default());
    }

    // The entry, by file name. Comparing whole paths would miss it: `drt run
    // app.dlua` gives a program of `app.dlua` and a directory of `.`, so the
    // walk's own spelling is `./app.dlua` and the two are the same file under
    // different names. The entry is always directly in this directory, which
    // is what makes the file name enough.
    let entry_file = program.file_name().and_then(|n| n.to_str());

    let mut found: BTreeMap<String, Module> = BTreeMap::new();
    let mut bytes: u64 = 0;

    // depth: the walk, bounded the way `deploy::copy_tree` is and for the same
    // reason — an explicit stack rather than recursion, so a deep tree is a
    // slow answer instead of a blown stack.
    let mut stack = vec![PathBuf::new()];
    while let Some(relative) = stack.pop() {
        let here = dir.join(&relative);
        let mut names = drt_platform::fs::read_dir(&here)
            .map_err(|e| format!("cannot read {}: {e}", here.display()))?;
        names.sort();
        for entry in names {
            let child = relative.join(&entry);
            let full = dir.join(&child);
            if drt_platform::fs::is_dir(&full) {
                stack.push(child);
                continue;
            }
            if relative.as_os_str().is_empty() && Some(entry.as_str()) == entry_file {
                continue;
            }
            let extension = full.extension().and_then(|e| e.to_str()).unwrap_or("");
            if extension == BYTECODE_EXTENSION {
                return Err(format!(
                    "{}: bytecode modules are not built yet -- drt loads source only until \
                     there is a verifier for them (GUARANTEES.md), and reaching one through \
                     the guest's `load` would put unverified bytecode inside the sandbox. \
                     Ship the `.{EXTENSION}` beside it, or remove this file.",
                    display(&child)
                ));
            }
            if extension != EXTENSION {
                continue;
            }
            let name = module_name(&child)?;
            let source = drt_platform::fs::read_to_string(&full)
                .map_err(|e| format!("cannot read {}: {e}", full.display()))?;
            bytes += source.len() as u64;
            if found.len() >= MAX_MODULES {
                return Err(format!(
                    "more than {MAX_MODULES} modules under {}; a node is source code, and a \
                     tree this size is one to look at before deploying",
                    dir.display()
                ));
            }
            if bytes > MAX_BYTES {
                return Err(format!(
                    "more than {} MiB of module source under {}; same reason",
                    MAX_BYTES / (1024 * 1024),
                    dir.display()
                ));
            }
            if let Some(clash) = found.insert(
                name.clone(),
                Module {
                    name: name.clone(),
                    file: display(&child),
                    source,
                },
            ) {
                // Unreachable while a component may not hold a `.`: path and
                // name are one-to-one under that rule. Here so that loosening
                // the rule is caught by a refusal rather than by one module
                // silently becoming another.
                return Err(format!(
                    "{} and {} are both the module `{name}`",
                    clash.file,
                    display(&child)
                ));
            }
        }
    }

    Ok(Modules {
        dir: display(&dir),
        modules: found.into_values().collect(),
    })
}

impl Modules {
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    pub fn len(&self) -> usize {
        self.modules.len()
    }

    /// The names, in the order they were found, which is sorted.
    pub fn names(&self) -> Vec<&str> {
        self.modules.iter().map(|m| m.name.as_str()).collect()
    }

    /// The chunk that gets loaded: the modules, a `require` over them, and the
    /// entry compiled and called under its own name.
    pub fn bootstrap(&self, entry_source: &str, entry_file: &str) -> Result<String, String> {
        let mut out = String::new();
        out.push_str(&format!(
            "-- Generated by drt ({}). This is the chunk that was loaded; the\n\
             -- program you wrote is compiled below, under its own name, so its\n\
             -- line numbers are its own.\n\
             --\n\
             -- {} module(s) beside it, each compiled here rather than on first\n\
             -- use, so a syntax error in one is a failure before any of this\n\
             -- node's code runs.\n\n",
            file!(),
            self.modules.len()
        ));

        out.push_str(&format!("local __dir = {}\n", quote(&self.dir)));
        out.push_str("local __mods = {\n");
        for module in &self.modules {
            let (open, close) = brackets(&module.source)?;
            out.push_str(&format!(
                "  {{ name = {}, file = {}, src = {}\n{}{} }},\n",
                quote(&module.name),
                quote(&module.file),
                open,
                module.source,
                close
            ));
        }
        out.push_str("}\n\n");

        out.push_str(&runtime());

        // The entry is loaded under exactly the name it would have had with
        // no modules beside it -- bare, where a module's is `@`-prefixed.
        // That asymmetry is deliberate: a module never had a chunk name
        // before and gets the one Lua uses for a file, while the entry had
        // one, and adding a module to a node must not change the shape of
        // that node's error messages.
        let (open, close) = brackets(entry_source)?;
        out.push_str(&format!(
            "\nlocal __entry, __entry_why = load({}\n{}{}, {})\n\
             if not __entry then error(tostring(__entry_why), 0) end\n\
             return __entry()\n",
            open,
            entry_source,
            close,
            quote(entry_file)
        ));
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// The generated runtime
// ---------------------------------------------------------------------------

/// The character class the Lua half tests with: what [`is_name_char`] accepts,
/// plus the `.` that separates components.
const NAME_CLASS_LUA: &str = "A-Za-z0-9_.";

/// `require`, and the rule it enforces, as the guest sees them.
///
/// The refusal strings are character-for-character the ones [`refuse_name`]
/// returns, which is what lets `the_two_copies_of_the_name_rule_agree` compare
/// them directly instead of comparing "did each refuse".
fn runtime() -> String {
    RUNTIME_TEMPLATE
        .replace("<<RESERVED>>", RESERVED_COMPONENT)
        .replace("<<CLASS>>", NAME_CLASS_LUA)
}

const RUNTIME_TEMPLATE: &str = r#"
local __chunk, __loaded, __loading = {}, {}, {}

for i = 1, #__mods do
  local m = __mods[i]
  -- `@` before the name is what makes a traceback print `db/claims.dlua:3:`
  -- rather than quoting the whole source back at the reader.
  local chunk, why = load(m.src, "@" .. m.file)
  if not chunk then
    error("drt modules: " .. m.file .. ": " .. tostring(why), 0)
  end
  __chunk[m.name] = chunk
end

-- Why this is not a module name, or nil if it is. The host holds the same
-- rule over the files it found; this one is for whatever the guest asks for,
-- which the host never sees.
local function __refusal(name)
  if type(name) ~= "string" then
    return "a module name must be a string, got " .. type(name)
  end
  if name == "" then
    return "a module name may not be empty"
  end
  if name:find("%.%.", 1) then
    return "`..` is not a module name component"
  end
  if name:sub(1, 1) == "." or name:sub(-1) == "." then
    return "a module name may not begin or end with `.`"
  end
  if name:find("[^<<CLASS>>]") then
    return "a module name holds letters, digits, `_` and the `.` between components"
  end
  if name:match("^[^.]+") == "<<RESERVED>>" then
    return "`<<RESERVED>>` is reserved -- a stdlib program is reached by the `<<RESERVED>>:` entry spelling, never by require"
  end
  return nil
end

function require(name)
  local why = __refusal(name)
  if why then
    error("require(" .. tostring(name) .. "): " .. why, 0)
  end
  -- A module that returned `false` is still a module that ran, so the cache
  -- is tested against nil and not for truth.
  local hit = __loaded[name]
  if hit ~= nil then
    return hit
  end
  local chunk = __chunk[name]
  if chunk == nil then
    error("require(" .. name .. "): no such module in " .. __dir, 0)
  end
  if __loading[name] then
    -- Without this the failure is a stack overflow, which names neither
    -- module in the cycle.
    error("require(" .. name .. "): required while it was still loading -- a cycle", 0)
  end
  __loading[name] = true
  local ok, value = pcall(chunk, name)
  __loading[name] = nil
  if not ok then
    error(value, 0)
  end
  -- Lua's own contract: a module that returns nothing is `true`, so that a
  -- second require can tell "ran, returned nothing" from "never ran".
  if value == nil then
    value = true
  end
  __loaded[name] = value
  return value
end
"#;

// ---------------------------------------------------------------------------
// The name rule
// ---------------------------------------------------------------------------

/// A module's name, from its path relative to the node's directory.
///
/// Dots are the separator, so a path component may not contain one: with
/// `my.helper.dlua` allowed, `my.helper` would name both it and
/// `my/helper.dlua`, and one of the two would win silently.
fn module_name(relative: &Path) -> Result<String, String> {
    let shown = display(relative);
    let mut components = Vec::new();
    for (index, part) in relative.components().enumerate() {
        let std::path::Component::Normal(part) = part else {
            return Err(format!(
                "{shown}: a module path holds names and nothing else -- no `..`, no root"
            ));
        };
        let part = part
            .to_str()
            .ok_or_else(|| format!("{shown}: not valid UTF-8, so it cannot be a module name"))?;
        let last = index + 1 == relative.components().count();
        let stem = if last {
            part.strip_suffix(&format!(".{EXTENSION}")).unwrap_or(part)
        } else {
            part
        };
        if stem.is_empty() || !stem.chars().all(is_name_char) {
            return Err(format!(
                "{shown}: `{stem}` cannot be part of a module name, which holds letters, \
                 digits and `_` between the dots"
            ));
        }
        components.push(stem);
    }
    let name = components.join(".");
    if let Some(why) = refuse_name(&name) {
        return Err(format!("{shown}: {why}"));
    }
    Ok(name)
}

/// A character a single name component may hold. The dot is the separator and
/// so is not one of these.
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Why this is not a module name, or `None` if it is.
///
/// The Rust half of the rule the generated Lua also carries; they are held
/// identical by `the_two_copies_of_the_name_rule_agree`.
pub fn refuse_name(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("a module name may not be empty".into());
    }
    if name.contains("..") {
        return Some("`..` is not a module name component".into());
    }
    if name.starts_with('.') || name.ends_with('.') {
        return Some("a module name may not begin or end with `.`".into());
    }
    if !name.chars().all(|c| is_name_char(c) || c == '.') {
        return Some(
            "a module name holds letters, digits, `_` and the `.` between components".into(),
        );
    }
    if name.split('.').next() == Some(RESERVED_COMPONENT) {
        return Some(format!(
            "`{RESERVED_COMPONENT}` is reserved -- a stdlib program is reached by the \
             `{RESERVED_COMPONENT}:` entry spelling, never by require"
        ));
    }
    None
}

// ---------------------------------------------------------------------------
// depth: embedding source in a chunk
// ---------------------------------------------------------------------------

/// A long-bracket pair that `source` cannot terminate.
///
/// Only the closing sequence can end a long string — an opening one inside is
/// ordinary text — so that is the only one that has to be absent.
fn brackets(source: &str) -> Result<(String, String), String> {
    for level in 0..=BRACKET_CAP {
        let equals = "=".repeat(level);
        let close = format!("]{equals}]");
        if !source.contains(&close) {
            return Ok((format!("[{equals}["), close));
        }
    }
    Err(format!(
        "this source cannot be embedded: it contains every long-bracket close from `]]` to \
         {BRACKET_CAP} `=` deep"
    ))
}

/// A Rust string as a Lua short string.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\{}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A path as a module file is named: `/` on every platform, because the name
/// goes into a chunk name a traceback prints and into a refusal a person
/// reads, and neither should change shape with the host.
fn display(path: &Path) -> String {
    let shown = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            std::path::Component::CurDir => None,
            std::path::Component::ParentDir => Some("..".to_string()),
            std::path::Component::RootDir => Some(String::new()),
            std::path::Component::Prefix(p) => Some(p.as_os_str().to_string_lossy().into_owned()),
        })
        .collect::<Vec<_>>()
        .join("/");
    // `.` drops to nothing once `CurDir` is filtered out, and "no such module
    // in " with the directory missing is the one place this string is read.
    if shown.is_empty() {
        ".".to_string()
    } else {
        shown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node laid out in the in-memory filesystem, with the lock that makes
    /// that safe. `testfs` and not a tempdir for the reason its header gives:
    /// the backend is installed for the process, so a test that goes to the
    /// real disk while another holds a `MemFs` reads from the `MemFs`.
    fn node(files: &[(&str, &str)]) -> (crate::testfs::Seeded, PathBuf) {
        let seeded = crate::testfs::seed(true, files);
        (seeded, PathBuf::from("/r/n/entry.dlua"))
    }

    #[test]
    fn a_module_beside_the_entry_is_found_and_the_entry_is_not() {
        let (_fs, entry) = node(&[
            ("/r/n/entry.dlua", "print('hi')\n"),
            ("/r/n/enc.dlua", "return {}\n"),
            ("/r/n/util/deep.dlua", "return {}\n"),
        ]);
        let found = discover(&entry).unwrap();
        assert_eq!(found.names(), vec!["enc", "util.deep"]);
    }

    /// Sorted, so two runs of the same tree generate the same chunk and a
    /// tree with two broken modules always names the same one first.
    #[test]
    fn the_order_is_the_name_order_and_not_the_directory_order() {
        let (_fs, entry) = node(&[
            ("/r/n/entry.dlua", ""),
            ("/r/n/zebra.dlua", "return {}\n"),
            ("/r/n/alpha.dlua", "return {}\n"),
            ("/r/n/m/beta.dlua", "return {}\n"),
        ]);
        assert_eq!(
            discover(&entry).unwrap().names(),
            vec!["alpha", "m.beta", "zebra"]
        );
    }

    /// The property every node that existed before this did depends on.
    #[test]
    fn a_program_with_nothing_beside_it_is_loaded_as_itself() {
        let (_fs, entry) = node(&[
            ("/r/n/entry.dlua", "print('hi')\n"),
            ("/r/n/app.json", "{}"),
            ("/r/n/notes.md", "# not a module"),
            // A `.lua` is a program somebody runs, not a module.
            ("/r/n/supervisor.lua", "print('also not')\n"),
        ]);
        let loaded = program_load(&entry).unwrap();
        assert_eq!(loaded.modules, 0);
        assert_eq!(loaded.source, "print('hi')\n", "the file's own source");
        assert_eq!(loaded.name, "entry.dlua", "under its own name");
    }

    #[test]
    fn bytecode_beside_the_entry_is_refused_rather_than_ignored() {
        let (_fs, entry) = node(&[("/r/n/entry.dlua", ""), ("/r/n/util/enc.dluac", "\x1bLua")]);
        let e = discover(&entry).unwrap_err();
        assert!(e.contains("util/enc.dluac"), "{e}");
        assert!(e.contains("verifier"), "it says what is missing: {e}");
    }

    /// A file that cannot be spelled as a module name is a file somebody
    /// meant to require. Loud, because the alternative is a module that is
    /// silently not there.
    #[test]
    fn a_file_whose_name_is_not_a_module_name_is_refused_by_name() {
        let (_fs, entry) = node(&[
            ("/r/n/entry.dlua", ""),
            ("/r/n/my-helper.dlua", "return {}"),
        ]);
        let e = discover(&entry).unwrap_err();
        assert!(e.contains("my-helper"), "{e}");
    }

    /// `my.helper.dlua` would be the module `my.helper`, and so would
    /// `my/helper.dlua`. Refusing the dot in a component is what keeps path
    /// and name one-to-one.
    #[test]
    fn a_dot_inside_a_component_is_refused_because_it_would_be_ambiguous() {
        let (_fs, entry) = node(&[
            ("/r/n/entry.dlua", ""),
            ("/r/n/my.helper.dlua", "return {}"),
        ]);
        let e = discover(&entry).unwrap_err();
        assert!(e.contains("my.helper"), "{e}");
    }

    #[test]
    fn a_reserved_first_component_is_refused_on_disk_too() {
        let (_fs, entry) = node(&[
            ("/r/n/entry.dlua", ""),
            ("/r/n/stdlib/relay.dlua", "return {}"),
        ]);
        let e = discover(&entry).unwrap_err();
        assert!(e.contains("reserved"), "{e}");
    }

    #[test]
    fn the_name_rule_refuses_what_the_design_says_it_refuses() {
        for bad in [
            "",
            "..secret",
            "a..b",
            "/etc/x",
            "util/enc",
            ".leading",
            "trailing.",
            "has space",
            "stdlib",
            "stdlib.anything",
        ] {
            assert!(refuse_name(bad).is_some(), "should refuse {bad:?}");
        }
        for good in ["a", "enc", "util.enc", "db.claims", "a_b.c1", "stdlibx.y"] {
            assert!(refuse_name(good).is_none(), "should accept {good:?}");
        }
    }

    /// The embedding has to survive source that contains its own delimiter.
    #[test]
    fn a_module_holding_a_long_bracket_close_gets_a_deeper_one() {
        let (open, close) = brackets("local s = [[a]]\n").unwrap();
        assert_eq!((open.as_str(), close.as_str()), ("[=[", "]=]"));
        let (open, close) = brackets("local s = [=[a]=]\n").unwrap();
        assert_eq!((open.as_str(), close.as_str()), ("[[", "]]"));
        // An opening sequence inside is ordinary text and forces nothing.
        let (open, _) = brackets("local s = \"[==[\"\n").unwrap();
        assert_eq!(open, "[[");
    }

    /// The newline after an opening long bracket is swallowed by Lua, which
    /// is what makes the embedded source exactly the file's source.
    #[test]
    fn the_generated_chunk_holds_each_source_verbatim() {
        let (_fs, entry) = node(&[
            ("/r/n/entry.dlua", "return 1\n"),
            ("/r/n/m.dlua", "local s = [[a]]\nreturn s\n"),
        ]);
        let found = discover(&entry).unwrap();
        let chunk = found.bootstrap("return 1\n", "entry.dlua").unwrap();
        assert!(
            chunk.contains("[=[\nlocal s = [[a]]\nreturn s\n]=]"),
            "source embedded verbatim under a deeper bracket:\n{chunk}"
        );
    }

    /// `drt run app.dlua` walks `.`, and a refusal that says "no such module
    /// in " with nothing after it names no directory at all.
    #[test]
    fn the_working_directory_is_shown_as_a_dot_and_not_as_nothing() {
        assert_eq!(display(Path::new(".")), ".");
        assert_eq!(display(Path::new("")), ".");
        assert_eq!(display(Path::new("a/b")), "a/b");
    }

    #[test]
    fn quoting_escapes_what_would_end_a_lua_string() {
        assert_eq!(quote(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(quote("a\nb"), r#""a\nb""#);
    }
}
