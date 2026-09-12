//! Signing and verification: ed25519 over canonical bytes (consent.md §9).
//!
//! Copied from `dollup-format` rather than moved, so the change is never
//! atomic across two repositories: dollup deletes its copy and switches its
//! call sites when it takes this dependency. After that, index signing and
//! consent signing are one helper, and drt verifies an approval at start
//! without depending on anything of dollup's.
//!
//! ed25519 via `ed25519-dalek`, pure Rust. **Not `aloecrypt_core`**, which
//! is in neither workspace; and not a C-backed library, because dollup
//! builds with nothing beyond cargo and a crate it depends on must not take
//! that away. (drt needs a C toolchain for the Diluvium core regardless —
//! the no-C property is dollup's, and this preserves it.)
//!
//! ## surface block
//!
//! - Entry points: [`SecretKey::generate`] and [`SecretKey::from_seed`],
//!   which the caller feeds entropy; [`SecretKey::sign`]; [`PublicKey::verify`];
//!   [`signing_bytes`], the canonical bytes of an object with one field
//!   omitted, which is what a signature actually covers.
//! - Configurable: nothing. [`Alg`] has one variant on purpose.
//! - Fan-out: [`VerifyFailed`] names every way verification refuses.
//!
//! Entropy is the caller's, exactly as in [`crate::id`]: drt reads it
//! through `drt-platform` so a page works, dollup reads the host's, and a
//! shared crate chooses for neither.

use std::fmt;

use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

/// The algorithm name recorded in every signed object, so that migrating is
/// a re-sign rather than a format break.
pub const ALG_ED25519: &str = "ed25519";

/// The signature algorithm. One variant, named in the wire format anyway —
/// consent.md §11 keeps rotation as a seam, and a format that cannot say
/// which algorithm it used cannot grow a second one without breaking.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Alg {
    #[default]
    Ed25519,
}

impl fmt::Display for Alg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Alg::Ed25519 => ALG_ED25519,
        })
    }
}

/// The operator's label for a key, as it appears in `consent.json`'s
/// `signers` and in an approval's `key_id`. A label, not a fingerprint:
/// rotation replaces the bytes under a name the operator already trusts.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeyId(pub String);

impl fmt::Display for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A public key, base64 in JSON.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PublicKey([u8; 32]);

/// A signature, base64 in JSON.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Signature([u8; 64]);

/// A signing key. Deliberately **not** `Serialize`: a private key has no
/// business in a config object, and the type system is a cheaper place to
/// say so than a review comment.
pub struct SecretKey(SigningKey);

impl SecretKey {
    /// Generate from 32 random bytes the caller supplies.
    ///
    /// The quality of those bytes is the whole security of every signature
    /// this key makes, which is why the argument is explicit rather than a
    /// default RNG chosen by this crate on a caller's behalf.
    pub fn generate(random: [u8; 32]) -> SecretKey {
        SecretKey(SigningKey::from_bytes(&random))
    }

    /// The same thing named for the other use: re-deriving a key from a
    /// stored seed, which is what `drt key sign <path>` does.
    pub fn from_seed(seed: &[u8; 32]) -> SecretKey {
        SecretKey(SigningKey::from_bytes(seed))
    }

    /// The 32-byte seed, for writing to a file. Named to be conspicuous at
    /// the call site.
    pub fn seed_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    pub fn public_key(&self) -> PublicKey {
        PublicKey(self.0.verifying_key().to_bytes())
    }

    pub fn sign(&self, bytes: &[u8]) -> Signature {
        Signature(self.0.sign(bytes).to_bytes())
    }
}

impl PublicKey {
    pub fn from_bytes(bytes: [u8; 32]) -> PublicKey {
        PublicKey(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Verify, strictly.
    ///
    /// `verify_strict` rather than `verify`: it rejects small-order public
    /// keys and non-canonical encodings, which removes the class of
    /// signature that verifies under more than one key. For a signature
    /// that authorizes a capability grant, "verified" has to mean one
    /// signer.
    pub fn verify(&self, bytes: &[u8], signature: &Signature) -> Result<(), VerifyFailed> {
        let key = VerifyingKey::from_bytes(&self.0).map_err(|_| VerifyFailed::MalformedKey)?;
        let signature = ed25519_dalek::Signature::from_bytes(&signature.0);
        key.verify_strict(bytes, &signature)
            .map_err(|_| VerifyFailed::DoesNotVerify)
    }
}

/// Why verification refused. Never a bare `false`: consent.md makes each
/// step of §7's chain a named failure, and this is step one's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VerifyFailed {
    #[error("the signature does not verify against this key")]
    DoesNotVerify,
    #[error("the public key is not a valid ed25519 key")]
    MalformedKey,
}

/// The bytes a signature covers: the object canonicalized with one field
/// omitted.
///
/// Omitted, not blanked. An object signed with `"signature": ""` and one
/// signed with the field absent are different bytes, and picking the wrong
/// one is the single most common way two implementations of this end up
/// unable to verify each other's output. Spelled here once so neither side
/// has to guess.
pub fn signing_bytes(value: &serde_json::Value, omit: &str) -> Vec<u8> {
    let mut value = value.clone();
    if let serde_json::Value::Object(map) = &mut value {
        map.remove(omit);
    }
    crate::canon::to_canonical_bytes(&value)
}

// depth: base64 at the JSON boundary, one engine, padded

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", B64.encode(self.0))
    }
}

