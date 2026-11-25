//! A separate, persistent guest disk. No host partitioning or formatting occurs.
use crate::ctx::Ctx;
use anyhow::{bail, Result};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

pub const DEFAULT_SIZE: &str = "32G";
pub const SERIAL: &str = "AFTERMATH_SYSTEM";

/// Create a thin qcow2 disk, or grow an existing disk without shrinking it.
pub fn ensure(ctx: &Ctx, size: &str) -> Result<PathBuf> {
    let capacity = crate::stratum::parse_size(size)?;
    let path = ctx.vm_dir().join("disk.qcow2");
    ctx.mkdir(&ctx.vm_dir())?;
    if !path.exists() {
        ctx.run(
            Command::new("qemu-img")
                .args(["create", "-f", "qcow2"])
                .arg(&path)
                .arg(size),
        )?;
    } else if !ctx.dry_run {
        let output = Command::new("qemu-img")
            .args(["info", "--output=json"])
            .arg(&path)
            .output()?;
        if !output.status.success() {
            bail!("cannot inspect guest disk {}", path.display());
        }
        let info: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        if info["format"] != "qcow2" || info.get("backing-filename").is_some() {
            bail!(
                "expected a standalone qcow2 guest disk at {}",
                path.display()
            );
        }
        if info["virtual-size"]
            .as_u64()
            .is_some_and(|existing| existing < capacity)
        {
            ctx.run(Command::new("qemu-img").arg("resize").arg(&path).arg(size))?;
        }
    }
    if !ctx.dry_run {
        println!(
            "guest disk: {} ({} byte file length; guest partitions are preserved)",
            path.display(),
            fs::metadata(&path)?.len()
        );
    }
    Ok(path)
}

/// A stable serial lets guest tests identify their own disposable disk safely.
pub fn attach(command: &mut Command, disk: &std::path::Path) {
    attach_named(command, disk, SERIAL);
}

/// Tests use a distinct serial so their partitioning probe rejects normal disks.
pub fn attach_named(command: &mut Command, disk: &std::path::Path, serial: &str) {
    command
        .arg("-drive")
        .arg(format!(
            "file={},format=qcow2,if=none,id=systemdisk",
            disk.display()
        ))
        .arg("-device")
        .arg(format!("virtio-blk-pci,drive=systemdisk,serial={serial}"));
}

/// Expose an HDA card even when a headless host has no sound server.
pub fn audio(command: &mut Command, backend: &str) {
    command.args([
        "-audiodev",
        &format!("driver={backend},id=audio0"),
        "-device",
        "ich9-intel-hda",
        "-device",
        "hda-output,audiodev=audio0",
    ]);
}

/// Prefer an available desktop sound server; headless hosts keep a silent card.
pub fn host_audio() -> &'static str {
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return "none";
    };
    let runtime = PathBuf::from(runtime);
    if runtime.join("pipewire-0").exists() {
        "pipewire"
    } else if runtime.join("pulse/native").exists() {
        "pa"
    } else {
        "none"
    }
}
