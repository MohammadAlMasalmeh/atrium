//! CRX3 packages: header parsing, signature verification, extension IDs, and safe unzipping.
//! Format: "Cr24", version 3, header length, a `CrxFileHeader` protobuf, then a zip archive.
use anyhow::{anyhow, bail, ensure, Context, Result};
use ring::signature;
use std::{io::Read, path::Path};

/// SHA-256 of the Chrome Web Store publisher key; store CRXs carry a proof signed by it.
const WEB_STORE_KEY_HASH: [u8; 32] = [
    0x61, 0xf7, 0xf2, 0xa6, 0xbf, 0xcf, 0x74, 0xcd, 0x0b, 0xc1, 0xfe, 0x24, 0x97, 0xcc, 0x9b, 0x04,
    0x25, 0x4c, 0x65, 0x8f, 0x79, 0xf2, 0x14, 0x53, 0x92, 0x86, 0x7e, 0xa8, 0x36, 0x63, 0x67, 0xcf,
];
const MAX_UNPACKED: u64 = 512 << 20;

#[derive(Debug)]
pub struct Verified<'a> {
    pub id: String,
    /// DER SubjectPublicKeyInfo of the developer key the ID derives from.
    pub public_key: Vec<u8>,
    pub archive: &'a [u8],
    pub from_web_store: bool,
}

/// Chromium IDs are the first 128 bits of SHA-256(public key), one letter a-p per nibble.
pub fn id_from_key(public_key: &[u8]) -> String {
    let hash = ring::digest::digest(&ring::digest::SHA256, public_key);
    hash.as_ref()[..16]
        .iter()
        .flat_map(|b| [b >> 4, b & 15])
        .map(|n| (b'a' + n) as char)
        .collect()
}

pub fn is_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| (b'a'..=b'p').contains(&b))
}

/// Accepts a bare extension ID or a Chrome Web Store detail URL.
pub fn parse_store_input(input: &str) -> Result<String> {
    let input = input.trim();
    if is_id(input) {
        return Ok(input.into());
    }
    let url = reqwest::Url::parse(input).map_err(|_| anyhow!("Enter a Chrome Web Store link or a 32-letter extension ID"))?;
    ensure!(
        matches!(url.host_str(), Some("chromewebstore.google.com" | "chrome.google.com")) && url.scheme() == "https",
        "Only Chrome Web Store links are supported"
    );
    url.path_segments()
        .into_iter()
        .flatten()
        .find(|s| is_id(s))
        .map(str::to_string)
        .ok_or_else(|| anyhow!("The link does not contain an extension ID"))
}

fn varint(b: &[u8], i: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*i).ok_or_else(|| anyhow!("truncated CRX header"))?;
        *i += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(value);
        }
    }
    bail!("invalid CRX header varint")
}

/// Length-delimited protobuf fields; other wire types are skipped.
fn fields(b: &[u8]) -> Result<Vec<(u64, &[u8])>> {
    let (mut i, mut out) = (0, Vec::new());
    while i < b.len() {
        let key = varint(b, &mut i)?;
        match key & 7 {
            0 => {
                varint(b, &mut i)?;
            }
            1 => i += 8,
            5 => i += 4,
            2 => {
                let len = varint(b, &mut i)? as usize;
                let end = i.checked_add(len).filter(|e| *e <= b.len()).ok_or_else(|| anyhow!("truncated CRX header"))?;
                out.push((key >> 3, &b[i..end]));
                i = end;
            }
            _ => bail!("unsupported CRX header field"),
        }
    }
    ensure!(i == b.len(), "truncated CRX header");
    Ok(out)
}

/// DER reader for SEQUENCE { AlgorithmIdentifier, BIT STRING }: returns (algorithm OID, key bits).
fn spki(der: &[u8]) -> Result<(&[u8], &[u8])> {
    fn tlv(b: &[u8]) -> Result<(u8, &[u8], &[u8])> {
        ensure!(b.len() >= 2, "invalid public key");
        let (tag, first) = (b[0], b[1] as usize);
        let (len, start) = if first < 0x80 {
            (first, 2)
        } else {
            let n = first & 0x7f;
            ensure!((1..=4).contains(&n) && b.len() >= 2 + n, "invalid public key");
            (b[2..2 + n].iter().fold(0usize, |a, x| a << 8 | *x as usize), 2 + n)
        };
        let end = start.checked_add(len).filter(|e| *e <= b.len()).ok_or_else(|| anyhow!("invalid public key"))?;
        Ok((tag, &b[start..end], &b[end..]))
    }
    let (tag, body, rest) = tlv(der)?;
    ensure!(tag == 0x30 && rest.is_empty(), "invalid public key");
    let (tag, algorithm, rest) = tlv(body)?;
    ensure!(tag == 0x30, "invalid public key");
    let (tag, oid, _) = tlv(algorithm)?;
    ensure!(tag == 0x06, "invalid public key");
    let (tag, bits, _) = tlv(rest)?;
    ensure!(tag == 0x03 && bits.first() == Some(&0), "invalid public key");
    Ok((oid, &bits[1..]))
}

