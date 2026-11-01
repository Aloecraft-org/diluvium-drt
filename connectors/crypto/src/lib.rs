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
//! crypto/hash            {data, alg?}              -> lowercase hex; sha256 (default) or sha1
//! crypto/hmac            {data, key?, expect?}     -> hex, or {valid}
//! crypto/jwt_sign        {claims, ttl?, key?}      -> a JWT string; `key` names a label or a signing key, carried as `kid`
//! crypto/jwt_verify      {token}                   -> {valid, claims?|reason}; `kid` selects the key, the key fixes `alg`
//! crypto/public_key      {key}                     -> the EdDSA key's public half as a JWK, for a JWKS
//! crypto/turn_credential {user, ttl?}              -> {username, password, …}
//! crypto/derive          {label, alg?}             -> nothing; `label` now names a key, `alg` HS256 (default) or EdDSA
//! crypto/totp            {key, alg?, digits?, period?}                  -> {code, step, expires}
//! crypto/totp_verify     {key, code, alg?, digits?, period?, window?, after?} -> {valid, step?|reason}
//! crypto/totp_enroll     {key, account, issuer?, alg?, digits?, period?} -> {secret, uri}; the one call that hands out a seed
//! ```
//!
//! What a `key` name resolves to, which is the fan-out every keyed call
//! goes through. A name is in at most one of these places: the config
//! refuses a configured name used twice, and `derive` a label that shadows
//! one.
//!
//! ```text
//! (no key)                the master's derived subkeys: HS256 for jwt, the hmac subkey for hmac
//! crypto.signing_keys[]   a configured EdDSA key: jwt_sign, jwt_verify, public_key
//! crypto.secrets[]        a peer's raw bytes: hmac, totp, totp_verify; refused by jwt_sign and totp_enroll
//! a derived label         this caller's: hmac, jwt (its alg), totp*, public_key when EdDSA
//! ```
//!
//! Randomness through the hostcall log is the canonical replay win: the
//! bytes arrive as a reply, so they are in the log, so a replay reproduces
//! the run exactly while the tokens stay unpredictable to anyone without
//! the key.
//!
//! ## The JWT decisions, because the mistakes here are famous
//!
//! - The header is **rebuilt, never read**. `jwt_verify` decodes the header
//!   segment for one member, `kid`; looks that label up for the caller;
//!   rebuilds the header that key would produce; and compares the segment
//!   against it byte-wise. `alg` is never read out of a token, so there is
//!   no header field a token can set to change how it is checked — that
//!   closes alg-confusion (`alg:none`, `alg:RS256`) structurally, and a
//!   header with two `kid`s or its members reordered fails the same compare
//!   with no rule about either (`doc/Plan-0.7.0.md` §5.2). A token with no
//!   `kid` rebuilds to the constant header and verifies under the default
//!   key, behind `jwt.accept_unkeyed` (default on, §5.3).
//! - The host owns `iat` and `exp`. `jwt_sign` drops any `iat`/`exp`/`nbf`
//!   a guest put in its claims and injects its own, so a guest cannot mint
//!   a token that never expires. `jwt_verify` **requires an integer `exp`**,
//!   so a token with no enforceable expiry is treated as one that has none.
//! - `jwt_verify` checks the MAC **before** it decodes or parses the
//!   payload, so the claims parser only ever runs on bytes this host
//!   signed. The one thing read ahead of the MAC is the header's `kid`,
//!   and the header is then compared whole against a rebuild, not trusted.
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
//! ## Derived labels: a runtime name for a key that stays in the host
//!
//! `crypto.secrets` is config-time. A room created at 14:02 has no entry in
//! a config written at deploy time, and a guest that built HKDF-expand out
//! of `hmac` would hold the result in its heap — which is its snapshot.
//! `crypto/derive {label}` registers a label for the calling node and
//! returns **nothing**; the guest gets a name it can pass to `hmac`, and
//! never the bytes (`doc/Plan-0.7.0.md` §4).
//!
//! - **A label never signs directly.** The rule the master already obeys,
//!   one level down: a label master is derived from the configured master,
//!   and each consumer derives its own subkey from *that* under the same
//!   two labels — `KDF_LABEL_HMAC` for `hmac`, `KDF_LABEL_JWT` for the JWT.
//!   One name, two non-interchangeable keys, so `host:crypto/hmac` is no
//!   more a JWT oracle for a label than it is for the master.
//! - **Derivation is deterministic**, a pure function of master and label,
//!   because the signer and the verifier may be different nodes. A second
//!   `derive` of a live label is idempotent, not a refusal.
//! - **The scope on `host:crypto/derive` is the control.** A deterministic
//!   label is re-mintable by anyone permitted to name it, so the grant
//!   carries the patterns it may name — `room:2`, or `room:*` — in the
//!   capability grammar's own trailing-`*` shape. A grant with no scope
//!   names nothing. Lifetime is the ownership dimension's: a label belongs
//!   to the node that derived it and is released when that node dies,
//!   which loses nothing, since it is derived rather than stored.
//! - **A label may not shadow a configured secret.** The name is refused
//!   at `derive`, naming both, rather than found by a signature that
//!   verifies on one host and not another.
//! - **`jwt_sign {key = <label>}` signs under the label's JWT subkey** and
//!   names the label in the header as `kid`, which is how a per-room or
//!   per-issuer signing key is reachable at runtime (§5.2). A
//!   `crypto.secrets` name is refused there by name: those bytes are
//!   shared with a peer by definition, and `KDF_LABEL_JWT` is public, so
//!   the peer could mint tokens this host would accept.
//!
//! ## EdDSA: the key picks the algorithm, the token never does
//!
//! - **`alg` belongs to a key, not to a token.** A label derived with
//!   `alg = "EdDSA"` signs Ed25519 (RFC 8037) under a keypair derived from
//!   its label master under [`KDF_LABEL_EDDSA`]; a `crypto.signing_keys`
//!   entry is a configured one. `jwt_verify` finds the key by `kid`, takes
//!   the algorithm from the key it found, rebuilds the header that key
//!   writes, and compares. A token cannot ask to be checked as HS256 under
//!   an EdDSA key's public bytes, which is the RS256/HS256 confusion in its
//!   EdDSA form, because nothing in it is read as an algorithm.
//! - **An EdDSA token always names its key.** There is no unkeyed EdDSA
//!   header, so `accept_unkeyed` stays an HS256-only switch.
//! - **`crypto/public_key` is the half that may leave.** It answers a JWK
//!   (`kty = "OKP"`, `crv = "Ed25519"`, `x`, and `kid`, `alg`, `use`), so a
//!   program serves a JWKS from the keys it names and another service
//!   verifies with any JWT library. The private half is in this process
//!   only, like every other key here. An HS256 key has no public half and
//!   is refused there by name.
//! - **A derived EdDSA key is deterministic**, like every label: two nodes
//!   with one master derive one keypair, and rotation is a new label, with
//!   the old one's JWK served beside the new one until its tokens expire.
//!
//! ## TOTP: verify by name, and enroll is the grant that sees a seed
//!
//! - RFC 6238 over RFC 4226's truncation, SHA-1 (what every authenticator
//!   assumes, so the default) or SHA-256, 6 to 8 digits, a period in
//!   seconds. The host owns the time: no call takes a timestamp, so a
//!   program cannot mint a code for later.
//! - **The seed is named, never passed.** `key` is a label this caller
//!   derived, whose seed is a third subkey under [`KDF_LABEL_TOTP`] (20
//!   bytes for SHA-1, 32 for SHA-256, RFC 6238's lengths), or a
//!   `crypto.secrets` entry for a seed provisioned somewhere else.
//! - **`crypto/totp_enroll` is the exception, and a separate grant.** An
//!   authenticator needs the seed, so enrolment answers it as base32 and as
//!   an `otpauth://` URI. Only a derived label enrols; a configured secret
//!   is never handed to a program. Grant `totp_enroll` to the enrolment
//!   program alone, and the program that checks codes never sees a seed.
//!   `crypto/totp`, the current code, is as good as the seed for one
//!   period, so it is a grant for a test or a robot account's login, and
//!   never the verifier's.
//!   Rotation is a new label (`totp:alice:2`), since a label's seed is a
//!   pure function of its name.
//! - **Single use is the caller's to keep, and the call makes it cheap.**
//!   RFC 6238 §5.2 says a verifier must not accept a code twice. The host
//!   keeps no state, so `totp_verify` answers the matched time `step`, and
//!   a caller that stores it and passes it back as `after` gets `replayed`
//!   for that code and any older one.
//!
//! ## Departures from the C, noted rather than hidden
//!
//! - `derive`, `public_key`, the EdDSA keys and the three TOTP calls are
//!   DRT's alone: `dhost_crypto.c` answers none of them, so a program that
//!   uses them runs under `drt` and not yet under the plain host.
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

