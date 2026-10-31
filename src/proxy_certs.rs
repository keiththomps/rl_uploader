use crate::{Error, Result, io_error, platform, protocol::HOST};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateRevocationListParams, CrlDistributionPoint,
    DnType, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    NameConstraints,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, io::Cursor, path::Path, sync::Arc, time::Duration};
use time::OffsetDateTime;
use zeroize::Zeroizing;

pub const HEALTH_PORT: u16 = 47639;
pub const SUBJECT: &str = "RL Uploader WebSocket Proxy";
pub fn trusted(dir: &Path) -> bool {
    let Ok(der) = fs::read(dir.join("proxy/root.cer")) else {
        return false;
    };
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Security::Cryptography::{
            CertCloseStore, CertEnumCertificatesInStore, CertFreeCertificateContext,
            CertOpenSystemStoreW,
        };
        let name: Vec<u16> = "ROOT\0".encode_utf16().collect();
        let store = CertOpenSystemStoreW(0, name.as_ptr());
        if store.is_null() {
            return false;
        }
        let mut current = std::ptr::null();
        let mut found = false;
        loop {
            current = CertEnumCertificatesInStore(store, current);
            if current.is_null() {
                break;
            }
            let cert = &*current;
            if std::slice::from_raw_parts(cert.pbCertEncoded, cert.cbCertEncoded as usize) == der {
                found = true;
                CertFreeCertificateContext(current);
                break;
            }
        }
        CertCloseStore(store, 0);
        found
    }
    #[cfg(not(windows))]
    {
        let _ = der;
        false
    }
}
pub fn manifest(dir: &Path) -> Result<Value> {
    let der = fs::read(dir.join("proxy/root.cer")).map_err(io_error)?;
    let (_, cert) = x509_parser::parse_x509_certificate(&der)
        .map_err(|_| Error::new("Invalid proxy certificate."))?;
    let expires = cert
        .validity()
        .not_after
        .to_datetime()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(io_error)?;
    Ok(
        json!({"host":HOST,"subject":SUBJECT,"sha1":hex::encode(sha1::Sha1::digest(&der)).to_uppercase(),"sha256":hex::encode(Sha256::digest(&der)),"expires":expires}),
    )
}
pub fn prepare(dir: &Path) -> Result<Value> {
    let root = dir.join("proxy");
    fs::create_dir_all(&root).map_err(io_error)?;
    if root.join("root.cer").exists() {
        let der = fs::read(root.join("root.cer")).map_err(io_error)?;
        let (_, cert) = x509_parser::parse_x509_certificate(&der)
            .map_err(|_| Error::new("Invalid proxy certificate."))?;
        if cert.validity().not_after.timestamp() > crate::now() as i64 + 2 * 86400 {
            // Load and validate the matching leaf key before claiming setup is ready.
            server_config(dir)?;
            return manifest(dir);
        }
        if trusted(dir) {
            return Err(Error::new(
                "The proxy certificate is expiring. Remove its trust before setting it up again.",
            ));
        }
    }
    let now = OffsetDateTime::now_utc();
    let before = now - time::Duration::minutes(5);
    let after = now + time::Duration::days(30);
    let mut ca = CertificateParams::new(Vec::<String>::new()).map_err(io_error)?;
    ca.distinguished_name = rcgen::DistinguishedName::new();
    ca.distinguished_name.push(DnType::CommonName, SUBJECT);
    ca.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca.not_before = before;
    ca.not_after = after;
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    ca.name_constraints = Some(NameConstraints {
        permitted_subtrees: vec![GeneralSubtree::DnsName(HOST.into())],
        excluded_subtrees: vec![],
    });
    let ca_key = KeyPair::generate().map_err(io_error)?;
    let ca_cert = ca.self_signed(&ca_key).map_err(io_error)?;
    let issuer = Issuer::new(ca, ca_key);
    let mut leaf = CertificateParams::new(vec![HOST.into()]).map_err(io_error)?;
    leaf.distinguished_name = rcgen::DistinguishedName::new();
    leaf.distinguished_name.push(DnType::CommonName, HOST);
    leaf.not_before = before;
    leaf.not_after = after;
    leaf.is_ca = IsCa::ExplicitNoCa;
    leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    leaf.use_authority_key_identifier_extension = true;
    leaf.crl_distribution_points = vec![CrlDistributionPoint {
        uris: vec![format!("http://127.0.0.1:{HEALTH_PORT}/rl-uploader.crl")],
    }];
    let leaf_key = KeyPair::generate().map_err(io_error)?;
    let cert = leaf.signed_by(&leaf_key, &issuer).map_err(io_error)?;
    let crl = CertificateRevocationListParams {
        this_update: before,
        next_update: after,
        crl_number: 1u64.into(),
        issuing_distribution_point: None,
        revoked_certs: vec![],
        key_identifier_method: rcgen::KeyIdMethod::Sha256,
    }
    .signed_by(&issuer)
    .map_err(io_error)?;
    let key_pem = Zeroizing::new(leaf_key.serialize_pem());
    // The CA key is never serialized. The leaf key is persisted only through DPAPI.
    platform::atomic_write(
        &root.join("leaf-key.dpapi"),
        &platform::dpapi(key_pem.as_bytes(), true)?,
    )?;
    platform::atomic_write(
        &root.join("chain.pem"),
        format!("{}{}", cert.pem(), ca_cert.pem()).as_bytes(),
    )?;
    platform::atomic_write(&root.join("rl-uploader.crl"), crl.der().as_ref())?;
    platform::atomic_write(&root.join("root.cer"), ca_cert.der().as_ref())?;
    let value = manifest(dir)?;
    platform::atomic_write(
        &root.join("certificate.json"),
        &serde_json::to_vec_pretty(&value).map_err(io_error)?,
    )?;
    Ok(value)
}
pub fn install(dir: &Path, expected_sha1: &str) -> Result<()> {
    if platform::game_running() != Some(false) {
        return Err(Error::new(
            "Close Rocket League before changing certificate trust.",
        ));
    }
    let value = manifest(dir)?;
    if !value["sha1"]
        .as_str()
        .unwrap_or_default()
        .eq_ignore_ascii_case(expected_sha1)
    {
        return Err(Error::new(
            "Proxy certificate changed. Review its new fingerprint before installing.",
        ));
    }
    let mut command = platform::hidden_command("certutil.exe");
    command
        .args(["-user", "-addstore", "Root"])
        .arg(dir.join("proxy/root.cer"));
    if !platform::command_output(command, Duration::from_secs(30))?
        .status
        .success()
        || !trusted(dir)
    {
        return Err(Error::new("Windows did not trust this proxy certificate."));
    }
    Ok(())
}
pub fn remove_trust(dir: &Path) -> Result<()> {
    if platform::game_running() != Some(false) {
        return Err(Error::new(
            "Close Rocket League before changing certificate trust.",
        ));
    }
    let value = manifest(dir)?;
    let hash = value["sha1"].as_str().unwrap();
    let mut command = platform::hidden_command("certutil.exe");
    command.args(["-user", "-delstore", "Root", hash]);
    if !platform::command_output(command, Duration::from_secs(30))?
        .status
        .success()
        || trusted(dir)
    {
        return Err(Error::new("Could not remove this app's proxy certificate."));
    }
    Ok(())
}
pub fn server_config(dir: &Path) -> Result<Arc<rustls::ServerConfig>> {
    let chain = fs::read(dir.join("proxy/chain.pem")).map_err(io_error)?;
    let certificates: Vec<_> = rustls_pemfile::certs(&mut Cursor::new(chain))
        .collect::<std::io::Result<_>>()
        .map_err(io_error)?;
    let key_bytes = platform::dpapi(
        &fs::read(dir.join("proxy/leaf-key.dpapi")).map_err(io_error)?,
        false,
    )?;
    let key = rustls_pemfile::private_key(&mut Cursor::new(key_bytes.as_slice()))
        .map_err(io_error)?
        .ok_or_else(|| Error::new("Proxy private key is missing."))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let certified = rustls::sign::CertifiedKey::from_der(certificates, key, &provider)
        .map_err(|_| Error::new("Proxy certificate and private key do not match."))?;
    let mut resolver = rustls::server::ResolvesServerCertUsingSni::new();
    resolver.add(HOST, certified).map_err(io_error)?;
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io_error)?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
    Ok(Arc::new(config))
}
pub fn client_config() -> Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    for certificate in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(certificate);
    }
    if roots.is_empty() {
        return Err(Error::new("Windows trust roots are unavailable."));
    }
    Ok(Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(io_error)?
        .with_root_certificates(roots)
        .with_no_client_auth(),
    ))
}
