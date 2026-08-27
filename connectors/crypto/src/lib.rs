//! The crypto connector: `host:crypto/*` (SPEC.md §7).
//!
//! The contract is `dhost_crypto.c`'s, and its one load-bearing property is
//! the whole reason the family exists: **the signing key lives in the host
//! and never in a guest.** A program is granted `host:crypto/jwt_sign`,
//! which is the right to *ask for* a signature, not the key. A compromised
//! instance cannot exfiltrate a secret it was never handed, and the key is
//! in neither its heap nor its snapshot.
//!
//! ```text
//! crypto/random          {bytes=N}                 -> N CSPRNG bytes, hex
//! crypto/hash            {data}                    -> lowercase hex, SHA-256
//! crypto/hmac            {data, key?, expect?}     -> hex, or {valid}
//! crypto/jwt_sign        {claims, ttl?}            -> a JWT-HS256 string
//! crypto/jwt_verify      {token}                   -> {valid, claims?|reason}
//! crypto/turn_credential {user, ttl?}              -> {username, password, …}
//! ```
//!
//! Randomness through the hostcall log is the canonical replay win: the
//! bytes arrive as a reply, so they are in the log, so a replay reproduces
//! the run exactly while the tokens stay unpredictable to anyone without
//! the key.
//!
//! ## The JWT decisions, because the mistakes here are famous
//!
//! - The header is **fixed**, and `jwt_verify` compares the header segment
//!   against its known base64url form rather than parsing it. That closes
//!   alg-confusion (`alg:none`, `alg:RS256`) structurally — there is no
//!   header field a token can set to change how it is checked.
//! - The host owns `iat` and `exp`. `jwt_sign` drops any `iat`/`exp`/`nbf`
//!   a guest put in its claims and injects its own, so a guest cannot mint
//!   a token that never expires. `jwt_verify` **requires an integer `exp`**,
//!   so a token with no enforceable expiry is treated as one that has none.
//! - `jwt_verify` checks the MAC **before** it decodes or parses anything,
//!   so the JSON parser only ever runs on bytes this host signed.
//! - The configured secret never signs directly. Two independent subkeys
//!   are derived from it — one for `crypto/hmac`, one for the JWT MAC — so
//!   that `crypto/hmac` (a general "sign these bytes" grant) cannot be used
//!   as an oracle to forge a JWT. Without this, a program holding only
//!   `host:crypto/hmac` could MAC a JWT signing-input itself and assemble a
//!   token, bypassing `host:crypto/jwt_sign` entirely.
//!
//! The KDF labels are `dhost_crypto.c`'s, byte for byte, so a deployment
//! that moves from the C host to DRT keeps the same subkeys and its
//! outstanding tokens still verify.
//!
//! ## Two departures from the C, both noted rather than hidden
//!
//! - The C's fixed tables (`DH_MAX_SECRETS`, `DH_MAX_TURN_URIS`) are
//!   consequences of having no allocator in the config path, not semantics.
//!   Named secrets and TURN URIs are unbounded here.
//! - A refusal the C answers `denied` (an `args.key` naming no configured
//!   secret; `turn_credential` with no `turn` block) is `error` here, with
//!   the same wording. `denied` is the dispatcher's word in DRT and never a
//!   connector's, so that a mock cannot diverge from a real backing on
//!   refusals; the cost is this one shade of meaning.
//! - A **float** claim serializes shortest-round-trip rather than the C's
//!   `%.17g`, so a token over float claims can differ in bytes between the
//!   two hosts (it parses to the same value, and each host verifies its
//!   own). Integer, string, bool and null claims — everything a real claim
//!   set is made of — are byte-identical.

use std::sync::Mutex;

use base64::Engine as _;
use hmac::{Mac, SimpleHmac};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use drt_caps::{Scope, ScopeType};
use drt_connector::{CallError, CallResult, Connector};

