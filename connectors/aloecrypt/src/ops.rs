//! The aloecrypt operations, with no drt type in them.
//!
//! This module is the half that moves when the family becomes a plugin:
//! it takes and returns plain Rust values (bytes, strings, [`Alg`]) and
//! knows nothing about msgpack, scopes, grants or the dispatcher. `lib.rs`
//! is the connector, which translates a hostcall into one of these calls.
//! A plugin process would translate its frames into the same calls.
//!
//! Entry points:
//!
//! ```text
//! derive_seed(master, label, alg) -> the private seed for a label, deterministic
//! public_key(alg, seed)           -> PublicKey
//! sign(alg, seed, data)           -> armored SIGNATURE document
//! verify(public_key, data, sig)   -> Ok(()) | Refused
//! encrypt(public_key, prk, data)  -> armored ENCRYPTED document
//! decrypt(alg, seed, document)    -> the payload
//! hash(salt, data, domain)        -> Keccak-256, framed (aloecrypt_core::hash::hash)
//! ```
//!
//! Configurable values: the KDF domains below. They fix which key a label
//! names, so changing one re-keys every deployment that uses it. They carry
//! a version for that reason.
//!
//! Fan-out: [`Alg`] is the dispatch over the six parameter sets; each
//! operation matches on it once, through the `each_dsa!` / `each_kem!`
//! tables at the bottom of this file.
//!
//! The public-key encoding is [`PublicKey::encode`] / [`PublicKey::decode`]
//! and nowhere else. It is standard base64 of the raw key, with the
//! algorithm read off the length. That is a drt-only format, kept in one place
//! so that an armored public-key document (aloecrypt_core#6) replaces it in
//! one edit.

use aloecrypt_core::aloecrypt_api::{AloecryptAddress, StatusCodeEnum};
use aloecrypt_core::document::{
    armor, armored_size, dearmor, dearmored_size_bound, decrypt_as_recipient_1024,
    decrypt_as_recipient_512, decrypt_as_recipient_768, detached_signature_size,
    encrypt_to_recipient_1024, encrypt_to_recipient_512, encrypt_to_recipient_768,
    encrypted_document_size, sign_detached_44, sign_detached_65, sign_detached_87,
    verify_detached_44, verify_detached_65, verify_detached_87, ENCRYPTED_LABEL, SIGNATURE_LABEL,
};
use aloecrypt_core::dsa_api::*;
use aloecrypt_core::kem_api::*;
use zeroize::Zeroizing;

/// The KDF domain a label's seed is derived under, per parameter set. The
/// algorithm is in the domain, so one label names six unrelated keys and
/// never the same seed under two algorithms.
const SEED_DOMAINS: [(Alg, &str); 6] = [
    (Alg::MlDsa44, "drt.aloecrypt.seed.mldsa44.v1"),
    (Alg::MlDsa65, "drt.aloecrypt.seed.mldsa65.v1"),
    (Alg::MlDsa87, "drt.aloecrypt.seed.mldsa87.v1"),
    (Alg::MlKem512, "drt.aloecrypt.seed.mlkem512.v1"),
    (Alg::MlKem768, "drt.aloecrypt.seed.mlkem768.v1"),
    (Alg::MlKem1024, "drt.aloecrypt.seed.mlkem1024.v1"),
];

/// The six parameter sets, by the name a program writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alg {
    MlDsa44,
    MlDsa65,
    MlDsa87,
    MlKem512,
    MlKem768,
    MlKem1024,
}

