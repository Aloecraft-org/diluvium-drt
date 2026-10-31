//! A value that may come from somewhere other than the file: the one shape
//! every credential in a root config takes.
//!
//! A plain string is the value itself, so every config written before this
//! type existed reads unchanged. An object with exactly one key says where
//! the value is instead:
//!
//! ```json
//! "auth": "a-literal-token",
//! "DRT-Caller-Token": { "prompt": "hidden" },
//! "X-One": { "env": "SOME_ENV" },
//! "X-Two": { "file": "%d/caller" }
//! ```
//!
//! Before this, the same idea was spelled three times per secret (`key`,
//! `key_env`, `key_file` on `turn`; `private_key`, `private_key_env`,
//! `private_key_file` on `wireguard`), and a header could only be a literal.
//! One type means one set of rules: a file loses one trailing newline, an
//! unset variable is refused by name, and a prompt with no terminal is
//! refused at once, by name, and never waits. That last one is what makes
//! `{"file": "%d/caller"}` under systemd's `LoadCredential` the natural
//! unattended setup and `{"prompt": "hidden"}` the natural attended one,
//! from the same file.
//!
//! Pure apart from [`Secret::resolve`] reading the environment and the
//! filesystem: the terminal is the caller's, passed in as a [`Prompter`],
//! because this crate builds for the browser and a browser has none.
//!
//! ## surface block
//!
//! - Entry points: [`Secret`], the field type; [`Secret::resolve`], the
//!   value now; [`Secret::describe`], where it comes from, for a message
//!   that must not print the value itself.
//! - Configurable values: none.
//! - Fan-out: [`Source`], the three places a value can be other than the
//!   file: `env`, `file`, `prompt`. [`Echo`], the two ways to prompt.

use std::fmt;
use std::path::PathBuf;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize};

/// A credential, as a config field holds it: the value, or where to find
/// it. See the module for the shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum Secret {
    /// The value, as written.
    Literal(String),
    /// Somewhere else: exactly one of `env`, `file`, `prompt`.
    From(Source),
}

/// Where a [`Secret`] that is not written in the file comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum Source {
    /// The named environment variable, read once when the value is needed.
    /// Unset is a refusal naming the variable.
    Env(String),
    /// A file's contents, less one trailing newline. Relative to the
    /// working directory, as every other path a secret names has been.
    File(PathBuf),
    /// Ask on the controlling terminal. With none (a systemd unit, a
    /// Windows scheduled task, a pipeline) the read is refused at once,
    /// naming the field; it never waits.
    Prompt(Echo),
}

/// Whether a prompted value is shown as it is typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Echo {
    /// Typed blind, as a password is.
    Hidden,
    /// Shown as typed: a name, not a secret, that should still not live in
    /// the file.
    Visible,
}

/// Reads one line from the controlling terminal: `what` names the field
/// for the prompt and for the refusal when there is no terminal. The
/// binary supplies the real one; a build with no terminal supplies one
/// that refuses.
pub type Prompter<'a> = &'a dyn Fn(&str, Echo) -> Result<String, String>;

impl Secret {
    /// The value now. `what` is the field's name in the config
    /// (`commands.p2p.headers.DRT-Caller-Token`), which every refusal
    /// names, so a unit that fails at start says which line to fix.
    pub fn resolve(&self, what: &str, prompt: Prompter<'_>) -> Result<String, String> {
        match self {
            Secret::Literal(value) => Ok(value.clone()),
            Secret::From(Source::Env(var)) => std::env::var(var)
                .map_err(|_| format!("{what}: the environment variable '{var}' is not set")),
            Secret::From(Source::File(path)) => {
                let mut text = std::fs::read_to_string(path)
                    .map_err(|e| format!("{what}: cannot read '{}': {e}", path.display()))?;
                // One trailing newline, the shape `echo > file` and every
                // credential store leave; anything more is the value's.
                if text.ends_with('\n') {
                    text.pop();
                }
                Ok(text)
            }
            Secret::From(Source::Prompt(echo)) => prompt(what, *echo),
        }
    }

    /// Where the value comes from, never the value: for a message about a
    /// field whose value must not be printed.
    pub fn describe(&self) -> String {
        match self {
            Secret::Literal(_) => "the value written in the config".into(),
            Secret::From(Source::Env(var)) => format!("the environment variable '{var}'"),
            Secret::From(Source::File(path)) => format!("the file '{}'", path.display()),
            Secret::From(Source::Prompt(Echo::Hidden)) => "a hidden prompt".into(),
            Secret::From(Source::Prompt(Echo::Visible)) => "a prompt".into(),
        }
    }

