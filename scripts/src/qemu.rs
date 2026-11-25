//! `cargo am run` / `cargo am qemu`: boot Aftermath in QEMU.
//!
//! Three ways to boot:
//! * default:   the GRUB disk image (out/aftermath-<ver>-x86_64.img) through UEFI
//! * --direct:  QEMU loads kernel + initramfs itself (fastest, no GRUB)
//! * --iso:     the ISO as a CD-ROM, like real hardware
//!
//! The serial console is always logged to out/vm/logs/<timestamp>.log and
//! out/vm/serial.log points at the newest log.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Result};
use clap::{Args, FromArgMatches};

use crate::ctx::Ctx;
use crate::deps::{OVMF_CODE, OVMF_VARS};
use crate::grub;
use crate::image::{self, CMDLINE_BASE};

const DEBUG_ARGS: &str = "earlyprintk=serial,ttyS0,115200 ignore_loglevel loglevel=8";

#[derive(Args)]
pub struct RunArgs {
    /// Skip GRUB: QEMU loads kernel + initramfs directly (fastest dev loop).
    #[arg(long, conflicts_with = "iso")]
    pub direct: bool,
    /// Let the VM write to the GRUB disk image (boot counter survives
    /// between runs). By default writes are thrown away when QEMU exits.
    #[arg(long)]
    pub persist: bool,
    /// Boot the packaged ISO as a CD-ROM instead of the GRUB disk image.
    #[arg(long)]
    pub iso: bool,
    /// Use UEFI firmware (the default for normal disk/ISO boots).
    #[arg(long, conflicts_with = "bios")]
    pub uefi: bool,
    /// Use legacy BIOS instead of the default UEFI firmware.
    #[arg(long, conflicts_with = "uefi")]
    pub bios: bool,
    /// Guest RAM. The live root stays on disk; Plasma is tested with 4G.
    #[arg(long, short, default_value = "4G")]
    pub mem: String,
    /// Number of virtual CPUs.
    #[arg(long, short, default_value_t = 4)]
    pub cpus: u32,
    /// No window: serial console (GRUB menu, kernel log, root shell) in this
    /// terminal. Quit QEMU with Ctrl-A then X.
    #[arg(long, short, conflicts_with = "headless")]
    pub serial: bool,
    /// No window and no terminal: only write the serial log (for automated
    /// test boots; stop it with Ctrl-C or `timeout`).
    #[arg(long)]
    pub headless: bool,
    /// Verbose kernel log (earlyprintk, ignore_loglevel). With GRUB boots,
    /// pick the "debug" menu entry instead.
    #[arg(long)]
    pub debug: bool,
    /// Disable KVM even if /dev/kvm is usable (slow software emulation).
    #[arg(long)]
    pub no_kvm: bool,
    /// Attach a persistent virtio disk out/vm/disk.qcow2 of this size
    /// (created on first use), e.g. `--disk 20G`.
    #[arg(long, value_name = "SIZE")]
    pub disk: Option<String>,
    /// Don't attach the strata disk out/strata.img (boots in Arch host mode).
    #[arg(long)]
    pub no_strata: bool,
    /// Throw away writes to the strata disk when QEMU exits (default: what
    /// you apt install in the VM stays on out/strata.img).
    #[arg(long)]
    pub strata_snapshot: bool,
    /// Don't attach the host overlay disk: RAM root, nothing is saved.
    #[arg(long)]
    pub no_overlay: bool,
    /// Attach the host overlay disk but throw its writes away at exit.
    #[arg(long)]
    pub overlay_snapshot: bool,
    /// Extra kernel command line arguments (--direct only).
    #[arg(long, value_name = "ARGS")]
    pub append: Option<String>,
    /// Don't build anything, just boot what is already in ./out.
    #[arg(long)]
    pub no_build: bool,
    /// Extra arguments passed straight to qemu-system-x86_64 (after `--`).
    #[arg(last = true)]
    pub qemu_args: Vec<String>,
}