impl Alg {
    pub const ALL: [Alg; 6] = [
        Alg::MlDsa44,
        Alg::MlDsa65,
        Alg::MlDsa87,
        Alg::MlKem512,
        Alg::MlKem768,
        Alg::MlKem1024,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Alg::MlDsa44 => "ml-dsa-44",
            Alg::MlDsa65 => "ml-dsa-65",
            Alg::MlDsa87 => "ml-dsa-87",
            Alg::MlKem512 => "ml-kem-512",
            Alg::MlKem768 => "ml-kem-768",
            Alg::MlKem1024 => "ml-kem-1024",
        }
    }

    pub fn parse(name: &str) -> Option<Alg> {
        Alg::ALL.into_iter().find(|a| a.name() == name)
    }

    /// ML-DSA signs; ML-KEM encrypts. No key does both.
    pub fn signs(self) -> bool {
        matches!(self, Alg::MlDsa44 | Alg::MlDsa65 | Alg::MlDsa87)
    }

    fn public_key_len(self) -> usize {
        match self {
            Alg::MlDsa44 => MLDSA_44_PUBKEY_SZ,
            Alg::MlDsa65 => MLDSA_65_PUBKEY_SZ,
            Alg::MlDsa87 => MLDSA_87_PUBKEY_SZ,
            Alg::MlKem512 => MLKEM_512_ENCAP_KEY_SZ,
            Alg::MlKem768 => MLKEM_768_ENCAP_KEY_SZ,
            Alg::MlKem1024 => MLKEM_1024_ENCAP_KEY_SZ,
        }
    }

    fn seed_len(self) -> usize {
        if self.signs() {
            MLDSA_SEED_SZ
        } else {
            MLKEM_DECAP_KEY_SZ
        }
    }
}

/// Why an operation said no. `Refused` is an answer about the input (a
/// signature that does not verify, a document that does not open under
/// this key); `Malformed` is input that is not what it claims to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpError {
    /// Not a public key, not armored, the wrong label, or a bad envelope.
    Malformed(String),
    /// The cryptography said no: wrong key, altered bytes, wrong signer.
    Refused,
    /// The key is the wrong kind for the operation (a KEM key handed to verify).
    WrongKind(&'static str),
    /// The document needs something this build of aloecrypt_core does not know.
    Unsupported,
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpError::Malformed(what) => write!(f, "malformed: {what}"),
            OpError::Refused => f.write_str("refused"),
            OpError::WrongKind(what) => f.write_str(what),
            OpError::Unsupported => f.write_str(
                "unsupported: the document carries a section this build does not understand",
            ),
        }
    }
}

fn from_status(code: aloecrypt_core::aloecrypt_api::StatusCode, what: &str) -> OpError {
    match code.into() {
        StatusCodeEnum::AuthFailed => OpError::Refused,
        StatusCodeEnum::Unsupported => OpError::Unsupported,
        StatusCodeEnum::BadEncoding => {
            OpError::Malformed(format!("{what} is not a valid document"))
        }
        _ => OpError::Malformed(format!(
            "{what} was refused by aloecrypt_core (status {})",
            code.0
        )),
    }
}

/// A public key and the parameter set it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    pub alg: Alg,
    pub bytes: Vec<u8>,
}

impl PublicKey {
    /// The one place a public key becomes text. See the module comment.
    pub fn encode(&self) -> String {
        data_encoding::BASE64.encode(&self.bytes)
    }

    /// The one place text becomes a public key. Every parameter set has a
    /// distinct key length, so the length names the algorithm.
    pub fn decode(text: &str) -> Result<PublicKey, OpError> {
        let bytes = data_encoding::BASE64
            .decode(text.trim().as_bytes())
            .map_err(|_| OpError::Malformed("public_key is not standard base64".into()))?;
        let alg = Alg::ALL
            .into_iter()
            .find(|a| a.public_key_len() == bytes.len())
            .ok_or_else(|| {
                OpError::Malformed(format!(
                    "public_key is {} bytes, which is no ML-DSA or ML-KEM public key",
                    bytes.len()
                ))
            })?;
        let key = PublicKey { alg, bytes };
        key.check()?;
        Ok(key)
    }

