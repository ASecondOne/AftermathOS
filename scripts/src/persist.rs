//! The persistent host root and `cargo am reset-vm`.
//!
//! out/host-overlay.img is a small sparse ext4 disk (label AFTERMATH_HOST).
//! At boot aftermath-early stacks it as an overlayfs upper layer on top of the
//! read-only image, so `pacman -Syu`, files in /root etc. survive reboots while
//! the image itself stays clean. Missing or broken disk -> RAM root (plan B).

use std::fs;
use std::io::{BufRead, Write};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::ctx::{which, Ctx};
use crate::{grub, image, stratum};

pub const HOST_LABEL: &str = "AFTERMATH_HOST";
pub const DEFAULT_SIZE: &str = "2G";

/// Create out/host-overlay.img if it doesn't exist (no root needed: the
/// filesystem starts empty).
pub fn ensure_host_overlay(ctx: &Ctx, size: &str) -> Result<()> {
    let img = ctx.host_overlay_image();
    if img.exists() {
        return Ok(());
    }
    if which("mkfs.ext4").is_none() && !ctx.dry_run {
        bail!("mkfs.ext4 not found: sudo pacman -S --needed e2fsprogs");
    }
    println!(
        "==> create sparse {} ({size}) for the persistent host root",
        img.display()
    );
    if !ctx.dry_run {
        ctx.mkdir(&ctx.out)?;
        let f = fs::File::create(&img).with_context(|| format!("create {}", img.display()))?;
        f.set_len(stratum::parse_size(size)?)?;
    }
    let mut c = Command::new("mkfs.ext4");
    c.args(["-F", "-q", "-L", HOST_LABEL, "-E", "root_owner=0:0"])
        .arg(&img);
    if let Err(e) = ctx.run(&mut c) {
        let _ = fs::remove_file(&img);
        return Err(e);
    }
    Ok(())
}

#[derive(Args)]
pub struct ResetArgs {
    /// Don't ask for confirmation.
    #[arg(long, short)]
    pub yes: bool,
    /// Keep the Ubuntu stratum as built (no new debootstrap download); the
    /// strata disk is still rebuilt from it, dropping what the VM changed.
    #[arg(long)]
    pub keep_strata: bool,
    /// Keep out/rootfs (skip the pacstrap download); the image is repacked.
    #[arg(long)]
    pub keep_rootfs: bool,
    /// Also delete the serial logs in out/vm/logs (kept by default; the
    /// session logs from the strata disk are saved there first).
    #[arg(long)]
    pub logs: bool,
    /// Size of the new host overlay disk.
    #[arg(long, default_value = DEFAULT_SIZE)]
    pub overlay_size: String,
}

pub fn reset_vm(ctx: &Ctx, a: &ResetArgs) -> Result<()> {
    let vm = ctx.vm_dir();
    println!("\x1b[1;31mcargo am reset-vm\x1b[0m: back to a freshly built VM. This will");
    println!(
        "  - delete {} (everything saved in the VM: pacman -Syu, /root, ...)",
        ctx.host_overlay_image().display()
    );
    println!(
        "  - delete VM state: {}/OVMF_VARS.4m.fd, {}/disk.qcow2",
        vm.display(),
        vm.display()
    );
    if a.logs {
        println!("  - delete the serial logs in {}/logs", vm.display());
    } else {
        println!(
            "  - keep {}/logs (session logs from the strata disk are saved there)",
            vm.display()
        );
    }
    if a.keep_strata {
        println!("  - rebuild the strata disk from out/strata (drops apt installs made in the VM)");
    } else {
        println!(
            "  - delete out/strata + strata disk and debootstrap Ubuntu again (sudo, download)"
        );
    }
    if a.keep_rootfs {
        println!("  - keep out/rootfs, repack the initramfs (sudo)");
    } else {
        println!("  - recreate out/rootfs with pacstrap (sudo, download), repack the initramfs");
    }
    println!("  - rebuild the GRUB disk image and a fresh empty host overlay disk");
    if ctx.dry_run {
        println!("(dry run: commands below are only printed)");
    } else if !a.yes && !confirm("Continue? [y/N] ")? {
        println!("aborted, nothing changed");
        return Ok(());
    }

    // 1. Save the session logs before the strata disk goes away.
    if !a.logs {
        save_session_logs(ctx)?;
    }

    // 2. VM state.
    ctx.remove(&ctx.host_overlay_image())?;
    ctx.remove(&vm.join("OVMF_VARS.4m.fd"))?;
    ctx.remove(&vm.join("disk.qcow2"))?;
    if a.logs {
        ctx.remove(&vm.join("logs"))?;
        ctx.remove(&vm.join("serial.log"))?;
    }

    // 3. Strata.
    ctx.remove(&ctx.strata_image())?;
    let image_args = stratum::ImageArgs { size: "8G".into() };
    if a.keep_strata && ctx.strata_dir().join("ubuntu/usr").is_dir() {
        stratum::image(ctx, &image_args)?;
    } else {
        stratum::clean(ctx)?;
        stratum::ubuntu(
            ctx,
            &stratum::UbuntuArgs {
                suite: "noble".into(),
                mirror: "http://archive.ubuntu.com/ubuntu".into(),
                force: true,
                minimal: false,
                image: stratum::ImageArgs { size: "8G".into() },
            },
        )?;
        stratum::image(ctx, &image_args)?;
    }

    // 4. Host image: rootfs, initramfs, GRUB disk.
    if !a.keep_rootfs {
        image::rootfs(
            ctx,
            &image::RootfsArgs {
                force: true,
                ..Default::default()
            },
        )?;
    }
    image::initramfs(ctx)?;
    grub::build_tree(ctx)?;
    grub::build_disk(ctx, &[])?;

    // 5. Empty overlay disk.
    ensure_host_overlay(ctx, &a.overlay_size)?;
    println!("\nfresh VM ready: `cargo am run`");
    Ok(())
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

/// `debugfs rdump /logs` from the strata disk into out/vm/logs/session-<now>/.
fn save_session_logs(ctx: &Ctx) -> Result<()> {
    let img = ctx.strata_image();
    if !img.exists() || which("debugfs").is_none() {
        return Ok(());
    }
    let dest = ctx.vm_dir().join("logs").join(format!(
        "session-{}",
        chrono::Local::now().format("%Y-%m-%d_%H-%M-%S")
    ));
    ctx.mkdir(&dest)?;
    let mut c = Command::new("debugfs");
    c.arg("-R")
        .arg(format!("rdump /logs {}", dest.display()))
        .arg(&img);
    // Missing /logs (never booted with logging) is fine.
    if ctx.run(&mut c).is_err() {
        println!("(no session logs on the strata disk)");
    }
    Ok(())
}
