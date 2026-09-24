//! The host's ICE credentials and DTLS certificate: made once, kept, and
//! read on every start, so the record a room holds survives a restart
//! (`doc/BrowserAccess.md` §2.2).
//!
//! ## surface block
//!
//! - Entry points: [`Identity::load_or_create`], [`Identity::generate`],
//!   [`Identity::fingerprint`].
//! - Configurable: nothing. The file's shape is [`FileV1`].
//! - Fan-out: none.

use std::path::Path;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use str0m::config::DtlsCert;
use str0m::IceCreds;

/// What the host is to every browser: one ufrag, one password, one
/// certificate.
#[derive(Clone)]
pub struct Identity {
    pub ufrag: String,
    pub pwd: String,
    pub cert: DtlsCert,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The password and the private key are secrets; the ufrag and the
        // fingerprint are published anyway.
        f.debug_struct("Identity")
            .field("ufrag", &self.ufrag)
            .field(
                "fingerprint",
                &crate::record::fingerprint_hex(&self.fingerprint()),
            )
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileV1 {
    v: u32,
    ufrag: String,
    pwd: String,
    /// DER, standard base64.
    certificate: String,
    /// DER, standard base64.
    private_key: String,
}

impl Identity {
    /// A fresh identity: str0m's own credential generator (RFC 8445's
    /// entropy floors) and a self-signed certificate from its crypto
    /// provider.
    pub fn generate() -> Result<Identity, String> {
        let creds = IceCreds::new();
        let cert = str0m::crypto::from_feature_flags()
            .dtls_provider
            .generate_certificate()
            .ok_or("webrtc: the crypto provider could not make a DTLS certificate")?;
        Ok(Identity {
            ufrag: creds.ufrag,
            pwd: creds.pass,
            cert,
        })
    }

    /// Read `path`, or make an identity and write it there `0600` when there
    /// is nothing to read. A file that exists and does not parse is refused
    /// by name rather than replaced: replacing it would change the record
    /// under every room that holds it.
    pub fn load_or_create(path: &Path) -> Result<Identity, String> {
        match std::fs::read(path) {
            Ok(bytes) => Self::from_file(path, &bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let id = Self::generate()?;
                id.write(path)?;
                Ok(id)
            }
            Err(e) => Err(format!(
                "webrtc: cannot read identity_file '{}': {e}",
                path.display()
            )),
        }
    }

    fn from_file(path: &Path, bytes: &[u8]) -> Result<Identity, String> {
        let bad = |why: String| format!("webrtc: identity_file '{}' {why}", path.display());
        let f: FileV1 =
            serde_json::from_slice(bytes).map_err(|e| bad(format!("is not an identity: {e}")))?;
        if f.v != 1 {
            return Err(bad(format!("is version {}, and this build reads 1", f.v)));
        }
        let certificate = STANDARD
            .decode(&f.certificate)
            .map_err(|e| bad(format!("has a certificate that is not base64: {e}")))?;
        let private_key = STANDARD
            .decode(&f.private_key)
            .map_err(|e| bad(format!("has a private key that is not base64: {e}")))?;
        Ok(Identity {
            ufrag: f.ufrag,
            pwd: f.pwd,
            cert: DtlsCert {
                certificate,
                private_key,
            },
        })
    }

    fn write(&self, path: &Path) -> Result<(), String> {
        let f = FileV1 {
            v: 1,
            ufrag: self.ufrag.clone(),
            pwd: self.pwd.clone(),
            certificate: STANDARD.encode(&self.cert.certificate),
            private_key: STANDARD.encode(&self.cert.private_key),
        };
        let json = serde_json::to_vec_pretty(&f).expect("an identity serializes");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        options
            .open(path)
            .and_then(|mut file| file.write_all(&json))
            .map_err(|e| {
                format!(
                    "webrtc: cannot write identity_file '{}': {e}",
                    path.display()
                )
            })
    }

    /// SHA-256 of the certificate's DER: what the record's `f` carries and
    /// what a browser's DTLS checks.
    pub fn fingerprint(&self) -> [u8; 32] {
        Sha256::digest(&self.cert.certificate).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("drt-rtc-identity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("id.json");
        let first = Identity::load_or_create(&path).unwrap();
        let again = Identity::load_or_create(&path).unwrap();
        assert_eq!(first.ufrag, again.ufrag);
        assert_eq!(first.pwd, again.pwd);
        assert_eq!(first.fingerprint(), again.fingerprint());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(&path, b"{}").unwrap();
        assert!(Identity::load_or_create(&path)
            .unwrap_err()
            .contains("is not an identity"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generated_credentials_fit_a_record() {
        let id = Identity::generate().unwrap();
        let r = crate::record::Record {
            ufrag: id.ufrag.clone(),
            pwd: id.pwd.clone(),
            fingerprint: id.fingerprint(),
            candidates: vec![],
        };
        r.encode()
            .expect("str0m's credentials are ice-chars within the record's bounds");
    }
}