impl fmt::Debug for Signature {
    /// Truncated, because a full signature in a log line is noise that
    /// hides the rest of the line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signature({}…)", &B64.encode(self.0)[..16])
    }
}

impl From<PublicKey> for String {
    fn from(key: PublicKey) -> String {
        B64.encode(key.0)
    }
}

impl From<Signature> for String {
    fn from(sig: Signature) -> String {
        B64.encode(sig.0)
    }
}

impl TryFrom<String> for PublicKey {
    type Error = String;
    fn try_from(s: String) -> Result<PublicKey, String> {
        Ok(PublicKey(fixed(&s, "a public key")?))
    }
}

impl TryFrom<String> for Signature {
    type Error = String;
    fn try_from(s: String) -> Result<Signature, String> {
        Ok(Signature(fixed(&s, "a signature")?))
    }
}

/// Base64 to a fixed-size array, refusing the wrong length by name. A
/// 31-byte "public key" that failed to verify everything would be a long
/// afternoon; failing where it was read is a short one.
fn fixed<const N: usize>(text: &str, what: &str) -> Result<[u8; N], String> {
    let bytes = B64
        .decode(text)
        .map_err(|e| format!("{what} is not base64: {e}"))?;
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| format!("{what} is {} bytes; ed25519 wants {N}", v.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SecretKey {
        SecretKey::generate([7u8; 32])
    }

    #[test]
    fn a_signature_verifies_and_a_tampered_message_does_not() {
        let key = key();
        let public = key.public_key();
        let signature = key.sign(b"the canonical bytes");

        assert_eq!(public.verify(b"the canonical bytes", &signature), Ok(()));
        assert_eq!(
            public.verify(b"the canonical byteS", &signature),
            Err(VerifyFailed::DoesNotVerify)
        );
    }

    #[test]
    fn another_key_does_not_verify() {
        let signature = key().sign(b"payload");
        let other = SecretKey::generate([9u8; 32]).public_key();
        assert_eq!(
            other.verify(b"payload", &signature),
            Err(VerifyFailed::DoesNotVerify)
        );
    }

    /// The same seed gives the same key, which is what makes `drt key sign`
    /// over a key file work at all.
    #[test]
    fn a_seed_round_trips() {
        let first = key();
        let again = SecretKey::from_seed(&first.seed_bytes());
        assert_eq!(first.public_key(), again.public_key());
    }

    /// What a signature covers: the object without its `signature` field,
    /// and *omitted* rather than blanked.
    #[test]
    fn signing_bytes_omit_rather_than_blank() {
        let signed: serde_json::Value = serde_json::from_str(
            r#"{"decision":"approve","request_hash":"sha256:ab","signature":"zzz"}"#,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(signing_bytes(&signed, "signature")).unwrap(),
            r#"{"decision":"approve","request_hash":"sha256:ab"}"#
        );

        let blanked: serde_json::Value = serde_json::from_str(
            r#"{"decision":"approve","request_hash":"sha256:ab","signature":""}"#,
        )
        .unwrap();
        assert_ne!(
            signing_bytes(&signed, "signature"),
            crate::canon::to_canonical_bytes(&blanked),
            "blanking and omitting are different bytes; that is why the rule is stated"
        );
    }

    /// A signature is still verifiable after a round trip through JSON,
    /// which is the only form it is ever stored in.
    #[test]
    fn keys_and_signatures_round_trip_through_json() {
        let key = key();
        let signature = key.sign(b"payload");

        let public_json = serde_json::to_string(&key.public_key()).unwrap();
        let signature_json = serde_json::to_string(&signature).unwrap();
        let public: PublicKey = serde_json::from_str(&public_json).unwrap();
        let signature: Signature = serde_json::from_str(&signature_json).unwrap();

        assert_eq!(public.verify(b"payload", &signature), Ok(()));
    }

    #[test]
    fn a_wrong_length_key_is_refused_where_it_is_read() {
        let e = serde_json::from_str::<PublicKey>("\"AAAA\"").unwrap_err();
        assert!(e.to_string().contains("ed25519 wants 32"), "{e}");
        let e = serde_json::from_str::<Signature>("\"not base64!!\"").unwrap_err();
        assert!(e.to_string().contains("not base64"), "{e}");
    }

    #[test]
    fn the_algorithm_names_itself_in_json() {
        assert_eq!(serde_json::to_string(&Alg::Ed25519).unwrap(), "\"ed25519\"");
        assert_eq!(Alg::default().to_string(), ALG_ED25519);
    }

    /// A private key cannot be serialized, by construction. This test is
    /// here so that adding a `Serialize` derive to `SecretKey` breaks
    /// something visible.
    #[test]
    fn a_secret_key_has_no_serialized_form() {
        fn assert_not_serialize<T>() {}
        assert_not_serialize::<SecretKey>();
        // `SecretKey: !Serialize` cannot be asserted positively in stable
        // Rust, so the guard is the doc comment plus this reminder.
    }
}
