//! Package-index signature verification (minisign / Ed25519).
//!
//! A tome may sign its `index.nuon` with [minisign](https://jedisct1.github.io/minisign/): the
//! author publishes a detached `index.nuon.minisig` alongside the index and declares the matching
//! public key in `tome.rn` (`packages.signer`). Because the index already records the `sha256` of
//! every archive it lists, a valid signature over the index transitively authenticates every
//! binary package — Grimoire never needs to sign archives individually.
//!
//! Trust is established **on first use**: the key seen the first time a tome syncs is pinned into
//! the tome's install-root state, and every later sync must verify against that pinned key. See
//! `src/tome/mod.rs` for the TOFU capture/enforce flow. This module is verify-only — Grimoire
//! never holds a private key; authors sign with the standard `minisign` tool.

use anyhow::{Context, Result};
use minisign_verify::{PublicKey, Signature};

/// The conventional extension for a detached minisign signature: `index.nuon` is signed into
/// `index.nuon.minisig`.
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

/// Confirms two public keys are the same minisign key. Used to detect a tome presenting a
/// different signer than the one pinned on first use (key rotation or host compromise), which is
/// refused rather than silently trusted. Both must decode as valid keys; the comparison is over
/// the canonical (trimmed) base64, which is deterministic for a given key.
pub fn keys_match(pinned_b64: &str, presented_b64: &str) -> Result<bool> {
    PublicKey::from_base64(pinned_b64.trim()).context("decode pinned signer public key")?;
    PublicKey::from_base64(presented_b64.trim()).context("decode presented signer public key")?;
    Ok(pinned_b64.trim() == presented_b64.trim())
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
    fn keys_match_is_reflexive_and_distinguishes_keys() {
        let (a, _) = sign(b"x");
        let (b, _) = sign(b"x");
        assert!(keys_match(&a, &a).expect("same key"));
        assert!(!keys_match(&a, &b).expect("different keys"));
    }

    #[test]
    fn rejects_malformed_inputs() {
        let (pubkey, signature) = sign(b"data");
        assert!(verify(b"data", "not a signature", &pubkey).is_err());
        assert!(verify(b"data", &signature, "not-a-key").is_err());
    }
}