use std::collections::BTreeMap;
use std::sync::Mutex;

use base64::Engine as _;
use ed25519_dalek::pkcs8::DecodePrivateKey as _;
use ed25519_dalek::Signer as _;
use hmac::{Mac, SimpleHmac};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use drt_caps::{Scope, ScopeType};
use drt_connector::{Asker, CallError, CallResult, Caller, Connector};

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
/// A derived label is a name, not a document.
const LABEL_MAX: usize = 256;
/// RFC 4226 §5.3: at least six digits, and the 31-bit truncation carries
/// nine at most; eight is what any authenticator offers.
const TOTP_DIGITS_MIN: i64 = 6;
const TOTP_DIGITS_MAX: i64 = 8;
const TOTP_DIGITS_DEFAULT: i64 = 6;
/// Seconds per time step. RFC 6238 §5.2 recommends thirty, and every
/// authenticator assumes it when an enrolment names none.
const TOTP_PERIOD_MIN: i64 = 10;
const TOTP_PERIOD_MAX: i64 = 300;
const TOTP_PERIOD_DEFAULT: i64 = 30;
/// Steps either side of now that `totp_verify` accepts. One is RFC 6238
/// §5.2's "at most one time step" for transmission delay, each way, to
/// absorb a clock that drifts in either direction.
const TOTP_WINDOW_DEFAULT: i64 = 1;
const TOTP_WINDOW_MAX: i64 = 10;
/// An enrolment's account and issuer are what an authenticator shows in
/// its list; past this they are not names.
const TOTP_NAME_MAX: usize = 128;

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
/// The root every derived label hangs off: `k_label = HMAC(master, this)`,
/// and a label master is `HMAC(k_label, label)`. DRT's own — the C host has
/// no runtime labels — and public for the same reason the other two are:
/// changing it re-keys every label in every deployment.
pub const KDF_LABEL_DERIVE: &[u8] = b"drt/crypto/derive/v1";
/// A label's TOTP seed is `HMAC(label master, this)`, truncated to the
/// algorithm's length. Its own subkey, so enrolling a label (which hands
/// the seed to an authenticator) discloses neither its hmac nor its JWT
/// key. Public and versioned for the same reason as the rest: changing it
/// re-seeds every enrolled authenticator.
pub const KDF_LABEL_TOTP: &[u8] = b"drt/crypto/totp/v1";
/// An EdDSA label's Ed25519 secret key is `HMAC(label master, this)`.
/// Distinct from [`KDF_LABEL_JWT`], so one label's HS256 and EdDSA keys are
/// unrelated bytes. Changing it re-keys every EdDSA label, and every JWKS
/// that publishes one.
pub const KDF_LABEL_EDDSA: &[u8] = b"drt/crypto/jwt-eddsa/v1";

/// `{"alg":"HS256","typ":"JWT"}`, base64url, unpadded. A **constant**,
/// never a parsed field: `jwt_verify` compares the header segment against
/// this rather than reading an `alg` out of the token, which is what closes
/// alg-confusion structurally.
pub const JWT_HEADER_B64: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";
/// The header of a token signed under a label: this prefix, the label as a
/// JSON string, and the closing brace —
/// `{"alg":"HS256","typ":"JWT","kid":"<label>"}` in that member order,
/// every time. `jwt_verify` rebuilds it from the label a token names and
/// compares byte-wise, so it is as much a constant as [`JWT_HEADER_B64`].
const JWT_HEADER_KID_PREFIX: &str = r#"{"alg":"HS256","typ":"JWT","kid":"#;
/// The header of every EdDSA token, built the same way:
/// `{"alg":"EdDSA","typ":"JWT","kid":"<name>"}`. There is no unkeyed form.
const JWT_HEADER_EDDSA_KID_PREFIX: &str = r#"{"alg":"EdDSA","typ":"JWT","kid":"#;

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
    drt_platform::clock::wall_secs()
        .map(|s| s as i64)
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
struct NamedSecret {
    name: String,
    #[serde(flatten)]
    secret: SecretSource,
}

/// One `crypto.signing_keys` entry: a configured asymmetric key, for a
/// deployment that holds its key material itself rather than deriving it
/// from the master. `alg` is required, so a second algorithm can be added
/// without a guess about what an entry that names none meant.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SigningKeyConfig {
    name: String,
    alg: String,
    /// PKCS#8 PEM, the shape `openssl genpkey -algorithm ed25519` writes.
    /// A secret value in the root config's shape (`drt_config::Secret`):
    /// the string itself, `{"env": NAME}` or `{"file": PATH}`.
    private_key: rmpv::Value,
}

/// A secret value as the root config spells one: a string is the value,
/// and an object with exactly one of `env` or `file` says where it is. A
/// file loses one trailing newline. `{"prompt": ...}` is refused by name:
/// a connector's scope is read when the first call arrives, with no
/// terminal to ask on. `what` is the field, for every refusal.
fn load_secret_value(value: &rmpv::Value, what: &str) -> Result<String, String> {
    if let Some(literal) = as_str(value) {
        return Ok(literal.to_string());
    }
    let rmpv::Value::Map(entries) = value else {
        return Err(format!(
            "{what}: a secret value is a string, {{\"env\": NAME}} or {{\"file\": PATH}}"
        ));
    };
    let [(k, v)] = entries.as_slice() else {
        return Err(format!(
            "{what}: a secret value object names exactly one of 'env' or 'file'"
        ));
    };
    let Some(from) = v.as_str().filter(|_| as_str(k).is_some()) else {
        return Err(format!(
            "{what}: '{}' takes a string",
            as_str(k).unwrap_or("?")
        ));
    };
    match as_str(k) {
        Some("env") => std::env::var(from)
            .map_err(|_| format!("{what}: the environment variable '{from}' is not set")),
        Some("file") => {
            let mut text = std::fs::read_to_string(from)
                .map_err(|e| format!("{what}: cannot read '{from}': {e}"))?;
            if text.ends_with('\n') {
                text.pop();
            }
            Ok(text)
        }
        Some("prompt") => Err(format!(
            "{what}: a connector scope cannot prompt, since it is read with no terminal; \
             use {{\"env\": NAME}} or {{\"file\": PATH}}"
        )),
        other => Err(format!(
            "{what}: unknown source '{}'; a secret value object names 'env' or 'file'",
            other.unwrap_or("?")
        )),
    }
}