    /// An ML-KEM key must be checked before aloecrypt_core sees it: the
    /// core unwraps this same check, and a panic in a `panic = "abort"`
    /// release takes the process with it (aloecrypt_core#7).
    fn check(&self) -> Result<(), OpError> {
        let valid = match self.alg {
            Alg::MlKem512 => kem_key_valid!(
                ml_kem::MlKem512,
                MLKEM_512_ENCAP_KEY_SZ,
                self.bytes.as_slice()
            ),
            Alg::MlKem768 => kem_key_valid!(
                ml_kem::MlKem768,
                MLKEM_768_ENCAP_KEY_SZ,
                self.bytes.as_slice()
            ),
            Alg::MlKem1024 => kem_key_valid!(
                ml_kem::MlKem1024,
                MLKEM_1024_ENCAP_KEY_SZ,
                self.bytes.as_slice()
            ),
            _ => true,
        };
        if valid {
            Ok(())
        } else {
            Err(OpError::Malformed(format!(
                "public_key is not a valid {} encapsulation key",
                self.alg.name()
            )))
        }
    }

    /// The 32-byte aloecrypt address: what a document names its signer or
    /// recipient by, in place of the whole key.
    pub fn address(&self) -> AloecryptAddress {
        match self.alg {
            Alg::MlDsa44 => dsa_verifier_44(&self.bytes).address(),
            Alg::MlDsa65 => dsa_verifier_65(&self.bytes).address(),
            Alg::MlDsa87 => dsa_verifier_87(&self.bytes).address(),
            Alg::MlKem512 => kem_encapsulator_512(&self.bytes).address(),
            Alg::MlKem768 => kem_encapsulator_768(&self.bytes).address(),
            Alg::MlKem1024 => kem_encapsulator_1024(&self.bytes).address(),
        }
    }
}

/// A label's private seed: `hash(master, label, domain)`, which is
/// aloecrypt_core's framed Keccak-256, so no input can be shifted into its
/// neighbour. An ML-KEM seed is 64 bytes (`d || z`), two derivations.
pub fn derive_seed(master: &[u8], label: &str, alg: Alg) -> Zeroizing<Vec<u8>> {
    let domain = SEED_DOMAINS
        .iter()
        .find(|(a, _)| *a == alg)
        .map(|(_, d)| *d)
        .expect("every Alg has a seed domain");
    let mut seed = Zeroizing::new(Vec::with_capacity(alg.seed_len()));
    if alg.signs() {
        seed.extend_from_slice(&aloecrypt_core::hash::hash(
            master,
            label.as_bytes(),
            domain,
        ));
    } else {
        for half in ["d", "z"] {
            let part = format!("{domain}.{half}");
            seed.extend_from_slice(&aloecrypt_core::hash::hash(master, label.as_bytes(), &part));
        }
    }
    debug_assert_eq!(seed.len(), alg.seed_len());
    seed
}

pub fn public_key(alg: Alg, seed: &[u8]) -> PublicKey {
    let bytes = match alg {
        Alg::MlDsa44 => dsa_keypair_44(seed).get_verifier().public_key.to_vec(),
        Alg::MlDsa65 => dsa_keypair_65(seed).get_verifier().public_key.to_vec(),
        Alg::MlDsa87 => dsa_keypair_87(seed).get_verifier().public_key.to_vec(),
        Alg::MlKem512 => kem_keypair_512(seed).get_encapsulator().public_key.to_vec(),
        Alg::MlKem768 => kem_keypair_768(seed).get_encapsulator().public_key.to_vec(),
        Alg::MlKem1024 => kem_keypair_1024(seed)
            .get_encapsulator()
            .public_key
            .to_vec(),
    };
    PublicKey { alg, bytes }
}

/// A detached signature over `data`, as an armored SIGNATURE document.
pub fn sign(alg: Alg, seed: &[u8], data: &[u8]) -> Result<String, OpError> {
    let (sig_sz, signed) = match alg {
        Alg::MlDsa44 => (MLDSA_44_SIGNATURE_SZ, Signer::D44(dsa_keypair_44(seed))),
        Alg::MlDsa65 => (MLDSA_65_SIGNATURE_SZ, Signer::D65(dsa_keypair_65(seed))),
        Alg::MlDsa87 => (MLDSA_87_SIGNATURE_SZ, Signer::D87(dsa_keypair_87(seed))),
        _ => {
            return Err(OpError::WrongKind(
                "an ML-KEM key does not sign; use an ml-dsa alg",
            ))
        }
    };
    let mut doc = vec![0u8; detached_signature_size(sig_sz)];
    let n = match &signed {
        Signer::D44(kp) => sign_detached_44(kp, data, &mut doc),
        Signer::D65(kp) => sign_detached_65(kp, data, &mut doc),
        Signer::D87(kp) => sign_detached_87(kp, data, &mut doc),
    }
    .map_err(|c| from_status(c, "the signature"))?;
    armor_text(SIGNATURE_LABEL, &doc[..n])
}