const RSA_OID: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const EC_OID: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];

/// Verifies every signature in the header, like Chromium's CRX verifier, and requires a
/// developer key matching the declared ID. `expected_id` pins the ID when it is known.
pub fn verify<'a>(crx: &'a [u8], expected_id: Option<&str>) -> Result<Verified<'a>> {
    ensure!(crx.len() >= 12 && &crx[..4] == b"Cr24", "Not a Chrome extension package (CRX)");
    let version = u32::from_le_bytes(crx[4..8].try_into().unwrap());
    ensure!(version == 3, "Unsupported CRX version {version}; only CRX3 is accepted");
    let header_len = u32::from_le_bytes(crx[8..12].try_into().unwrap()) as usize;
    let header = crx.get(12..12 + header_len).context("truncated CRX header")?;
    let archive = &crx[12 + header_len..];
    let header = fields(header)?;
    let signed_data = header.iter().find(|(f, _)| *f == 10000).map(|(_, v)| *v).context("CRX has no signed header data")?;
    let crx_id = fields(signed_data)?.into_iter().find(|(f, _)| *f == 1).map(|(_, v)| v).context("CRX has no ID")?;
    ensure!(crx_id.len() == 16, "CRX ID has the wrong length");
    let mut message = b"CRX3 SignedData\x00".to_vec();
    message.extend_from_slice(&(signed_data.len() as u32).to_le_bytes());
    message.extend_from_slice(signed_data);
    message.extend_from_slice(archive);

    let (mut developer_key, mut from_web_store) = (None, false);
    for (field, proof) in &header {
        if *field != 2 && *field != 3 {
            continue;
        }
        let proof = fields(proof)?;
        let get = |n| proof.iter().find(|(f, _)| *f == n).map(|(_, v)| *v);
        let (key, sig) = (get(1).context("CRX proof has no key")?, get(2).context("CRX proof has no signature")?);
        let (oid, bits) = spki(key)?;
        let algorithm: &dyn signature::VerificationAlgorithm = match (field, oid) {
            // Chromium still accepts the 1024-bit developer keys of older Web Store extensions.
            (2, RSA_OID) => &signature::RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY,
            (3, EC_OID) => &signature::ECDSA_P256_SHA256_ASN1,
            _ => bail!("CRX uses an unsupported key type"),
        };
        signature::UnparsedPublicKey::new(algorithm, bits)
            .verify(&message, sig)
            .map_err(|_| anyhow!("CRX signature verification failed"))?;
        let hash = ring::digest::digest(&ring::digest::SHA256, key);
        if hash.as_ref() == WEB_STORE_KEY_HASH {
            from_web_store = true;
        }
        if *field == 2 && hash.as_ref()[..16] == *crx_id {
            developer_key = Some(key.to_vec());
        }
    }
    let public_key = developer_key.context("CRX is not signed by the key its ID derives from")?;
    let id = id_from_key(&public_key);
    if let Some(expected) = expected_id {
        ensure!(id == expected, "CRX is for extension {id}, not {expected}");
    }
    Ok(Verified { id, public_key, archive, from_web_store })
}

fn u16_at(b: &[u8], i: usize) -> Result<usize> {
    Ok(u16::from_le_bytes(b.get(i..i + 2).context("truncated zip")?.try_into().unwrap()) as usize)
}
fn u32_at(b: &[u8], i: usize) -> Result<usize> {
    Ok(u32::from_le_bytes(b.get(i..i + 4).context("truncated zip")?.try_into().unwrap()) as usize)
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.contains('\\')
        && !name.contains('\0')
        && name.split('/').all(|part| part != ".." && part != ".")
}