pub fn run(ctx: &Ctx, a: &RunArgs) -> Result<()> {
    if !a.no_build {
        image::ensure_kernel(ctx)?;
        image::ensure_rootfs(ctx)?;
        image::ensure_initramfs(ctx)?;
        if a.iso {
            image::ensure_iso(ctx)?;
        } else if !a.direct {
            grub::ensure_disk(ctx)?;
        }
    }

    let vm = ctx.vm_dir();
    ctx.mkdir(&vm)?;
    let log = new_log(ctx)?;

    let mut q = Command::new("qemu-system-x86_64");
    q.args(["-name", "Aftermath", "-machine", "q35"]);
    q.args(["-m", &a.mem, "-smp", &a.cpus.to_string()]);

    let kvm_ok = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok();
    if kvm_ok && !a.no_kvm {
        q.args(["-enable-kvm", "-cpu", "host"]);
    } else {
        if !a.no_kvm {
            println!("warning: /dev/kvm not usable, falling back to slow TCG emulation");
        }
        q.args(["-cpu", "max"]);
    }

    if uses_uefi(a) {
        let code = first_existing(OVMF_CODE)
            .ok_or_else(|| anyhow::anyhow!("OVMF not found; install edk2-ovmf"))?;
        let vars_src = first_existing(OVMF_VARS)
            .ok_or_else(|| anyhow::anyhow!("OVMF_VARS not found; install edk2-ovmf"))?;
        // UEFI variables are writable, so each VM gets its own copy.
        let vars = vm.join("OVMF_VARS.4m.fd");
        if !vars.exists() && !ctx.dry_run {
            std::fs::copy(vars_src, &vars)?;
        }
        q.arg("-drive").arg(format!(
            "if=pflash,format=raw,unit=0,readonly=on,file={code}"
        ));
        q.arg("-drive").arg(format!(
            "if=pflash,format=raw,unit=1,file={}",
            vars.display()
        ));
    }

    if a.append.is_some() && !a.direct {
        println!("note: --append only applies to --direct (in GRUB press `e` to edit an entry)");
    }
    if a.debug && !a.direct {
        println!("note: with GRUB, choose \"Aftermath OS (debug: verbose kernel log)\"");
    }

    if a.direct {
        for p in [ctx.kernel_image(), ctx.initramfs(), ctx.root_image()] {
            require(ctx, &p)?;
        }
        // Shells start on every console; the last console= is /dev/console.
        let console = if a.serial || a.headless {
            "console=tty0 console=ttyS0,115200"
        } else {
            "console=ttyS0,115200 console=tty0"
        };
        let mut cmdline = format!("{CMDLINE_BASE} {console}");
        if a.debug {
            cmdline = format!("{cmdline} {DEBUG_ARGS}");
        }
        if let Some(extra) = &a.append {
            cmdline = format!("{cmdline} {extra}");
        }
        q.arg("-kernel").arg(ctx.kernel_image());
        q.arg("-initrd").arg(ctx.initramfs());
        q.arg("-append").arg(cmdline);
        q.arg("-drive").arg(format!(
            "file={},format=raw,if=virtio,readonly=on",
            ctx.root_image().display()
        ));
    } else if a.iso {
        require(ctx, &ctx.iso())?;
        q.arg("-drive").arg(format!(
            "file={},media=cdrom,if=none,id=bootcd,readonly=on",
            ctx.iso().display()
        ));
        q.args(["-device", "ide-cd,drive=bootcd,bootindex=0"]);
    } else {
        let img = grub::disk_image(ctx);
        require(ctx, &img)?;
        // snapshot=on: writes (grubenv boot counter) last only for this QEMU
        // process, i.e. across guest reboots but not across `cargo am run`s.
        let snap = if a.persist { "off" } else { "on" };
        q.arg("-drive").arg(format!(
            "file={},format=raw,if=none,id=bootdisk,snapshot={snap}",
            img.display()
        ));
        q.args(["-device", "virtio-blk-pci,drive=bootdisk,bootindex=0"]);
    }

    if let Some(size) = &a.disk {
        let disk = vm.join("disk.qcow2");
        if !disk.exists() {
            let mut c = Command::new("qemu-img");
            c.args(["create", "-f", "qcow2"]).arg(&disk).arg(size);
            ctx.run(&mut c)?;
        }
        q.arg("-drive")
            .arg(format!("file={},if=virtio,format=qcow2", disk.display()));
    }

    // Host overlay disk: makes the host root persistent (aftermath-early).
    if a.no_overlay {
        println!("host overlay disk not attached (--no-overlay): RAM root, changes are not saved");
    } else {
        crate::persist::ensure_host_overlay(ctx, crate::persist::DEFAULT_SIZE)?;
        let snap = if a.overlay_snapshot { "on" } else { "off" };
        q.arg("-drive").arg(format!(
            "file={},format=raw,if=virtio,snapshot={snap}",
            ctx.host_overlay_image().display()
        ));
    }

    // Strata disk (Ubuntu etc.): found by its label AFTERMATH_STRATA at boot.
    let strata = ctx.strata_image();
    if a.no_strata {
        println!("strata disk not attached (--no-strata): Arch host mode");
    } else if strata.exists() {
        let snap = if a.strata_snapshot { "on" } else { "off" };
        q.arg("-drive").arg(format!(
            "file={},format=raw,if=virtio,snapshot={snap}",
            strata.display()
        ));
    } else {
        println!(
            "note: no strata disk ({}): booting without strata. Build one with `cargo am stratum ubuntu`",
            strata.display()
        );
    }

    q.args(["-nic", "user,model=virtio-net-pci"]);
    q.args(["-device", "virtio-rng-pci"]);

    // Serial port 0: always logged to out/vm/logs/<ts>.log.
    let log_s = log.display().to_string();
    if a.serial {
        // Terminal + log; QEMU monitor multiplexed on the same terminal.
        q.arg("-chardev")
            .arg(format!("stdio,id=ser0,mux=on,signal=off,logfile={log_s}"));
        q.args([
            "-serial",
            "chardev:ser0",
            "-mon",
            "chardev=ser0,mode=readline",
        ]);
        q.args(["-display", "none"]);
    } else {
        q.arg("-chardev").arg(format!("file,id=ser0,path={log_s}"));
        q.args(["-serial", "chardev:ser0"]);
        if a.headless {
            q.args(["-display", "none", "-monitor", "none"]);
        } else {
            q.args(["-vga", "virtio", "-display", "gtk"]);
            q.args(["-device", "qemu-xhci", "-device", "usb-tablet"]);
        }
    }

    q.args(&a.qemu_args);
    println!("serial log: {log_s}  (also out/vm/serial.log)");
    ctx.run(&mut q)
}

