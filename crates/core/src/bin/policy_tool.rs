//! `thclaws-policy-tool` — operator CLI for org policy file lifecycle.
//!
//! Subcommands:
//!   keygen    Generate a fresh Ed25519 keypair, write public/private files.
//!   sign      Sign a policy JSON document with a private key.
//!   verify    Verify a signed policy file against a public key.
//!   inspect   Pretty-print a policy file (signed or not) for review.
//!   fingerprint  Compute the SHA-256 fingerprint of a binary file.
//!
//! This tool is the *only* place in the codebase that handles signing.
//! The main runtime binary (`thclaws`) only verifies — keeping signing
//! out means a leaked source tree is not a key-compromise vector.
//!
//! Operators are expected to run `keygen` once per customer on an
//! air-gapped machine, store the private key offline, and use `sign`
//! to produce signed policy files for distribution.

use base64::Engine;
use clap::{Parser, Subcommand};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use thclaws_core::policy::verify::{canonical_signed_payload, verify_policy, KeySource};

/// Fresh 32-byte secret material from the OS RNG. `getrandom` is in
/// the workspace dep tree already (catalogue-seed uses it indirectly);
/// keeps us independent of `rand_core` version drift between releases
/// of `ed25519-dalek`.
fn random_secret_bytes() -> Result<[u8; 32], String> {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).map_err(|e| format!("OS RNG failed: {e}"))?;
    Ok(buf)
}

#[derive(Parser)]
#[command(
    name = "thclaws-policy-tool",
    version,
    about = "thClaws org policy file lifecycle CLI"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate a fresh Ed25519 keypair. Writes raw 32-byte files by
    /// default (suitable for `THCLAWS_POLICY_PUBKEY_PATH` at build time).
    Keygen {
        /// Output path for the public key (raw 32 bytes).
        #[arg(long, default_value = "thclaws-policy.pub")]
        public: PathBuf,
        /// Output path for the private key (raw 32 bytes — protect well).
        #[arg(long, default_value = "thclaws-policy.key")]
        private: PathBuf,
        /// Refuse to overwrite existing files.
        #[arg(long)]
        force: bool,
    },
    /// Sign a policy JSON document. Reads the input file, removes any
    /// existing `signature`, canonicalizes, signs with the private
    /// key, and writes a copy with the new signature appended.
    Sign {
        /// Input policy file (JSON, with or without existing signature).
        input: PathBuf,
        /// Private key file (raw 32 bytes).
        #[arg(long)]
        private_key: PathBuf,
        /// Output file. Defaults to overwriting the input.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Verify a signed policy file against a public key. Exits 0 on
    /// success, non-zero with a message on failure.
    Verify {
        /// Signed policy file.
        input: PathBuf,
        /// Public key file (raw 32 bytes, base64, or PEM).
        #[arg(long)]
        public_key: PathBuf,
    },
    /// Pretty-print a policy file's structure. Useful for reviewing
    /// what an admin shipped before signing it.
    Inspect { input: PathBuf },
    /// Compute the SHA-256 fingerprint of a binary file. Use the output
    /// in `binding.binary_fingerprint` of a policy that should bind
    /// to a specific build.
    Fingerprint { binary: PathBuf },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Keygen {
            public,
            private,
            force,
        } => keygen(public, private, force),
        Cmd::Sign {
            input,
            private_key,
            output,
        } => sign(input, private_key, output),
        Cmd::Verify { input, public_key } => verify(input, public_key),
        Cmd::Inspect { input } => inspect(input),
        Cmd::Fingerprint { binary } => fingerprint(binary),
    }
}

fn keygen(public: PathBuf, private: PathBuf, force: bool) -> Result<(), String> {
    if !force {
        for p in [&public, &private] {
            if p.exists() {
                return Err(format!(
                    "{p:?} exists — pass --force to overwrite (be careful with --private)"
                ));
            }
        }
    }
    let secret = random_secret_bytes()?;
    let signing = SigningKey::from_bytes(&secret);
    let verifying = signing.verifying_key();
    std::fs::write(&public, verifying.to_bytes()).map_err(|e| format!("write public key: {e}"))?;
    std::fs::write(&private, signing.to_bytes()).map_err(|e| format!("write private key: {e}"))?;
    // Best-effort tighten permissions on the private key. Failure is
    // non-fatal but logged — a Windows host has no chmod.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600));
    }
    println!("✓ public key  → {}", public.display());
    println!("✓ private key → {} (chmod 600 on Unix)", private.display());
    println!("\nNext steps:");
    println!(
        "  • For an enterprise build, set THCLAWS_POLICY_PUBKEY_PATH={} at build time",
        public.display()
    );
    println!(
        "  • Keep {} OFFLINE. A leaked private key invalidates every signed policy.",
        private.display()
    );
    Ok(())
}

