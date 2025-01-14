use tokio_rustls::rustls::{pki_types::CertificateDer, pki_types::PrivateKeyDer, ServerConfig};
use rustls_pemfile::{certs, pkcs8_private_keys, rsa_private_keys};
use std::fs::File;
use std::io::{BufReader, Read};

// Configure TLS using paths from config
pub fn configure_tls(cert_path: &str, key_path: &str) -> Result<ServerConfig, Box<dyn std::error::Error>> {
    // Load and parse the certificate
    let cert_file = File::open(cert_path)?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs = certs(&mut cert_reader)?
        .into_iter()
        .map(|c| CertificateDer::from(c))
        .collect::<Vec<CertificateDer>>();

    if certs.is_empty() {
        return Err("No valid certificates found in the PEM file".into());
    }

    // Load and parse the private key
    let key_file = File::open(key_path)?;
    let mut key_reader = BufReader::new(key_file);
    let keys = pkcs8_private_keys(&mut key_reader)?;

    let private_key = if !keys.is_empty() {
        PrivateKeyDer::Pkcs8(keys[0].clone().into())
    } else {
        // Fallback to RSA private keys if no PKCS#8 keys are found
        let rsa_keys = rsa_private_keys(&mut key_reader)?;
        if rsa_keys.is_empty() {
            return Err("No valid private keys found in the PEM file".into());
        }
        PrivateKeyDer::Pkcs1(rsa_keys[0].clone().into())
    };

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, private_key)?;

    Ok(config)
}
