//! The aloecrypt connector: `host:aloecrypt/*`, post-quantum signatures and
//! encryption from aloecrypt_core, with every private key in the host.
//!
//! ```text
//! aloecrypt/public_key {key, alg}                 -> {alg, public_key, address}
//! aloecrypt/sign       {key, data, alg?}          -> an armored ALOECRYPT SIGNATURE; alg ml-dsa-65 (default), -44, -87
//! aloecrypt/verify     {public_key, data, signature} -> {valid} or {valid = false, reason}
//! aloecrypt/encrypt    {public_key, data}         -> an armored ALOECRYPT ENCRYPTED to that key's holder
//! aloecrypt/decrypt    {key, document, alg?}      -> the payload; alg ml-kem-768 (default), -512, -1024
//! aloecrypt/hash       {data, salt?, domain?}     -> lowercase hex Keccak-256, framed
//! ```
//!
//! Configurable values: the wiring scope is `{ key | key_env | key_file }`,
//! the master every label's key is derived from, at least [`KEY_MIN`]
//! bytes. With no scope the family is wired keyless: `verify`, `encrypt`
//! and `hash` answer, which is what a page that only checks signatures
//! needs, and the three keyed calls refuse by name. [`DATA_MAX`] bounds
//! every byte argument.
//!
//! Fan-out: [`AloecryptConnector::call_as`] is the dispatch table, one arm
//! per call. The six parameter sets fan out inside `ops`.
//!
//! ## Keys
//!
//! - **A key is a label, never bytes.** `key = "doc:1"` names the keypair
//!   derived from the master and that label under the algorithm's domain
//!   (`ops::derive_seed`). The guest never holds a private seed, so it is
//!   in neither its heap nor its snapshot, which is the crypto connector's
//!   rule carried over.
//! - **Derivation is deterministic.** Two hosts with one master derive one
//!   keypair, so a signer and a decrypter can be different processes, and
//!   nothing is stored.
//! - **Who may use a label is the grant's scope.** `sign` and `decrypt`
//!   use the private half. Their grants carry the label patterns they may
//!   name (`"doc:*"`), in the capability grammar's trailing-`*` shape, and a
//!   grant with no scope names nothing. `public_key` hands out only the
//!   public half and needs no pattern.
//! - **Public keys are text** in one encoding, kept in `ops::PublicKey`
//!   until aloecrypt_core has an armored public-key document
//!   (aloecrypt_core#6).
//!
//! ## Plugin-ready
//!
//! `ops` holds every aloecrypt call and no drt type. This file only turns
//! msgpack into `ops` arguments and `ops` results back into msgpack. When
//! the family moves to a plugin, the plugin links `ops` and this file is
//! what it replaces.

use std::sync::OnceLock;

use drt_caps::{Scope, ScopeType};
use drt_connector::{Asker, CallError, CallResult, Caller, Connector};
use zeroize::Zeroizing;

pub mod ops;
use ops::{Alg, OpError, PublicKey};

/// The shortest master accepted. A master is the key to every label.
pub const KEY_MIN: usize = 32;
/// The longest master read; the rest is ignored, as `crypto`'s is.
pub const KEY_MAX: usize = 512;
/// The longest label.
pub const LABEL_MAX: usize = 256;
/// The largest `data`, `document` or `signature` argument, in bytes. The
/// envelope is built in memory, so this bounds what one call allocates.
pub const DATA_MAX: usize = 16 * 1024 * 1024;
/// The algorithm `sign` uses when a call names none: NIST category 3.
pub const DEFAULT_SIGN_ALG: Alg = Alg::MlDsa65;
/// The algorithm `decrypt` uses when a call names none: NIST category 3.
pub const DEFAULT_KEM_ALG: Alg = Alg::MlKem768;
/// The domain `hash` frames its input under when a call names none.
pub const DEFAULT_HASH_DOMAIN: &str = "drt.aloecrypt.hash.v1";

pub struct AloecryptConnector {
    /// The master, read once at the first call. A scope is fixed for the
    /// process, so one read is the whole lifetime.
    master: OnceLock<Result<Option<Zeroizing<Vec<u8>>>, String>>,
}

impl AloecryptConnector {
    pub fn new() -> Self {
        AloecryptConnector {
            master: OnceLock::new(),
        }
    }
}

impl Default for AloecryptConnector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Connector for AloecryptConnector {
    fn scope_type(&self) -> Box<dyn ScopeType> {
        Box::new(AloecryptScopeType)
    }

    /// The caller-blind path presents as the root with no grants, so the
    /// keyed calls that need a scoped grant refuse there.
    async fn call(
        &self,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        let asker = Asker {
            caller: Caller::Root,
            grants: &[],
        };
        self.call_as(&asker, call, args, scope).await
    }