/// The `jwt` block: what `jwt_verify` accepts beyond a token that names its
/// key. Unknown keys here are refused at wiring like the rest of the scope.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct JwtConfig {
    /// Whether a token with no `kid` verifies under the default key. On by
    /// default, which costs nothing and is right for anyone holding tokens
    /// already issued; off, every token must name its key, and retiring
    /// the default key is a config change rather than a release
    /// (`doc/Plan-0.7.0.md` §5.3).
    #[serde(default = "default_accept_unkeyed")]
    accept_unkeyed: bool,
}

fn default_accept_unkeyed() -> bool {
    true
}

/// The wiring: the master secret, the default ttl, and the optional
/// blocks. `ConnectorWiring::scope` carries a *place* for `fs` and `sql`;
/// for `crypto` the place is the key, and it is the one scope in the system
/// whose contents deliberately never reach the program it serves.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CryptoScope {
    #[serde(flatten)]
    master: SecretSource,
    #[serde(default)]
    default_ttl: Option<i64>,
    #[serde(default)]
    turn: Option<TurnConfig>,
    #[serde(default)]
    secrets: Vec<NamedSecret>,
    #[serde(default)]
    signing_keys: Vec<SigningKeyConfig>,
    #[serde(default)]
    jwt: Option<JwtConfig>,
}

