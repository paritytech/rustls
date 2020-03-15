#![deny(unused)]
#![forbid(unused_must_use)]

use webpki;
use sct;
use std;
use std::sync::Arc;

use crate::key::Certificate;
use crate::msgs::handshake::DigitallySignedStruct;
use crate::msgs::handshake::SCTList;
use crate::msgs::enums::SignatureScheme;
use crate::error::TLSError;
use crate::anchors::{DistinguishedNames, RootCertStore};
use crate::anchors::OwnedTrustAnchor;
#[cfg(feature = "logging")]
use crate::log::{warn, debug};
mod x509;

type SignatureAlgorithms = &'static [&'static webpki::SignatureAlgorithm];

/// Which signature verification mechanisms we support.  No particular
/// order.
static SUPPORTED_SIG_ALGS: SignatureAlgorithms = &[
    &webpki::ECDSA_P256_SHA256,
    &webpki::ECDSA_P256_SHA384,
    &webpki::ECDSA_P384_SHA256,
    &webpki::ECDSA_P384_SHA384,
    &webpki::RSA_PSS_2048_8192_SHA256_LEGACY_KEY,
    &webpki::RSA_PSS_2048_8192_SHA384_LEGACY_KEY,
    &webpki::RSA_PSS_2048_8192_SHA512_LEGACY_KEY,
    &webpki::RSA_PKCS1_2048_8192_SHA256,
    &webpki::RSA_PKCS1_2048_8192_SHA384,
    &webpki::RSA_PKCS1_2048_8192_SHA512,
    &webpki::RSA_PKCS1_3072_8192_SHA384
];

/// Marker types.  These are used to bind the fact some verification
/// (certificate chain or handshake signature) has taken place into
/// protocol states.  We use this to have the compiler check that there
/// are no 'goto fail'-style elisions of important checks before we
/// reach the traffic stage.
///
/// These types are public, but cannot be directly constructed.  This
/// means their origins can be precisely determined by looking
/// for their `assertion` constructors.
pub struct HandshakeSignatureValid(());
impl HandshakeSignatureValid { pub fn assertion() -> Self { Self { 0: () } } }

pub struct FinishedMessageVerified(());
impl FinishedMessageVerified { pub fn assertion() -> Self { Self { 0: () } } }

/// Zero-sized marker type representing verification of a server cert chain.
pub struct ServerCertVerified(());
impl ServerCertVerified {
    /// Make a `ServerCertVerified`
    pub fn assertion() -> Self { Self { 0: () } }
}

/// Zero-sized marker type representing verification of a client cert chain.
pub struct ClientCertVerified(());
impl ClientCertVerified {
    /// Make a `ClientCertVerified`
    pub fn assertion() -> Self { Self { 0: () } }
}

/// Something that can verify a server certificate chain
pub trait ServerCertVerifier : Send + Sync {
    /// Verify a the certificate chain `presented_certs` against the roots
    /// configured in `roots`.  Make sure that `dns_name` is quoted by
    /// the top certificate in the chain.
    fn verify_server_cert(&self,
                          roots: &RootCertStore,
                          presented_certs: &[Certificate],
                          dns_name: webpki::DNSNameRef,
                          ocsp_response: &[u8]) -> Result<ServerCertVerified, TLSError>;
}

/// Something that can verify a client certificate chain
pub trait ClientCertVerifier : Send + Sync {
    /// Returns `true` to enable the server to request a client certificate and
    /// `false` to skip requesting a client certificate. Defaults to `true`.
    fn offer_client_auth(&self) -> bool { true }

    /// Return `Some(true)` to require a client certificate and `Some(false)` to make
    /// client authentication optional. Return `None` to abort the connection.
    /// Defaults to `Some(self.offer_client_auth())`.
    ///
    /// `sni` is the server name quoted by the client in its ClientHello; it has
    /// been validated as a proper DNS name but is otherwise untrusted.
    fn client_auth_mandatory(&self, _sni: Option<&webpki::DNSName>) -> Option<bool> {
        Some(self.offer_client_auth())
    }

