//! Load-time modules through the real binary: `doc/Modules.md`'s acceptance
//! list, plus the one thing only an end-to-end run can hold — that the rule
//! the host applies to files and the rule the guest applies to `require`
//! arguments are the same rule.
//!
//! ## surface block
//!
//! - Entry points: [`drt`], the binary under test; [`node`], a directory with
//!   an entry and whatever modules a test wants beside it.
//! - Configurable values: none. Every name here is a module name a test
//!   chooses.
//! - Fan-out: the acceptance items, one test each, named for what they hold.
//!
//! `.dluac` (design item 4) and `diluvium analyze` (item 8) are not here.
//! Bytecode modules are deferred — `modules.rs` says why, and the unit test
//! beside it holds the refusal. `analyze` is a tool in the C core with no CLI
//! on this side, so "the analyzer and the loader name the same set" has
//! nothing to run against yet; `the_generated_chunk_names_every_module` holds
//! the loader's half of it.

use std::path::{Path, PathBuf};
use std::process::Command;

fn drt() -> Command {
    Command::new(env!("CARGO_BIN_EXE_drt"))
}

/// A node: an entry, and the modules beside it. Returns the tempdir (hold it)
/// and the entry's path.
fn node(entry: &str, modules: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("entry.dlua"), entry).unwrap();
    for (path, source) in modules {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, source).unwrap();
    }
    let entry = dir.path().join("entry.dlua");
    (dir, entry)
}

/// Run it and give back (stdout, stderr, ok).
fn run(entry: &Path) -> (String, String, bool) {
    let out = drt().arg("run").arg("-f").arg(entry).output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// The entry is not one of its own modules, however it was spelled on the
/// command line.
///
/// `drt run app.dlua` from inside the directory gives a program of
/// `app.dlua` and a directory of `.`, so the walk finds `./app.dlua` — the
/// same file under a different name. Comparing paths missed it, the entry
/// became a module called `app`, a bootstrap was generated for a node with
/// no modules at all, and `require` appeared in a guest that is supposed not
/// to have one. The examples gate caught it; this holds it.
#[test]
fn a_program_run_by_a_relative_name_is_not_a_module_of_its_own() {
    let (dir, _entry) = node("print(\"require is\", type(require))\n", &[]);
    let out = drt()
        .arg("run")
        .arg("entry.dlua")
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("require is\tnil"),
        "a node with no modules generates nothing, so the seal is untouched: {stdout}"
    );
}

/// Acceptance 1: a module's return value, and the same value the second time.
#[test]
fn a_required_module_returns_its_value_and_runs_once() {
    let (_dir, entry) = node(
        r#"
local enc = require("util.enc")
print("hex", enc.hex(255))
print("same", require("util.enc") == enc)
print("ran", enc.runs)
"#,
        &[(
            "util/enc.dlua",
            r#"
local M = { runs = (RUNS or 0) + 1 }
RUNS = M.runs
function M.hex(n) return string.format("%x", n) end
return M
"#,
        )],
    );
    let (stdout, stderr, ok) = run(&entry);
    assert!(ok, "stderr: {stderr}");
    assert!(stdout.contains("hex\tff"), "{stdout}");
    assert!(
        stdout.contains("same\ttrue"),
        "one table, not two: {stdout}"
    );
    assert!(
        stdout.contains("ran\t1"),
        "the chunk ran once, not once per require: {stdout}"
    );
}

/// A module that returns nothing is `true`, as in Lua — so a second `require`
/// can tell "ran and returned nothing" from "never ran".
#[test]
fn a_module_that_returns_nothing_is_true() {
    let (_dir, entry) = node(
        "print(\"value\", require(\"quiet\"))\n",
        &[("quiet.dlua", "local x = 1\n")],
    );
    let (stdout, stderr, ok) = run(&entry);
    assert!(ok, "stderr: {stderr}");
    assert!(stdout.contains("value\ttrue"), "{stdout}");
}

/// Acceptance 2 and 5, as `doc/Modules.md` restates them: a bad name is
/// refused **at call**, by name, with the reason. The design said "at load";
/// the host only ever sees the directory, so a name that reaches `require`
/// cannot be refused before the call that passes it.
#[test]
fn a_bad_name_is_refused_at_the_call_with_its_reason() {
    for (name, expected) in [
        ("..secret", "`..` is not a module name component"),
        ("/etc/x", "a module name holds letters"),
        ("util/enc", "a module name holds letters"),
        ("stdlib.anything", "is reserved"),
        ("", "may not be empty"),
        ("trailing.", "may not begin or end with"),
    ] {
        let (_dir, entry) = node(
            &format!("require({:?})\n", name),
            &[("util/enc.dlua", "return {}\n")],
        );
        let (_stdout, stderr, ok) = run(&entry);
        assert!(!ok, "require({name:?}) should refuse");
        assert!(
            stderr.contains(expected),
            "require({name:?}) should say {expected:?}, said: {stderr}"
        );
        assert!(
            stderr.contains(name) || name.is_empty(),
            "and should name it: {stderr}"
        );
    }
}

