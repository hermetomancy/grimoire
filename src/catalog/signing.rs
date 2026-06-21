//! Per-package signature verification (minisign / Ed25519).
//!
//! Every package in a signed tome — both its source rune (`package.rn`) and its published binary
//! archive (`archive.tar.zst`) — carries a detached `.minisig` signature. The tome's manifest
//! declares one or more minisign public keys (`signers: ["..."]`) that may sign packages. Trust
//! is established **on first use**: the key set seen the first time a tome syncs is pinned into
//! the tome's install-root state, and every later sync must present the same set. See
//! `src/tome/mod.rs` for the TOFU capture flow.
//!
//! This module is verify-only — Grimoire never holds a private key; authors sign with the
//! standard `minisign` tool.

use anyhow::{Context, Result, bail};
use minisign_verify::{PublicKey, Signature};
use std::path::Path;

/// The conventional extension for a detached minisign signature: `archive.tar.zst` is signed
/// into `archive.tar.zst.minisig`. (The index document itself is not signed — its archive
/// hashes are authenticated by each archive's own signature plus its checksum.)
pub const SIGNATURE_EXTENSION: &str = "minisig";

/// Verifies `data` against a detached minisign `signature` using `public_key_b64` — the bare
/// base64 key string (the non-comment line of a `minisign -p` public-key file), as stored in a
/// tome manifest's `packages.signer`. Returns an error with an actionable message on any failure:
/// a malformed key, a malformed signature, or a signature that does not match the data.
pub fn verify(data: &[u8], signature: &str, public_key_b64: &str) -> Result<()> {
    let public_key = PublicKey::from_base64(public_key_b64.trim())
        .map_err(|err| anyhow::anyhow!("invalid signer public key: {err}"))?;
    let signature = Signature::decode(signature)
        .map_err(|err| anyhow::anyhow!("malformed signature: {err}"))?;
    public_key
        .verify(data, &signature, false)
        .map_err(|err| anyhow::anyhow!("signature does not verify: {err}"))
}

/// Verifies `data` against `signature` using any of `pubkeys`. Returns `Ok` if at least one key
/// verifies the signature.
pub fn verify_any(data: &[u8], signature: &str, pubkeys: &[String]) -> Result<()> {
    let mut last_err = None;
    for pubkey in pubkeys {
        match verify(data, signature, pubkey) {
            Ok(()) => return Ok(()),
            Err(e) => last_err = Some(e),
        }
    }
    if let Some(e) = last_err {
        Err(e)
    } else {
        bail!("no public keys provided to verify signature")
    }
}

/// Verifies the detached signature at `{path}.minisig` against `path`'s contents using any of
/// `pubkeys`.
pub fn verify_detached(path: &Path, pubkeys: &[String]) -> Result<()> {
    let sig_path = format!("{}.{SIGNATURE_EXTENSION}", path.display());
    let signature = std::fs::read_to_string(&sig_path)
        .with_context(|| format!("read signature file {sig_path}"))?;
    let data =
        std::fs::read(path).with_context(|| format!("read file to verify {}", path.display()))?;
    verify_any(&data, &signature, pubkeys)
        .with_context(|| format!("verify signature for {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generates a throwaway keypair and signs `data`, returning `(public_key_b64, signature)`
    /// in the same shapes Grimoire consumes: the bare base64 public key and the `.minisig` text.
    fn sign(data: &[u8]) -> (String, String) {
        let keypair = minisign::KeyPair::generate_unencrypted_keypair().expect("keypair");
        let signature = minisign::sign(
            Some(&keypair.pk),
            &keypair.sk,
            std::io::Cursor::new(data),
            Some("test trusted comment"),
            Some("test"),
        )
        .expect("sign")
        .into_string();
        (keypair.pk.to_base64(), signature)
    }

    #[test]
    fn verifies_a_valid_signature() {
        let data = b"index contents";
        let (pubkey, signature) = sign(data);
        assert!(verify(data, &signature, &pubkey).is_ok());
    }

    #[test]
    fn rejects_tampered_data() {
        let (pubkey, signature) = sign(b"index contents");
        let err = verify(b"index contents (tampered)", &signature, &pubkey).unwrap_err();
        assert!(
            err.to_string().contains("does not verify"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_signature_from_a_different_key() {
        let data = b"index contents";
        let (_pubkey, signature) = sign(data);
        let (other_pubkey, _other_signature) = sign(data);
        assert!(verify(data, &signature, &other_pubkey).is_err());
    }

    #[test]
    fn rejects_malformed_inputs() {
        let (pubkey, signature) = sign(b"data");
        assert!(verify(b"data", "not a signature", &pubkey).is_err());
        assert!(verify(b"data", &signature, "not-a-key").is_err());
    }
}