    async fn call_as(
        &self,
        asker: &Asker<'_>,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        let args = args.as_ref();
        match call {
            "aloecrypt/public_key" => do_public_key(self.master(scope)?, args),
            "aloecrypt/sign" => do_sign(self.master(scope)?, asker, args),
            "aloecrypt/verify" => do_verify(args),
            "aloecrypt/encrypt" => do_encrypt(args),
            "aloecrypt/decrypt" => do_decrypt(self.master(scope)?, asker, args),
            "aloecrypt/hash" => do_hash(args),
            other => Err(CallError::new(format!(
                "the aloecrypt connector answers public_key, sign, verify, encrypt, decrypt \
                 and hash; '{other}' is none of them"
            ))),
        }
    }
}

// depth: the six calls

fn do_public_key(master: Option<&[u8]>, args: Option<&rmpv::Value>) -> CallResult {
    const CALL: &str = "aloecrypt/public_key";
    let label = label_arg(CALL, args)?;
    let alg = match field(args, "alg") {
        Some(_) => alg_arg(CALL, args, None)?,
        None => {
            return Err(CallError::new(format!(
                "{CALL}: args.alg is required, since one label names a key under each of {}",
                alg_names()
            )))
        }
    };
    let master = need_master(CALL, master)?;
    let key = ops::public_key(alg, &ops::derive_seed(master, label, alg));
    Ok(map(vec![
        ("alg", rmpv::Value::from(alg.name())),
        ("public_key", rmpv::Value::from(key.encode())),
        (
            "address",
            rmpv::Value::from(data_encoding::HEXLOWER.encode(&key.address())),
        ),
    ]))
}

fn do_sign(master: Option<&[u8]>, asker: &Asker<'_>, args: Option<&rmpv::Value>) -> CallResult {
    const CALL: &str = "aloecrypt/sign";
    let label = label_arg(CALL, args)?;
    let alg = alg_arg(CALL, args, Some(DEFAULT_SIGN_ALG))?;
    if !alg.signs() {
        return Err(CallError::new(format!(
            "{CALL}: {} is a KEM and does not sign; use ml-dsa-44, ml-dsa-65 or ml-dsa-87",
            alg.name()
        )));
    }
    let data = bytes_arg(CALL, args, "data")?;
    let master = need_master(CALL, master)?;
    permitted(CALL, asker, label)?;
    let seed = ops::derive_seed(master, label, alg);
    ops::sign(alg, &seed, data)
        .map(rmpv::Value::from)
        .map_err(|e| CallError::new(format!("{CALL}: {e}")))
}

/// A signature that does not verify is an answer, not an error. A public
/// key that is not one is the caller's mistake and is refused.
fn do_verify(args: Option<&rmpv::Value>) -> CallResult {
    const CALL: &str = "aloecrypt/verify";
    let key = public_key_arg(CALL, args)?;
    let data = bytes_arg(CALL, args, "data")?;
    let signature = text_arg(CALL, args, "signature")?;
    let reason = match ops::verify(&key, data, signature) {
        Ok(()) => return Ok(map(vec![("valid", rmpv::Value::from(true))])),
        Err(OpError::Refused) => "mismatch",
        Err(OpError::Malformed(_)) => "malformed",
        Err(OpError::Unsupported) => "unsupported",
        Err(e @ OpError::WrongKind(_)) => return Err(CallError::new(format!("{CALL}: {e}"))),
    };
    Ok(map(vec![
        ("valid", rmpv::Value::from(false)),
        ("reason", rmpv::Value::from(reason)),
    ]))
}

/// The KEM randomness is the host's entropy, never an argument: a guest
/// that chose it could replay an encapsulation.
fn do_encrypt(args: Option<&rmpv::Value>) -> CallResult {
    const CALL: &str = "aloecrypt/encrypt";
    let key = public_key_arg(CALL, args)?;
    let data = bytes_arg(CALL, args, "data")?;
    let mut prk = Zeroizing::new([0u8; 32]);
    drt_platform::entropy::fill(&mut prk[..])
        .map_err(|e| CallError::new(format!("{CALL}: {e}")))?;
    ops::encrypt(&key, *prk, data)
        .map(rmpv::Value::from)
        .map_err(|e| CallError::new(format!("{CALL}: {e}")))
}

