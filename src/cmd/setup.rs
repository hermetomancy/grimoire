//! Set up the fixed Grimoire store directory (`/grm` on POSIX systems).
//!
//! When `GRIMOIRE_ROOT` is set, the store lives under the install root and no system-wide setup
//! is needed. Otherwise this command creates the fixed store directory that makes baked absolute
//! paths portable across users and machines.

use anyhow::{Context, Result, bail};
use std::{env, fs, os::unix::ffi::OsStrExt, path::Path};

use crate::util::paths;

pub fn setup(args: crate::cli::SetupArgs) -> Result<()> {
    if env::var_os("GRIMOIRE_ROOT").is_some() {
        let root = paths::install_root()?;
        crate::util::output::note(&format!(
            "GRIMOIRE_ROOT is set; using {} as the store root. No system-wide setup needed.",
            root.display()
        ));
        ensure_profile_on_path(args.dry_run)?;
        if args.bootstrap {
            crate::util::output::note(
                "`grm setup --bootstrap` is skipped when GRIMOIRE_ROOT is set; configure tomes \
                 and install grimoire inside that root explicitly",
            );
        }
        return Ok(());
    }
    if args.dry_run {
        #[cfg(target_os = "macos")]
        crate::util::output::note(
            "would register /grm in /etc/synthetic.conf (requires sudo) and prompt for a \
             reboot so macOS creates the synthetic root directory",
        );
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        crate::util::output::note(
            "would create /grm (requires sudo) and chown it to the current user",
        );
        ensure_profile_on_path(true)?;
        if args.bootstrap {
            crate::util::output::note(&format!(
                "would then add the {CORE_TOME_URL} and {WORLD_TOME_URL} tomes and install \
                 grimoire through itself"
            ));
        } else {
            crate::util::output::note(
                "would leave bootstrap untouched; run `grm setup --bootstrap` to add tomes and \
                 install grimoire through itself",
            );
        }
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    setup_macos()?;

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    setup_linux()?;

    ensure_profile_on_path(false)?;
    if args.bootstrap {
        bootstrap_core()?;
    }
    Ok(())
}

/// Puts the active profile's `bin` on PATH by appending one line to the invoking shell's
/// rc file — zsh, bash, and fish are recognised via `$SHELL`; anything else gets the line
/// printed for manual setup. Idempotent: an rc file that already mentions the profile bin
/// is left alone, as is a session whose PATH already contains it.
fn ensure_profile_on_path(dry_run: bool) -> Result<()> {
    let bin = crate::profile::current_profile_link()?.join("bin");
    if env::var_os("PATH")
        .map(|path| env::split_paths(&path).any(|entry| entry == bin))
        .unwrap_or(false)
    {
        return Ok(());
    }

    let home = env::var("HOME").context("HOME is not set; cannot locate a shell rc file")?;
    let line = path_line(&shell_name(), &display_with_home(&bin, &home));
    let Some(rc) = rc_file(&shell_name(), Path::new(&home)) else {
        crate::util::output::note("add the profile bin to your shell's PATH:");
        crate::util::output::note(&format!("  {line}"));
        return Ok(());
    };

    if fs::read_to_string(&rc)
        .map(|content| content.contains("profiles/current/bin"))
        .unwrap_or(false)
    {
        return Ok(());
    }
    if dry_run {
        crate::util::output::note(&format!("would append to {}: {line}", rc.display()));
        return Ok(());
    }
    if let Some(parent) = rc.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create rc directory {}", parent.display()))?;
    }
    let mut content = fs::read_to_string(&rc).unwrap_or_default();
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str("\n# grimoire: active profile bin\n");
    content.push_str(&line);
    content.push('\n');
    fs::write(&rc, content).with_context(|| format!("append PATH line to {}", rc.display()))?;
    crate::util::output::report(&format!(
        "added the profile bin to PATH in {}; restart your shell (or `source` it) to use it",
        rc.display()
    ));
    Ok(())
}

/// The basename of `$SHELL`, lowercased; empty when unset.
fn shell_name() -> String {
    env::var("SHELL")
        .ok()
        .and_then(|shell| {
            shell
                .rsplit('/')
                .next()
                .map(|name| name.to_ascii_lowercase())
        })
        .unwrap_or_default()
}

