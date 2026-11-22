//! Release signing for T7's over-the-air updates.
//!
//!     t7-release keygen SECRET_FILE
//!         Writes a new signing key (hex) to SECRET_FILE and prints the public
//!         key to embed in the client. The secret is never printed.
//!
//!     t7-release manifest --version V --base-url URL --out latest.json TARGET=FILE...
//!         Hashes and signs each build with the key in $T7_SIGNING_KEY (hex) and
//!         writes the manifest. Each FILE is served at URL/<file name>.

use std::path::Path;
use std::process::ExitCode;

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use t7_proto::update::{Asset, Manifest, from_hex, parse_version, signed_message, to_hex};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("keygen") => keygen(&args[1..]),
        Some("manifest") => manifest(&args[1..]),
        _ => {
            Err("usage: t7-release keygen SECRET_FILE | manifest --version V --base-url URL --out FILE TARGET=FILE..."
                .into())
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn keygen(args: &[String]) -> Result<(), String> {
    let [out] = args else { return Err("usage: t7-release keygen SECRET_FILE".into()) };
    if Path::new(out).exists() {
        return Err(format!("{out} already exists; not overwriting a key"));
    }
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| e.to_string())?;
    let key = SigningKey::from_bytes(&seed);
    std::fs::write(out, to_hex(&seed)).map_err(|e| e.to_string())?;
    println!("public key: {}", to_hex(key.verifying_key().as_bytes()));
    Ok(())
}

fn manifest(args: &[String]) -> Result<(), String> {
    let mut version = None;
    let mut base_url = None;
    let mut out = None;
    let mut files = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--version" => version = it.next().cloned(),
            "--base-url" => base_url = it.next().cloned(),
            "--out" => out = it.next().cloned(),
            spec => files.push(spec.split_once('=').ok_or(format!("expected TARGET=FILE, got {spec}"))?),
        }
    }
    let version = version.ok_or("--version is required")?;
    parse_version(&version).ok_or(format!("{version} is not MAJOR.MINOR.PATCH"))?;
    let base_url = base_url.ok_or("--base-url is required")?;
    let out = out.ok_or("--out is required")?;

    let secret = std::env::var("T7_SIGNING_KEY").map_err(|_| "T7_SIGNING_KEY is not set")?;
    let seed: [u8; 32] =
        from_hex(&secret).and_then(|b| b.try_into().ok()).ok_or("T7_SIGNING_KEY must be 64 hex characters")?;
    let key = SigningKey::from_bytes(&seed);

    let mut assets = Vec::new();
    for (target, file) in files {
        let bytes = std::fs::read(file).map_err(|e| format!("{file}: {e}"))?;
        let sha256 = to_hex(&Sha256::digest(&bytes));
        let signature = key.sign(signed_message(&version, target, &sha256).as_bytes());
        let name = Path::new(file).file_name().and_then(|n| n.to_str()).ok_or(format!("bad file name {file}"))?;
        assets.push(Asset {
            target: target.to_owned(),
            url: format!("{}/{name}", base_url.trim_end_matches('/')),
            sha256,
            signature: to_hex(&signature.to_bytes()),
        });
        println!("signed {target}: {name}");
    }
    let manifest = Manifest { version, assets };
    let json = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    std::fs::write(&out, json).map_err(|e| e.to_string())?;
    println!("wrote {out} (public key {})", to_hex(key.verifying_key().as_bytes()));
    Ok(())
}