    /// The value, when it is written in the file and needs no reading.
    pub fn literal(&self) -> Option<&str> {
        match self {
            Secret::Literal(value) => Some(value),
            Secret::From(_) => None,
        }
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Secret {
        Secret::Literal(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Secret {
        Secret::Literal(value.to_string())
    }
}

// depth: reading one, with a refusal that names the key
//
// Not `#[serde(untagged)]` on the way in: an untagged enum's only error is
// "did not match any variant", which hides `envv` for `env`. A string is
// the literal; a map is read as `Source` itself, whose error names the key
// it refused and the three it takes.
impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Secret, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Secret;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(
                    "a string, or an object with one of `env`, `file` or `prompt` \
                     (`{\"env\": \"NAME\"}`, `{\"file\": \"/path\"}`, \
                     `{\"prompt\": \"hidden\"}`)",
                )
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Secret, E> {
                Ok(Secret::Literal(v.to_string()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Secret, E> {
                Ok(Secret::Literal(v))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Secret, A::Error> {
                Source::deserialize(de::value::MapAccessDeserializer::new(map)).map(Secret::From)
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(json: &str) -> Result<Secret, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
    }

    fn no_terminal(what: &str, _: Echo) -> Result<String, String> {
        Err(format!("{what}: no terminal"))
    }

    #[test]
    fn a_string_is_the_literal_and_an_object_names_its_source() {
        assert_eq!(read(r#""tok""#), Ok(Secret::Literal("tok".into())));
        assert_eq!(
            read(r#"{"env": "X"}"#),
            Ok(Secret::From(Source::Env("X".into())))
        );
        assert_eq!(
            read(r#"{"file": "%d/caller"}"#),
            Ok(Secret::From(Source::File("%d/caller".into())))
        );
        assert_eq!(
            read(r#"{"prompt": "hidden"}"#),
            Ok(Secret::From(Source::Prompt(Echo::Hidden)))
        );
        assert_eq!(
            read(r#"{"prompt": "visible"}"#),
            Ok(Secret::From(Source::Prompt(Echo::Visible)))
        );
    }

    /// A misspelled source is named, with the ones that would be taken,
    /// rather than "did not match any variant".
    #[test]
    fn a_wrong_source_is_refused_by_name() {
        let e = read(r#"{"envv": "X"}"#).unwrap_err();
        assert!(e.contains("envv") && e.contains("env"), "{e}");
        let e = read(r#"{"prompt": "blind"}"#).unwrap_err();
        assert!(e.contains("blind") && e.contains("hidden"), "{e}");
        assert!(read(r#"{"env": "X", "file": "/y"}"#).is_err());
        assert!(read("{}").is_err());
        assert!(read("42").is_err());
    }

    /// It writes back as it was read, so a config that round-trips keeps
    /// its spelling.
    #[test]
    fn it_round_trips() {
        for json in [
            r#""tok""#,
            r#"{"env":"X"}"#,
            r#"{"file":"/run/x"}"#,
            r#"{"prompt":"hidden"}"#,
        ] {
            let s = read(json).unwrap();
            assert_eq!(serde_json::to_string(&s).unwrap(), json);
        }
    }

    #[test]
    fn resolve_reads_each_source_and_names_the_field_when_it_cannot() {
        let lit = Secret::Literal("v".into());
        assert_eq!(lit.resolve("f", &no_terminal), Ok("v".into()));

        let unset = Secret::From(Source::Env("DRT_TEST_SECRET_THAT_IS_NOT_SET".into()));
        let e = unset
            .resolve("commands.p2p.headers.X", &no_terminal)
            .unwrap_err();
        assert!(
            e.contains("commands.p2p.headers.X") && e.contains("DRT_TEST_SECRET_THAT_IS_NOT_SET"),
            "{e}"
        );

        let dir = std::env::temp_dir().join(format!("drt-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cred");
        std::fs::write(&path, "s3cret\n").unwrap();
        let file = Secret::From(Source::File(path.clone()));
        assert_eq!(file.resolve("f", &no_terminal), Ok("s3cret".into()));
        std::fs::remove_dir_all(&dir).unwrap();
        let e = file.resolve("f", &no_terminal).unwrap_err();
        assert!(e.starts_with("f: cannot read"), "{e}");

        let prompt = Secret::From(Source::Prompt(Echo::Hidden));
        assert_eq!(
            prompt.resolve("the.field", &no_terminal),
            Err("the.field: no terminal".into())
        );
    }
}