/// The rc file setup appends the PATH line to, per shell. `None` for shells we do not
/// recognise — the caller prints the line for manual setup instead of guessing.
fn rc_file(shell: &str, home: &Path) -> Option<std::path::PathBuf> {
    match shell {
        "zsh" => Some(home.join(".zshrc")),
        "bash" => Some(home.join(".bashrc")),
        "fish" => Some(home.join(".config/fish/conf.d/grimoire.fish")),
        _ => None,
    }
}

/// The PATH line in the shell's own dialect.
fn path_line(shell: &str, bin: &str) -> String {
    match shell {
        "fish" => format!("fish_add_path --global \"{bin}\""),
        _ => format!("export PATH=\"{bin}:$PATH\""),
    }
}

/// Renders `path` with the home directory abbreviated to `$HOME`, so the rc line stays
/// portable across machines sharing dotfiles.
fn display_with_home(path: &Path, home: &str) -> String {
    let rendered = path.display().to_string();
    match rendered.strip_prefix(home) {
        Some(rest) => format!("$HOME{rest}"),
        None => rendered,
    }
}

const CORE_TOME_URL: &str = "https://github.com/hermetomancy/tome-core";
const WORLD_TOME_URL: &str = "https://github.com/hermetomancy/tome-world";

/// The dogfooding tail of `grm setup`: once the store is usable, ensure the core and world tomes
/// are configured and install `grimoire` through itself. Store preparation and bootstrap are one
/// user-facing setup flow, so failures after `/grm` exists are reported as setup failures rather
/// than quiet warnings that leave a half-bootstrapped root.
fn bootstrap_core() -> Result<()> {
    let store = Path::new("/grm");
    if !store.exists() || !is_writable(store)? {
        // The macOS first run ends here: /grm appears after the reboot, and the rerun of
        // `grm setup` the instructions ask for performs the bootstrap.
        return Ok(());
    }
    // Setup itself runs lock-free (there is no install root to lock before it exists);
    // the bootstrap mutates shared state, so serialise it like any other mutation.
    let _lock = crate::util::process_lock::acquire()?;

    ensure_bootstrap_tome("core", CORE_TOME_URL)?;
    ensure_bootstrap_tome("world", WORLD_TOME_URL)?;

    let grimoire_installed = crate::install::InstalledWorld::load_default()?
        .iter()
        .any(|state| state.name == "grimoire");
    if grimoire_installed {
        return Ok(());
    }
    crate::util::output::note("installing grimoire through itself…");
    crate::install::install(crate::cli::InstallArgs {
        packages: vec!["grimoire".to_owned()],
        from_source: false,
        locked: false,
        sha256: None,
        dry_run: false,
    })
    .context("install grimoire through itself")?;
    Ok(())
}

fn ensure_bootstrap_tome(name: &str, url: &str) -> Result<()> {
    if crate::tome::load_tomes()?
        .iter()
        .any(|tome| tome.name == name)
    {
        return Ok(());
    }
    crate::util::output::note(&format!("adding the {name} tome from {url}…"));
    crate::tome::add(crate::cli::TomeAddArgs {
        git_url: url.to_owned(),
        ref_name: "main".to_owned(),
        signer: Vec::new(),
        dry_run: false,
    })
    .with_context(|| format!("add the {name} tome from {url}"))
}