pub fn run_default(ctx: &Ctx) -> Result<()> {
    let matches = RunArgs::augment_args(clap::Command::new("run"))
        .try_get_matches_from(["run", "--no-build"])?;
    run(ctx, &RunArgs::from_arg_matches(&matches)?)
}

fn uses_uefi(args: &RunArgs) -> bool {
    args.uefi || (!args.bios && !args.direct)
}

/// out/vm/logs/<YYYY-mm-dd_HH-MM-SS>.log, and point out/vm/serial.log at it.
fn new_log(ctx: &Ctx) -> Result<PathBuf> {
    let logs = ctx.vm_dir().join("logs");
    ctx.mkdir(&logs)?;
    let name = format!("{}.log", chrono::Local::now().format("%Y-%m-%d_%H-%M-%S"));
    let log = logs.join(&name);
    if !ctx.dry_run {
        let link = ctx.vm_dir().join("serial.log");
        // Keep an old plain-file serial.log (from before per-run logs) instead of deleting it.
        if link.is_file() && !link.is_symlink() {
            std::fs::rename(&link, logs.join("old-serial.log"))?;
        }
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(Path::new("logs").join(&name), &link)?;
    }
    Ok(log)
}

fn require(ctx: &Ctx, p: &Path) -> Result<()> {
    if !p.exists() && !ctx.dry_run {
        bail!(
            "{} not found (build it first, see `cargo am --help`)",
            p.display()
        );
    }
    Ok(())
}

fn first_existing(cands: &[&'static str]) -> Option<&'static str> {
    cands.iter().copied().find(|p| Path::new(p).exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> RunArgs {
        let matches = RunArgs::augment_args(clap::Command::new("run"))
            .try_get_matches_from(args)
            .unwrap();
        RunArgs::from_arg_matches(&matches).unwrap()
    }

    #[test]
    fn desktop_boot_defaults_to_uefi_with_explicit_bios_fallback() {
        let default = parse(&["run"]);
        assert!(uses_uefi(&default));
        assert_eq!(default.mem, "4G");
        assert!(uses_uefi(&parse(&["run", "--iso"])));
        assert!(!uses_uefi(&parse(&["run", "--bios"])));
        assert!(!uses_uefi(&parse(&["run", "--direct"])));
        assert!(uses_uefi(&parse(&["run", "--direct", "--uefi"])));
        assert!(RunArgs::augment_args(clap::Command::new("run"))
            .try_get_matches_from(["run", "--bios", "--uefi"])
            .is_err());
    }
}
