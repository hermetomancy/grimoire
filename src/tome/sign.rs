//! `grm tome sign`: generate and sign a tome's trust artifacts.
//!
//! A signed tome authenticates two things (see `src/catalog/signing.rs`): its source runes, via a
//! `runes-manifest.nuon` that pins every rune's content hash and carries a detached `.minisig`;
//! and each published binary archive, via its own `.minisig`. Authoring those by hand is the
//! error-prone part — the manifest in particular has no generator — so this command writes the
//! manifest from the current `runes/` tree and signs it plus every archive in `dist/` with a
//! minisign secret key.
//!
//! Signing is the one place Grimoire touches a private key. It reads the key only to sign (an
//! encrypted key prompts for its password via minisign) and never persists it; everything else in
//! the trust path is verify-only against the tome's pinned public keys.

use anyhow::{Context, Result, anyhow, bail};
use std::{
    io::Cursor,
    path::{Path, PathBuf},
};

use nu_protocol::{Record, Span, Value};

use crate::{
    archive::archive_hash, catalog::signing, cli::TomeSignArgs, nu::nuon_io, util::output::report,
};

use super::lint::{collect_problems, rune_files};

/// The minisign manifest format `src/tome/verify.rs` expects (`format: 1`).
const RUNES_MANIFEST_FORMAT: i64 = 1;

pub fn sign(args: TomeSignArgs) -> Result<()> {
    let root = &args.path;
    if !root.join("tome.rn").exists() {
        bail!("{} is not a tome (missing tome.rn)", root.display());
    }

    // Never sign a broken tome: a bad rune would be hashed into the manifest and shipped with a
    // valid signature, so the signature would authenticate garbage. Lint first, hard-fail.
    let problems = collect_problems(root)?;
    if !problems.is_empty() {
        for p in &problems {
            crate::util::output::problem(p);
        }
        bail!(
            "refusing to sign: {} lint problem(s); run `grm tome lint` and fix them first",
            problems.len()
        );
    }

    // Load the secret key once. An encrypted key (the minisign default) prompts for its password
    // here; a passwordless key (`minisign -W`) loads silently.
    let seckey_path = args.seckey.clone().unwrap_or_else(default_seckey_path);
    let secret_key = load_secret_key(&seckey_path)?;
    let public_key = minisign::PublicKey::from_secret_key(&secret_key)
        .map_err(|e| anyhow!("derive public key from secret key: {e}"))?;
    let public_key_b64 = public_key.to_base64();

    // 1. Generate runes-manifest.nuon from the current runes/ tree.
    let manifest_path = write_runes_manifest(root)?;

    // 2. Sign the manifest and every built archive in dist/. The manifest lives at the tome root
    //    (it is committed and synced); archives live in the git-untracked dist/ publish dir.
    let mut signed = vec![sign_file(
        &manifest_path,
        &secret_key,
        &public_key,
        &public_key_b64,
    )?];
    let dist = root.join("dist");
    for archive in archives(&dist)? {
        signed.push(sign_file(
            &archive,
            &secret_key,
            &public_key,
            &public_key_b64,
        )?);
    }

    report(&format!(
        "signed {} artifact(s) with key {}",
        signed.len(),
        public_key_b64
    ));
    Ok(())
}

/// Writes `runes-manifest.nuon` at the tome root: `{ format: 1, runes: { "<file>.rn": "sha256:…" } }`,
/// the shape `verify::read_runes_manifest` reads. Returns the path written.
fn write_runes_manifest(root: &Path) -> Result<PathBuf> {
    let runes_dir = root.join("runes");
    let mut runes = Record::new();
    for path in rune_files(&runes_dir)? {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("rune path has no file name: {}", path.display()))?;
        let hash = archive_hash(&path).with_context(|| format!("hash rune {}", path.display()))?;
        runes.push(name, Value::string(hash, Span::unknown()));
    }

    let mut manifest = Record::new();
    manifest.push("format", Value::int(RUNES_MANIFEST_FORMAT, Span::unknown()));
    manifest.push("runes", Value::record(runes, Span::unknown()));

    let manifest_path = root.join("runes-manifest.nuon");
    nuon_io::write_nuon(&manifest_path, &Value::record(manifest, Span::unknown()))?;
    Ok(manifest_path)
}