/// Check an armored SIGNATURE document over `data` against `key`.
pub fn verify(key: &PublicKey, data: &[u8], signature: &str) -> Result<(), OpError> {
    if !key.alg.signs() {
        return Err(OpError::WrongKind(
            "public_key is an ML-KEM key, which verifies nothing; pass an ML-DSA public key",
        ));
    }
    let doc = dearmor_text(signature, SIGNATURE_LABEL, "signature")?;
    match key.alg {
        Alg::MlDsa44 => verify_detached_44(&dsa_verifier_44(&key.bytes), data, &doc),
        Alg::MlDsa65 => verify_detached_65(&dsa_verifier_65(&key.bytes), data, &doc),
        Alg::MlDsa87 => verify_detached_87(&dsa_verifier_87(&key.bytes), data, &doc),
        _ => unreachable!("signs() checked above"),
    }
    .map_err(|c| from_status(c, "signature"))
}

/// Encrypt `data` to `key`'s holder, as an armored ENCRYPTED document.
/// `prk` is the encapsulation's randomness and must be fresh entropy.
pub fn encrypt(
    key: &PublicKey,
    prk: [u8; MLKEM_PRK_SEED_SZ],
    data: &[u8],
) -> Result<String, OpError> {
    let cipher_sz =
        match key.alg {
            Alg::MlKem512 => MLKEM_512_CIPHER_SZ,
            Alg::MlKem768 => MLKEM_768_CIPHER_SZ,
            Alg::MlKem1024 => MLKEM_1024_CIPHER_SZ,
            _ => return Err(OpError::WrongKind(
                "public_key is an ML-DSA key, which encrypts nothing; pass an ML-KEM public key",
            )),
        };
    key.check()?;
    let mut doc = vec![0u8; encrypted_document_size(cipher_sz, data.len())];
    let n = match key.alg {
        Alg::MlKem512 => {
            encrypt_to_recipient_512(&kem_encapsulator_512(&key.bytes), prk, data, &mut doc)
        }
        Alg::MlKem768 => {
            encrypt_to_recipient_768(&kem_encapsulator_768(&key.bytes), prk, data, &mut doc)
        }
        Alg::MlKem1024 => {
            encrypt_to_recipient_1024(&kem_encapsulator_1024(&key.bytes), prk, data, &mut doc)
        }
        _ => unreachable!("matched above"),
    }
    .map_err(|c| from_status(c, "the payload"))?;
    armor_text(ENCRYPTED_LABEL, &doc[..n])
}

/// Open an armored ENCRYPTED document addressed to the label's key.
pub fn decrypt(alg: Alg, seed: &[u8], document: &str) -> Result<Vec<u8>, OpError> {
    if alg.signs() {
        return Err(OpError::WrongKind(
            "an ML-DSA key decrypts nothing; use an ml-kem alg",
        ));
    }
    let doc = dearmor_text(document, ENCRYPTED_LABEL, "document")?;
    // The payload is never longer than the envelope that carries it.
    let mut out = Zeroizing::new(vec![0u8; doc.len()]);
    let n = match alg {
        Alg::MlKem512 => decrypt_as_recipient_512(&kem_keypair_512(seed), &doc, &mut out),
        Alg::MlKem768 => decrypt_as_recipient_768(&kem_keypair_768(seed), &doc, &mut out),
        Alg::MlKem1024 => decrypt_as_recipient_1024(&kem_keypair_1024(seed), &doc, &mut out),
        _ => unreachable!("signs() checked above"),
    }
    .map_err(|c| from_status(c, "document"))?;
    Ok(out[..n].to_vec())
}