/// Acceptance 3: a name that is well-formed and absent names the module and
/// the directory searched, because "no such module" without the directory is
/// not actionable when a deploy may have put you somewhere else.
#[test]
fn a_missing_module_names_itself_and_the_directory() {
    let (dir, entry) = node("require(\"nope\")\n", &[("util/enc.dlua", "return {}\n")]);
    let (_stdout, stderr, ok) = run(&entry);
    assert!(!ok);
    assert!(stderr.contains("nope"), "{stderr}");
    assert!(
        stderr.contains(dir.path().to_str().unwrap()),
        "the directory searched: {stderr}"
    );
}

/// Acceptance 5's other half: `stdlib:` still resolves, and reserving the
/// component did not disturb it.
#[test]
fn the_stdlib_entry_spelling_still_works() {
    let out = drt()
        .arg("run")
        .arg("-p")
        .arg("preflight")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    // No root here, so preflight names no profile -- which is the pre-existing
    // answer and, for this test, proof that `stdlib:` resolved far enough to
    // give it rather than being refused as a module name.
    assert!(
        stderr.contains("names no profile"),
        "stdlib resolution is untouched: {stderr}"
    );
}

/// Acceptance 6, both halves, in two runs of one program: a module the
/// program writes **while running** is not visible to it, and is visible the
/// next time it loads.
///
/// The program writes the file itself rather than the test writing it, which
/// is what makes this the live case: the walk has already happened by the time
/// any guest code runs, so the table cannot grow no matter who writes what.
/// That is the property the design wants — what `diluvium analyze` saw over
/// the directory is what the loader saw.
#[cfg(feature = "connector-fs")]
#[test]
fn a_module_written_while_running_is_not_visible_until_the_next_load() {
    let (dir, entry) = node(
        r#"
-- pcall, because a missing module raises: without it this program would die
-- on the first line and never reach the write that is the point of the test.
print("before", (pcall(require, "late")))
host.call("fs/write", { path = "late.dlua", data = "return { v = 1 }\n" })
print("after", (pcall(require, "late")))
"#,
        &[("early.dlua", "return {}\n")],
    );
    std::fs::write(
        dir.path().join("drt.json"),
        format!(
            r#"{{ "caps": [{{ "capability": "host:fs/*" }}],
                  "connectors": {{ "fs": {{ "scope": {{
                      "scope": {:?}, "access": "readwrite", "max_bytes": 65536 }} }} }} }}"#,
            dir.path().to_str().unwrap()
        ),
    )
    .unwrap();

    let with_config = |entry: &Path| {
        let out = drt()
            .arg("run")
            .arg("-f")
            .arg(entry)
            .arg("--config")
            .arg(dir.path().join("drt.json"))
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    // First load: `late.dlua` does not exist when the walk runs. The program
    // creates it and requires it again, and the table has not grown, so the
    // second require refuses exactly as the first did.
    let (stdout, stderr) = with_config(&entry);
    assert!(
        stdout.contains("before\tfalse"),
        "absent at load, so absent: {stdout} {stderr}"
    );
    assert!(
        stdout.contains("after\tfalse"),
        "still absent after the program wrote it -- the table is fixed at \
         load: {stdout} {stderr}"
    );
    assert!(
        dir.path().join("late.dlua").is_file(),
        "and the file really was written, so it is the table that did not \
         change and not the disk: {stdout} {stderr}"
    );

    // Second load: the walk sees it now, so both requires answer.
    let (stdout, stderr) = with_config(&entry);
    assert!(
        stdout.contains("before\ttrue") && stdout.contains("after\ttrue"),
        "after a reload it is there: {stdout} {stderr}"
    );
}

/// A syntax error in a module is a failure before the entry's first line, and
/// it names the module's file and the line inside it.
#[test]
fn a_module_that_does_not_compile_fails_before_the_entry_runs() {
    let (_dir, entry) = node(
        "print(\"THE ENTRY RAN\")\n",
        &[("util/broken.dlua", "this is not lua\n")],
    );
    let (stdout, stderr, ok) = run(&entry);
    assert!(!ok);
    assert!(
        !stdout.contains("THE ENTRY RAN"),
        "the entry must not have run: {stdout}"
    );
    assert!(stderr.contains("util/broken.dlua"), "{stderr}");
    assert!(stderr.contains("syntax error"), "{stderr}");
}

