//! Who we are to the phone: a service ID and a self-signed TLS certificate.
//!
//! In "Everyone" mode iOS does not validate the certificate, it only needs
//! TLS. Both are created on first use and kept in ~/.local/share/lindrop so
//! the phone sees the same receiver from one session to the next.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

pub struct Ident {
    /// 12 hex digits, the instance name of our `_airdrop._tcp` service.
    pub service_id: String,
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
}

impl Ident {
    pub fn load_or_create(dir: &Path, name: &str) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("creo {}", dir.display()))?;
        let id_path = dir.join("id");
        let cert_path = dir.join("certificato.der");
        let key_path = dir.join("chiave.der");

        let service_id = match fs::read_to_string(&id_path) {
            Ok(s) if s.trim().len() == 12 => s.trim().to_string(),
            _ => {
                let id = hex(&random::<6>());
                fs::write(&id_path, &id)?;
                id
            }
        };

        if let (Ok(cert), Ok(key)) = (fs::read(&cert_path), fs::read(&key_path)) {
            return Ok(Self { service_id, cert: cert.into(), key: key.into() });
        }

        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_RSA_SHA256).context("genero la chiave RSA")?;
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
        params.distinguished_name.push(rcgen::DnType::CommonName, name);
        let cert = params.self_signed(&key)?;
        write_private(&key_path, &key.serialize_der())?;
        fs::write(&cert_path, cert.der())?;
        Ok(Self { service_id, cert: cert.der().clone(), key: key.serialize_der().into() })
    }

    pub fn server_config(&self) -> Result<Arc<rustls::ServerConfig>> {
        let cfg = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![self.cert.clone()], PrivateKeyDer::Pkcs8(self.key.clone_key()))?;
        Ok(Arc::new(cfg))
    }

    pub fn client_config(&self) -> Result<Arc<rustls::ClientConfig>> {
        let cfg = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAny::new()))
            .with_client_auth_cert(vec![self.cert.clone()], PrivateKeyDer::Pkcs8(self.key.clone_key()))?;
        Ok(Arc::new(cfg))
    }
}

/// The phone's certificate is self-signed too (Everyone mode): accept it, but
/// still check the handshake signatures so the session itself is sound.
#[derive(Debug)]
struct AcceptAny(Arc<rustls::crypto::CryptoProvider>);

impl AcceptAny {
    fn new() -> Self {
        Self(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
    }
}

impl rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        msg: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(msg, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        msg: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(msg, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    std::io::Write::write_all(&mut f, data)?;
    Ok(())
}

pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).expect("/dev/urandom");
    b
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