/// Signs `path` into a detached `{path}.minisig`, then verifies the signature it just wrote
/// against the signing key — a cheap guard that a key/format slip can't ship an archive whose
/// signature won't verify on the install side. Returns the signature path.
fn sign_file(
    path: &Path,
    secret_key: &minisign::SecretKey,
    public_key: &minisign::PublicKey,
    public_key_b64: &str,
) -> Result<PathBuf> {
    let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let signature = minisign::sign(Some(public_key), secret_key, Cursor::new(&data), None, None)
        .map_err(|e| anyhow!("sign {}: {e}", path.display()))?;

    let sig_path = PathBuf::from(format!(
        "{}.{}",
        path.display(),
        signing::SIGNATURE_EXTENSION
    ));
    std::fs::write(&sig_path, signature.into_string())
        .with_context(|| format!("write signature {}", sig_path.display()))?;

    signing::verify_detached(path, &[public_key_b64.to_string()])
        .with_context(|| format!("self-verify signature for {}", path.display()))?;
    Ok(sig_path)
}

/// The `.tar.zst` archives directly under `dist`, sorted for stable output. An absent or empty
/// `dist/` is fine — a source-only tome signs just its runes manifest.
fn archives(dist: &Path) -> Result<Vec<PathBuf>> {
    if !dist.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dist)? {
        let path = entry?.path();
        if path.is_file()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".tar.zst"))
        {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// Loads a minisign secret key, supporting both forms: a passwordless key (`minisign -W`) loads
/// without a prompt, while an encrypted key (the default) falls through to minisign's interactive
/// password prompt. minisign's own `from_file` only handles the encrypted form, so we dispatch on
/// the unencrypted attempt first (which fails fast, without prompting, on an encrypted key).
fn load_secret_key(path: &Path) -> Result<minisign::SecretKey> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read minisign secret key {}", path.display()))?;
    let parse = || minisign::SecretKeyBox::from_string(&text);
    if let Ok(secret_key) = parse().and_then(|b| b.into_unencrypted_secret_key()) {
        return Ok(secret_key);
    }
    parse()
        .and_then(|b| b.into_secret_key(None))
        .map_err(|e| anyhow!("load minisign secret key {}: {e}", path.display()))
}

/// The minisign default secret-key location, `~/.minisign/minisign.key`. Falls back to a bare
/// relative path when the home directory is unknown, so the load error names the path.
fn default_seckey_path() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".minisign").join("minisign.key"))
        .unwrap_or_else(|| PathBuf::from(".minisign/minisign.key"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end: a throwaway key signs a scratch tome, and the manifest + archive signatures
    /// it writes verify against the matching public key. Guards the manifest shape and the
    /// sign/verify round-trip — the parts a key or format slip would silently break.
    #[test]
    fn signs_manifest_and_archives_that_verify() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("runes")).unwrap();
        std::fs::create_dir_all(root.join("dist")).unwrap();
        std::fs::write(
            root.join("tome.rn"),
            crate::tome::tome_manifest_template("scratch", "scratch tome"),
        )
        .unwrap();
        std::fs::write(
            root.join("runes/hello.rn"),
            crate::tome::rune_template("hello", "1.0.0"),
        )
        .unwrap();
        // A stand-in published archive; sign() signs the bytes verbatim, contents are irrelevant.
        std::fs::write(root.join("dist/hello-1.0.0.tar.zst"), b"not really zstd").unwrap();

        // Write an unencrypted keypair to disk in the on-file format `from_file` reads.
        let keypair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let seckey_path = root.join("minisign.key");
        std::fs::write(&seckey_path, keypair.sk.to_box(None).unwrap().into_string()).unwrap();
        let pubkey_b64 = keypair.pk.to_base64();

        sign(TomeSignArgs {
            path: root.to_path_buf(),
            seckey: Some(seckey_path),
        })
        .expect("sign");

        // The manifest and both signatures exist and verify against the public key.
        let manifest = root.join("runes-manifest.nuon");
        assert!(manifest.exists(), "runes-manifest.nuon written");
        for signed in [&manifest, &root.join("dist/hello-1.0.0.tar.zst")] {
            signing::verify_detached(signed, std::slice::from_ref(&pubkey_b64))
                .unwrap_or_else(|e| panic!("verify {}: {e}", signed.display()));
        }

        // The manifest is exactly the shape the installer's verify path reads: signature,
        // `format: 1`, every rune hash matching, and no unlisted runes.
        crate::tome::verify_runes_manifest(root, &[pubkey_b64])
            .expect("generated manifest must satisfy the installer verify path");
    }
}