fn setup_posix(path: &Path) -> Result<()> {
    if path.exists() {
        if is_writable(path)? {
            crate::util::output::report(&format!(
                "Grimoire store {} is already set up.",
                path.display()
            ));
            return Ok(());
        }
        if let Some((uid, gid)) = sudo_identity() {
            chown_path(path, uid, gid)?;
            crate::util::output::report(&format!(
                "Made {} writable for the invoking user.",
                path.display()
            ));
            return Ok(());
        }
        bail!(
            "{} exists but is not writable. Run: sudo chown $(whoami): {}",
            path.display(),
            path.display()
        );
    }

    fs::create_dir_all(path)
        .with_context(|| format!("create {} (try running with sudo)", path.display()))?;

    if let Some((uid, gid)) = sudo_identity() {
        chown_path(path, uid, gid)?;
        crate::util::output::report(&format!(
            "Created {} and made it owned by the invoking user.",
            path.display()
        ));
    } else {
        crate::util::output::report(&format!("Created {} (owned by root).", path.display()));
        crate::util::output::note(&format!(
            "To make it user-writable, run: sudo chown $(whoami): {}",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn setup_linux() -> Result<()> {
    setup_posix(Path::new("/grm"))
}

#[cfg(target_os = "macos")]
fn setup_macos() -> Result<()> {
    let path = Path::new("/grm");

    if path.exists() {
        return setup_posix(path);
    }

    let synthetic = Path::new("/etc/synthetic.conf");
    let marker = "grm";

    let content = if synthetic.exists() {
        fs::read_to_string(synthetic).with_context(|| format!("read {}", synthetic.display()))?
    } else {
        String::new()
    };

    if content
        .lines()
        .any(|line| line.split_whitespace().next() == Some(marker))
    {
        bail!(
            "'{marker}' is already registered in {} but {} does not exist yet. \
             Reboot your Mac, then rerun `grm setup` if needed.",
            synthetic.display(),
            path.display()
        );
    }

    let mut new_content = content.clone();
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str("grm\n");

    let temp = synthetic.with_extension("grimoire-tmp");
    fs::write(&temp, new_content).with_context(|| format!("write temporary {}", temp.display()))?;
    fs::rename(&temp, synthetic)
        .with_context(|| format!("atomically update {}", synthetic.display()))?;

    crate::util::output::report(&format!("Added '{marker}' to {}.", synthetic.display()));
    crate::util::output::note(&format!(
        "Reboot your Mac. After reboot, {} will exist.",
        path.display()
    ));
    crate::util::output::note("Then rerun `grm setup` to adjust permissions, or run:");
    crate::util::output::note(&format!("  sudo chown $(whoami): {}", path.display()));
    Ok(())
}

/// Best-effort check whether the current process can write into `dir`.
/// Returns `false` if `dir` is a symlink to prevent following it to an arbitrary target.
fn is_writable(dir: &Path) -> Result<bool> {
    if fs::symlink_metadata(dir)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Ok(false);
    }
    let probe = dir.join(".grimoire-write-test");
    match fs::File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

/// Returns the uid/gid of the user that invoked sudo, if available.
fn sudo_identity() -> Option<(u32, u32)> {
    let uid = env::var("SUDO_UID").ok()?.parse::<u32>().ok()?;
    let gid = env::var("SUDO_GID").ok()?.parse::<u32>().ok()?;
    Some((uid, gid))
}

fn chown_path(path: &Path, uid: u32, gid: u32) -> Result<()> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("invalid path {}", path.display()))?;
    // SAFETY: lchown is a POSIX syscall; c_path is a valid NUL-terminated string.
    let rc = unsafe { libc::lchown(c_path.as_ptr(), uid, gid) };
    if rc != 0 {
        bail!(
            "lchown {} to uid {uid} gid {gid}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_writable_detects_writable_directory() {
        let temp = tempfile::tempdir().unwrap();
        assert!(is_writable(temp.path()).unwrap());
        assert!(!temp.path().join(".grimoire-write-test").exists());
    }

    #[test]
    fn is_writable_detects_non_writable_directory() {
        // A non-existent path is not writable.
        assert!(!is_writable(Path::new("/does/not/exist/.grimoire-test")).unwrap());
    }

    #[test]
    fn rc_file_per_shell_and_unknown_shells_get_none() {
        let home = Path::new("/home/u");
        assert_eq!(rc_file("zsh", home).unwrap(), home.join(".zshrc"));
        assert_eq!(rc_file("bash", home).unwrap(), home.join(".bashrc"));
        assert_eq!(
            rc_file("fish", home).unwrap(),
            home.join(".config/fish/conf.d/grimoire.fish")
        );
        assert!(rc_file("nu", home).is_none());
        assert!(rc_file("", home).is_none());
    }

    #[test]
    fn path_lines_speak_each_shells_dialect() {
        assert_eq!(
            path_line("zsh", "$HOME/.grimoire/profiles/current/bin"),
            "export PATH=\"$HOME/.grimoire/profiles/current/bin:$PATH\""
        );
        assert_eq!(
            path_line("fish", "$HOME/.grimoire/profiles/current/bin"),
            "fish_add_path --global \"$HOME/.grimoire/profiles/current/bin\""
        );
    }

    #[test]
    fn home_prefix_abbreviates_for_portable_rc_lines() {
        assert_eq!(
            display_with_home(
                Path::new("/home/u/.grimoire/profiles/current/bin"),
                "/home/u"
            ),
            "$HOME/.grimoire/profiles/current/bin"
        );
        assert_eq!(
            display_with_home(Path::new("/grm/profiles/u/bin"), "/home/u"),
            "/grm/profiles/u/bin"
        );
    }
}
