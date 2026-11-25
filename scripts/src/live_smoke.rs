//! Real GRUB/QEMU reboots on disposable disks: persistence and rescue contracts.
//! Reuse the graphical smoke's production-layout image without copying packages.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::ctx::Ctx;

/// Verify all reboot contracts using 2 GiB and either BIOS or UEFI firmware.
pub fn run(ctx: &Ctx, bios: bool) -> Result<()> {
    if ctx.dry_run {
        println!("Use private disks to test saved changes, clean boot, GRUB counting, independent BusyBox and missing-root rescue.");
        return Ok(());
    }
    let private = prepare(ctx, bios)?;
    let mut guest = launch(&private, bios)?;
    let output = guest.0.stdout.take().context("no serial stream")?;
    let mut serial = Serial::new(output, &private.out.join("serial.log"))?;
    for phase in [
        "save",
        "verify",
        "clean",
        "restore",
        "unconfirmed",
        "counted",
        "rescue",
        "missing",
    ] {
        let rescue = matches!(phase, "rescue" | "missing");
        // Ignore a dying shell's prompt until GRUB launches the next kernel.
        serial.wait_for("Booting `")?;
        serial.wait_for(if rescue { "aftermath-rescue# " } else { "]# " })?;
        let media = if rescue {
            "/mnt/vda1"
        } else {
            "/aftermath/image/media"
        };
        writeln!(
            guest.0.stdin.as_mut().context("no serial input")?,
            "{media}/boot/live-smoke {phase}"
        )?;
        serial.wait_for(&format!("AM_LIVE_{}_OK", phase.to_uppercase()))?;
        println!(
            "Live boot smoke: {phase} passed ({})",
            if bios { "BIOS" } else { "UEFI" }
        );
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while guest.0.try_wait()?.is_none() {
        if Instant::now() > deadline {
            bail!("guest failed to power off");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("Live boot smoke passed: persistence, whiteouts, clean boot, GRUB acknowledgement/counting, BusyBox and missing-root rescue (2 GiB).");
    Ok(())
}

/// Make a new boot disk; only immutable kernel/image inputs are hard-linked.
fn prepare(ctx: &Ctx, bios: bool) -> Result<Ctx> {
    let source = Ctx {
        root: ctx.root.clone(),
        out: ctx.out.join("vm/desktop-smoke/boot"),
        dry_run: false,
    };
    if !source.root_image().is_file() {
        bail!("run `cargo am smoke-desktop --grub` first to build the private SquashFS fixture");
    }
    let private = Ctx {
        root: ctx.root.clone(),
        out: ctx.out.join(if bios {
            "vm/live-smoke-bios"
        } else {
            "vm/live-smoke-uefi"
        }),
        dry_run: false,
    };
    ctx.remove(&private.out)?;
    fs::create_dir_all(
        private
            .kernel_image()
            .parent()
            .context("no kernel directory")?,
    )?;
    for (src, dst) in [
        (source.root_image(), private.root_image()),
        (source.initramfs(), private.initramfs()),
        (source.kernel_image(), private.kernel_image()),
    ] {
        fs::hard_link(src, dst)?;
    }
    let probe = build_probe(ctx)?;
    crate::grub::build_tree(&private)?;
    crate::grub::build_disk(
        &private,
        &[
            ("aftermath_test".into(), "live-smoke".into()),
            ("aftermath_next".into(), "aftermath-console".into()),
        ],
    )?;
    ctx.run(
        Command::new("mcopy")
            .arg("-i")
            .arg(format!(
                "{}@@1048576",
                crate::grub::disk_image(&private).display()
            ))
            .arg(probe)
            .arg("::/boot/live-smoke"),
    )?;
    crate::persist::ensure_host_overlay(&private, "512M")?;
    Ok(private)
}

/// Compile the probe statically so bootstrap rescue can execute it too.
fn build_probe(ctx: &Ctx) -> Result<PathBuf> {
    ctx.run(
        Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .current_dir(&ctx.root)
            .args([
                "build",
                "--release",
                "-p",
                "aftermath-init",
                "--example",
                "live-smoke",
                "--target",
                "x86_64-unknown-linux-gnu",
            ])
            .env("RUSTFLAGS", "-C target-feature=+crt-static"),
    )?;
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.root.join("target"));
    Ok(target.join("x86_64-unknown-linux-gnu/release/examples/live-smoke"))
}

/// No saved user VM disks are attached. Reboots run inside the same QEMU process.
fn launch(ctx: &Ctx, bios: bool) -> Result<Guest> {
    let mut command = Command::new("qemu-system-x86_64");
    command.args([
        "-machine", "q35", "-m", "2G", "-smp", "2", "-display", "none", "-vga", "virtio",
        "-serial", "stdio", "-monitor", "none",
    ]);
    if !bios {
        firmware(ctx, &mut command)?;
    }
    command
        .arg("-drive")
        .arg(format!(
            "file={},format=raw,if=none,id=bootdisk",
            crate::grub::disk_image(ctx).display()
        ))
        .args(["-device", "virtio-blk-pci,drive=bootdisk,bootindex=0"])
        .arg("-drive")
        .arg(format!(
            "file={},format=raw,if=virtio",
            ctx.host_overlay_image().display()
        ))
        .args([
            "-device",
            "virtio-rng-pci",
            "-nic",
            "user,model=virtio-net-pci",
        ]);
    if OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok()
    {
        command.args(["-enable-kvm", "-cpu", "host"]);
    } else {
        command.args(["-cpu", "max"]);
    }
    Ok(Guest(
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?,
    ))
}

/// Firmware variables are private and writable; the shared code image is not.
fn firmware(ctx: &Ctx, command: &mut Command) -> Result<()> {
    let code = crate::deps::OVMF_CODE
        .iter()
        .find(|path| Path::new(path).exists())
        .context("OVMF missing")?;
    let template = crate::deps::OVMF_VARS
        .iter()
        .find(|path| Path::new(path).exists())
        .context("OVMF variables missing")?;
    let vars = ctx.out.join("OVMF_VARS.fd");
    fs::copy(template, &vars)?;
    command
        .arg("-drive")
        .arg(format!(
            "if=pflash,format=raw,unit=0,readonly=on,file={code}"
        ))
        .arg("-drive")
        .arg(format!(
            "if=pflash,format=raw,unit=1,file={}",
            vars.display()
        ));
    Ok(())
}

/// Kill a failed test guest before releasing its private disk files.
struct Guest(Child);
impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Consume serial markers once, retaining trailing output for the next phase.
struct Serial {
    receiver: Receiver<Vec<u8>>,
    pending: String,
    log: PathBuf,
}
impl Serial {
    fn new(mut output: impl Read + Send + 'static, path: &Path) -> Result<Self> {
        let mut log = File::create(path)?;
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0; 8192];
            while let Ok(size) = output.read(&mut buffer) {
                if size == 0 {
                    break;
                }
                let _ = log.write_all(&buffer[..size]);
                let _ = log.flush();
                if sender.send(buffer[..size].to_vec()).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            receiver,
            pending: String::new(),
            log: path.to_owned(),
        })
    }

    /// Every wait is bounded; failures name the complete serial artifact.
    fn wait_for(&mut self, marker: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if self.pending.contains("AM_LIVE_FAILED") {
                bail!("guest assertion failed; see {}", self.log.display());
            }
            if let Some(index) = self.pending.find(marker) {
                self.pending.drain(..index + marker.len());
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("waiting for {marker}; see {}", self.log.display());
            }
            match self.receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(data) => self.pending.push_str(&String::from_utf8_lossy(&data)),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}