fn do_decrypt(master: Option<&[u8]>, asker: &Asker<'_>, args: Option<&rmpv::Value>) -> CallResult {
    const CALL: &str = "aloecrypt/decrypt";
    let label = label_arg(CALL, args)?;
    let alg = alg_arg(CALL, args, Some(DEFAULT_KEM_ALG))?;
    if alg.signs() {
        return Err(CallError::new(format!(
            "{CALL}: {} signs and does not decrypt; use ml-kem-512, ml-kem-768 or ml-kem-1024",
            alg.name()
        )));
    }
    let document = text_arg(CALL, args, "document")?;
    let master = need_master(CALL, master)?;
    permitted(CALL, asker, label)?;
    let seed = ops::derive_seed(master, label, alg);
    match ops::decrypt(alg, &seed, document) {
        Ok(payload) => Ok(rmpv::Value::Binary(payload)),
        // One refusal for wrong key, wrong recipient and altered bytes:
        // telling them apart is an oracle (aloecrypt_core's StatusCode).
        Err(OpError::Refused) => Err(CallError::new(format!(
            "{CALL}: the document does not open under '{}' ({})",
            short(label),
            alg.name()
        ))),
        Err(e) => Err(CallError::new(format!("{CALL}: {e}"))),
    }
}

fn do_hash(args: Option<&rmpv::Value>) -> CallResult {
    const CALL: &str = "aloecrypt/hash";
    let data = bytes_arg(CALL, args, "data")?;
    let salt = match field(args, "salt") {
        None => &[][..],
        Some(_) => bytes_arg(CALL, args, "salt")?,
    };
    let domain = match field(args, "domain") {
        None => DEFAULT_HASH_DOMAIN,
        Some(_) => text_arg(CALL, args, "domain")?,
    };
    Ok(rmpv::Value::from(
        data_encoding::HEXLOWER.encode(&ops::hash(salt, data, domain)),
    ))
}

// depth: the master, the grant check and argument reading

impl AloecryptConnector {
    fn master(&self, scope: Option<&Scope>) -> Result<Option<&[u8]>, CallError> {
        match self.master.get_or_init(|| load_master(scope)) {
            Ok(m) => Ok(m.as_ref().map(|m| m.as_slice())),
            Err(e) => Err(CallError::new(e.clone())),
        }
    }
}

fn need_master<'a>(call: &str, master: Option<&'a [u8]>) -> Result<&'a [u8], CallError> {
    master.ok_or_else(|| {
        CallError::new(format!(
            "{call}: this aloecrypt wiring has no master key, so it holds no keys; \
             set one of key, key_env or key_file in config.connectors.aloecrypt.scope"
        ))
    })
}

/// The wiring scope, read once. No scope is a keyless wiring.
fn load_master(scope: Option<&Scope>) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
    let Some(Scope(value)) = scope else {
        return Ok(None);
    };
    let rmpv::Value::Map(entries) = value else {
        return Err("aloecrypt: the scope must be a map: { key | key_env | key_file }".into());
    };
    let get = |name: &str| {
        entries
            .iter()
            .find(|(k, _)| as_str(k) == Some(name))
            .map(|(_, v)| v)
    };
    for (k, _) in entries {
        match as_str(k) {
            Some("key" | "key_env" | "key_file") => {}
            other => {
                return Err(format!(
                "aloecrypt: unknown scope field '{}'; the scope is {{ key | key_env | key_file }}",
                other.unwrap_or("?")
            ))
            }
        }
    }
    let named = ["key", "key_env", "key_file"]
        .iter()
        .filter(|n| get(n).is_some())
        .count();
    if named > 1 {
        return Err("aloecrypt: set only one of key, key_env and key_file".into());
    }
    let mut bytes = if let Some(path) = get("key_file") {
        let path = as_str(path).ok_or("aloecrypt: key_file must be a path")?;
        let mut b = std::fs::read(path)
            .map_err(|e| format!("aloecrypt: cannot read key_file '{path}': {e}"))?;
        if b.last() == Some(&b'\n') {
            b.pop();
        }
        b
    } else if let Some(var) = get("key_env") {
        let var = as_str(var).ok_or("aloecrypt: key_env must be a variable name")?;
        std::env::var(var)
            .map_err(|_| format!("aloecrypt: env var '{var}' (key_env) is not set"))?
            .into_bytes()
    } else if let Some(inline) = get("key") {
        as_str(inline)
            .ok_or("aloecrypt: key must be a string")?
            .as_bytes()
            .to_vec()
    } else {
        return Ok(None);
    };
    bytes.truncate(KEY_MAX);
    if bytes.len() < KEY_MIN {
        return Err(format!(
            "aloecrypt: the master key is shorter than {KEY_MIN} bytes; it is the key to every label"
        ));
    }
    Ok(Some(Zeroizing::new(bytes)))
}

struct AloecryptScopeType;

impl ScopeType for AloecryptScopeType {
    fn describe(&self) -> &str {
        "a master key: { key | key_env | key_file }, or no scope for a keyless wiring \
         (verify, encrypt and hash)"
    }

