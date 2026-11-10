//! Construction of HTTP clients with certificate verification that works on every platform.

use reqwest::{Certificate, ClientBuilder};

/// Returns a [`ClientBuilder`] that trusts the usual public roots plus `extra_roots`.
///
/// reqwest normally defers to the operating system's certificate verifier. On Android that
/// verifier needs a Java component and JNI setup this crate cannot provide, and it cannot
/// be combined with extra roots at all, so there the Mozilla root set is compiled in and
/// used instead.
pub fn client_builder(extra_roots: Vec<Certificate>) -> ClientBuilder {
    #[cfg(target_os = "android")]
    {
        let mut roots = extra_roots;
        roots.extend(
            webpki_root_certs::TLS_SERVER_ROOT_CERTS
                .iter()
                .filter_map(|cert| Certificate::from_der(cert.as_ref()).ok()),
        );
        ClientBuilder::new().tls_certs_only(roots)
    }

    #[cfg(not(target_os = "android"))]
    {
        ClientBuilder::new().tls_certs_merge(extra_roots)
    }
}
