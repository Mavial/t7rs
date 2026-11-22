//! TLS 1.3 for the control connection.
//!
//! A server has a self-signed certificate. A client pins the certificate's
//! SHA-256 fingerprint the first time it connects to an address (trust on
//! first use) and refuses a different certificate after that, so the
//! handshake signatures are what prove the server is the one seen before.

use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, ring, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, ConnectionCommon, DigitallySignedStruct, ServerConfig, SignatureScheme};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
pub use tokio_rustls::TlsAcceptor;
pub use tokio_rustls::client::TlsStream as ClientStream;
pub use tokio_rustls::server::TlsStream as ServerStream;
use tokio_rustls::TlsConnector;

/// The first byte of every TLS handshake record, so of every ClientHello.
pub const HANDSHAKE_RECORD: u8 = 0x16;

/// Label for the connection's [`exporter`] value.
pub const EXPORTER_LABEL: &[u8] = b"t7 identity v1";

/// SHA-256 of a certificate (DER), written as 32 colon-separated hex bytes
/// like `openssl x509 -fingerprint -sha256` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn of(cert_der: &[u8]) -> Self {
        Self(Sha256::digest(cert_der).into())
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(":")?;
            }
            write!(f, "{byte:02X}")?;
        }
        Ok(())
    }
}

impl FromStr for Fingerprint {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const INVALID: &str = "a fingerprint is 32 hex bytes separated by colons";
        let mut bytes = [0u8; 32];
        let mut parts = s.trim().split(':');
        for byte in &mut bytes {
            let part = parts.next().filter(|p| p.len() == 2).ok_or(INVALID)?;
            *byte = u8::from_str_radix(part, 16).map_err(|_| INVALID)?;
        }
        if parts.next().is_some() {
            return Err(INVALID);
        }
        Ok(Self(bytes))
    }
}

impl Serialize for Fingerprint {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

/// Accepts TLS 1.3 with the server's certificate and its PKCS#8 private key, both DER.
pub fn acceptor(cert: Vec<u8>, key: Vec<u8>) -> Result<TlsAcceptor, rustls::Error> {
    let config = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert.into()], PrivatePkcs8KeyDer::from(key).into())?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

