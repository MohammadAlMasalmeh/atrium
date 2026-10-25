//! Release signing tool for the auto-updater.
//!
//!   aside_sign_update keygen --secret <path-to-secret-key> [--public <path-to-public-key>]
//!   aside_sign_update public --secret <path-to-secret-key>                          (prints the public key)
//!   aside_sign_update sign --secret <path-to-secret-key> --version 1.2.3 ARCHIVE   (writes ARCHIVE.sig)
//!   aside_sign_update verify --public <path-to-public-key> --version 1.2.3 ARCHIVE [--signature FILE]
//!
//! The secret key is a base64 PKCS#8 Ed25519 document; keep it out of the repository.
//! `--secret-env NAME` reads it from an environment variable instead (CI secrets).
#[path = "../update_sig.rs"]
mod update_sig;

use anyhow::{bail, Context, Result};
use base64::Engine;
use ring::signature::{Ed25519KeyPair, KeyPair};
use std::{io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf};

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

struct Args {
    command: String,
    options: std::collections::HashMap<String, String>,
    positional: Vec<String>,
}

fn parse() -> Result<Args> {
    let mut raw = std::env::args().skip(1);
    let command = raw.next().unwrap_or_default();
    let mut options = std::collections::HashMap::new();
    let mut positional = Vec::new();
    while let Some(arg) = raw.next() {
        if let Some(name) = arg.strip_prefix("--") {
            options.insert(name.to_string(), raw.next().with_context(|| format!("--{name} needs a value"))?);
        } else {
            positional.push(arg);
        }
    }
    Ok(Args { command, options, positional })
}

fn secret(args: &Args) -> Result<Ed25519KeyPair> {
    let text = match (args.options.get("secret"), args.options.get("secret-env")) {
        (Some(path), _) => std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?,
        (None, Some(name)) => std::env::var(name).with_context(|| format!("{name} is not set"))?,
        _ => bail!("pass --secret FILE or --secret-env NAME"),
    };
    let der = b64().decode(text.trim()).context("the secret key is not base64")?;
    Ed25519KeyPair::from_pkcs8(&der).map_err(|_| anyhow::anyhow!("the secret key is not an Ed25519 PKCS#8 key"))
}

fn write_private(path: &str, contents: &str) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("cannot create {path} (it must not exist yet)"))?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

fn archive(args: &Args) -> Result<(PathBuf, String)> {
    let [path] = args.positional.as_slice() else { bail!("pass exactly one archive path") };
    let version = args.options.get("version").context("pass --version")?;
    Ok((PathBuf::from(path), version.trim_start_matches('v').to_string()))
}

fn main() -> Result<()> {
    let args = parse()?;
    match args.command.as_str() {
        "keygen" => {
            let path = args.options.get("secret").context("pass --secret FILE for the new secret key")?;
            let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .map_err(|_| anyhow::anyhow!("key generation failed"))?;
            let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|_| anyhow::anyhow!("key generation failed"))?;
            write_private(path, &format!("{}\n", b64().encode(pkcs8.as_ref())))?;
            let public = b64().encode(pair.public_key().as_ref());
            if let Some(public_path) = args.options.get("public") {
                std::fs::write(public_path, format!("{public}\n"))?;
            }
            println!("{public}");
        }
        "public" => println!("{}", b64().encode(secret(&args)?.public_key().as_ref())),
        "sign" => {
            let pair = secret(&args)?;
            let (path, version) = archive(&args)?;
            let (sha256, size) = update_sig::digest_file(&path)?;
            let signature = b64().encode(pair.sign(update_sig::statement(&version, &sha256, size).as_bytes()).as_ref());
            let out = args.options.get("out").map(PathBuf::from).unwrap_or_else(|| {
                let mut name = path.clone().into_os_string();
                name.push(".sig");
                name.into()
            });
            std::fs::write(&out, format!("{signature}\n"))?;
            eprintln!("signed {} ({size} bytes, sha256 {sha256}) as version {version}", path.display());
            println!("{}", out.display());
        }
        "verify" => {
            let public_text = args.options.get("public").context("pass --public FILE")?;
            let public = update_sig::parse_public_key(&std::fs::read_to_string(public_text)?)
                .context("the public key file does not contain a 32-byte base64 key")?;
            let (path, version) = archive(&args)?;
            let sig_path = args.options.get("signature").cloned().unwrap_or_else(|| format!("{}.sig", path.display()));
            let (sha256, size) = update_sig::digest_file(&path)?;
            update_sig::verify(&public, &version, &sha256, size, &std::fs::read_to_string(sig_path)?)?;
            println!("valid");
        }
        _ => bail!("usage: aside_sign_update keygen|sign|verify (see the source header)"),
    }
    Ok(())
}