pub fn hash(salt: &[u8], data: &[u8], domain: &str) -> [u8; 32] {
    aloecrypt_core::hash::hash(salt, data, domain)
}

// depth: armor, and building the core's types from bytes

fn armor_text(label: &str, payload: &[u8]) -> Result<String, OpError> {
    let mut out = vec![0u8; armored_size(label.len(), payload.len())];
    let n = armor(label, payload, &mut out).map_err(|c| from_status(c, "the armor"))?;
    out.truncate(n);
    String::from_utf8(out).map_err(|_| OpError::Malformed("armor produced non-UTF-8 text".into()))
}

fn dearmor_text(text: &str, label: &str, what: &str) -> Result<Vec<u8>, OpError> {
    let mut out = vec![0u8; dearmored_size_bound(text.len())];
    let n = dearmor(text.as_bytes(), label, &mut out).map_err(|_| {
        OpError::Malformed(format!(
            "{what} is not an armored ALOECRYPT {label} document"
        ))
    })?;
    out.truncate(n);
    Ok(out)
}

enum Signer {
    D44(MlDsa44Keypair),
    D65(MlDsa65Keypair),
    D87(MlDsa87Keypair),
}

/// `EncapsulationKey::new` is the check the core unwraps (FIPS 203's
/// modulus check); answering it here first is what keeps it from panicking.
macro_rules! kem_key_valid {
    ($params:ty, $n:expr, $bytes:expr) => {
        match <[u8; $n]>::try_from($bytes) {
            Ok(arr) => ml_kem::EncapsulationKey::<$params>::new(&arr.into()).is_ok(),
            Err(_) => false,
        }
    };
}
use kem_key_valid;

/// One constructor per parameter set, from bytes whose length the caller
/// already checked. A wrong length here is a bug in this file, not input.
macro_rules! each_dsa {
    ($($kp_fn:ident, $v_fn:ident, $kp:ty, $v:ty, $pk:ty, $pk_trait:ident;)*) => {$(
        fn $kp_fn(seed: &[u8]) -> $kp {
            let seed: &MlDsaPrivateSeed = seed.try_into().expect("an ML-DSA seed is 32 bytes");
            <$kp as MlDsaKeypair>::from_seed(seed)
        }
        fn $v_fn(bytes: &[u8]) -> $v {
            let pk: &$pk = bytes.try_into().expect("public key length checked by decode");
            <$v as $pk_trait>::unpack_bytes(pk)
        }
    )*};
}

each_dsa! {
    dsa_keypair_44, dsa_verifier_44, MlDsa44Keypair, MlDsa44Verifier, MlDsa44Pubkey, IMlDsa44Pubkey;
    dsa_keypair_65, dsa_verifier_65, MlDsa65Keypair, MlDsa65Verifier, MlDsa65Pubkey, IMlDsa65Pubkey;
    dsa_keypair_87, dsa_verifier_87, MlDsa87Keypair, MlDsa87Verifier, MlDsa87Pubkey, IMlDsa87Pubkey;
}

macro_rules! each_kem {
    ($($kp_fn:ident, $e_fn:ident, $kp:ty, $e:ident, $pk:ty;)*) => {$(
        fn $kp_fn(seed: &[u8]) -> $kp {
            let seed: &MlKemPrivateSeed = seed.try_into().expect("an ML-KEM seed is 64 bytes");
            <$kp as MlKemKeypair>::from_seed(seed)
        }
        /// Only after [`PublicKey::check`]: the core unwraps this key.
        fn $e_fn(bytes: &[u8]) -> $e {
            let pk: $pk = bytes.try_into().expect("public key length checked by decode");
            $e { public_key: pk }
        }
    )*};
}

each_kem! {
    kem_keypair_512, kem_encapsulator_512, MlKem512Keypair, MlKem512Encapsulator, MlKem512Pubkey;
    kem_keypair_768, kem_encapsulator_768, MlKem768Keypair, MlKem768Encapsulator, MlKem768Pubkey;
    kem_keypair_1024, kem_encapsulator_1024, MlKem1024Keypair, MlKem1024Encapsulator, MlKem1024Pubkey;
}
