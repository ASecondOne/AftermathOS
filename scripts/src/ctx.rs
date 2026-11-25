//! Shared paths and a small command runner that prints what it executes.

use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct Ctx {
    /// Repository root (where the workspace Cargo.toml is).
    pub root: PathBuf,
    /// Build output directory: <root>/out
    pub out: PathBuf,
    pub dry_run: bool,
}

impl Ctx {
    pub fn new(dry_run: bool) -> Result<Self> {
        // scripts/Cargo.toml lives one level below the repo root.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .context("scripts crate has no parent dir")?
            .to_path_buf();
        let out = root.join("out");
        Ok(Self { root, out, dry_run })
    }

    // ---- well-known paths -------------------------------------------------
    pub fn linux_src(&self) -> PathBuf {
        self.root.join("linux")
    }
    pub fn kernel_build(&self) -> PathBuf {
        self.out.join("kernel-build")
    }
    pub fn kernel_image(&self) -> PathBuf {
        self.out.join("kernel/vmlinuz-aftermath")
    }
    /// Staging dir for `make modules_install` (contains usr/lib/modules/...).
    pub fn kernel_modules(&self) -> PathBuf {
        self.out.join("kernel/modules")
    }
    pub fn rootfs(&self) -> PathBuf {
        self.out.join("rootfs")
    }
    pub fn overlay_stage(&self) -> PathBuf {
        self.out.join("overlay")
    }
    pub fn initramfs(&self) -> PathBuf {
        self.out.join("initramfs-aftermath.img")
    }
    /// Disk-backed live root, also included in the GRUB disk and ISO.
    pub fn root_image(&self) -> PathBuf {
        self.out.join("rootfs-aftermath.squashfs")
    }
    pub fn iso_root(&self) -> PathBuf {
        self.out.join("iso-root")
    }
    pub fn iso(&self) -> PathBuf {
        self.out.join(format!("aftermath-{VERSION}-x86_64.iso"))
    }
    /// Built strata (root-owned), one dir per stratum.
    pub fn strata_dir(&self) -> PathBuf {
        self.out.join("strata")
    }
    /// Small ext4 disk (label AFTERMATH_HOST): the writable overlay layer that
    /// makes the host root persistent.
    pub fn host_overlay_image(&self) -> PathBuf {
        self.out.join("host-overlay.img")
    }
    /// The ext4 strata disk (label AFTERMATH_STRATA) attached by `run`.
    pub fn strata_image(&self) -> PathBuf {
        self.out.join("strata.img")
    }
    pub fn vm_dir(&self) -> PathBuf {
        self.out.join("vm")
    }
    pub fn config(&self) -> PathBuf {
        self.root.join("config")
    }

    // ---- running things ---------------------------------------------------

    /// Run a command, echoing it first. Fails on non-zero exit.
    pub fn run(&self, cmd: &mut Command) -> Result<()> {
        announce(cmd);
        if self.dry_run {
            return Ok(());
        }
        let st = cmd.status().with_context(|| {
            format!("failed to start `{}`", cmd.get_program().to_string_lossy())
        })?;
        if !st.success() {
            bail!("command failed ({st}): {}", show(cmd));
        }
        Ok(())
    }

    /// Run a command with stdout redirected into `path` (file owned by you,
    /// even when the command itself runs under sudo).
    pub fn run_to_file(&self, cmd: &mut Command, path: &Path) -> Result<()> {
        announce(cmd);
        println!("    > {}", path.display());
        if self.dry_run {
            return Ok(());
        }
        let tmp = path.with_extension("partial");
        let f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        let st = cmd.stdout(Stdio::from(f)).status()?;
        if !st.success() {
            let _ = std::fs::remove_file(&tmp);
            bail!("command failed ({st}): {}", show(cmd));
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Create a directory (respects --dry-run).
    pub fn mkdir(&self, p: &Path) -> Result<()> {
        if self.dry_run {
            println!("==> mkdir -p {}", p.display());
            return Ok(());
        }
        std::fs::create_dir_all(p).with_context(|| format!("mkdir {}", p.display()))
    }

    /// Remove a user-owned directory or file (respects --dry-run).
    pub fn remove(&self, p: &Path) -> Result<()> {
        if !p.exists() && p.symlink_metadata().is_err() {
            return Ok(());
        }
        println!("==> rm -rf {}", p.display());
        if self.dry_run {
            return Ok(());
        }
        if p.is_dir() {
            std::fs::remove_dir_all(p)
        } else {
            std::fs::remove_file(p)
        }
        .with_context(|| format!("remove {}", p.display()))
    }
}

/// `sudo <prog> <args...>`
pub fn sudo<I, S>(args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut c = Command::new("sudo");
    c.args(args);
    c
}

fn announce(cmd: &Command) {
    if cmd.get_program() == "sudo" {
        println!("\x1b[1;33m==> [sudo]\x1b[0m {}", show(cmd));
    } else {
        println!("\x1b[1;34m==>\x1b[0m {}", show(cmd));
    }
}

/// Render a command the way you would type it in a terminal.
pub fn show(cmd: &Command) -> String {
    let mut s = String::new();
    if let Some(d) = cmd.get_current_dir() {
        s.push_str(&format!("cd {} && ", d.display()));
    }
    for (k, v) in cmd.get_envs() {
        if let Some(v) = v {
            s.push_str(&format!("{}={} ", k.to_string_lossy(), quote(v)));
        }
    }
    s.push_str(&quote(cmd.get_program()));
    for a in cmd.get_args() {
        s.push(' ');
        s.push_str(&quote(a));
    }
    s
}

fn quote(s: &OsStr) -> String {
    let s = s.to_string_lossy();
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c));
    if safe {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Is `cmd` on $PATH?
pub fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(cmd))
        .find(|p| p.is_file())
}

pub fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::symlink_metadata(p).and_then(|m| m.modified()).ok()
}

/// Newest mtime of anything under `p` (files, dirs and symlinks).
pub fn newest_mtime(p: &Path) -> Option<SystemTime> {
    let mut best = mtime(p);
    if p.is_dir() && !p.is_symlink() {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                best = best.max(newest_mtime(&e.path()));
            }
        }
    }
    best
}

/// True if `target` is missing or older than any of `inputs`.
pub fn is_stale(target: &Path, inputs: &[Option<SystemTime>]) -> bool {
    match mtime(target) {
        None => true,
        Some(t) => inputs.iter().flatten().any(|i| *i > t),
    }
}