    /// Returns the subject names of the client authentication trust anchors to
    /// share with the client when requesting client authentication.
    ///
    /// Return `None` to abort the connection.
    ///
    /// `sni` is the server name quoted by the client in its ClientHello; it has
    /// been validated as a proper DNS name but is otherwise untrusted.
    fn client_auth_root_subjects(&self, sni: Option<&webpki::DNSName>) -> Option<DistinguishedNames>;

    /// Verify a certificate chain. `presented_certs` is the certificate chain from the client.
    ///
    /// `sni` is the server name quoted by the client in its ClientHello; it has
    /// been validated as a proper DNS name but is otherwise untrusted.
    fn verify_client_cert(&self,
                          presented_certs: &[Certificate],
                          sni: Option<&webpki::DNSName>) -> Result<ClientCertVerified, TLSError>;
}

/// Default `ServerCertVerifier`, see the trait impl for more information.
pub struct WebPKIVerifier {
    /// time provider
    pub time: fn() -> Result<webpki::Time, TLSError>,
}

impl ServerCertVerifier for WebPKIVerifier {
    /// Will verify the certificate is valid in the following ways:
    /// - Signed by a  trusted `RootCertStore` CA
    /// - Not Expired
    /// - Valid for DNS entry
    /// - OCSP data is present
    fn verify_server_cert(&self,
                          roots: &RootCertStore,
                          presented_certs: &[Certificate],
                          dns_name: webpki::DNSNameRef,
                          ocsp_response: &[u8]) -> Result<ServerCertVerified, TLSError> {
        let (cert, chain, trustroots) = prepare(roots, presented_certs)?;
        let now = (self.time)()?;
        let cert = cert.verify_is_valid_tls_server_cert(SUPPORTED_SIG_ALGS,
                &webpki::TLSServerTrustAnchors(&trustroots), &chain, now)
            .map_err(TLSError::WebPKIError)
            .map(|_| cert)?;

        if !ocsp_response.is_empty() {
            debug!("Unvalidated OCSP response: {:?}", ocsp_response.to_vec());
        }

        cert.verify_is_valid_for_dns_name(dns_name)
            .map_err(TLSError::WebPKIError)
            .map(|_| ServerCertVerified::assertion())
    }
}

impl WebPKIVerifier {
    /// Create a new `WebPKIVerifier`
    pub fn new() -> WebPKIVerifier {
        WebPKIVerifier {
            time: try_now,
        }
    }
}