fn sign(input: PathBuf, private_key: PathBuf, output: Option<PathBuf>) -> Result<(), String> {
    let body = std::fs::read_to_string(&input).map_err(|e| format!("read {input:?}: {e}"))?;
    let mut doc: Value =
        serde_json::from_str(&body).map_err(|e| format!("parse {input:?}: {e}"))?;
    let key_bytes =
        std::fs::read(&private_key).map_err(|e| format!("read {private_key:?}: {e}"))?;
    if key_bytes.len() != 32 {
        return Err(format!(
            "private key at {private_key:?} is {} bytes; expected 32 raw Ed25519 bytes",
            key_bytes.len()
        ));
    }
    let arr: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| "internal: 32-byte cast failed".to_string())?;
    let signing = SigningKey::from_bytes(&arr);

    // Strip any existing signature before canonicalizing.
    if let Value::Object(map) = &mut doc {
        map.remove("signature");
    }
    let canonical = canonical_signed_payload(&doc);
    let sig = signing.sign(&canonical);
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
    if let Value::Object(map) = &mut doc {
        map.insert("signature".to_string(), Value::String(sig_b64));
    }

    let out_path = output.unwrap_or(input);
    let pretty = serde_json::to_string_pretty(&doc).map_err(|e| format!("re-serialize: {e}"))?;
    std::fs::write(&out_path, pretty).map_err(|e| format!("write {out_path:?}: {e}"))?;
    println!("✓ signed → {}", out_path.display());
    Ok(())
}

fn verify(input: PathBuf, public_key: PathBuf) -> Result<(), String> {
    let body = std::fs::read_to_string(&input).map_err(|e| format!("read {input:?}: {e}"))?;
    let doc: Value = serde_json::from_str(&body).map_err(|e| format!("parse {input:?}: {e}"))?;
    let key_bytes = std::fs::read(&public_key).map_err(|e| format!("read {public_key:?}: {e}"))?;
    let key_bytes = if key_bytes.len() == 32 {
        key_bytes
    } else {
        // Try text-form fallback (base64 or PEM).
        let text = std::str::from_utf8(&key_bytes)
            .map_err(|_| "public key is neither 32 raw bytes nor UTF-8 text".to_string())?;
        decode_text_pubkey(text)?
    };
    if key_bytes.len() != 32 {
        return Err(format!(
            "public key decoded to {} bytes; expected 32",
            key_bytes.len()
        ));
    }
    let arr: [u8; 32] = key_bytes.try_into().map_err(|_| "32-byte cast")?;
    let verifying = VerifyingKey::from_bytes(&arr).map_err(|e| format!("invalid pubkey: {e}"))?;
    verify_policy(&doc, &KeySource::Embedded(verifying), &input)
        .map_err(|e| format!("verification failed: {e}"))?;
    println!(
        "✓ {} verifies against {}",
        input.display(),
        public_key.display()
    );
    Ok(())
}

fn inspect(input: PathBuf) -> Result<(), String> {
    let body = std::fs::read_to_string(&input).map_err(|e| format!("read {input:?}: {e}"))?;
    let doc: Value = serde_json::from_str(&body).map_err(|e| format!("parse {input:?}: {e}"))?;
    println!("policy: {}", input.display());
    if let Some(v) = doc.get("version") {
        println!("  version:    {v}");
    }
    if let Some(v) = doc.get("issuer").and_then(|v| v.as_str()) {
        println!("  issuer:     {v}");
    }
    if let Some(v) = doc.get("issued_at").and_then(|v| v.as_str()) {
        println!("  issued_at:  {v}");
    }
    if let Some(v) = doc.get("expires_at").and_then(|v| v.as_str()) {
        println!("  expires_at: {v}");
    }
    if let Some(b) = doc.get("binding") {
        println!("  binding:");
        if let Some(v) = b.get("org_id").and_then(|v| v.as_str()) {
            println!("    org_id:             {v}");
        }
        if let Some(v) = b.get("binary_fingerprint").and_then(|v| v.as_str()) {
            println!("    binary_fingerprint: {v}");
        }
    }
    if let Some(p) = doc.get("policies").and_then(|v| v.as_object()) {
        println!("  policies:");
        for (key, block) in p {
            let enabled = block
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            println!("    {key}: enabled={enabled}");
        }
    }
    let signed = doc.get("signature").is_some();
    println!("  signed:     {signed}");
    Ok(())
}

fn fingerprint(binary: PathBuf) -> Result<(), String> {
    let bytes = std::fs::read(&binary).map_err(|e| format!("read {binary:?}: {e}"))?;
    let mut h = Sha256::new();
    h.update(&bytes);
    println!("sha256:{:x}", h.finalize());
    Ok(())
}

fn decode_text_pubkey(text: &str) -> Result<Vec<u8>, String> {
    let trimmed = text.trim();
    let inner: String = if trimmed.starts_with("-----BEGIN") {
        trimmed
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect::<Vec<_>>()
            .join("")
    } else {
        trimmed.replace(['\n', '\r', ' '], "")
    };
    base64::engine::general_purpose::STANDARD
        .decode(&inner)
        .map_err(|e| format!("base64 decode failed: {e}"))
}
