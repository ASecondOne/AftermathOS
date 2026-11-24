//! `cargo am` – the Aftermath build tool ("xtask").
//!
//! Every step is a subcommand. Steps that need root (pacstrap, writing into
//! the root-owned rootfs) call `sudo` explicitly and print the full command
//! first, so you always know what runs as root. `--dry-run` prints every
//! command without running anything.

mod ctx;
mod deps;
mod grub;
mod image;
mod kernel;
mod logs;
mod persist;
mod qemu;
mod stratum;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::ctx::Ctx;

#[derive(Parser)]
#[command(
    name = "cargo am",
    bin_name = "cargo am",
    version,
    about = "Build and boot Aftermath: kernel, rootfs, initramfs, ISO, QEMU",
    after_help = "Typical first run:\n  \
        cargo am check-deps\n  \
        cargo am kernel\n  \
        cargo am rootfs      (sudo)\n  \
        cargo am run         (sudo once to pack the initramfs; boots via GRUB)\n  \
        cargo am stratum ubuntu  (sudo, optional: Ubuntu + apt next to Arch)\n  \
        cargo am run --direct  (skip GRUB, fastest)\n  \
        cargo am iso\n\nAll outputs go to ./out (gitignored)."
)]
struct Cli {
    /// Print the commands that would run, but don't run them.
    #[arg(long, short = 'n', global = true)]
    dry_run: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check which host tools are installed and print the pacman packages
    /// for anything missing.
    CheckDeps,
    /// Build the Linux kernel from ./linux (out-of-tree, into out/kernel-build).
    Kernel(kernel::KernelArgs),
    /// Build the Aftermath init (crates/aftermath-init), statically linked.
    Init,
    /// Create the Arch rootfs in out/rootfs with pacstrap (needs sudo).
    Rootfs(image::RootfsArgs),
    /// Copy init + overlay + kernel modules into the rootfs and pack it as
    /// out/initramfs-aftermath.img (needs sudo, the rootfs is root-owned).
    Initramfs,
    /// Generate the GRUB menu, theme and fonts (out/grub), and with --disk
    /// also the BIOS+UEFI bootable disk image (no sudo needed).
    Grub(GrubArgs),
    /// Build the bootable ISO out/aftermath-<version>-x86_64.iso
    /// (builds the kernel / rootfs / initramfs first if they are missing).
    Iso,
    /// Boot Aftermath in QEMU (builds whatever is missing or outdated first).
    #[command(visible_alias = "qemu")]
    Run(qemu::RunArgs),
    /// Build, list or remove strata (Ubuntu via debootstrap, needs sudo);
    /// they go on the strata disk out/strata.img.
    #[command(subcommand)]
    Stratum(stratum::StratumCmd),
    /// Show the VM's session logs (every console + kernel log) from the
    /// strata disk, e.g. `cargo am logs`, `cargo am logs -f tty1 -b 1`.
    Logs(logs::LogsArgs),
    /// Throw away everything the VM changed and rebuild a fresh VM (asks
    /// first; --yes skips the question, -n shows what would happen).
    ResetVm(persist::ResetArgs),
    /// Delete build outputs in ./out.
    Clean(image::CleanArgs),
}

#[derive(clap::Args)]
struct GrubArgs {
    /// Also build out/aftermath-<version>-x86_64.img (needs kernel + initramfs).
    #[arg(long)]
    disk: bool,
    /// Extra KEY=VALUE for the disk's grubenv, e.g. `aftermath_test=panic` to
    /// pass `aftermath.test=panic` to every boot (testing the fallbacks).
    #[arg(long, value_name = "KEY=VALUE", requires = "disk")]
    grubenv: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let ctx = Ctx::new(cli.dry_run)?;
    match cli.cmd {
        Cmd::CheckDeps => deps::check_deps(),
        Cmd::Kernel(a) => kernel::build(&ctx, &a),
        Cmd::Init => image::build_init(&ctx).map(|_| ()),
        Cmd::Rootfs(a) => image::rootfs(&ctx, &a),
        Cmd::Initramfs => image::initramfs(&ctx),
        Cmd::Grub(a) => {
            grub::build_tree(&ctx)?;
            if a.disk {
                let env = a
                    .grubenv
                    .iter()
                    .map(|kv| {
                        kv.split_once('=')
                            .map(|(k, v)| (k.to_owned(), v.to_owned()))
                            .ok_or_else(|| anyhow::anyhow!("--grubenv expects KEY=VALUE, got {kv}"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                grub::build_disk(&ctx, &env)?;
            }
            Ok(())
        }
        Cmd::Iso => image::iso(&ctx),
        Cmd::Run(a) => qemu::run(&ctx, &a),
        Cmd::Stratum(s) => stratum::run(&ctx, &s),
        Cmd::Logs(a) => logs::run(&ctx, &a),
        Cmd::ResetVm(a) => persist::reset_vm(&ctx, &a),
        Cmd::Clean(a) => image::clean(&ctx, &a),
    }
}