type CertChainAndRoots<'a, 'b> = (webpki::EndEntityCert<'a>,
                                  Vec<&'a [u8]>,
                                  Vec<webpki::TrustAnchor<'b>>);

fn prepare<'a, 'b>(roots: &'b RootCertStore, presented_certs: &'a [Certificate])
                   -> Result<CertChainAndRoots<'a, 'b>, TLSError> {
    if presented_certs.is_empty() {
        return Err(TLSError::NoCertificatesPresented);
    }

    // EE cert must appear first.
    let cert = webpki::EndEntityCert::from(&presented_certs[0].0)
        .map_err(TLSError::WebPKIError)?;

    let chain: Vec<&'a [u8]> = presented_certs.iter()
        .skip(1)
        .map(|cert| cert.0.as_ref())
        .collect();

    let trustroots: Vec<webpki::TrustAnchor> = roots.roots
        .iter()
        .map(OwnedTrustAnchor::to_trust_anchor)
        .collect();

    Ok((cert, chain, trustroots))
}

fn try_now() -> Result<webpki::Time, TLSError> {
    webpki::Time::try_from(std::time::SystemTime::now())
        .map_err( |_ | TLSError::FailedToGetCurrentTime)
}

/// A `ClientCertVerifier` that will ensure that every client provides a trusted
/// certificate, without any name checking.
pub struct AllowAnyAuthenticatedClient {
    roots: RootCertStore,
}

impl AllowAnyAuthenticatedClient {
    /// Construct a new `AllowAnyAuthenticatedClient`.
    ///
    /// `roots` is the list of trust anchors to use for certificate validation.
    pub fn new(roots: RootCertStore) -> Arc<dyn ClientCertVerifier> {
        Arc::new(AllowAnyAuthenticatedClient { roots })
    }
}

impl ClientCertVerifier for AllowAnyAuthenticatedClient {
    fn offer_client_auth(&self) -> bool { true }

    fn client_auth_mandatory(&self, _sni: Option<&webpki::DNSName>) -> Option<bool> { Some(true) }

    fn client_auth_root_subjects(&self, _sni: Option<&webpki::DNSName>) -> Option<DistinguishedNames> {
        Some(self.roots.get_subjects())
    }

    fn verify_client_cert(&self, presented_certs: &[Certificate], _sni: Option<&webpki::DNSName>)
                          -> Result<ClientCertVerified, TLSError> {
        let (cert, chain, trustroots) = prepare(&self.roots, presented_certs)?;
        let now = try_now()?;
        cert.verify_is_valid_tls_client_cert(
                SUPPORTED_SIG_ALGS, &webpki::TLSClientTrustAnchors(&trustroots),
                &chain, now)
            .map_err(TLSError::WebPKIError)
            .map(|_| ClientCertVerified::assertion())
    }
}

/// A `ClientCertVerifier` that will allow both anonymous and authenticated
/// clients, without any name checking.
///
/// Client authentication will be requested during the TLS handshake. If the
/// client offers a certificate then this acts like
/// `AllowAnyAuthenticatedClient`, otherwise this acts like `NoClientAuth`.
pub struct AllowAnyAnonymousOrAuthenticatedClient {
    inner: AllowAnyAuthenticatedClient,
}

impl AllowAnyAnonymousOrAuthenticatedClient {
    /// Construct a new `AllowAnyAnonymousOrAuthenticatedClient`.
    ///
    /// `roots` is the list of trust anchors to use for certificate validation.
    pub fn new(roots: RootCertStore) -> Arc<dyn ClientCertVerifier> {
        Arc::new(AllowAnyAnonymousOrAuthenticatedClient {
            inner: AllowAnyAuthenticatedClient { roots }
        })
    }
}

impl ClientCertVerifier for AllowAnyAnonymousOrAuthenticatedClient {
    fn offer_client_auth(&self) -> bool { self.inner.offer_client_auth() }

    fn client_auth_mandatory(&self, _sni: Option<&webpki::DNSName>) -> Option<bool> { Some(false) }

    fn client_auth_root_subjects(&self, sni: Option<&webpki::DNSName>) -> Option<DistinguishedNames> {
        self.inner.client_auth_root_subjects(sni)
    }

    fn verify_client_cert(&self, presented_certs: &[Certificate], sni: Option<&webpki::DNSName>)
            -> Result<ClientCertVerified, TLSError> {
        self.inner.verify_client_cert(presented_certs, sni)
    }
}

/// Turns off client authentication.
pub struct NoClientAuth;

impl NoClientAuth {
    /// Constructs a `NoClientAuth` and wraps it in an `Arc`.
    pub fn new() -> Arc<dyn ClientCertVerifier> { Arc::new(NoClientAuth) }
}

impl ClientCertVerifier for NoClientAuth {
    fn offer_client_auth(&self) -> bool { false }

    fn client_auth_root_subjects(&self, _sni: Option<&webpki::DNSName>) -> Option<DistinguishedNames> {
        unimplemented!();
    }

    fn verify_client_cert(&self,_presented_certs: &[Certificate], _sni: Option<&webpki::DNSName>)
                          -> Result<ClientCertVerified, TLSError> {
        unimplemented!();
    }
}

fn convert_scheme(scheme: SignatureScheme) -> Result<(), TLSError> {
    match scheme {
        // nb. for TLS1.2 the curve is not fixed by SignatureScheme.
        SignatureScheme::ECDSA_NISTP256_SHA256 |
        SignatureScheme::ECDSA_NISTP384_SHA384 |

        SignatureScheme::RSA_PKCS1_SHA256 |
        SignatureScheme::RSA_PKCS1_SHA384 |
        SignatureScheme::RSA_PKCS1_SHA512 |

        SignatureScheme::RSA_PSS_SHA256 |
        SignatureScheme::RSA_PSS_SHA384 |
        SignatureScheme::RSA_PSS_SHA512 => Ok(()),

        _ => {
            let error_msg = format!("received unadvertised sig scheme {:?}", scheme);
            Err(TLSError::PeerMisbehavedError(error_msg))
        }
    }
}

/// Verify the signed `message` using the public key quoted in
/// `cert` and algorithm and signature in `dss`.
///
/// `cert` MUST have been authenticated before using this function,
/// typically using `verify_cert`.
pub fn verify_signed_struct(message: &[u8],
                            cert: &Certificate,
                            dss: &DigitallySignedStruct)
                            -> Result<HandshakeSignatureValid, TLSError> {
    convert_scheme(dss.scheme)?;
    x509::verify_certificate_signature(&dss.sig.0, message, &cert.0, dss.scheme, false)
        .map_err(TLSError::WebPKIError)
        .map(|_| HandshakeSignatureValid::assertion())
}

fn convert_alg_tls13(scheme: SignatureScheme) -> Result<(), TLSError> {
    use crate::msgs::enums::SignatureScheme::*;

    match scheme {
        ECDSA_NISTP256_SHA256 |
        ECDSA_NISTP384_SHA384 |
        RSA_PSS_SHA256 |
        RSA_PSS_SHA384 |
        RSA_PSS_SHA512 => Ok(()),
        _ => {
            let error_msg = format!("received unsupported sig scheme {:?}", scheme);
            Err(TLSError::PeerMisbehavedError(error_msg))
        }
    }
}

pub fn verify_tls13(cert: &Certificate,
                    dss: &DigitallySignedStruct,
                    handshake_hash: &[u8],
                    context_string_with_0: &[u8])
                    -> Result<HandshakeSignatureValid, TLSError> {
    convert_alg_tls13(dss.scheme)?;

    let mut msg = Vec::new();
    msg.resize(64, 0x20u8);
    msg.extend_from_slice(context_string_with_0);
    msg.extend_from_slice(handshake_hash);

    x509::verify_certificate_signature(&dss.sig.0, &msg, &cert.0, dss.scheme, true)
        .map_err(TLSError::WebPKIError)
        .map(|_| HandshakeSignatureValid::assertion())
}

fn unix_time_millis() -> Result<u64, TLSError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|dur| dur.as_secs())
        .map_err(|_| TLSError::FailedToGetCurrentTime)
        .and_then(|secs| secs.checked_mul(1000)
                  .ok_or(TLSError::FailedToGetCurrentTime))
}

