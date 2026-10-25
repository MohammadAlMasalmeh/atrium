//! Release archive signatures: Ed25519 over a statement binding the version, SHA-256, and size.
//! Shared by the browser's updater and the `aside_sign_update` release tool.
use anyhow::{bail, Context, Result};
use base64::Engine;
use std::io::Read;

pub const DOMAIN: &str = "aside-update-v1";

/// SHA-256 (hex) and byte length of a file, read in chunks.
pub fn digest_file(path: &std::path::Path) -> Result<(String, u64)> {
    let mut file = std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size += read as u64;
        context.update(&buffer[..read]);
    }
    let hex = context.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect();
    Ok((hex, size))
}

pub fn statement(version: &str, sha256: &str, size: u64) -> String {
    format!("{DOMAIN}\nversion={version}\nsha256={sha256}\nsize={size}\n")
}

fn decode(text: &str, what: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .with_context(|| format!("{what} is not valid base64"))
}

/// The base64 public key from text that may contain `#` comment lines.
pub fn parse_public_key(text: &str) -> Option<String> {
    let key: String = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    let bytes = decode(&key, "public key").ok()?;
    (bytes.len() == 32).then_some(key)
}

/// Checks `signature` (base64) over the archive's statement with `public_key` (base64, 32 bytes).
pub fn verify(public_key: &str, version: &str, sha256: &str, size: u64, signature: &str) -> Result<()> {
    let key = decode(public_key, "public key")?;
    if key.len() != 32 {
        bail!("the update public key must be 32 bytes");
    }
    let signature = decode(signature, "signature")?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(statement(version, sha256, size).as_bytes(), &signature)
        .map_err(|_| anyhow::anyhow!("the update signature is not valid for this archive and version"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::KeyPair;

    #[test]
    fn signature_binds_version_digest_and_size() {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        let public = b64(pair.public_key().as_ref());
        let sig = b64(pair.sign(statement("1.2.3", "ab", 10).as_bytes()).as_ref());
        assert!(verify(&public, "1.2.3", "ab", 10, &sig).is_ok());
        assert!(verify(&public, "1.2.4", "ab", 10, &sig).is_err());
        assert!(verify(&public, "1.2.3", "ac", 10, &sig).is_err());
        assert!(verify(&public, "1.2.3", "ab", 11, &sig).is_err());
        assert_eq!(parse_public_key(&format!("# comment\n{public}\n")), Some(public));
        assert_eq!(parse_public_key("# none yet\n"), None);
    }
}