/// A configured secret is at most this long; longer is a file that is not a
/// key. The C's `CRYPTO_KEY_MAX`.
const KEY_MAX: usize = 512;
/// And at least this long. Sixteen bytes is the floor below which a MAC key
/// is decoration.
const KEY_MIN: usize = 16;
const JWT_MAX_TOKEN: usize = 8192;
const JWT_MAX_JSON: usize = 6144;
const JSON_MAX_DEPTH: usize = 32;
const RANDOM_MAX: usize = 1024;
const RANDOM_DEFAULT: usize = 32;
/// RFC 8489 caps TURN's USERNAME at 513 bytes; 256 for the user part leaves
/// room for any expiry and the colon with margin.
const TURN_USER_MAX: usize = 256;
/// Ten years. A ttl beyond this is a typo, not a policy.
const TTL_MAX: i64 = 315_360_000;

/// Domain separation: the master secret signs nothing directly, it only
/// keys these two derivations. Versioned, so a future scheme can coexist.
///
/// These are `dhost_crypto.c`'s labels byte for byte, and they are public
/// because they are **wire compatibility**, not an implementation detail: a
/// deployment that moves from the C host to DRT keeps the same subkeys, so
/// its outstanding tokens still verify. Changing one is a breaking change
/// to every token in flight.
pub const KDF_LABEL_HMAC: &[u8] = b"diluvium/crypto/hmac/v1";
/// See [`KDF_LABEL_HMAC`].
pub const KDF_LABEL_JWT: &[u8] = b"diluvium/crypto/jwt-hs256/v1";

/// `{"alg":"HS256","typ":"JWT"}`, base64url, unpadded. A **constant**,
/// never a parsed field: `jwt_verify` compares the header segment against
/// this rather than reading an `alg` out of the token, which is what closes
/// alg-confusion structurally.
pub const JWT_HEADER_B64: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";

const B64URL: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;
const B64STD: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

