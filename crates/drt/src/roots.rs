//! PEM trust anchors named on the command line, for the verbs that dial TLS.
//!
//! One loader and one store builder, because there is one rule and it
//! should be stated in one place: a certificate named here is trusted **in
//! addition to** the public roots and never instead of them. A client that
//! could narrow its trust to a single certificate is a footgun, and the
//! case that exists in the field is an internal or intercepting CA that
//! must be trusted beside the public ones.
//!
//! `connectors/rest` states the same rule for the `rest` scope's
//! `extra_roots` and deliberately does not share this code: it is a
//! separate crate, and its refusals name a config key where these name a
//! flag. The rule is duplicated; the wording is not, and neither is the
//! reasoning — `connectors/rest/src/lib.rs` carries both.
//!
//! ## surface block
//!
//! - Entry points: [`load_roots`], PEM files to certificates;
//!   [`load_roots_named`], the same with the refusals naming a config key
//!   rather than the flag; and [`store`], certificates to a
//!   `RootCertStore` with webpki's beside them.
//! - Configurable: nothing here. The flag is `--extra-root` on every verb
//!   that takes one, and its name appears in every refusal below unless
//!   the caller names the key it read instead.

use tokio_rustls::rustls::pki_types::CertificateDer;
use tokio_rustls::rustls::RootCertStore;

/// The PEM files `--extra-root` names, read and parsed **before anything is
/// dialed**, so a wrong path or a key file handed over by mistake is a
/// refusal by name rather than a TLS error on the first connection.
pub fn load_roots(paths: &[std::path::PathBuf]) -> Result<Vec<CertificateDer<'static>>, String> {
    load_roots_named("--extra-root", paths)
}

/// [`load_roots`] with the refusals naming `key` instead of the flag: the
/// `tunnel` block's `extra_roots` is the same list read from a file, and a
/// refusal should name the line the operator wrote.
pub fn load_roots_named(
    key: &str,
    paths: &[std::path::PathBuf],
) -> Result<Vec<CertificateDer<'static>>, String> {
    use tokio_rustls::rustls::pki_types::pem::PemObject;
    let mut out = Vec::new();
    for path in paths {
        let name = path.display();
        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(path)
            .map_err(|e| format!("{key} '{name}': {e}"))?
            .collect::<Result<_, _>>()
            .map_err(|e| format!("{key} '{name}': {e}"))?;
        if certs.is_empty() {
            return Err(format!("{key} '{name}': no certificate in it"));
        }
        // webpki has to be able to use it, or it is trusted for nothing --
        // the check `connectors/rest`'s loader has always made. Parsing as
        // a certificate and working as a trust anchor are different
        // questions, and asking the second one here is what keeps the
        // promise in this function's first sentence: the alternative is a
        // file accepted at load and refused at dial, which is exactly the
        // late, obscure failure the flag exists to prevent.
        let mut probe = RootCertStore::empty();
        let (_, ignored) = probe.add_parsable_certificates(certs.iter().cloned());
        if ignored > 0 {
            return Err(format!(
                "{key} '{name}': {ignored} certificate(s) are not usable as trust anchors"
            ));
        }
        out.extend(certs);
    }
    Ok(out)
}

/// webpki's public roots, plus `extra`. Never `extra` alone.
///
/// The one function that decides what a DRT client trusts, so that
/// "added, never substituted" is a property of the code and not of every
/// caller remembering it.
pub fn store(extra: &[CertificateDer<'static>]) -> RootCertStore {
    let mut store = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    // Unusable certificates were refused by `load_roots`, so anything
    // dropped here was not named on the command line -- ignoring the count
    // is safe only because of that, which is why the two live together.
    let _ = store.add_parsable_certificates(extra.iter().cloned());
    store
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Added, never substituted — asserted on the store itself.
    ///
    /// This property cannot be seen from an integration test: a client that
    /// quietly narrowed its trust to the one named CA would still reach the
    /// edge signed by that CA, pass, and then refuse every ordinary host in
    /// the field. Proving it needs a count, and the count is here. Same
    /// shape and same reasoning as `connectors/rest`'s
    /// `extra_roots_are_parsed_at_startup_and_added_beside_webpki`.
    #[test]
    fn store_adds_beside_webpki_and_never_replaces_it() {
        let public = webpki_roots::TLS_SERVER_ROOTS.len();
        assert!(
            public > 0,
            "webpki ships roots, or nothing below means much"
        );

        // No extras is exactly the public set, unchanged.
        assert_eq!(store(&[]).roots.len(), public);

        let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert = CertificateDer::from(issued.cert.der().to_vec());
        let with = store(std::slice::from_ref(&cert));
        assert_eq!(
            with.roots.len(),
            public + 1,
            "one more than the public set: the named CA beside them, not instead"
        );
    }

    /// A file that is not a certificate is refused, by flag and by name,
    /// before anything is dialed.
    #[test]
    fn a_file_that_is_not_a_certificate_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("not-a-cert.pem");
        std::fs::write(&junk, b"this is not a certificate\n").unwrap();
        let e = load_roots(&[junk]).unwrap_err();
        assert!(e.contains("--extra-root"), "{e}");
        assert!(e.contains("not-a-cert.pem"), "{e}");
    }

    /// A real certificate loads, and an empty file is its own refusal —
    /// the case a `cat`-ed or truncated PEM produces.
    #[test]
    fn an_empty_pem_is_refused_rather_than_silently_trusting_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();

        let good = dir.path().join("ca.pem");
        std::fs::write(&good, issued.cert.pem()).unwrap();
        assert_eq!(load_roots(&[good]).unwrap().len(), 1);

        let empty = dir.path().join("empty.pem");
        std::fs::write(&empty, b"").unwrap();
        let e = load_roots(&[empty]).unwrap_err();
        assert!(e.contains("no certificate in it"), "{e}");
    }
}
