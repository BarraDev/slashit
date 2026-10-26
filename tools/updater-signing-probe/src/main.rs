// SPDX-License-Identifier: MPL-2.0
//! Verifies a Tauri updater signature the way `tauri-plugin-updater` does.
//!
//! usage: slashit-updater-signing-probe verify <pubkey-file> <signature-file> <payload-file>
//!
//! - `pubkey-file`: the value of `plugins.updater.pubkey` in `tauri.conf.json`,
//!   which is base64 of a minisign public key.
//! - `signature-file`: a `.sig` file as the Tauri CLI writes it, which is base64
//!   of a minisign signature.
//! - `payload-file`: the bytes the signature is supposed to cover.
//!
//! The plugin decodes the key and the signature the same way and calls
//! `verify(payload, signature, allow_legacy = true)`, so a VERIFIED here is the
//! check the installed application will perform.
//!
//! Exit status: 0 when the signature verifies, 1 when it is rejected, 2 when an
//! input cannot be read or decoded. Diagnostics name a category and never echo
//! key or signature material.

use base64::Engine;
use minisign_verify::{PublicKey, Signature};
use std::process::ExitCode;

enum Verdict {
    /// The inputs are well formed, and the signature does not cover the payload
    /// under this key.
    Rejected(String),
    /// An input could not be read or decoded, so no verdict on the signature.
    Input(String),
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: slashit-updater-signing-probe verify <pubkey-file> <signature-file> <payload-file>";
    let [command, pubkey_path, signature_path, payload_path] = args.as_slice() else {
        eprintln!("{usage}");
        return ExitCode::from(2);
    };
    if command != "verify" {
        eprintln!("{usage}");
        return ExitCode::from(2);
    }
    match verify(pubkey_path, signature_path, payload_path) {
        Ok(()) => {
            println!("VERIFIED");
            ExitCode::SUCCESS
        }
        Err(Verdict::Rejected(reason)) => {
            println!("REJECTED: {reason}");
            ExitCode::from(1)
        }
        Err(Verdict::Input(reason)) => {
            eprintln!("INPUT ERROR: {reason}");
            ExitCode::from(2)
        }
    }
}

fn verify(pubkey_path: &str, signature_path: &str, payload_path: &str) -> Result<(), Verdict> {
    let pubkey_text = decode_base64_file(pubkey_path, "public key")?;
    let public_key = PublicKey::decode(&pubkey_text)
        .map_err(|error| Verdict::Input(format!("the public key does not decode: {error}")))?;

    let signature_text = decode_base64_file(signature_path, "signature")?;
    let signature = Signature::decode(&signature_text)
        .map_err(|error| Verdict::Input(format!("the signature does not decode: {error}")))?;

    let payload = std::fs::read(payload_path)
        .map_err(|error| Verdict::Input(format!("the payload cannot be read: {error}")))?;

    public_key
        .verify(&payload, &signature, true)
        .map_err(|error| Verdict::Rejected(format!("the signature does not verify under this public key ({error})")))
}

/// Read a base64 file and return the text it encodes.
fn decode_base64_file(path: &str, what: &str) -> Result<String, Verdict> {
    let encoded = std::fs::read_to_string(path)
        .map_err(|error| Verdict::Input(format!("the {what} file cannot be read: {error}")))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|_| Verdict::Input(format!("the {what} file is not base64")))?;
    String::from_utf8(bytes).map_err(|_| Verdict::Input(format!("the {what} is not text")))
}