type HmacSha256 = SimpleHmac<Sha256>;
type HmacSha1 = SimpleHmac<sha1::Sha1>;

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("hmac takes any key length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.as_bytes();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in s.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// Wall clock, seconds. The host owns the time — `jwt_sign` takes a ttl and
/// never a timestamp, and `jwt_verify` reads `exp` against this.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// The scope: where the key comes from, and nothing a guest can name
// ---------------------------------------------------------------------------

/// One secret, from whichever of the three sources the config named. The
/// field names are the config's, so a refusal tells the operator which knob
/// to turn.
#[derive(Debug, Clone, Default, Deserialize)]
struct SecretSource {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    key_file: Option<std::path::PathBuf>,
    #[serde(default)]
    key_env: Option<String>,
}

impl SecretSource {
    /// Read the bytes. `label` prefixes the refusal, since the same three
    /// knobs appear under `crypto`, under `crypto.turn` and under each
    /// entry of `crypto.secrets`.
    fn load(&self, label: &str) -> Result<Vec<u8>, String> {
        let mut bytes = if let Some(path) = &self.key_file {
            let mut b = std::fs::read(path)
                .map_err(|e| format!("{label}: cannot read key_file '{}': {e}", path.display()))?;
            // Trim one trailing newline, the common shape of a key file.
            if b.last() == Some(&b'\n') {
                b.pop();
            }
            b
        } else if let Some(var) = &self.key_env {
            std::env::var(var)
                .map_err(|_| format!("{label}: env var '{var}' (key_env) is not set"))?
                .into_bytes()
        } else if let Some(inline) = &self.key {
            inline.clone().into_bytes()
        } else {
            Vec::new()
        };
        bytes.truncate(KEY_MAX);
        if bytes.len() < KEY_MIN {
            return Err(format!(
                "{label}: the key is missing or shorter than {KEY_MIN} bytes \
                 (set one of key_file, key_env, key)"
            ));
        }
        Ok(bytes)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct TurnConfig {
    #[serde(flatten)]
    secret: SecretSource,
    #[serde(default)]
    ttl: Option<i64>,
    /// Echoed verbatim in the reply, so the answer is a complete ICE server
    /// entry and no program hard-codes where coturn lives.
    #[serde(default)]
    uris: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct NamedSecret {
    name: String,
    #[serde(flatten)]
    secret: SecretSource,
}

/// The wiring: the master secret, the default ttl, and the two optional
/// blocks. `ConnectorWiring::scope` carries a *place* for `fs` and `sql`;
/// for `crypto` the place is the key, and it is the one scope in the system
/// whose contents deliberately never reach the program it serves.
#[derive(Debug, Clone, Deserialize)]
struct CryptoScope {
    #[serde(flatten)]
    master: SecretSource,
    #[serde(default)]
    default_ttl: Option<i64>,
    #[serde(default)]
    turn: Option<TurnConfig>,
    #[serde(default)]
    secrets: Vec<NamedSecret>,
}

/// The derived working keys. The master is not kept: nothing signs with it
/// directly, so it does not outlive [`Keys::derive`].
struct Keys {
    k_hmac: [u8; 32],
    k_jwt: [u8; 32],
    default_ttl: i64,
    /// The TURN shared secret, **raw**: coturn recomputes the MAC with the
    /// same bytes, so there is nothing to derive. Absent means the
    /// deployment configured no `turn` block and the call refuses.
    turn: Option<TurnKeys>,
    /// Named raw secrets for `crypto/hmac` interop: each is some peer's
    /// shared secret (a webhook sender's), raw for the same reason as
    /// TURN's.
    secrets: Vec<(String, Vec<u8>)>,
}

struct TurnKeys {
    secret: Vec<u8>,
    ttl: i64,
    uris: Vec<String>,
}

impl Drop for Keys {
    fn drop(&mut self) {
        // The subkeys do not linger in a freed page. Volatile so the writes
        // are not elided as dead stores.
        for b in self.k_hmac.iter_mut().chain(self.k_jwt.iter_mut()) {
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

impl Keys {
    fn derive(scope: &CryptoScope) -> Result<Keys, String> {
        let master = scope.master.load("crypto")?;
        // Distinct labels keep the `crypto/hmac` grant from doubling as a
        // JWT-forging oracle: the two grants sign under different keys, and
        // neither subkey discloses the other or the master.
        let k_hmac = hmac_sha256(&master, KDF_LABEL_HMAC);
        let k_jwt = hmac_sha256(&master, KDF_LABEL_JWT);
        drop(master);
        let turn = match &scope.turn {
            None => None,
            Some(t) => Some(TurnKeys {
                secret: t.secret.load("crypto.turn")?,
                ttl: clamp_ttl(t.ttl).unwrap_or(86_400),
                uris: t.uris.clone(),
            }),
        };
        let mut secrets = Vec::with_capacity(scope.secrets.len());
        for s in &scope.secrets {
            if s.name.is_empty() {
                return Err("crypto.secrets: an entry has an empty name".into());
            }
            secrets.push((
                s.name.clone(),
                s.secret.load(&format!("crypto.secrets['{}']", s.name))?,
            ));
        }
        Ok(Keys {
            k_hmac,
            k_jwt,
            default_ttl: clamp_ttl(scope.default_ttl).unwrap_or(3600),
            turn,
            secrets,
        })
    }
}

fn clamp_ttl(ttl: Option<i64>) -> Option<i64> {
    match ttl {
        Some(t) if t > 0 && t <= TTL_MAX => Some(t),
        _ => None,
    }
}

fn parse_scope(scope: Option<&Scope>) -> Result<CryptoScope, String> {
    let Some(Scope(value)) = scope else {
        return Err("scope is required: name the signing key (key_file, key_env or key)".into());
    };
    rmpv::ext::from_value(value.clone()).map_err(|e| format!("scope does not parse: {e}"))
}

struct CryptoScopeType;

impl ScopeType for CryptoScopeType {
    fn describe(&self) -> &str {
        "a signing key: { key_file | key_env | key, default_ttl?, turn?, secrets? }"
    }

    fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
        // The full load, at startup, while the operator is still looking at
        // the terminal: an unreadable key_file or an unset key_env is a
        // configuration mistake, and discovering it at the first jwt_sign
        // is discovering it in production.
        Keys::derive(&parse_scope(scope)?).map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// msgpack <-> JSON, for the claims only
// ---------------------------------------------------------------------------

/// A string argument. `dmsgpack.c` decodes msgpack `bin` and `str`
/// identically into a Lua string, so both arrive here and both are strings.
fn as_str(value: &rmpv::Value) -> Option<&str> {
    match value {
        rmpv::Value::String(s) => s.as_str(),
        rmpv::Value::Binary(b) => std::str::from_utf8(b).ok(),
        _ => None,
    }
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

fn int_field(args: Option<&rmpv::Value>, name: &str) -> Option<i64> {
    field(args, name)?.as_i64()
}

/// One claim value as JSON. `Err` for a value a JSON claim cannot carry —
/// a non-string map key, a non-finite float, bytes that are not text, or a
/// nesting past [`JSON_MAX_DEPTH`].
fn to_json(value: &rmpv::Value, depth: usize) -> Result<serde_json::Value, ()> {
    if depth > JSON_MAX_DEPTH {
        return Err(());
    }
    Ok(match value {
        rmpv::Value::Nil => serde_json::Value::Null,
        rmpv::Value::Boolean(b) => serde_json::Value::Bool(*b),
        rmpv::Value::Integer(i) => {
            if let Some(n) = i.as_i64() {
                serde_json::Value::from(n)
            } else if let Some(n) = i.as_u64() {
                serde_json::Value::from(n)
            } else {
                return Err(());
            }
        }
        rmpv::Value::F32(f) => finite(*f as f64)?,
        rmpv::Value::F64(f) => finite(*f)?,
        rmpv::Value::String(_) | rmpv::Value::Binary(_) => {
            // Not text is not a claim: a token that carries it would fail
            // its own parse on the way back.
            serde_json::Value::String(as_str(value).ok_or(())?.to_string())
        }
        rmpv::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|v| to_json(v, depth + 1))
                .collect::<Result<_, _>>()?,
        ),
        rmpv::Value::Map(entries) => {
            let mut obj = serde_json::Map::with_capacity(entries.len());
            for (k, v) in entries {
                obj.insert(as_str(k).ok_or(())?.to_string(), to_json(v, depth + 1)?);
            }
            serde_json::Value::Object(obj)
        }
        rmpv::Value::Ext(..) => return Err(()),
    })
}

fn finite(f: f64) -> Result<serde_json::Value, ()> {
    // nan/inf have no JSON form, and would mint a token that fails its own
    // parse.
    serde_json::Number::from_f64(f)
        .map(serde_json::Value::Number)
        .ok_or(())
}

fn from_json(value: &serde_json::Value) -> rmpv::Value {
    match value {
        serde_json::Value::Null => rmpv::Value::Nil,
        serde_json::Value::Bool(b) => rmpv::Value::Boolean(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                rmpv::Value::from(i)
            } else if let Some(u) = n.as_u64() {
                rmpv::Value::from(u)
            } else {
                rmpv::Value::F64(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => rmpv::Value::from(s.as_str()),
        serde_json::Value::Array(items) => {
            rmpv::Value::Array(items.iter().map(from_json).collect())
        }
        serde_json::Value::Object(obj) => rmpv::Value::Map(
            obj.iter()
                .map(|(k, v)| (rmpv::Value::from(k.as_str()), from_json(v)))
                .collect(),
        ),
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

fn verify_fail(reason: &str) -> CallResult {
    // A verdict is an answer, not an error: {valid=false} arrives with
    // status ok, so a program branches on the field rather than on a status
    // it would have to distinguish from a real failure.
    Ok(map(vec![
        ("valid", rmpv::Value::Boolean(false)),
        ("reason", rmpv::Value::from(reason)),
    ]))
}

// ---------------------------------------------------------------------------
// The connector
// ---------------------------------------------------------------------------

/// `host:crypto/*`. Holds the derived subkeys for the scope it was wired
/// with; the scope is parsed and the key read **once**, on first call, and
/// re-derived only if a different scope is ever handed in.
#[derive(Default)]
pub struct CryptoConnector {
    keys: Mutex<Option<(Scope, std::sync::Arc<Keys>)>>,
}

impl CryptoConnector {
    pub fn new() -> Self {
        Self::default()
    }

    fn keys(&self, scope: Option<&Scope>) -> Result<std::sync::Arc<Keys>, CallError> {
        let Some(scope) = scope else {
            return Err(CallError::new(
                "the crypto connector is wired with no signing key",
            ));
        };
        let mut slot = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((cached, keys)) = slot.as_ref() {
            if cached == scope {
                return Ok(keys.clone());
            }
        }
        let parsed = parse_scope(Some(scope)).map_err(CallError::new)?;
        let keys = std::sync::Arc::new(Keys::derive(&parsed).map_err(CallError::new)?);
        *slot = Some((scope.clone(), keys.clone()));
        Ok(keys)
    }
}

fn do_random(args: Option<&rmpv::Value>) -> CallResult {
    let n = int_field(args, "bytes").unwrap_or(RANDOM_DEFAULT as i64);
    if n < 1 || n > RANDOM_MAX as i64 {
        return Err(CallError::new(format!(
            "crypto/random: bytes must be 1..{RANDOM_MAX}"
        )));
    }
    let mut buf = vec![0u8; n as usize];
    getrandom::fill(&mut buf)
        .map_err(|e| CallError::new(format!("crypto/random: no entropy source: {e}")))?;
    // Hex, because a guest has no base64/hex library and a raw-byte string
    // is awkward to carry: hex is what a token id or a nonce wants anyway.
    Ok(rmpv::Value::from(to_hex(&buf)))
}

fn do_hash(args: Option<&rmpv::Value>) -> CallResult {
    let data = field(args, "data")
        .and_then(as_str)
        .ok_or_else(|| CallError::new("crypto/hash: args.data must be a string"))?;
    Ok(rmpv::Value::from(to_hex(&Sha256::digest(data.as_bytes()))))
}

fn do_hmac(keys: &Keys, args: Option<&rmpv::Value>) -> CallResult {
    let data = field(args, "data")
        .and_then(as_str)
        .ok_or_else(|| CallError::new("crypto/hmac: args.data must be a string"))?;
    // args.key names a configured raw secret (crypto.secrets), for MACs a
    // peer computes with the same bytes — a webhook signature. Absent, the
    // derived subkey signs, as it always has.
    let key: &[u8] = match field(args, "key") {
        None => &keys.k_hmac,
        Some(named) => {
            let name = as_str(named).ok_or_else(|| {
                CallError::new("crypto/hmac: args.key must be a string naming a configured secret")
            })?;
            match keys.secrets.iter().find(|(n, _)| n == name) {
                Some((_, bytes)) => bytes,
                // A name this deployment does not configure is a refusal,
                // not a fallback to the default key: silently signing under
                // a different key than the caller asked for is how a
                // webhook signature ships broken.
                None => {
                    return Err(CallError::new(format!(
                        "this deployment configures no secret named '{}' \
                         (config.connectors.crypto.secrets)",
                        &name[..name.len().min(64)]
                    )))
                }
            }
        }
    };
    let mac = hmac_sha256(key, data.as_bytes());
    // args.expect (hex) turns the call into a verification: the compare
    // runs here, constant-time, so no guest ever writes the `==` that
    // leaks.
    if let Some(expect) = field(args, "expect") {
        let expect = as_str(expect)
            .ok_or_else(|| CallError::new("crypto/hmac: args.expect must be a hex string"))?;
        let want = from_hex(expect)
            .filter(|w| w.len() == mac.len())
            .ok_or_else(|| {
                CallError::new(format!(
                    "crypto/hmac: args.expect must be {} hex digits",
                    mac.len() * 2
                ))
            })?;
        let valid = bool::from(subtle::ConstantTimeEq::ct_eq(&mac[..], &want[..]));
        return Ok(map(vec![("valid", rmpv::Value::Boolean(valid))]));
    }
    Ok(rmpv::Value::from(to_hex(&mac)))
}

fn do_jwt_sign(keys: &Keys, args: Option<&rmpv::Value>) -> CallResult {
    let iat = now_secs();
    let ttl = clamp_ttl(int_field(args, "ttl")).unwrap_or(keys.default_ttl);

    // The payload JSON, built in the claims' own order so the token bytes
    // match the C host's for the same claim set: the guest's claims, minus
    // any iat/exp/nbf it tried to set, plus the host's own iat and exp.
    let mut payload = String::from("{");
    if let Some(claims) = field(args, "claims") {
        let rmpv::Value::Map(entries) = claims else {
            return Err(CallError::new("crypto/jwt_sign: args.claims must be a map"));
        };
        for (k, v) in entries {
            let k = as_str(k)
                .ok_or_else(|| CallError::new("crypto/jwt_sign: claim keys must be strings"))?;
            // The host owns these three; drop whatever the guest set.
            if matches!(k, "iat" | "exp" | "nbf") {
                continue;
            }
            let v = to_json(v, 1)
                .map_err(|()| CallError::new("crypto/jwt_sign: a claim has no JSON form"))?;
            if payload.len() > 1 {
                payload.push(',');
            }
            payload.push_str(&serde_json::Value::String(k.to_string()).to_string());
            payload.push(':');
            payload.push_str(&v.to_string());
        }
    }
    if payload.len() > 1 {
        payload.push(',');
    }
    // The one append that must not be dropped: it carries exp. A payload
    // without it would be a validly-signed token that never expires.
    payload.push_str(&format!("\"iat\":{iat},\"exp\":{}}}", iat + ttl));
    if payload.len() > JWT_MAX_JSON {
        return Err(CallError::new("crypto/jwt_sign: the claims are too large"));
    }

    // token = header "." base64url(payload) "." base64url(hmac(header.payload))
    let mut token = String::with_capacity(payload.len() * 2);
    token.push_str(JWT_HEADER_B64);
    token.push('.');
    token.push_str(&B64URL.encode(payload.as_bytes()));
    let mac = hmac_sha256(&keys.k_jwt, token.as_bytes());
    token.push('.');
    token.push_str(&B64URL.encode(mac));
    if token.len() > JWT_MAX_TOKEN {
        return Err(CallError::new("crypto/jwt_sign: the token is too large"));
    }
    Ok(rmpv::Value::from(token))
}

fn do_jwt_verify(keys: &Keys, args: Option<&rmpv::Value>) -> CallResult {
    let token = field(args, "token")
        .and_then(as_str)
        .ok_or_else(|| CallError::new("crypto/jwt_verify: args.token must be a string"))?;
    if token.len() > JWT_MAX_TOKEN {
        return verify_fail("oversized");
    }

    // Structure: exactly two dots, and the header segment is the one we
    // emit. Comparing the header rather than parsing it is what closes
    // alg-confusion — there is no field a token can set to be checked
    // differently.
    let Some(dot1) = token.find('.') else {
        return verify_fail("malformed");
    };
    let Some(dot2) = token[dot1 + 1..].find('.').map(|i| dot1 + 1 + i) else {
        return verify_fail("malformed");
    };
    if &token[..dot1] != JWT_HEADER_B64 {
        return verify_fail("alg");
    }

    // The MAC over "header.payload" and a constant-time compare against the
    // token's signature, BEFORE decoding or parsing anything: the parser
    // below only ever runs on bytes this host signed with its own key.
    let mac = hmac_sha256(&keys.k_jwt, &token.as_bytes()[..dot2]);
    let expect = B64URL.encode(mac);
    let sig = &token[dot2 + 1..];
    if sig.len() != expect.len()
        || !bool::from(subtle::ConstantTimeEq::ct_eq(
            sig.as_bytes(),
            expect.as_bytes(),
        ))
    {
        return verify_fail("signature");
    }

    let Ok(payload) = B64URL.decode(&token[dot1 + 1..dot2]) else {
        return verify_fail("payload");
    };
    if payload.len() > JWT_MAX_JSON + 4 {
        return verify_fail("payload");
    }
    let Ok(serde_json::Value::Object(claims)) = serde_json::from_slice(&payload) else {
        return verify_fail("payload");
    };

    // exp/nbf against the host clock — the host owns the time.
    let now = now_secs();
    // An integer exp is **required**. A signed token without one has no
    // enforceable expiry; treating it as valid would make a missing or
    // string-typed exp a forever-token. Every token this host mints carries
    // one, so this only ever rejects a foreign or crafted token.
    let Some(exp) = claims.get("exp").and_then(|v| v.as_i64()) else {
        return verify_fail("expired");
    };
    if now >= exp {
        return verify_fail("expired");
    }
    if let Some(nbf) = claims.get("nbf").and_then(|v| v.as_i64()) {
        if now < nbf {
            return verify_fail("not_yet_valid");
        }
    }
    Ok(map(vec![
        ("valid", rmpv::Value::Boolean(true)),
        ("claims", from_json(&serde_json::Value::Object(claims))),
    ]))
}

/// coturn's `use-auth-secret` scheme (the "REST API For Access To TURN
/// Services" draft): the username is `"<expiry-unix>:<user>"` and the
/// password is standard base64 of `HMAC-SHA1(shared_secret, username)`.
///
/// This is the one exception to the derived-subkeys rule: the TURN server
/// holds the **same** secret and recomputes the MAC, so a derived subkey
/// would produce MACs coturn cannot check. The secret still never reaches a
/// guest — that part of the contract is unchanged.
///
/// HMAC-SHA1 because the scheme fixes it. SHA-1's collision breaks do not
/// apply to HMAC as used here, and interop leaves no choice anyway; do not
/// reuse the primitive for anything else.
fn do_turn_credential(keys: &Keys, args: Option<&rmpv::Value>) -> CallResult {
    let Some(turn) = &keys.turn else {
        return Err(CallError::new(
            "this deployment configures no TURN shared secret \
             (config.connectors.crypto.turn), so 'crypto/turn_credential' is not wired",
        ));
    };
    let user = field(args, "user")
        .and_then(as_str)
        .ok_or_else(|| CallError::new("crypto/turn_credential: args.user must be a string"))?;
    if user.is_empty() || user.len() > TURN_USER_MAX || user.contains('\0') {
        return Err(CallError::new(format!(
            "crypto/turn_credential: args.user must be 1..{TURN_USER_MAX} bytes with no NUL"
        )));
    }
    // The host owns the expiry, exactly as jwt_sign owns exp: the call
    // takes a ttl, never a timestamp. The expiry is in the username in
    // cleartext; if the guest chose it, a far-future credential would be
    // one field away.
    let ttl = clamp_ttl(int_field(args, "ttl")).unwrap_or(turn.ttl);
    let expires = now_secs() + ttl;
    let username = format!("{expires}:{user}");
    let mut mac = <HmacSha1 as Mac>::new_from_slice(&turn.secret).expect("hmac takes any key");
    mac.update(username.as_bytes());
    let password = B64STD.encode(mac.finalize().into_bytes());
    let mut out = vec![
        ("username", rmpv::Value::from(username)),
        ("password", rmpv::Value::from(password)),
        ("expires", rmpv::Value::from(expires)),
    ];
    if !turn.uris.is_empty() {
        out.push((
            "uris",
            rmpv::Value::Array(
                turn.uris
                    .iter()
                    .map(|u| rmpv::Value::from(u.as_str()))
                    .collect(),
            ),
        ));
    }
    Ok(map(out))
}

#[async_trait::async_trait]
impl Connector for CryptoConnector {
    fn scope_type(&self) -> Box<dyn ScopeType> {
        Box::new(CryptoScopeType)
    }

    async fn call(
        &self,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        let args = args.as_ref();
        // hash and random need no key, but the wiring is still the gate:
        // a build that wires no crypto scope answers nothing in the family.
        let keys = self.keys(scope)?;
        match call {
            "crypto/random" => do_random(args),
            "crypto/hash" => do_hash(args),
            "crypto/hmac" => do_hmac(&keys, args),
            "crypto/jwt_sign" => do_jwt_sign(&keys, args),
            "crypto/jwt_verify" => do_jwt_verify(&keys, args),
            "crypto/turn_credential" => do_turn_credential(&keys, args),
            other => Err(CallError::new(format!(
                "the crypto connector has no call '{other}'"
            ))),
        }
    }
}