pub fn verify_scts(cert: &Certificate,
                   scts: &SCTList,
                   logs: &[&sct::Log]) -> Result<(), TLSError> {
    let mut valid_scts = 0;
    let now = unix_time_millis()?;
    let mut last_sct_error = None;

    for sct in scts {
        #[cfg_attr(not(feature = "logging"), allow(unused_variables))]
        match sct::verify_sct(&cert.0, &sct.0, now, logs) {
            Ok(index) => {
                debug!("Valid SCT signed by {} on {}",
                      logs[index].operated_by, logs[index].description);
                valid_scts += 1;
            }
            Err(e) => {
                if e.should_be_fatal() {
                    return Err(TLSError::InvalidSCT(e));
                }
                debug!("SCT ignored because {:?}", e);
                last_sct_error = Some(e);
            }
        }
    }

    /* If we were supplied with some logs, and some SCTs,
     * but couldn't verify any of them, fail the handshake. */
    if !logs.is_empty() && !scts.is_empty() && valid_scts == 0 {
        warn!("No valid SCTs provided");
        return Err(TLSError::InvalidSCT(last_sct_error.unwrap()));
    }

    Ok(())
}

pub fn supported_verify_schemes() -> &'static [SignatureScheme] {
    &[
        SignatureScheme::ECDSA_NISTP384_SHA384,
        SignatureScheme::ECDSA_NISTP256_SHA256,

        SignatureScheme::RSA_PSS_SHA512,
        SignatureScheme::RSA_PSS_SHA384,
        SignatureScheme::RSA_PSS_SHA256,

        SignatureScheme::RSA_PKCS1_SHA512,
        SignatureScheme::RSA_PKCS1_SHA384,
        SignatureScheme::RSA_PKCS1_SHA256,
    ]
}
