//! `_`-prefixed keys are comments. Everywhere.
//!
//! JSON has no comment syntax, and the reasoning that lived in a
//! `.host.lua`'s comments lives in `_`-prefixed keys now
//! (`examples/deployment.json` set the convention). A comment must not
//! reach a reader, for two reasons that pull the same way. Every config
//! struct refuses a key it does not know (issue #31: a misspelled bound was
//! a bound that silently did not apply, and `creat` for `create` was a
//! database Litestream could not replicate while `/health` answered 200),
//! so a comment left in place would be refused as a typo. And a map whose
//! keys are names, such as `relay.labels`, read a comment as a name with a
//! value of the wrong shape and refused it naming the struct rather than
//! the convention.
//!
//! One rule, no exemptions: a key beginning with [`MARKER`] is removed
//! wherever it sits -- in a struct, in a map of names, inside a connector's
//! `scope`, inside `args` -- before anything reads the document. The old
//! caveat that a connector's scope was passed through verbatim, with an
//! underscore key there being data, is gone with it; no connector ever read
//! one. A comment is a comment because of how it is spelled, not because
//! of where it sits, which is the property the file's author can see.
//!
//! Removed in stream order, not by way of a `serde_json::Value`. A
//! `Value`'s object sorts its keys in one build and keeps them in another
//! (`_preserve-order-test`), and a loader must write the same bytes either
//! way: a connector's `scope` is the map the file spelled, in the order the
//! file spelled it, and the corpus snapshots hold the loader to that.
//! [`Strip`] wraps the deserializer reading the text and drops a comment
//! key before the reader sees it; [`strip`] is the same rule on a `Value`
//! already in hand, for a caller that has one and no order to keep.
//!
//! Pure. Here rather than in drt because dollup reads the same documents
//! and should strip them the same way.
//!
//! ## surface block
//!
//! - Entry points: [`Strip`], the adapter the loader reads through;
//!   [`strip`], the rule on a parsed `Value`; [`is_comment`], the rule for
//!   one key.
//! - Configurable values: [`MARKER`], the prefix that makes a key a comment.
//! - Fan-out: none. Maps are filtered, sequences and enums are walked,
//!   scalars pass through.

use std::fmt;

use serde::de::{
    self, DeserializeSeed, Deserializer, EnumAccess, IgnoredAny, IntoDeserializer, MapAccess,
    SeqAccess, VariantAccess, Visitor,
};
use serde_json::Value;

/// The prefix that makes a key a comment.
pub const MARKER: char = '_';

/// Whether a key is a comment.
pub fn is_comment(key: &str) -> bool {
    key.starts_with(MARKER)
}

/// Remove every comment key from `value`, at every depth.
pub fn strip(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| !is_comment(key));
            for child in map.values_mut() {
                strip(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip(item);
            }
        }
        _ => {}
    }
}

/// A deserializer that reads through another and drops every comment key
/// before the reader sees it, at every depth, leaving everything else as
/// and where the document has it.
pub struct Strip<D>(pub D);

// depth: forwarding every serde hook through the filter
//
// Every `deserialize_*` hands the inner deserializer a `Wrap`ped visitor.
// `Wrap` hands `Map` the map access, `Seq` the sequence access and `Enum`
// the enum access, and each of those wraps the seeds it is given so that a
// nested value is read through `Strip` again. `Map` is the one with a job:
// it reads each key as a string first, skips a comment and its value, and
// offers any other key to the caller's seed as the string it was.

macro_rules! forward_deserialize {
    ($de:lifetime; $($method:ident),* $(,)?) => {
        $(
            fn $method<V: Visitor<$de>>(self, visitor: V) -> Result<V::Value, D::Error> {
                self.0.$method(Wrap(visitor))
            }
        )*
    };
}

impl<'de, D: Deserializer<'de>> Deserializer<'de> for Strip<D> {
    type Error = D::Error;

    forward_deserialize!('de;
        deserialize_any, deserialize_bool, deserialize_i8, deserialize_i16, deserialize_i32,
        deserialize_i64, deserialize_i128, deserialize_u8, deserialize_u16, deserialize_u32,
        deserialize_u64, deserialize_u128, deserialize_f32, deserialize_f64, deserialize_char,
        deserialize_str, deserialize_string, deserialize_bytes, deserialize_byte_buf,
        deserialize_option, deserialize_unit, deserialize_seq, deserialize_map,
        deserialize_identifier, deserialize_ignored_any,
    );

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.0.deserialize_unit_struct(name, Wrap(visitor))
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.0.deserialize_newtype_struct(name, Wrap(visitor))
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.0.deserialize_tuple(len, Wrap(visitor))
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.0.deserialize_tuple_struct(name, len, Wrap(visitor))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.0.deserialize_struct(name, fields, Wrap(visitor))
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.0.deserialize_enum(name, variants, Wrap(visitor))
    }

    fn is_human_readable(&self) -> bool {
        self.0.is_human_readable()
    }
}