/// The derived working keys. The master is not kept: nothing signs with it
/// directly, so it does not outlive [`Keys::derive`].
struct Keys {
    k_hmac: [u8; 32],
    k_jwt: [u8; 32],
    /// What every runtime label derives from. Kept so the master need not
    /// be; a label master is one HMAC from here and never from the master.
    k_label: [u8; 32],
    default_ttl: i64,
    /// The TURN shared secret, **raw**: coturn recomputes the MAC with the
    /// same bytes, so there is nothing to derive. Absent means the
    /// deployment configured no `turn` block and the call refuses.
    turn: Option<TurnKeys>,
    /// Named raw secrets for `crypto/hmac` interop: each is some peer's
    /// shared secret (a webhook sender's), raw for the same reason as
    /// TURN's.
    secrets: Vec<(String, Vec<u8>)>,
    /// `crypto.signing_keys`, parsed. `SigningKey` zeroizes itself on drop.
    signing_keys: Vec<(String, ed25519_dalek::SigningKey)>,
    /// `jwt.accept_unkeyed`: whether a token naming no `kid` verifies under
    /// `k_jwt`.
    accept_unkeyed: bool,
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
        for b in self
            .k_hmac
            .iter_mut()
            .chain(self.k_jwt.iter_mut())
            .chain(self.k_label.iter_mut())
        {
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
        let k_label = hmac_sha256(&master, KDF_LABEL_DERIVE);
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
        let mut signing_keys = Vec::with_capacity(scope.signing_keys.len());
        for k in &scope.signing_keys {
            let what = format!("crypto.signing_keys['{}']", k.name);
            if k.name.is_empty() {
                return Err("crypto.signing_keys: an entry has an empty name".into());
            }
            // One name, one key: a name found in two places would sign
            // under one and verify under the other depending on which
            // lookup ran first.
            if secrets.iter().any(|(n, _)| n == &k.name)
                || signing_keys.iter().any(|(n, _)| n == &k.name)
            {
                return Err(format!(
                    "{what}: the name is already used in crypto.secrets or crypto.signing_keys"
                ));
            }
            if k.alg != "EdDSA" {
                return Err(format!(
                    "{what}: alg '{}' is not one this host signs with; it signs 'EdDSA'",
                    k.alg
                ));
            }
            let pem = load_secret_value(&k.private_key, &format!("{what}.private_key"))?;
            let key = ed25519_dalek::SigningKey::from_pkcs8_pem(pem.trim()).map_err(|e| {
                format!(
                    "{what}.private_key: not an Ed25519 PKCS#8 PEM key \
                     (`openssl genpkey -algorithm ed25519` writes one): {e}"
                )
            })?;
            signing_keys.push((k.name.clone(), key));
        }
        Ok(Keys {
            k_hmac,
            k_jwt,
            k_label,
            default_ttl: clamp_ttl(scope.default_ttl).unwrap_or(3600),
            turn,
            secrets,
            signing_keys,
            accept_unkeyed: scope
                .jwt
                .as_ref()
                .map(|j| j.accept_unkeyed)
                .unwrap_or_else(default_accept_unkeyed),
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
        "a signing key: { key_file | key_env | key, default_ttl?, turn?, secrets?, signing_keys?, jwt? }"
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

/// The key a JWT is signed and checked under, which fixes the algorithm:
/// the token's header is rebuilt from this and never read for an `alg`.
#[derive(Clone)]
enum JwtKey {
    Hs256([u8; 32]),
    EdDsa(ed25519_dalek::SigningKey),
}

impl JwtKey {
    fn alg(&self) -> &'static str {
        match self {
            JwtKey::Hs256(_) => "HS256",
            JwtKey::EdDsa(_) => "EdDSA",
        }
    }
}

impl Drop for JwtKey {
    fn drop(&mut self) {
        // `SigningKey` zeroizes itself; the HMAC key is ours to clear.
        if let JwtKey::Hs256(k) = self {
            for b in k.iter_mut() {
                unsafe { std::ptr::write_volatile(b, 0) };
            }
        }
    }
}

/// One registered label's subkeys. The label master they came from is not
/// kept: nothing signs with it, so it does not outlive `derive`.
struct LabelKeys {
    hmac: [u8; 32],
    /// The TOTP seed, before truncation to the algorithm's length.
    totp: [u8; 32],
    /// HS256 or EdDSA, as `derive {alg}` chose.
    jwt: JwtKey,
}

impl Drop for LabelKeys {
    fn drop(&mut self) {
        for b in self.hmac.iter_mut().chain(self.totp.iter_mut()) {
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

impl LabelKeys {
    /// Deterministic in `(k_label, label, alg)`, which is what lets two
    /// nodes that both derive `room:2` reach the same key.
    fn derive(k_label: &[u8; 32], label: &str, alg: LabelAlg) -> LabelKeys {
        let mut master = hmac_sha256(k_label, label.as_bytes());
        let jwt = match alg {
            LabelAlg::Hs256 => JwtKey::Hs256(hmac_sha256(&master, KDF_LABEL_JWT)),
            LabelAlg::EdDsa => {
                let mut secret = hmac_sha256(&master, KDF_LABEL_EDDSA);
                let key = ed25519_dalek::SigningKey::from_bytes(&secret);
                for b in secret.iter_mut() {
                    unsafe { std::ptr::write_volatile(b, 0) };
                }
                JwtKey::EdDsa(key)
            }
        };
        let keys = LabelKeys {
            hmac: hmac_sha256(&master, KDF_LABEL_HMAC),
            totp: hmac_sha256(&master, KDF_LABEL_TOTP),
            jwt,
        };
        for b in master.iter_mut() {
            unsafe { std::ptr::write_volatile(b, 0) };
        }
        keys
    }
}

/// What `derive {alg}` names: which JWT key a label carries.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LabelAlg {
    Hs256,
    EdDsa,
}

/// `host:crypto/*`. Holds the derived subkeys for the scope it was wired
/// with; the scope is parsed and the key read **once**, on first call, and
/// re-derived only if a different scope is ever handed in.
///
/// `labels` is the ownership dimension applied to a name-addressed
/// resource: keyed by `(who derived it, label)`, so one node's registration
/// is invisible to another's and dies with it, while the bytes behind the
/// same label are identical for every owner.
#[derive(Default)]
pub struct CryptoConnector {
    keys: Mutex<Option<(Scope, std::sync::Arc<Keys>)>>,
    labels: Mutex<BTreeMap<(Caller, String), LabelKeys>>,
}

impl CryptoConnector {
    pub fn new() -> Self {
        Self::default()
    }

    /// The hmac subkey of a label `caller` registered, or `None` if it did
    /// not — which is the same answer whether the label was never derived
    /// or was released with its owner.
    fn label_hmac_key(&self, caller: Caller, label: &str) -> Option<[u8; 32]> {
        self.labels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(caller, label.to_string()))
            .map(|k| k.hmac)
    }

    /// The JWT key of a label `caller` registered — the other half of
    /// §4.2a's split, and never the same bytes as [`Self::label_hmac_key`].
    fn label_jwt_key(&self, caller: Caller, label: &str) -> Option<JwtKey> {
        self.labels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(caller, label.to_string()))
            .map(|k| k.jwt.clone())
    }

    /// The TOTP seed of a label `caller` registered, untruncated.
    fn label_totp_seed(&self, caller: Caller, label: &str) -> Option<[u8; 32]> {
        self.labels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(caller, label.to_string()))
            .map(|k| k.totp)
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
    drt_platform::entropy::fill(&mut buf)
        .map_err(|e| CallError::new(format!("crypto/random: {e}")))?;
    // Hex, because a guest has no base64/hex library and a raw-byte string
    // is awkward to carry: hex is what a token id or a nonce wants anyway.
    Ok(rmpv::Value::from(to_hex(&buf)))
}

/// `crypto/hash {data, alg?}`, lowercase hex, SHA-256 unless `alg` says
/// otherwise.
///
/// **SHA-1 is here for interop and not as a choice.** Protocols that
/// predate its collapse still demand it -- a git object id, a WebSocket
/// accept key, an old webhook signature -- and a program that must speak
/// one of them otherwise reimplements SHA-1 in Lua, badly, inside the
/// sandbox. It is not a security primitive any more and nothing here
/// treats it as one: `hmac`, `jwt_sign` and `derive` are untouched, the
/// only SHA-1 this connector signs with is TURN's, and the sole way to
/// reach this one is to name it.
///
/// An `alg` this host does not know is refused by name rather than
/// falling back to the default: a program that asked for a digest it did
/// not get would compare hex against the wrong thing and call it a
/// mismatch.
fn do_hash(args: Option<&rmpv::Value>) -> CallResult {
    let data = field(args, "data")
        .and_then(as_str)
        .ok_or_else(|| CallError::new("crypto/hash: args.data must be a string"))?;
    let alg = match field(args, "alg") {
        None => "sha256",
        Some(named) => as_str(named).ok_or_else(|| {
            CallError::new("crypto/hash: args.alg must be a string, 'sha256' or 'sha1'")
        })?,
    };
    let hex = match alg {
        "sha256" => to_hex(&Sha256::digest(data.as_bytes())),
        "sha1" => to_hex(&sha1::Sha1::digest(data.as_bytes())),
        other => {
            return Err(CallError::new(format!(
                "crypto/hash: unknown alg '{other}'; this host hashes with 'sha256' or 'sha1'"
            )))
        }
    };
    Ok(rmpv::Value::from(hex))
}

fn do_hmac(
    c: &CryptoConnector,
    keys: &Keys,
    caller: Caller,
    args: Option<&rmpv::Value>,
) -> CallResult {
    let data = field(args, "data")
        .and_then(as_str)
        .ok_or_else(|| CallError::new("crypto/hmac: args.data must be a string"))?;
    // args.key names either a configured raw secret (crypto.secrets), for
    // MACs a peer computes with the same bytes — a webhook signature — or a
    // label this caller derived, whose hmac subkey signs. Absent, the
    // derived subkey signs, as it always has.
    let label_key: Option<[u8; 32]>;
    let key: &[u8] = match field(args, "key") {
        None => &keys.k_hmac,
        Some(named) => {
            let name = as_str(named).ok_or_else(|| {
                CallError::new(
                    "crypto/hmac: args.key must be a string naming a configured secret or a derived label",
                )
            })?;
            match keys.secrets.iter().find(|(n, _)| n == name) {
                Some((_, bytes)) => bytes,
                None => match c.label_hmac_key(caller, name) {
                    Some(k) => {
                        label_key = Some(k);
                        label_key.as_ref().map(|k| &k[..]).unwrap_or(&[])
                    }
                    // A name this deployment does not configure and this
                    // caller did not derive is a refusal, not a fallback to
                    // the default key: silently signing under a different
                    // key than the caller asked for is how a webhook
                    // signature ships broken. Never-derived and released
                    // read alike here, on purpose.
                    None => {
                        return Err(CallError::new(format!(
                            "this deployment configures no secret named '{}' \
                             (config.connectors.crypto.secrets), and this caller \
                             derived no label by that name (crypto/derive)",
                            &name[..name.len().min(64)]
                        )))
                    }
                },
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

/// The header a token signed under `name` with `key` carries, base64url
/// unpadded: the algorithm's prefix ([`JWT_HEADER_KID_PREFIX`] or
/// [`JWT_HEADER_EDDSA_KID_PREFIX`]), the name as a JSON string, the closing
/// brace. Built the same way by `jwt_sign` and by `jwt_verify`'s rebuild,
/// which is what makes the byte compare meaningful.
fn kid_header_b64(key: &JwtKey, name: &str) -> String {
    let prefix = match key {
        JwtKey::Hs256(_) => JWT_HEADER_KID_PREFIX,
        JwtKey::EdDsa(_) => JWT_HEADER_EDDSA_KID_PREFIX,
    };
    let mut header = String::with_capacity(prefix.len() + name.len() + 3);
    header.push_str(prefix);
    header.push_str(&serde_json::Value::String(name.to_string()).to_string());
    header.push('}');
    B64URL.encode(header.as_bytes())
}

/// The `kid` a token's header segment names, if it names one as a string.
/// This is the **only** thing ever read out of a header: it selects which
/// key to look up, and the header is then rebuilt from that key and
/// compared whole. A segment that does not decode, or decodes to something
/// other than an object, or carries no string `kid`, names nothing —
/// and then only the constant header can match.
fn header_kid(segment: &str) -> Option<String> {
    let bytes = B64URL.decode(segment).ok()?;
    let serde_json::Value::Object(members) = serde_json::from_slice(&bytes).ok()? else {
        return None;
    };
    members.get("kid")?.as_str().map(str::to_string)
}

/// The JWT key a name resolves to for `caller`: a configured signing key,
/// or a label the caller derived. The config refuses a signing key named
/// like a secret, and `derive` a label named like either, so the order
/// here decides nothing.
fn named_jwt_key(c: &CryptoConnector, keys: &Keys, caller: Caller, name: &str) -> Option<JwtKey> {
    if let Some((_, k)) = keys.signing_keys.iter().find(|(n, _)| n == name) {
        return Some(JwtKey::EdDsa(k.clone()));
    }
    c.label_jwt_key(caller, name)
}

/// The signature segment over `input`, base64url.
fn jwt_signature(key: &JwtKey, input: &[u8]) -> String {
    match key {
        JwtKey::Hs256(k) => B64URL.encode(hmac_sha256(k, input)),
        JwtKey::EdDsa(k) => B64URL.encode(k.sign(input).to_bytes()),
    }
}

/// Whether `sig` (the token's third segment) is `key`'s signature over
/// `input`. HS256 recomputes and compares in constant time; EdDSA decodes
/// and runs the strict check, which refuses a non-canonical `S` and a
/// small-order key. Either way a segment that is not the one encoding of
/// the 32 or 64 bytes fails, so one token has one spelling.
fn jwt_signature_ok(key: &JwtKey, input: &[u8], sig: &str) -> bool {
    match key {
        JwtKey::Hs256(k) => {
            let expect = B64URL.encode(hmac_sha256(k, input));
            sig.len() == expect.len()
                && bool::from(subtle::ConstantTimeEq::ct_eq(
                    sig.as_bytes(),
                    expect.as_bytes(),
                ))
        }
        JwtKey::EdDsa(k) => {
            let Ok(bytes) = B64URL.decode(sig) else {
                return false;
            };
            let Ok(bytes) = <[u8; 64]>::try_from(bytes.as_slice()) else {
                return false;
            };
            if B64URL.encode(bytes) != sig {
                return false;
            }
            let signature = ed25519_dalek::Signature::from_bytes(&bytes);
            k.verifying_key().verify_strict(input, &signature).is_ok()
        }
    }
}

fn do_jwt_sign(
    c: &CryptoConnector,
    keys: &Keys,
    caller: Caller,
    args: Option<&rmpv::Value>,
) -> CallResult {
    let iat = now_secs();
    let ttl = clamp_ttl(int_field(args, "ttl")).unwrap_or(keys.default_ttl);

    // args.key names a configured signing key or a label this caller
    // derived (§5.2): the token carries the name as `kid` and is signed
    // under that key, in the algorithm the key has. A label signs under
    // its JWT key, never its hmac subkey, which is what keeps
    // `crypto/hmac {key = label}` from being a forging oracle for it
    // (§4.2a). Absent, the constant header and the default derived JWT
    // key, as it always was.
    let (header, key): (String, JwtKey) = match field(args, "key") {
        None => (JWT_HEADER_B64.to_string(), JwtKey::Hs256(keys.k_jwt)),
        Some(named) => {
            let name = as_str(named).ok_or_else(|| {
                CallError::new(
                    "crypto/jwt_sign: args.key must be a string naming a derived label (crypto/derive) \
                     or a configured signing key (crypto.signing_keys)",
                )
            })?;
            // A configured secret is refused by name, and before the labels
            // are consulted: those bytes are shared with a peer by
            // definition, and KDF_LABEL_JWT is public, so a subkey derived
            // from them is one the peer can derive too. The subkey split
            // protects a key from a guest that never sees the bytes; it
            // cannot protect one from a party that already has them.
            if keys.secrets.iter().any(|(n, _)| n == name) {
                return Err(CallError::new(format!(
                    "crypto/jwt_sign: '{}' is a configured secret \
                     (config.connectors.crypto.secrets); those bytes are shared with a peer \
                     and cannot back a token this host trusts — derive a label (crypto/derive) \
                     and name that instead",
                    &name[..name.len().min(64)]
                )));
            }
            match named_jwt_key(c, keys, caller, name) {
                Some(k) => (kid_header_b64(&k, name), k),
                // Never-derived and released read alike, as in hmac.
                None => {
                    return Err(CallError::new(format!(
                        "crypto/jwt_sign: this caller derived no label named '{}' (crypto/derive), \
                         and this deployment configures no signing key by that name \
                         (config.connectors.crypto.signing_keys)",
                        &name[..name.len().min(64)]
                    )))
                }
            }
        }
    };

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

    // token = header "." base64url(payload) "." base64url(sig(header.payload))
    let mut token = String::with_capacity(payload.len() * 2);
    token.push_str(&header);
    token.push('.');
    token.push_str(&B64URL.encode(payload.as_bytes()));
    let sig = jwt_signature(&key, token.as_bytes());
    token.push('.');
    token.push_str(&sig);
    if token.len() > JWT_MAX_TOKEN {
        return Err(CallError::new("crypto/jwt_sign: the token is too large"));
    }
    Ok(rmpv::Value::from(token))
}

fn do_jwt_verify(
    c: &CryptoConnector,
    keys: &Keys,
    caller: Caller,
    args: Option<&rmpv::Value>,
) -> CallResult {
    let token = field(args, "token")
        .and_then(as_str)
        .ok_or_else(|| CallError::new("crypto/jwt_verify: args.token must be a string"))?;
    if token.len() > JWT_MAX_TOKEN {
        return verify_fail("oversized");
    }

    // Structure: exactly two dots.
    let Some(dot1) = token.find('.') else {
        return verify_fail("malformed");
    };
    let Some(dot2) = token[dot1 + 1..].find('.').map(|i| dot1 + 1 + i) else {
        return verify_fail("malformed");
    };
    let header = &token[..dot1];

    // The key is selected from the `kid` the header names and from nothing
    // else in the token (§5.2): look the name up for this caller, rebuild
    // the header that key would produce — in the key's algorithm, not one
    // the token claims — and compare the segment against it byte for byte.
    // `alg` is never read, so there is no field a token can set to be
    // checked differently: an EdDSA key's public bytes are never an HMAC
    // key, and an HS256 label never checks an Ed25519 signature. A header
    // carrying two `kid`s, or its members in another order, fails the
    // compare with no rule about either. No `kid` rebuilds to the constant
    // header and the default key — the branch `accept_unkeyed` bounds
    // (§5.3).
    let key: JwtKey = match header_kid(header) {
        Some(kid) => {
            let Some(k) = named_jwt_key(c, keys, caller, &kid) else {
                return verify_fail("kid");
            };
            if header != kid_header_b64(&k, &kid) {
                return verify_fail("alg");
            }
            k
        }
        None => {
            if header != JWT_HEADER_B64 {
                return verify_fail("alg");
            }
            if !keys.accept_unkeyed {
                return verify_fail("unkeyed");
            }
            JwtKey::Hs256(keys.k_jwt)
        }
    };

    // The signature over "header.payload", BEFORE decoding or parsing the
    // payload: the claims parser below only ever runs on bytes this host
    // signed with its own key.
    if !jwt_signature_ok(&key, &token.as_bytes()[..dot2], &token[dot2 + 1..]) {
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

/// `crypto/public_key {key}`: the public half of an EdDSA key as a JWK
/// (RFC 8037 §2), with the name as `kid` so the JWK and the tokens it
/// checks agree on it. The one answer in this family that is about a key
/// rather than made with one, and safe to publish by construction.
fn do_public_key(
    c: &CryptoConnector,
    keys: &Keys,
    caller: Caller,
    args: Option<&rmpv::Value>,
) -> CallResult {
    let name = field(args, "key").and_then(as_str).ok_or_else(|| {
        CallError::new(
            "crypto/public_key: args.key must be a string naming an EdDSA label (crypto/derive) \
             or a configured signing key (crypto.signing_keys)",
        )
    })?;
    let shown = &name[..name.len().min(64)];
    match &named_jwt_key(c, keys, caller, name) {
        Some(JwtKey::EdDsa(k)) => Ok(map(vec![
            ("kty", "OKP".into()),
            ("crv", "Ed25519".into()),
            ("x", B64URL.encode(k.verifying_key().to_bytes()).into()),
            ("kid", name.into()),
            ("alg", "EdDSA".into()),
            ("use", "sig".into()),
        ])),
        Some(JwtKey::Hs256(_)) => Err(CallError::new(format!(
            "crypto/public_key: '{shown}' is an HS256 key, which has no public half; \
             derive it with alg = \"EdDSA\" for a key another service can verify with"
        ))),
        None => Err(CallError::new(format!(
            "crypto/public_key: this caller derived no label named '{shown}' (crypto/derive), \
             and this deployment configures no signing key by that name \
             (config.connectors.crypto.signing_keys)"
        ))),
    }
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

/// The label patterns a set of grant scopes names: each scope is one
/// pattern or an array of them, in the capability grammar's trailing-`*`
/// shape. Anything else in a scope names nothing.
fn label_patterns(grants: &[Scope]) -> Vec<&str> {
    let mut out = Vec::new();
    for Scope(value) in grants {
        match value {
            rmpv::Value::String(_) | rmpv::Value::Binary(_) => out.extend(as_str(value)),
            rmpv::Value::Array(items) => out.extend(items.iter().filter_map(as_str)),
            _ => {}
        }
    }
    out
}

fn do_derive(
    c: &CryptoConnector,
    keys: &Keys,
    asker: &Asker<'_>,
    args: Option<&rmpv::Value>,
) -> CallResult {
    let label = field(args, "label")
        .and_then(as_str)
        .ok_or_else(|| CallError::new("crypto/derive: args.label must be a string"))?;
    if label.is_empty() || label.len() > LABEL_MAX || label.contains('\0') {
        return Err(CallError::new(format!(
            "crypto/derive: args.label must be 1..{LABEL_MAX} bytes with no NUL"
        )));
    }
    // The scope on the grant is the control (§4.3a): a deterministic label
    // is re-mintable by anyone permitted to name it, so who may name what
    // is decided here and nowhere else. The refusal says what this grant
    // allows and nothing about whether the label exists for someone else.
    let patterns = label_patterns(asker.grants);
    if !patterns.iter().any(|p| drt_caps::implies(p, label)) {
        return Err(CallError::new(if patterns.is_empty() {
            format!(
                "crypto/derive: '{}' is outside this instance's host:crypto/derive grant, \
                 which names no labels; a derive grant needs a scope of label patterns \
                 (\"room:2\", or \"room:*\")",
                &label[..label.len().min(64)]
            )
        } else {
            format!(
                "crypto/derive: '{}' is outside this instance's host:crypto/derive grant, \
                 which allows: {}",
                &label[..label.len().min(64)],
                patterns.join(", ")
            )
        }));
    }
    // A label may not shadow a configured secret. Found here, by name,
    // rather than by a signature that verifies on one host and not another.
    if keys.secrets.iter().any(|(n, _)| n == label) {
        return Err(CallError::new(format!(
            "crypto/derive: '{}' is already a configured secret \
             (config.connectors.crypto.secrets); a derived label may not shadow one",
            &label[..label.len().min(64)]
        )));
    }
    // Nor a configured signing key, for the same reason, found the same way.
    if keys.signing_keys.iter().any(|(n, _)| n == label) {
        return Err(CallError::new(format!(
            "crypto/derive: '{}' is already a configured signing key \
             (config.connectors.crypto.signing_keys); a derived label may not shadow one",
            &label[..label.len().min(64)]
        )));
    }
    // Which JWT key the label carries. The choice is the label's, made
    // here, so that jwt_verify can take the algorithm from the key and
    // never from a token.
    let alg = match field(args, "alg") {
        None => LabelAlg::Hs256,
        Some(v) => match as_str(v) {
            Some("HS256") => LabelAlg::Hs256,
            Some("EdDSA") => LabelAlg::EdDsa,
            _ => {
                return Err(CallError::new(
                    "crypto/derive: args.alg must be 'HS256' (the default) or 'EdDSA'",
                ))
            }
        },
    };
    let mut table = c.labels.lock().unwrap_or_else(|e| e.into_inner());
    // Idempotent: the bytes are a pure function of the master, the label
    // and the alg, so re-deriving is re-registering, and a refusal here
    // would leave a second owner of the same name unable to reach it. A
    // second derive that names the other alg is refused instead: one name
    // carrying two JWT keys would sign under one and be checked under the
    // other.
    let entry = table
        .entry((asker.caller, label.to_string()))
        .or_insert_with(|| LabelKeys::derive(&keys.k_label, label, alg));
    let held = entry.jwt.alg();
    let asked = match alg {
        LabelAlg::Hs256 => "HS256",
        LabelAlg::EdDsa => "EdDSA",
    };
    if held != asked {
        return Err(CallError::new(format!(
            "crypto/derive: '{}' is already derived here with alg '{held}', not '{asked}'; \
             one label carries one JWT key, so name another label",
            &label[..label.len().min(64)]
        )));
    }
    // Nothing comes back. That is the point.
    Ok(rmpv::Value::Nil)
}

// depth: TOTP (RFC 6238 over RFC 4226)

/// The two hashes TOTP runs on here. SHA-1 is RFC 6238's default and what
/// an enrolment that names none means to every authenticator; it is HMAC,
/// where SHA-1's collision breaks do not reach.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TotpAlg {
    Sha1,
    Sha256,
}

impl TotpAlg {
    /// RFC 6238's seed length for the hash: its output size.
    fn seed_len(self) -> usize {
        match self {
            TotpAlg::Sha1 => 20,
            TotpAlg::Sha256 => 32,
        }
    }

    /// The `algorithm` parameter of an `otpauth://` URI.
    fn uri_name(self) -> &'static str {
        match self {
            TotpAlg::Sha1 => "SHA1",
            TotpAlg::Sha256 => "SHA256",
        }
    }
}

/// The parameters every TOTP call shares, read and bounded once.
struct TotpParams {
    alg: TotpAlg,
    digits: u32,
    period: u64,
}

fn totp_params(call: &str, args: Option<&rmpv::Value>) -> Result<TotpParams, CallError> {
    let alg = match field(args, "alg") {
        None => TotpAlg::Sha1,
        Some(v) => match as_str(v) {
            Some("sha1") => TotpAlg::Sha1,
            Some("sha256") => TotpAlg::Sha256,
            _ => {
                return Err(CallError::new(format!(
                    "{call}: args.alg must be 'sha1' (the default) or 'sha256'"
                )))
            }
        },
    };
    let digits = bounded_int(
        call,
        args,
        "digits",
        TOTP_DIGITS_DEFAULT,
        TOTP_DIGITS_MIN,
        TOTP_DIGITS_MAX,
    )?;
    let period = bounded_int(
        call,
        args,
        "period",
        TOTP_PERIOD_DEFAULT,
        TOTP_PERIOD_MIN,
        TOTP_PERIOD_MAX,
    )?;
    Ok(TotpParams {
        alg,
        digits: digits as u32,
        period: period as u64,
    })
}

/// An optional integer argument in `min..=max`, `default` when absent. Out
/// of range is a refusal naming the range, never a clamp: a program that
/// asked for nine digits and got eight would enrol one thing and check
/// another.
fn bounded_int(
    call: &str,
    args: Option<&rmpv::Value>,
    name: &str,
    default: i64,
    min: i64,
    max: i64,
) -> Result<i64, CallError> {
    match field(args, name) {
        None => Ok(default),
        Some(v) => v
            .as_i64()
            .filter(|n| (min..=max).contains(n))
            .ok_or_else(|| {
                CallError::new(format!(
                    "{call}: args.{name} must be an integer {min}..{max}"
                ))
            }),
    }
}

/// RFC 4226 §5.3: HMAC over the counter, dynamic truncation, the low
/// `digits` decimal digits, zero-padded.
fn hotp(seed: &[u8], alg: TotpAlg, counter: u64, digits: u32) -> String {
    let msg = counter.to_be_bytes();
    let mac: Vec<u8> = match alg {
        TotpAlg::Sha1 => {
            let mut m = <HmacSha1 as Mac>::new_from_slice(seed).expect("hmac takes any key length");
            m.update(&msg);
            m.finalize().into_bytes().to_vec()
        }
        TotpAlg::Sha256 => hmac_sha256(seed, &msg).to_vec(),
    };
    let offset = (mac[mac.len() - 1] & 0x0f) as usize;
    let binary = (u32::from(mac[offset] & 0x7f) << 24)
        | (u32::from(mac[offset + 1]) << 16)
        | (u32::from(mac[offset + 2]) << 8)
        | u32::from(mac[offset + 3]);
    let code = binary % 10u32.pow(digits);
    format!("{code:0width$}", width = digits as usize)
}

/// The time step now: RFC 6238's `T = floor((now - T0) / X)` with `T0 = 0`.
fn totp_step_now(period: u64) -> u64 {
    (now_secs().max(0) as u64) / period
}

/// The seed a `key` names, truncated to the algorithm's length when it is
/// a label's. `enrolling` refuses a configured secret: those bytes were
/// provisioned outside this host and are never handed to a program.
fn totp_seed(
    c: &CryptoConnector,
    keys: &Keys,
    caller: Caller,
    call: &str,
    args: Option<&rmpv::Value>,
    alg: TotpAlg,
    enrolling: bool,
) -> Result<Vec<u8>, CallError> {
    let name = field(args, "key").and_then(as_str).ok_or_else(|| {
        CallError::new(format!(
            "{call}: args.key must be a string naming a derived label (crypto/derive){}",
            if enrolling {
                ""
            } else {
                " or a configured secret (crypto.secrets)"
            }
        ))
    })?;
    let shown = &name[..name.len().min(64)];
    if let Some((_, bytes)) = keys.secrets.iter().find(|(n, _)| n == name) {
        if enrolling {
            return Err(CallError::new(format!(
                "{call}: '{shown}' is a configured secret (config.connectors.crypto.secrets), \
                 which is never handed to a program; enrol a derived label (crypto/derive) instead"
            )));
        }
        return Ok(bytes.clone());
    }
    match c.label_totp_seed(caller, name) {
        Some(seed) => Ok(seed[..alg.seed_len()].to_vec()),
        None => Err(CallError::new(format!(
            "{call}: this caller derived no label named '{shown}' (crypto/derive){}",
            if enrolling {
                ""
            } else {
                ", and this deployment configures no secret by that name \
                 (config.connectors.crypto.secrets)"
            }
        ))),
    }
}

/// `crypto/totp {key, alg?, digits?, period?}`: the code for now, the step
/// it belongs to, and when it stops being the current one (unix seconds).
fn do_totp(
    c: &CryptoConnector,
    keys: &Keys,
    caller: Caller,
    args: Option<&rmpv::Value>,
) -> CallResult {
    let p = totp_params("crypto/totp", args)?;
    let seed = totp_seed(c, keys, caller, "crypto/totp", args, p.alg, false)?;
    let step = totp_step_now(p.period);
    Ok(map(vec![
        ("code", hotp(&seed, p.alg, step, p.digits).into()),
        ("step", rmpv::Value::from(step)),
        ("expires", rmpv::Value::from((step + 1) * p.period)),
    ]))
}

/// `crypto/totp_verify {key, code, ..., window?, after?}`. A verdict, like
/// `jwt_verify`'s: a wrong code is `{valid = false, reason}` with status
/// ok. Reasons: `malformed` (not `digits` decimal digits), `mismatch`,
/// `replayed` (it matched, but at a step no later than `after`).
fn do_totp_verify(
    c: &CryptoConnector,
    keys: &Keys,
    caller: Caller,
    args: Option<&rmpv::Value>,
) -> CallResult {
    const CALL: &str = "crypto/totp_verify";
    let p = totp_params(CALL, args)?;
    let window = bounded_int(
        CALL,
        args,
        "window",
        TOTP_WINDOW_DEFAULT,
        0,
        TOTP_WINDOW_MAX,
    )? as u64;
    let after = match field(args, "after") {
        None => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| {
            CallError::new(format!(
                "{CALL}: args.after must be a step a previous totp_verify answered"
            ))
        })?),
    };
    // A code is a string, because a leading zero is part of it. An integer
    // is taken too and zero-padded, since a Lua program that read the code
    // as a number has lost the zeros and nothing else.
    let code: String = match field(args, "code") {
        Some(v) if as_str(v).is_some() => as_str(v).unwrap_or_default().to_string(),
        Some(v) if v.as_u64().is_some() => {
            let n = v.as_u64().unwrap_or_default();
            format!("{n:0width$}", width = p.digits as usize)
        }
        _ => {
            return Err(CallError::new(format!(
                "{CALL}: args.code must be a string of digits"
            )))
        }
    };
    let seed = totp_seed(c, keys, caller, CALL, args, p.alg, false)?;
    if code.len() != p.digits as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
        return verify_fail("malformed");
    }

    // Every step in the window is computed and compared, in constant time
    // per compare, and the scan does not stop at a match, so how long the
    // call takes says nothing about where in the window a code fell.
    let now = totp_step_now(p.period);
    let mut matched: Option<u64> = None;
    let mut replayed = false;
    for step in now.saturating_sub(window)..=now.saturating_add(window) {
        let candidate = hotp(&seed, p.alg, step, p.digits);
        let hit = bool::from(subtle::ConstantTimeEq::ct_eq(
            candidate.as_bytes(),
            code.as_bytes(),
        ));
        if hit {
            if after.is_some_and(|a| step <= a) {
                replayed = true;
            } else if matched.is_none() {
                matched = Some(step);
            }
        }
    }
    match matched {
        Some(step) => Ok(map(vec![
            ("valid", rmpv::Value::Boolean(true)),
            ("step", rmpv::Value::from(step)),
        ])),
        None if replayed => verify_fail("replayed"),
        None => verify_fail("mismatch"),
    }
}

/// `crypto/totp_enroll {key, account, issuer?, ...}`: the seed of a label,
/// as base32 and as the `otpauth://` URI an authenticator scans. This is
/// the one call in the family whose answer is key material, and it is
/// its own grant so that the program checking codes never holds it.
fn do_totp_enroll(
    c: &CryptoConnector,
    keys: &Keys,
    caller: Caller,
    args: Option<&rmpv::Value>,
) -> CallResult {
    const CALL: &str = "crypto/totp_enroll";
    let p = totp_params(CALL, args)?;
    let name_arg = |name: &str, required: bool| -> Result<Option<&str>, CallError> {
        match field(args, name).map(as_str) {
            None if !required => Ok(None),
            Some(Some(v)) if !v.is_empty() && v.len() <= TOTP_NAME_MAX && !v.contains(':') => {
                Ok(Some(v))
            }
            _ => Err(CallError::new(format!(
                "{CALL}: args.{name} must be a string of 1..{TOTP_NAME_MAX} bytes with no ':' \
                 (the URI's label is issuer:account)"
            ))),
        }
    };
    let account = name_arg("account", true)?.unwrap_or_default();
    let issuer = name_arg("issuer", false)?;
    let seed = totp_seed(c, keys, caller, CALL, args, p.alg, true)?;
    let secret = base32(&seed);

    // Google's Key URI format: label is `issuer:account`, percent-encoded,
    // and `issuer` is repeated as a parameter, which is the one current
    // authenticators read. Every parameter is written out, defaults too,
    // so the URI means one thing to every app.
    let mut uri = String::from("otpauth://totp/");
    if let Some(issuer) = issuer {
        uri.push_str(&percent_encode(issuer));
        uri.push(':');
    }
    uri.push_str(&percent_encode(account));
    uri.push_str("?secret=");
    uri.push_str(&secret);
    if let Some(issuer) = issuer {
        uri.push_str("&issuer=");
        uri.push_str(&percent_encode(issuer));
    }
    uri.push_str(&format!(
        "&algorithm={}&digits={}&period={}",
        p.alg.uri_name(),
        p.digits,
        p.period
    ));
    Ok(map(vec![("secret", secret.into()), ("uri", uri.into())]))
}

/// RFC 4648 §6 base32, unpadded, which is how an `otpauth://` URI carries
/// a seed.
fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for &b in bytes {
        buffer = (buffer << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

/// RFC 3986 percent-encoding of everything but the unreserved characters.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[async_trait::async_trait]
impl Connector for CryptoConnector {
    fn scope_type(&self) -> Box<dyn ScopeType> {
        Box::new(CryptoScopeType)
    }

    /// The caller-blind path presents as the root with no grants: every
    /// call that needs no identity works exactly as before, and `derive`
    /// — which needs a scoped grant — refuses, since an unknown caller is
    /// not permitted to name any label.
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
        // hash and random need no key, but the wiring is still the gate:
        // a build that wires no crypto scope answers nothing in the family.
        let keys = self.keys(scope)?;
        match call {
            "crypto/random" => do_random(args),
            "crypto/hash" => do_hash(args),
            "crypto/hmac" => do_hmac(self, &keys, asker.caller, args),
            "crypto/jwt_sign" => do_jwt_sign(self, &keys, asker.caller, args),
            "crypto/jwt_verify" => do_jwt_verify(self, &keys, asker.caller, args),
            "crypto/public_key" => do_public_key(self, &keys, asker.caller, args),
            "crypto/turn_credential" => do_turn_credential(&keys, args),
            "crypto/derive" => do_derive(self, &keys, asker, args),
            "crypto/totp" => do_totp(self, &keys, asker.caller, args),
            "crypto/totp_verify" => do_totp_verify(self, &keys, asker.caller, args),
            "crypto/totp_enroll" => do_totp_enroll(self, &keys, asker.caller, args),
            other => Err(CallError::new(format!(
                "the crypto connector has no call '{other}'"
            ))),
        }
    }

    /// A node is gone; its labels go with it. Nothing is lost by that, and
    /// that is a claim rather than a shrug: a label is a pure function of
    /// the master and its name, so whatever it signed still verifies and
    /// whoever needs it again derives it again.
    fn release(&self, caller: &Caller) -> Vec<String> {
        self.labels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(owner, _), _| owner != caller);
        Vec::new()
    }
}

// depth: the published vectors, against the private primitives
//
// The connector's calls read the host clock, which a test cannot set, so
// the RFC tables are checked here against the arithmetic those calls run.
// `tests/totp.rs` checks the calls against an independent implementation
// at the real time.
#[cfg(test)]
mod vectors {
    use super::*;

    /// RFC 4226 Appendix D: HOTP over the ASCII seed, counters 0 to 9.
    #[test]
    fn rfc4226_hotp() {
        let seed = b"12345678901234567890";
        let want = [
            "755224", "287082", "359152", "969429", "338314", "254676", "287922", "162583",
            "399871", "520489",
        ];
        for (counter, code) in want.iter().enumerate() {
            assert_eq!(
                hotp(seed, TotpAlg::Sha1, counter as u64, 6),
                *code,
                "counter {counter}"
            );
        }
    }

    /// RFC 6238 Appendix B, the SHA-1 and SHA-256 columns: eight digits,
    /// a thirty-second period, each hash's own seed.
    #[test]
    fn rfc6238_totp() {
        let sha1 = b"12345678901234567890";
        let sha256 = b"12345678901234567890123456789012";
        let table: [(u64, &str, &str); 6] = [
            (59, "94287082", "46119246"),
            (1111111109, "07081804", "68084774"),
            (1111111111, "14050471", "67062674"),
            (1234567890, "89005924", "91819424"),
            (2000000000, "69279037", "90698825"),
            (20000000000, "65353130", "77737706"),
        ];
        for (time, want1, want256) in table {
            let step = time / 30;
            assert_eq!(hotp(sha1, TotpAlg::Sha1, step, 8), want1, "sha1 at {time}");
            assert_eq!(
                hotp(sha256, TotpAlg::Sha256, step, 8),
                want256,
                "sha256 at {time}"
            );
        }
    }

    /// RFC 4648 §10, less the padding an `otpauth://` URI leaves off.
    #[test]
    fn rfc4648_base32() {
        for (input, want) in [
            ("", ""),
            ("f", "MY"),
            ("fo", "MZXQ"),
            ("foo", "MZXW6"),
            ("foob", "MZXW6YQ"),
            ("fooba", "MZXW6YTB"),
            ("foobar", "MZXW6YTBOI"),
        ] {
            assert_eq!(base32(input.as_bytes()), want, "{input:?}");
        }
        // RFC 6238's SHA-1 seed, as every authenticator test page spells it.
        assert_eq!(
            base32(b"12345678901234567890"),
            "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"
        );
    }

    #[test]
    fn percent_encoding_keeps_only_the_unreserved() {
        assert_eq!(percent_encode("alice@example.com"), "alice%40example.com");
        assert_eq!(percent_encode("Acme Corp"), "Acme%20Corp");
        assert_eq!(percent_encode("a-b.c_d~e"), "a-b.c_d~e");
    }
}
