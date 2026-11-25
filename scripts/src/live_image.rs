//! Build a compressed disk-backed root and a self-contained rescue bootstrap.
//!
//! Production preserves rootfs ownership. Private tests may normalize ownership
//! because their package trees are extracted without host root privileges.

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::ctx::{sudo, Ctx};

/// Publish both boot artifacts from the already staged root filesystem.
pub fn build(ctx: &Ctx, root: &Path, init: &Path, privileged: bool) -> Result<()> {
    squashfs(ctx, root, privileged)?;
    bootstrap(ctx, root, init)
}

/// Compress the root in place on disk; it is never copied into the initramfs.
fn squashfs(ctx: &Ctx, root: &Path, privileged: bool) -> Result<()> {
    let image = ctx.root_image();
    let temporary = image.with_extension("partial");
    ctx.remove(&temporary)?;
    let mut command = if privileged {
        sudo(["mksquashfs"])
    } else {
        Command::new("mksquashfs")
    };
    command.arg(root).arg(&temporary).args([
        "-noappend",
        "-comp",
        "zstd",
        "-b",
        "1M",
        "-no-progress",
        "-processors",
        "4",
    ]);
    if !privileged {
        command.arg("-all-root");
    }
    ctx.run(&mut command)?;
    if !ctx.dry_run {
        fs::rename(temporary, &image)?;
    }
    println!("SquashFS root: {}", image.display());
    Ok(())
}

/// The bootstrap contains only static Rust early/init, static BusyBox and identity.
pub fn bootstrap(ctx: &Ctx, root: &Path, init: &Path) -> Result<()> {
    let stage = ctx.out.join("bootstrap");
    ctx.remove(&stage)?;
    ctx.mkdir(&stage)?;
    if !ctx.dry_run {
        stage_bootstrap(&stage, root, init)?;
    }
    ctx.run_to_file(
        Command::new("bsdtar")
            .args([
                "--format", "newc", "--uid", "0", "--gid", "0", "--zstd", "-cf", "-", "-C",
            ])
            .arg(&stage)
            .arg("."),
        &ctx.initramfs(),
    )?;
    if !ctx.dry_run {
        let size = fs::metadata(ctx.initramfs())?.len();
        if size > 32 * 1024 * 1024 {
            bail!("bootstrap unexpectedly exceeds 32 MiB ({size} bytes)");
        }
        println!(
            "bootstrap initramfs: {} ({} KiB)",
            ctx.initramfs().display(),
            size / 1024
        );
    }
    Ok(())
}

/// Include rescue independently of the compressed root, with no shared libraries.
fn stage_bootstrap(stage: &Path, root: &Path, init: &Path) -> Result<()> {
    for directory in [
        "usr/bin",
        "usr/lib/aftermath/rescue",
        "etc/aftermath",
        "dev",
        "proc",
        "sys",
        "run",
        "tmp",
        "mnt",
        "root",
        "aftermath",
    ] {
        fs::create_dir_all(stage.join(directory))?;
    }
    for (source, target) in [
        (init.to_path_buf(), "usr/bin/aftermath-init"),
        (
            init.with_file_name("aftermath-early"),
            "usr/lib/aftermath/aftermath-early",
        ),
        (
            root.join("usr/lib/aftermath/rescue/busybox"),
            "usr/lib/aftermath/rescue/busybox",
        ),
    ] {
        fs::copy(&source, stage.join(target))
            .with_context(|| format!("bootstrap {}", source.display()))?;
        fs::set_permissions(stage.join(target), fs::Permissions::from_mode(0o755))?;
    }
    for name in ["init", "poweroff", "reboot", "halt"] {
        symlink("aftermath-init", stage.join("usr/bin").join(name))?;
    }
    symlink(
        "../lib/aftermath/rescue/busybox",
        stage.join("usr/bin/busybox"),
    )?;
    for (name, target) in [
        ("bin", "usr/bin"),
        ("sbin", "usr/bin"),
        ("lib", "usr/lib"),
        ("lib64", "usr/lib"),
    ] {
        symlink(target, stage.join(name))?;
    }
    for name in ["build-id", "version"] {
        fs::copy(
            root.join("etc/aftermath").join(name),
            stage.join("etc/aftermath").join(name),
        )?;
    }
    fs::write(
        stage.join("etc/aftermath/live-root"),
        "SquashFS bootstrap v1\n",
    )?;
    Ok(())
}