/// Extracts stored and deflated entries into `dest`, rejecting paths that escape it.
pub fn unzip(archive: &[u8], dest: &Path) -> Result<()> {
    let eocd = (0..archive.len().saturating_sub(21))
        .rev()
        .take(65557)
        .find(|&i| archive[i..].starts_with(b"PK\x05\x06"))
        .context("CRX archive is not a zip file")?;
    let count = u16_at(archive, eocd + 10)?;
    let mut at = u32_at(archive, eocd + 16)?;
    let mut total = 0u64;
    for _ in 0..count {
        ensure!(archive.get(at..at + 4) == Some(b"PK\x01\x02"), "corrupt zip directory");
        let method = u16_at(archive, at + 10)?;
        let compressed = u32_at(archive, at + 20)?;
        let size = u32_at(archive, at + 24)?;
        let (name_len, extra_len, comment_len) = (u16_at(archive, at + 28)?, u16_at(archive, at + 30)?, u16_at(archive, at + 32)?);
        let local = u32_at(archive, at + 42)?;
        let name = std::str::from_utf8(archive.get(at + 46..at + 46 + name_len).context("truncated zip")?)
            .context("zip entry name is not UTF-8")?
            .to_string();
        at += 46 + name_len + extra_len + comment_len;
        ensure!(safe_name(&name), "zip entry has an unsafe path: {name}");
        let path = dest.join(&name);
        if name.ends_with('/') {
            std::fs::create_dir_all(&path)?;
            continue;
        }
        ensure!(archive.get(local..local + 4) == Some(b"PK\x03\x04"), "corrupt zip entry");
        let start = local + 30 + u16_at(archive, local + 26)? + u16_at(archive, local + 28)?;
        let data = archive.get(start..start + compressed).context("truncated zip entry")?;
        total += size as u64;
        ensure!(total <= MAX_UNPACKED, "extension is larger than 512 MiB unpacked");
        let bytes = match method {
            0 => data.to_vec(),
            8 => {
                let mut out = Vec::with_capacity(size);
                flate2::read::DeflateDecoder::new(data).take(size as u64 + 1).read_to_end(&mut out)?;
                out
            }
            _ => bail!("zip entry {name} uses unsupported compression"),
        };
        ensure!(bytes.len() == size, "zip entry {name} has the wrong size");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_inputs_resolve_to_ids() {
        let id = "ddkjiahejlhfcafbddmgiahcphecmpfh";
        assert_eq!(parse_store_input(id).unwrap(), id);
        assert_eq!(parse_store_input(&format!("https://chromewebstore.google.com/detail/ublock-origin-lite/{id}?hl=en")).unwrap(), id);
        assert_eq!(parse_store_input(&format!("https://chrome.google.com/webstore/detail/x/{id}")).unwrap(), id);
        assert!(parse_store_input(&format!("https://evil.example/detail/{id}")).is_err());
        assert!(parse_store_input("not an id").is_err());
    }

    #[test]
    fn rejects_malformed_packages() {
        assert!(verify(b"PK\x03\x04", None).is_err());
        let mut v2 = b"Cr24".to_vec();
        v2.extend_from_slice(&2u32.to_le_bytes());
        v2.extend_from_slice(&0u32.to_le_bytes());
        assert!(verify(&v2, None).unwrap_err().to_string().contains("CRX3"));
        let mut empty = b"Cr24".to_vec();
        empty.extend_from_slice(&3u32.to_le_bytes());
        empty.extend_from_slice(&0u32.to_le_bytes());
        assert!(verify(&empty, None).is_err());
    }

    #[test]
    fn zip_paths_cannot_escape() {
        assert!(safe_name("js/background.js"));
        for bad in ["../x", "/etc/x", "a/../../x", "a\\b", "./x"] {
            assert!(!safe_name(bad), "{bad}");
        }
    }

    /// Built by tools/crx_pack.py.
    #[test]
    fn verifies_and_unpacks_a_signed_crx() {
        let crx = include_bytes!("../tools/fixtures/signed.crx").to_vec();
        let verified = verify(&crx, None).unwrap();
        assert_eq!(verified.id, "agocccklecdfkkeinaonifnocpeoamnj");
        assert_eq!(verified.id, id_from_key(&verified.public_key));
        assert!(!verified.from_web_store);
        assert!(verify(&crx, Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")).is_err());
        let mut tampered = crx.clone();
        let last = tampered.len() - 30;
        tampered[last] ^= 1;
        assert!(verify(&tampered, None).is_err());
        let dir = std::env::temp_dir().join(format!("atrium-crx-{}", uuid::Uuid::new_v4()));
        unzip(verified.archive, &dir).unwrap();
        assert!(dir.join("manifest.json").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