#[derive(Debug)]
pub enum ConnectError {
    /// The server's certificate is not the one pinned for this address.
    CertChanged { pinned: Fingerprint, presented: Fingerprint },
    Io(std::io::Error),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::CertChanged { .. } => f.write_str("the server's certificate has changed"),
            ConnectError::Io(e) => write!(f, "TLS handshake failed: {e}"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// Runs the client side of the handshake. `pinned` is the fingerprint from an
/// earlier visit; with `None` any certificate is accepted (first use).
/// Returns the stream and the server's fingerprint, to pin.
pub async fn connect<S>(stream: S, pinned: Option<Fingerprint>) -> Result<(ClientStream<S>, Fingerprint), ConnectError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let provider = provider();
    let verifier = Arc::new(PinnedCert {
        pinned,
        presented: OnceLock::new(),
        algorithms: provider.signature_verification_algorithms,
    });
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("ring supports TLS 1.3")
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    // Certificates are pinned, not issued for a name, so the name doesn't matter.
    let name = ServerName::try_from("t7").expect("valid DNS name");
    let result = TlsConnector::from(Arc::new(config)).connect(name, stream).await;
    let presented = verifier.presented.get().copied();
    match (result, pinned, presented) {
        (Ok(stream), _, Some(presented)) => Ok((stream, presented)),
        (Err(_), Some(pinned), Some(presented)) if pinned != presented => {
            Err(ConnectError::CertChanged { pinned, presented })
        }
        (Err(e), _, _) => Err(ConnectError::Io(e)),
        (Ok(_), _, None) => unreachable!("a full handshake always verifies the certificate"),
    }
}

/// 32 bytes that both ends of one TLS connection derive, and nobody else can:
/// what a signature proving an identity on this connection binds to.
pub fn exporter<D>(conn: &ConnectionCommon<D>) -> [u8; 32] {
    conn.export_keying_material([0u8; 32], EXPORTER_LABEL, None)
        .expect("the handshake is complete")
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

/// Checks the certificate against the pin and the handshake signatures
/// against the certificate's key.
#[derive(Debug)]
struct PinnedCert {
    pinned: Option<Fingerprint>,
    presented: OnceLock<Fingerprint>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let presented = Fingerprint::of(end_entity);
        let _ = self.presented.set(presented);
        match self.pinned {
            Some(pinned) if pinned != presented => {
                Err(rustls::Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure))
            }
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::server::{ClientHello, ResolvesServerCert};
    use rustls::sign::CertifiedKey;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn new_cert() -> (Vec<u8>, Vec<u8>) {
        let pair = rcgen::generate_simple_self_signed(["t7".to_owned()]).unwrap();
        (pair.cert.der().to_vec(), pair.signing_key.serialize_der())
    }

    /// Handshakes over an in-memory pipe; returns both exporters and the fingerprint seen.
    async fn handshake(
        acceptor: TlsAcceptor,
        pinned: Option<Fingerprint>,
    ) -> Result<([u8; 32], [u8; 32], Fingerprint), ConnectError> {
        let (client_io, server_io) = tokio::io::duplex(16 * 1024);
        let server = tokio::spawn(async move {
            let mut tls = acceptor.accept(server_io).await.ok()?;
            let mut byte = [0u8; 1];
            tls.read_exact(&mut byte).await.ok()?;
            Some(exporter(tls.get_ref().1))
        });
        let (mut tls, seen) = connect(client_io, pinned).await?;
        tls.write_all(b"x").await.unwrap();
        tls.flush().await.unwrap();
        let client_exporter = exporter(tls.get_ref().1);
        Ok((client_exporter, server.await.unwrap().expect("server side"), seen))
    }

    fn server(cert: &(Vec<u8>, Vec<u8>)) -> TlsAcceptor {
        acceptor(cert.0.clone(), cert.1.clone()).unwrap()
    }

    #[tokio::test]
    async fn pins_on_first_use_and_refuses_a_changed_certificate() {
        let cert = new_cert();
        let (client, server_side, seen) = handshake(server(&cert), None).await.unwrap();
        assert_eq!(seen, Fingerprint::of(&cert.0));
        assert_eq!(client, server_side, "both ends derive the same exporter");

        let (again, _, _) = handshake(server(&cert), Some(seen)).await.unwrap();
        assert_ne!(again, client, "each connection has its own exporter");

        let other = new_cert();
        match handshake(server(&other), Some(seen)).await {
            Err(ConnectError::CertChanged { pinned, presented }) => {
                assert_eq!(pinned, seen);
                assert_eq!(presented, Fingerprint::of(&other.0));
            }
            res => panic!("expected CertChanged, got {:?}", res.map(|r| r.2)),
        }
    }

    #[derive(Debug)]
    struct Always(Arc<CertifiedKey>);

    impl ResolvesServerCert for Always {
        fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
            Some(self.0.clone())
        }
    }

    #[tokio::test]
    async fn the_pinned_certificate_without_its_key_is_refused() {
        // An impostor replays the real server's certificate but has to sign
        // the handshake with a key of its own.
        let real = new_cert();
        let impostor_key = provider().key_provider.load_private_key(PrivatePkcs8KeyDer::from(new_cert().1).into()).unwrap();
        let replayed = CertifiedKey::new(vec![real.0.clone().into()], impostor_key);
        let config = ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Always(Arc::new(replayed))));
        let res = handshake(TlsAcceptor::from(Arc::new(config)), Some(Fingerprint::of(&real.0))).await;
        assert!(matches!(res, Err(ConnectError::Io(_))), "{:?}", res.map(|r| r.2));
    }

    #[test]
    fn fingerprint_text_roundtrip() {
        let fp = Fingerprint::of(b"cert");
        let text = fp.to_string();
        assert_eq!(text.len(), 32 * 3 - 1);
        assert_eq!(text.parse::<Fingerprint>(), Ok(fp));
        assert_eq!(text.to_lowercase().parse::<Fingerprint>(), Ok(fp));
        assert!(text[3..].parse::<Fingerprint>().is_err());
        assert!(format!("{text}:00").parse::<Fingerprint>().is_err());
    }
}