struct Wrap<V>(V);

macro_rules! forward_visit {
    ($($method:ident: $ty:ty),* $(,)?) => {
        $(
            fn $method<E: de::Error>(self, v: $ty) -> Result<V::Value, E> {
                self.0.$method(v)
            }
        )*
    };
}

impl<'de, V: Visitor<'de>> Visitor<'de> for Wrap<V> {
    type Value = V::Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.expecting(f)
    }

    forward_visit!(
        visit_bool: bool, visit_i8: i8, visit_i16: i16, visit_i32: i32, visit_i64: i64,
        visit_i128: i128, visit_u8: u8, visit_u16: u16, visit_u32: u32, visit_u64: u64,
        visit_u128: u128, visit_f32: f32, visit_f64: f64, visit_char: char, visit_str: &str,
        visit_borrowed_str: &'de str, visit_string: String, visit_bytes: &[u8],
        visit_borrowed_bytes: &'de [u8], visit_byte_buf: Vec<u8>,
    );

    fn visit_none<E: de::Error>(self) -> Result<V::Value, E> {
        self.0.visit_none()
    }

    fn visit_unit<E: de::Error>(self) -> Result<V::Value, E> {
        self.0.visit_unit()
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<V::Value, D::Error> {
        self.0.visit_some(Strip(d))
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(self, d: D) -> Result<V::Value, D::Error> {
        self.0.visit_newtype_struct(Strip(d))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<V::Value, A::Error> {
        self.0.visit_seq(Seq(seq))
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<V::Value, A::Error> {
        self.0.visit_map(Map(map))
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<V::Value, A::Error> {
        self.0.visit_enum(Enum(data))
    }
}

struct Seed<S>(S);

impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for Seed<S> {
    type Value = S::Value;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<S::Value, D::Error> {
        self.0.deserialize(Strip(d))
    }
}

struct Seq<A>(A);

impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for Seq<A> {
    type Error = A::Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, A::Error> {
        self.0.next_element_seed(Seed(seed))
    }

    fn size_hint(&self) -> Option<usize> {
        self.0.size_hint()
    }
}

struct Map<A>(A);

impl<'de, A: MapAccess<'de>> MapAccess<'de> for Map<A> {
    type Error = A::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, A::Error> {
        loop {
            let Some(key) = self.0.next_key::<String>()? else {
                return Ok(None);
            };
            if is_comment(&key) {
                self.0.next_value::<IgnoredAny>()?;
                continue;
            }
            let key: de::value::StringDeserializer<A::Error> = key.into_deserializer();
            return seed.deserialize(key).map(Some);
        }
    }

    fn next_value_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<T::Value, A::Error> {
        self.0.next_value_seed(Seed(seed))
    }

    fn size_hint(&self) -> Option<usize> {
        self.0.size_hint()
    }
}

struct Enum<A>(A);

impl<'de, A: EnumAccess<'de>> EnumAccess<'de> for Enum<A> {
    type Error = A::Error;
    type Variant = Variant<A::Variant>;

    fn variant_seed<T: DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<(T::Value, Self::Variant), A::Error> {
        let (value, variant) = self.0.variant_seed(Seed(seed))?;
        Ok((value, Variant(variant)))
    }
}

struct Variant<A>(A);

impl<'de, A: VariantAccess<'de>> VariantAccess<'de> for Variant<A> {
    type Error = A::Error;

    fn unit_variant(self) -> Result<(), A::Error> {
        self.0.unit_variant()
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, A::Error> {
        self.0.newtype_variant_seed(Seed(seed))
    }

    fn tuple_variant<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, A::Error> {
        self.0.tuple_variant(len, Wrap(visitor))
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, A::Error> {
        self.0.struct_variant(fields, Wrap(visitor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[test]
    fn a_comment_goes_wherever_it_sits() {
        let mut v = json!({
            "_top": "why",
            "numeric": { "_why": 1, "max_elements": 64 },
            "connectors": { "_note": "a map of names", "fs": { "scope": { "_note": "inside a scope", "scope": "." } } },
            "labels": { "_note": "a map of names", "abc": { "_who": "this machine", "park_key": "k" } },
            "list": [ { "_x": 1, "y": 2 }, 3 ]
        });
        strip(&mut v);
        assert_eq!(
            v,
            json!({
                "numeric": { "max_elements": 64 },
                "connectors": { "fs": { "scope": { "scope": "." } } },
                "labels": { "abc": { "park_key": "k" } },
                "list": [ { "y": 2 }, 3 ]
            })
        );
    }

    #[test]
    fn a_key_is_a_comment_by_its_first_character_alone() {
        assert!(is_comment("_"));
        assert!(is_comment("__"));
        assert!(is_comment("_note"));
        assert!(!is_comment("note_"));
        assert!(!is_comment("a_b"));
        assert!(!is_comment(""));
    }

    #[test]
    fn scalars_and_empties_are_left_alone() {
        for mut v in [json!(1), json!("s"), json!(null), json!([]), json!({})] {
            let before = v.clone();
            strip(&mut v);
            assert_eq!(v, before);
        }
    }

    fn s(text: &str) -> rmpv::Value {
        rmpv::Value::String(rmpv::Utf8String::from(text))
    }

    fn map(pairs: &[(&str, rmpv::Value)]) -> rmpv::Value {
        rmpv::Value::Map(pairs.iter().map(|(k, v)| (s(k), v.clone())).collect())
    }

    // rmpv keeps a map in the order it was read, which is what the
    // assertion is about: `serde_json::Value` would sort it or not by
    // build, and the loader must not.
    #[test]
    fn read_through_a_comment_goes_wherever_it_sits_and_the_rest_keeps_its_order() {
        let text = r#"{"_top":"why","z":{"_why":1,"scope":".","access":"read","_and":2,"max_bytes":1},"a":[{"_x":1,"y":2},3],"_end":0}"#;
        let mut json = serde_json::Deserializer::from_str(text);
        let got = rmpv::Value::deserialize(Strip(&mut json)).unwrap();
        json.end().unwrap();
        let want = map(&[
            (
                "z",
                map(&[
                    ("scope", s(".")),
                    ("access", s("read")),
                    ("max_bytes", rmpv::Value::from(1u64)),
                ]),
            ),
            (
                "a",
                rmpv::Value::Array(vec![
                    map(&[("y", rmpv::Value::from(2u64))]),
                    rmpv::Value::from(3u64),
                ]),
            ),
        ]);
        assert_eq!(got, want);
    }

    #[test]
    fn read_through_a_reader_that_refuses_unknown_keys_never_sees_a_comment() {
        #[derive(Deserialize, Debug, PartialEq)]
        #[serde(deny_unknown_fields)]
        struct Outer {
            n: u32,
            #[serde(flatten)]
            inner: Inner,
            shape: Shape,
            names: std::collections::BTreeMap<String, Named>,
        }
        #[derive(Deserialize, Debug, PartialEq)]
        #[serde(deny_unknown_fields)]
        struct Inner {
            k: Option<String>,
        }
        #[derive(Deserialize, Debug, PartialEq)]
        #[serde(untagged)]
        enum Shape {
            One(String),
            Full { allow: Vec<String> },
        }
        #[derive(Deserialize, Debug, PartialEq)]
        #[serde(deny_unknown_fields)]
        struct Named {
            key: String,
        }

        let text = r#"{"_c":"x","n":1,"k":"v","_d":1,"shape":{"_e":"c","allow":["a"]},"names":{"_f":"a comment, not a name","abc":{"_g":1,"key":"k"}}}"#;
        let mut json = serde_json::Deserializer::from_str(text);
        let got = Outer::deserialize(Strip(&mut json)).unwrap();
        json.end().unwrap();
        assert_eq!(
            got,
            Outer {
                n: 1,
                inner: Inner {
                    k: Some("v".into())
                },
                shape: Shape::Full {
                    allow: vec!["a".into()]
                },
                names: [("abc".to_string(), Named { key: "k".into() })].into(),
            }
        );

        let mut json =
            serde_json::Deserializer::from_str(r#"{"n":1,"nn":2,"shape":"s","names":{}}"#);
        let err = Outer::deserialize(Strip(&mut json))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field `nn`"), "{err}");

        let mut json = serde_json::Deserializer::from_str(
            r#"{"n":1,"shape":"s","names":{"abc":{"kye":"k"}}}"#,
        );
        let err = Outer::deserialize(Strip(&mut json))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field `kye`, expected `key`"), "{err}");
    }
}
