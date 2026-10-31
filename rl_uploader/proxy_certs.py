"""A dedicated, short-lived certificate for one Rocket League hostname.

The CA signing key is discarded after provisioning. Only the leaf private key
is retained, encrypted with Windows DPAPI. No general-purpose signing service.
"""

from datetime import datetime, timedelta, timezone
import hashlib
import json
import os
from pathlib import Path
import secrets
import ssl
import subprocess
import sys
import tempfile

from .cloud_protocol import CloudError
from .platform import _dpapi

HOST = "ws.rlpp.psynet.gg"
HEALTH_PORT = 47639
SUBJECT = "RL Uploader WebSocket Proxy"


def crypto():
    vendor = Path(__file__).resolve().parent.parent / ".vendor"
    if vendor.is_dir() and str(vendor) not in sys.path:
        sys.path.insert(0, str(vendor))
    try:
        from cryptography import x509
        from cryptography.hazmat.primitives import hashes, serialization
        from cryptography.hazmat.primitives.asymmetric import rsa
        return x509, hashes, serialization, rsa
    except ImportError:
        raise CloudError("Run Setup periodic capture.cmd to install certificate support.") from None


def prepare(data_dir):
    x509, hashes, serialization, rsa = crypto()
    root = Path(data_dir) / "proxy"
    root.mkdir(parents=True, exist_ok=True)
    manifest = root / "certificate.json"
    if manifest.exists():
        existing = json.loads(manifest.read_text())
        cert = x509.load_der_x509_certificate((root / "root.cer").read_bytes())
        if cert.not_valid_after_utc > datetime.now(timezone.utc) + timedelta(days=2):
            return existing
        if trusted(data_dir):
            raise CloudError("The proxy certificate is expiring. Remove its trust before setting it up again.")
    now = datetime.now(timezone.utc)
    ca_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    leaf_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    issuer = x509.Name([x509.NameAttribute(x509.oid.NameOID.COMMON_NAME, SUBJECT)])
    ca = (x509.CertificateBuilder().subject_name(issuer).issuer_name(issuer)
          .public_key(ca_key.public_key()).serial_number(x509.random_serial_number())
          .not_valid_before(now - timedelta(minutes=5)).not_valid_after(now + timedelta(days=30))
          .add_extension(x509.BasicConstraints(ca=True, path_length=0), critical=True)
          .add_extension(x509.NameConstraints(permitted_subtrees=[x509.DNSName(HOST)], excluded_subtrees=None), critical=True)
          .add_extension(x509.KeyUsage(False, False, False, False, False, True, True, None, None), critical=True)
          .add_extension(x509.SubjectKeyIdentifier.from_public_key(ca_key.public_key()), critical=False)
          .sign(ca_key, hashes.SHA256()))
    distribution = x509.DistributionPoint(full_name=[x509.UniformResourceIdentifier(
        f"http://127.0.0.1:{HEALTH_PORT}/rl-uploader.crl")], relative_name=None, reasons=None, crl_issuer=None)
    leaf = (x509.CertificateBuilder()
            .subject_name(x509.Name([x509.NameAttribute(x509.oid.NameOID.COMMON_NAME, HOST)]))
            .issuer_name(issuer).public_key(leaf_key.public_key()).serial_number(x509.random_serial_number())
            .not_valid_before(now - timedelta(minutes=5)).not_valid_after(now + timedelta(days=30))
            .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
            .add_extension(x509.SubjectAlternativeName([x509.DNSName(HOST)]), critical=False)
            .add_extension(x509.ExtendedKeyUsage([x509.oid.ExtendedKeyUsageOID.SERVER_AUTH]), critical=False)
            .add_extension(x509.KeyUsage(True, False, True, False, False, False, False, None, None), critical=True)
            .add_extension(x509.AuthorityKeyIdentifier.from_issuer_public_key(ca_key.public_key()), critical=False)
            .add_extension(x509.CRLDistributionPoints([distribution]), critical=False)
            .sign(ca_key, hashes.SHA256()))
    crl = (x509.CertificateRevocationListBuilder().issuer_name(issuer)
           .last_update(now - timedelta(minutes=5)).next_update(now + timedelta(days=30))
           .add_extension(x509.AuthorityKeyIdentifier.from_issuer_public_key(ca_key.public_key()), critical=False)
           .sign(ca_key, hashes.SHA256()))
    der = ca.public_bytes(serialization.Encoding.DER)
    key = leaf_key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8,
                                 serialization.NoEncryption())
    (root / "root.cer").write_bytes(der)
    (root / "chain.pem").write_bytes(leaf.public_bytes(serialization.Encoding.PEM) + ca.public_bytes(serialization.Encoding.PEM))
    (root / "leaf-key.dpapi").write_bytes(_dpapi(key, True))
    (root / "rl-uploader.crl").write_bytes(crl.public_bytes(serialization.Encoding.DER))
    result = {"host": HOST, "subject": SUBJECT, "sha1": hashlib.sha1(der).hexdigest().upper(),
              "sha256": hashlib.sha256(der).hexdigest(), "expires": ca.not_valid_after_utc.isoformat()}
    manifest.write_text(json.dumps(result, indent=2) + "\n")
    # ca_key has never been serialized or persisted.
    return result


def trusted(data_dir):
    try:
        der = (Path(data_dir) / "proxy" / "root.cer").read_bytes()
        return any(cert == der for cert, kind, trust in ssl.enum_certificates("ROOT") if kind == "x509_asn")
    except (OSError, AttributeError):
        return False


def install_trust(data_dir):
    result = prepare(data_dir)
    cert = (Path(data_dir) / "proxy" / "root.cer").resolve()
    call = subprocess.run(["certutil.exe", "-user", "-addstore", "Root", str(cert)], capture_output=True,
                          timeout=30, creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
    if call.returncode or not trusted(data_dir):
        raise CloudError("Windows did not trust the proxy certificate. Periodic capture remains unavailable.")
    return result


def remove_trust(data_dir):
    der = (Path(data_dir) / "proxy" / "root.cer").read_bytes()
    thumbprint = hashlib.sha1(der).hexdigest()
    call = subprocess.run(["certutil.exe", "-user", "-delstore", "Root", thumbprint], capture_output=True,
                          timeout=30, creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
    if call.returncode or trusted(data_dir):
        raise CloudError("Could not remove this app's proxy certificate.")


def server_context(data_dir):
    _, _, serialization, _ = crypto()
    root = Path(data_dir) / "proxy"
    key = serialization.load_pem_private_key(_dpapi((root / "leaf-key.dpapi").read_bytes(), False), password=None)
    password = secrets.token_bytes(32)
    encrypted = key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8,
                                   serialization.BestAvailableEncryption(password))
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.minimum_version = ssl.TLSVersion.TLSv1_2
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=root, suffix=".encrypted-key", delete=False) as stream:
            temporary = Path(stream.name)
            stream.write(encrypted)
        context.load_cert_chain(root / "chain.pem", temporary, password)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
    def check_name(socket, name, initial):
        if name != HOST:
            return ssl.ALERT_DESCRIPTION_UNRECOGNIZED_NAME
    context.set_servername_callback(check_name)
    return context