    /// The full load at startup, while the operator is still looking: an
    /// unset key_env is a configuration mistake, not a first-call surprise.
    fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
        load_master(scope).map(|_| ())
    }
}

/// The label patterns on the grants that permit this call. Each scope is
/// one pattern or an array of them; anything else names nothing.
fn permitted(call: &str, asker: &Asker<'_>, label: &str) -> Result<(), CallError> {
    let mut patterns: Vec<&str> = Vec::new();
    for Scope(value) in asker.grants {
        match value {
            rmpv::Value::String(_) | rmpv::Value::Binary(_) => patterns.extend(as_str(value)),
            rmpv::Value::Array(items) => patterns.extend(items.iter().filter_map(as_str)),
            _ => {}
        }
    }
    if patterns.iter().any(|p| drt_caps::implies(p, label)) {
        return Ok(());
    }
    let grant = format!("host:{call}");
    Err(CallError::new(if patterns.is_empty() {
        format!(
            "{call}: '{}' is outside this instance's {grant} grant, which names no labels; \
             the grant needs a scope of label patterns (\"doc:1\", or \"doc:*\")",
            short(label)
        )
    } else {
        format!(
            "{call}: '{}' is outside this instance's {grant} grant, which allows: {}",
            short(label),
            patterns.join(", ")
        )
    }))
}

fn label_arg<'a>(call: &str, args: Option<&'a rmpv::Value>) -> Result<&'a str, CallError> {
    let label = field(args, "key")
        .and_then(as_str)
        .ok_or_else(|| CallError::new(format!("{call}: args.key must be a string naming a key")))?;
    if label.is_empty() || label.len() > LABEL_MAX || label.contains('\0') {
        return Err(CallError::new(format!(
            "{call}: args.key must be 1..{LABEL_MAX} bytes with no NUL"
        )));
    }
    Ok(label)
}

fn alg_arg(call: &str, args: Option<&rmpv::Value>, default: Option<Alg>) -> Result<Alg, CallError> {
    match field(args, "alg") {
        None => default.ok_or_else(|| CallError::new(format!("{call}: args.alg is required"))),
        Some(named) => as_str(named).and_then(Alg::parse).ok_or_else(|| {
            CallError::new(format!("{call}: args.alg must be one of {}", alg_names()))
        }),
    }
}

fn public_key_arg(call: &str, args: Option<&rmpv::Value>) -> Result<PublicKey, CallError> {
    let text = text_arg(call, args, "public_key")?;
    PublicKey::decode(text).map_err(|e| CallError::new(format!("{call}: {e}")))
}

/// A byte argument. `dmsgpack.c` decodes `bin` and `str` alike into a Lua
/// string, so a guest's bytes arrive as either, and both are bytes here.
fn bytes_arg<'a>(
    call: &str,
    args: Option<&'a rmpv::Value>,
    name: &str,
) -> Result<&'a [u8], CallError> {
    let bytes = match field(args, name) {
        Some(rmpv::Value::String(s)) => s.as_bytes(),
        Some(rmpv::Value::Binary(b)) => b.as_slice(),
        _ => {
            return Err(CallError::new(format!(
                "{call}: args.{name} must be a string"
            )))
        }
    };
    if bytes.len() > DATA_MAX {
        return Err(CallError::new(format!(
            "{call}: args.{name} is {} bytes; the most one call takes is {DATA_MAX}",
            bytes.len()
        )));
    }
    Ok(bytes)
}

fn text_arg<'a>(
    call: &str,
    args: Option<&'a rmpv::Value>,
    name: &str,
) -> Result<&'a str, CallError> {
    let bytes = bytes_arg(call, args, name)?;
    std::str::from_utf8(bytes)
        .map_err(|_| CallError::new(format!("{call}: args.{name} must be text")))
}

fn field<'a>(args: Option<&'a rmpv::Value>, name: &str) -> Option<&'a rmpv::Value> {
    match args? {
        rmpv::Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| as_str(k) == Some(name))
            .map(|(_, v)| v),
        _ => None,
    }
}

fn as_str(value: &rmpv::Value) -> Option<&str> {
    match value {
        rmpv::Value::String(s) => s.as_str(),
        rmpv::Value::Binary(b) => std::str::from_utf8(b).ok(),
        _ => None,
    }
}

fn map(entries: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    rmpv::Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (rmpv::Value::from(k), v))
            .collect(),
    )
}

fn alg_names() -> String {
    Alg::ALL
        .iter()
        .map(|a| a.name())
        .collect::<Vec<_>>()
        .join(", ")
}

fn short(label: &str) -> &str {
    let mut end = label.len().min(64);
    while !label.is_char_boundary(end) {
        end -= 1;
    }
    &label[..end]
}
