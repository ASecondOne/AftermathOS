//! Download into private staging on the strata disk, validate, then publish
//! with a same-filesystem rename. No installed tree is ever overwritten.

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn install(store: &Path, name: &str, image: &str) -> Result<(), String> {
    install_with(store, name, |stage| {
        let layout = stage.join("image");
        let bundle = stage.join("bundle");
        run(Command::new("/usr/bin/skopeo")
            .args(["copy", "--retry-times", "3"])
            .arg(image)
            .arg(format!("oci:{}:stratum", layout.display())))?;
        run(Command::new("/usr/bin/umoci")
            .args(["unpack", "--image"])
            .arg(format!("{}:stratum", layout.display()))
            .arg(&bundle))
    })
}

fn install_with(
    store: &Path,
    name: &str,
    fetch: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    // This installer currently validates Fedora specifically; do not accept
    // arbitrary directory names or turn a new catalog entry into a raw import.
    if name != "fedora" {
        return Err(format!("no automatic installer for `{name}`"));
    }
    let _lock = lock(store)?;
    let target = store.join(name);
    if fs::symlink_metadata(&target).is_ok() {
        validate(&target)?;
        return Ok(()); // Re-enabling preserves all installed packages.
    }

    let path = store.join(format!(".install-{name}-{}", std::process::id()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .map_err(|e| format!("create staging {}: {e}", path.display()))?;
    let stage = Staging(path);
    fetch(&stage.0)?;
    let root = stage.0.join("bundle/rootfs");
    validate(&root)?;
    // The lock serializes installers. renameat2 also rejects an existing
    // destination (even an empty directory), including external races.
    nix::fcntl::renameat2(
        None,
        &root,
        None,
        &target,
        nix::fcntl::RenameFlags::RENAME_NOREPLACE,
    )
    .map_err(|e| format!("publish {}: {e}", target.display()))?;
    Ok(())
}

fn validate(root: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(root).map_err(|e| format!("{}: {e}", root.display()))?;
    let release = fs::read_to_string(root.join("usr/lib/os-release"))
        .or_else(|_| fs::read_to_string(root.join("etc/os-release")))
        .unwrap_or_default();
    let fedora = release.lines().any(|line| {
        line.strip_prefix("ID=")
            .is_some_and(|id| id.trim_matches('"') == "fedora")
    });
    if !meta.is_dir()
        || !fedora
        || !root.join("usr/bin/bash").is_file()
        || !root.join("usr/bin/dnf").is_file()
        || !root.join("usr/bin/rpm").is_file()
    {
        return Err(format!("{} is not a complete Fedora rootfs (requires Fedora os-release, bash, dnf and rpm); existing files were kept", root.display()));
    }
    Ok(())
}

fn lock(store: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(store.join(".install.lock"))
        .map_err(|e| format!("open strata install lock: {e}"))?;
    // SAFETY: file owns a live descriptor. Keep the lock in subprocesses too,
    // so killing the parent cannot allow another installer during unpacking.
    unsafe {
        if libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) != 0 {
            return Err(
                "another stratum installation is in progress; retry when it finishes".into(),
            );
        }
        if libc::fcntl(file.as_raw_fd(), libc::F_SETFD, 0) == -1 {
            return Err(format!(
                "inherit install lock: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(file)
}

fn run(cmd: &mut Command) -> Result<(), String> {
    let program = cmd.get_program().to_owned();
    // Bound a hung download/unpack without blocking the engine or leaving a
    // partial root in the installed location. timeout kills its process group.
    let status = Command::new("/usr/bin/timeout")
        .args(["--signal=TERM", "--kill-after=10s", "30m"])
        .arg(&program)
        .args(cmd.get_args())
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::io::stderr()))
        .status()
        .map_err(|e| {
            format!(
                "{}: {e} (host needs skopeo, umoci and coreutils)",
                program.to_string_lossy()
            )
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{} failed ({status}); check network, free strata-disk space and the messages above",
            program.to_string_lossy()
        ))
    }
}

struct Staging(PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir_all(&self.0) {
            eprintln!(
                "aftermath-strata: cannot remove staging {}: {e}",
                self.0.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn store() -> Staging {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "aftermath-install-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Staging(path)
    }

    fn fedora(stage: &Path) -> Result<(), String> {
        let root = stage.join("bundle/rootfs");
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        fs::create_dir_all(root.join("usr/lib")).unwrap();
        fs::write(root.join("usr/lib/os-release"), "ID=fedora\n").unwrap();
        for bin in ["bash", "dnf", "rpm"] {
            fs::write(root.join("usr/bin").join(bin), "test fixture").unwrap();
        }
        Ok(())
    }

    #[test]
    fn publishes_complete_tree_and_keeps_existing_install() {
        let store = store();
        install_with(&store.0, "fedora", fedora).unwrap();
        fs::write(store.0.join("fedora/keep"), "user packages").unwrap();
        install_with(&store.0, "fedora", |_| panic!("must not download twice")).unwrap();
        assert!(store.0.join("fedora/keep").exists());
        assert!(!store
            .0
            .join(format!(".install-fedora-{}", std::process::id()))
            .exists());
    }

    #[test]
    fn failed_download_and_incomplete_image_leave_no_install_and_can_retry() {
        let store = store();
        assert!(install_with(&store.0, "fedora", |_| Err("offline".into())).is_err());
        assert!(!store.0.join("fedora").exists());
        assert!(install_with(&store.0, "fedora", |stage| {
            fedora(stage)?;
            fs::remove_file(stage.join("bundle/rootfs/usr/bin/dnf")).unwrap();
            Ok(())
        })
        .is_err());
        assert!(!store.0.join("fedora").exists());
        install_with(&store.0, "fedora", fedora).unwrap();
    }

    #[test]
    fn rejects_competing_installer_and_unknown_name() {
        let store = store();
        let held = lock(&store.0).unwrap();
        assert!(install_with(&store.0, "fedora", |_| panic!()).is_err());
        drop(held);
        assert!(install_with(&store.0, "../arch", |_| panic!()).is_err());
    }

    #[test]
    fn preserves_partial_existing_tree_and_external_destination() {
        let store = store();
        fs::create_dir(store.0.join("fedora")).unwrap();
        fs::write(store.0.join("fedora/keep"), "keep").unwrap();
        assert!(install_with(&store.0, "fedora", |_| panic!()).is_err());
        assert!(store.0.join("fedora/keep").exists());
        fs::remove_dir_all(store.0.join("fedora")).unwrap();
        assert!(install_with(&store.0, "fedora", |stage| {
            fedora(stage)?;
            fs::create_dir(store.0.join("fedora")).unwrap();
            Ok(())
        })
        .is_err());
        assert!(!store.0.join("fedora/usr").exists());
    }
}