/// The reason each module is its own `load` and the bootstrap is not
/// prepended: both files keep their own name and their own line numbers.
#[test]
fn a_traceback_names_the_module_and_the_entry_at_their_own_lines() {
    let (_dir, entry) = node(
        "local enc = require(\"util.enc\")\nenc.boom()\n",
        &[(
            "util/enc.dlua",
            "local M = {}\nfunction M.boom()\n  error(\"inside\")\nend\nreturn M\n",
        )],
    );
    let (_stdout, stderr, ok) = run(&entry);
    assert!(!ok);
    assert!(
        stderr.contains("util/enc.dlua:3"),
        "the module's own line: {stderr}"
    );
    assert!(
        // `[string "entry.dlua"]:2`, which is the shape `drt run entry.dlua`
        // gives with no modules in sight. Adding one does not change it.
        stderr.contains("entry.dlua\"]:2"),
        "and the entry's own line, unshifted by the generated chunk: {stderr}"
    );
}

/// A cycle is a named failure and not a stack overflow, which would name
/// neither module in it.
#[test]
fn a_require_cycle_is_named() {
    let (_dir, entry) = node(
        "require(\"a\")\n",
        &[
            ("a.dlua", "local b = require(\"b\")\nreturn {}\n"),
            ("b.dlua", "local a = require(\"a\")\nreturn {}\n"),
        ],
    );
    let (_stdout, stderr, ok) = run(&entry);
    assert!(!ok);
    assert!(stderr.contains("a cycle"), "{stderr}");
    assert!(!stderr.contains("stack overflow"), "{stderr}");
}

/// Acceptance 7: requiring granted nothing. The same denied call is denied
/// identically whether it sits in a module or in the entry.
#[test]
fn a_module_holds_exactly_the_caps_the_node_holds() {
    const CALL: &str = "host.call(\"host:fs/read\", { path = \"/etc/hostname\" })\n";
    let (_dir, from_module) = node(
        "require(\"reader\").peek()\n",
        &[(
            "reader.dlua",
            &format!("local M = {{}}\nfunction M.peek()\n  {CALL}end\nreturn M\n"),
        )],
    );
    let (_out, module_stderr, module_ok) = run(&from_module);

    let (_dir2, direct) = node(CALL, &[]);
    let (_out, direct_stderr, direct_ok) = run(&direct);

    assert!(!module_ok && !direct_ok, "both denied");
    let denial = "denied: no connector is wired for 'host:fs/read'";
    assert!(module_stderr.contains(denial), "{module_stderr}");
    assert!(
        direct_stderr.contains(denial),
        "the same refusal, not a different one: {direct_stderr}"
    );
}

/// Acceptance 8, the half that has something to run against: the loader's
/// table is every module in the directory and nothing else.
#[test]
fn the_generated_chunk_names_every_module() {
    let (_dir, entry) = node(
        r#"
local names = {}
for _, n in ipairs({ "a", "b.c", "b.d", "e_1" }) do
  names[#names + 1] = n .. "=" .. tostring(pcall(require, n))
end
print(table.concat(names, " "))
"#,
        &[
            ("a.dlua", "return {}\n"),
            ("b/c.dlua", "return {}\n"),
            ("b/d.dlua", "return {}\n"),
            ("e_1.dlua", "return {}\n"),
            // Not modules, and so not required-able.
            ("b/notes.md", "# no"),
            ("b/prog.lua", "print('no')\n"),
        ],
    );
    let (stdout, stderr, ok) = run(&entry);
    assert!(ok, "stderr: {stderr}");
    assert!(
        stdout.contains("a=true b.c=true b.d=true e_1=true"),
        "{stdout}"
    );
}

/// Nested requires work, because a module is `load`ed against the same
/// globals the entry has and so sees the same `require`.
#[test]
fn a_module_may_require_another_module() {
    let (_dir, entry) = node(
        "print(require(\"top\").deep)\n",
        &[
            ("top.dlua", "return { deep = require(\"nested.leaf\").v }\n"),
            ("nested/leaf.dlua", "return { v = \"reached\" }\n"),
        ],
    );
    let (stdout, stderr, ok) = run(&entry);
    assert!(ok, "stderr: {stderr}");
    assert!(stdout.contains("reached"), "{stdout}");
}

/// The node's directory is the program's, under `start` as under `run`, and
/// it does not depend on where the operator is standing.
///
/// This is the other half of "a relative `program` path resolves against the
/// config": the program is found beside its config, and so are its modules.
#[test]
fn start_finds_the_modules_beside_the_program_from_any_directory() {
    let (dir, _entry) = node(
        "print(\"tag\", require(\"util.enc\").tag())\n",
        &[(
            "util/enc.dlua",
            "local M = {}\nfunction M.tag() return \"reached\" end\nreturn M\n",
        )],
    );
    std::fs::write(
        dir.path().join("app.json"),
        r#"{ "program": { "path": "entry.dlua" } }"#,
    )
    .unwrap();

    // From a directory that is not the node's, so nothing can be found by
    // accident of the working directory.
    let elsewhere = tempfile::tempdir().unwrap();
    let out = drt()
        .arg("start")
        .arg("--config")
        .arg(dir.path().join("app.json"))
        .current_dir(elsewhere.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("tag\treached"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The one test that holds the duplicated rule: every case goes through the
/// host's `refuse_name` and through the generated Lua's `__refusal`, and the
/// two must return the same string.
///
/// They are written twice because they cannot be written once — the host is
/// not in the loop when a guest calls `require`. This is what stops the two
/// drifting.
#[test]
fn the_two_copies_of_the_name_rule_agree() {
    const CASES: &[&str] = &[
        "",
        "a",
        "enc",
        "util.enc",
        "db.claims",
        "a_b.c1",
        "..secret",
        "a..b",
        "/etc/x",
        "util/enc",
        ".leading",
        "trailing.",
        "has space",
        "stdlib",
        "stdlib.anything",
        "stdlibx.y",
        "Mixed.Case_9",
        "dot.dot..dot",
        "tab\there",
    ];

    // The guest's half, asked over the same list. A node with one module, so
    // a bootstrap is generated at all, and an entry that prints the Lua
    // rule's answer for each case.
    let mut program = String::from("local cases = {\n");
    for case in CASES {
        program.push_str(&format!("  {case:?},\n"));
    }
    program.push_str(
        "}\n\
         for i = 1, #cases do\n\
         \x20 local ok, why = pcall(require, cases[i])\n\
         \x20 if ok then\n\
         \x20   print(\"ACCEPT\")\n\
         \x20 else\n\
         \x20   print(\"REFUSE\" .. \"\\t\" .. why)\n\
         \x20 end\n\
         end\n",
    );

    let (_dir, entry) = node(&program, &[("only.dlua", "return {}\n")]);
    let (stdout, stderr, _ok) = run(&entry);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        CASES.len(),
        "one answer per case; stderr: {stderr}"
    );

    for (case, line) in CASES.iter().zip(lines) {
        let host = drt::modules::refuse_name(case);
        match (host, line) {
            // The host refuses and so does the guest, with the same words.
            (Some(why), line) => {
                let guest = line
                    .strip_prefix("REFUSE\t")
                    .unwrap_or_else(|| panic!("{case:?}: host refused, guest did not: {line}"));
                let guest = guest
                    .strip_prefix(&format!("require({case}): "))
                    .unwrap_or(guest);
                assert_eq!(guest, why, "{case:?}: the two rules disagree");
            }
            // The host accepts it, so the guest must not refuse it *as a
            // name*. It may still be absent, which is a different answer.
            (None, line) => {
                if let Some(rest) = line.strip_prefix("REFUSE\t") {
                    assert!(
                        rest.contains("no such module"),
                        "{case:?}: host accepted the name, guest refused it: {rest}"
                    );
                }
            }
        }
    }
}

/// The release smoke, as a test: `drt run smoke.lua` at the root of a
/// checkout. The walk treats the entry's directory as the node, so it met
/// `examples/25-modules/app.dlua`, which no `require` could name, and refused
/// the run. A directory that is not a component is not entered now; a file
/// under one that is stays a module, and a badly named file there stays a
/// refusal (`a_dot_inside_a_component_is_refused_because_it_would_be_ambiguous`
/// in `drt_config::modules`).
///
/// This lived only in `release.yml` before, which runs on a rehearsal and not
/// on a push -- every other test built its node in an empty directory.
#[test]
fn a_program_run_from_a_directory_holding_unrelated_trees_still_runs() {
    let (_dir, entry) = node(
        "print(require(\"util.enc\").ok)\n",
        &[
            ("util/enc.dlua", "return { ok = \"reached\" }\n"),
            // Reachable by name, so a module, harmless and never required.
            ("target/debug/build.lua", "return {}\n"),
            // Not components, so not entered -- and the first would refuse,
            // the second does not even parse.
            (
                "examples/25-modules/app.dlua",
                "print(require(\"text.case\"))\n",
            ),
            (".git/hooks/pre-commit.lua", "this is not lua\n"),
            ("my-lib/x.dlua", "error('never loaded')\n"),
        ],
    );
    assert_eq!(
        drt::modules::discover(&entry).unwrap().names(),
        ["target.debug.build", "util.enc"]
    );
    let (stdout, stderr, ok) = run(&entry);
    assert!(ok, "stderr: {stderr}");
    assert!(stdout.contains("reached"), "{stdout}");
}
