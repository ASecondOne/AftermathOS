//! init binary, rootfs, initramfs, ISO and clean.
//!
//! Boot design (for now): the whole rootfs is packed into one zstd cpio
//! initramfs. The kernel unpacks it into RAM and runs `rdinit=/usr/bin/init`
//! (our aftermath-init) directly, so there is no mkinitcpio, no squashfs and
//! no switch_root yet. Simple and fast to iterate on; see README "Next steps".

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::ctx::{is_stale, mtime, newest_mtime, sudo, Ctx, VERSION};
use crate::{grub, kernel};

/// Kernel command line shared by the ISO and direct QEMU boot.
/// `panic=5`: a kernel panic reboots after 5 s instead of freezing, so the GRUB
/// boot counter can pick a fallback entry.
pub const CMDLINE_BASE: &str = "rdinit=/usr/bin/init panic=5";
const INIT_TARGET: &str = "x86_64-unknown-linux-gnu";

// ---------------------------------------------------------------- init ----

/// Build aftermath-init (PID 1), aftermath-engine, aftermath-strata and strat
/// as static binaries; returns the path of aftermath-init (the rest sit next
/// to it).
pub fn build_init(ctx: &Ctx) -> Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut c = Command::new(cargo);
    c.current_dir(&ctx.root)
        .args([
            "build",
            "--release",
            "--package",
            "aftermath-init",
            "--package",
            "aftermath-engine",
            "--package",
            "aftermath-strata",
            "--package",
            "aftermath-ctl",
            "--target",
            INIT_TARGET,
        ])
        // Static glibc binary: PID 1 must not break when libraries change.
        .env("RUSTFLAGS", "-C target-feature=+crt-static");
    ctx.run(&mut c)?;
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.root.join("target"));
    Ok(target_dir.join(INIT_TARGET).join("release/aftermath-init"))
}

// -------------------------------------------------------------- rootfs ----

#[derive(Args, Default)]
pub struct RootfsArgs {
    /// Delete an existing rootfs and create it again.
    #[arg(long)]
    pub force: bool,
    /// Install packages added to config/packages.x86_64 (or -p) into the
    /// existing rootfs instead of recreating it.
    #[arg(long, conflicts_with = "force")]
    pub update: bool,
    /// Extra package(s) to install on top of config/packages.x86_64.
    #[arg(long = "package", short = 'p')]
    pub extra: Vec<String>,
}

fn rootfs_exists(ctx: &Ctx) -> bool {
    ctx.rootfs().join("usr/bin/bash").exists()
}

pub fn rootfs(ctx: &Ctx, a: &RootfsArgs) -> Result<()> {
    let root = ctx.rootfs();
    let list = ctx.config().join("packages.x86_64");
    let mut pkgs: Vec<String> = fs::read_to_string(&list)
        .with_context(|| format!("read {}", list.display()))?
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim().to_owned())
        .filter(|l| !l.is_empty())
        .collect();
    pkgs.extend(a.extra.iter().cloned());

    if rootfs_exists(ctx) && a.update {
        // pacstrap into an existing root just runs `pacman -r root -Sy ...`.
        let mut c = sudo(["pacstrap", "-c", "-C"]);
        c.arg(ctx.config().join("pacman.conf"))
            .arg(&root)
            .arg("--needed")
            .args(&pkgs);
        ctx.run(&mut c)?;
        println!("\nrootfs updated: {}", root.display());
        return Ok(());
    }
    if rootfs_exists(ctx) && !a.force {
        println!(
            "rootfs already exists at {} (--update installs new packages, --force recreates it)",
            root.display()
        );
        if is_stale(&root.join("var/lib/pacman/local"), &[mtime(&list)]) {
            println!("note: config/packages.x86_64 changed since; run `cargo am rootfs --update`");
        }
        return Ok(());
    }
    if root.exists() {
        sudo_rm_rootfs(ctx)?;
    }

    ctx.mkdir(&ctx.out)?;
    // The rootfs directory itself must be root-owned (it becomes "/").
    ctx.run(sudo(["install", "-d", "-m", "0755", "-o", "root", "-g", "root"]).arg(&root))?;
    let mut c = sudo(["pacstrap", "-c", "-K", "-C"]);
    c.arg(ctx.config().join("pacman.conf"))
        .arg(&root)
        .args(&pkgs);
    ctx.run(&mut c)?;
    println!("\nrootfs ready: {}", root.display());
    Ok(())
}

/// `sudo rm -rf out/rootfs`, with guards against deleting the wrong thing.
fn sudo_rm_rootfs(ctx: &Ctx) -> Result<()> {
    let root = ctx.rootfs();
    if !root.exists() {
        return Ok(());
    }
    let canon = root.canonicalize()?;
    if !canon.ends_with("out/rootfs") || !canon.starts_with(ctx.root.canonicalize()?) {
        bail!("refusing to delete unexpected path {}", canon.display());
    }
    let mounts = fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    let prefix = canon.to_string_lossy().into_owned();
    if mounts
        .lines()
        .any(|l| l.split(' ').nth(1).is_some_and(|m| m.starts_with(&prefix)))
    {
        bail!(
            "something is still mounted under {prefix}; unmount it first \
             (see `findmnt -R {prefix}`)"
        );
    }
    ctx.run(sudo(["rm", "-rf", "--one-file-system"]).arg(&canon))
}

// ----------------------------------------------------------- initramfs ----

pub fn initramfs(ctx: &Ctx) -> Result<()> {
    let init = build_init(ctx)?;
    pack_initramfs(ctx, &init)
}

/// Rebuild the initramfs only if init, overlay, modules or packages changed.
pub fn ensure_initramfs(ctx: &Ctx) -> Result<()> {
    let init = build_init(ctx)?;
    let inputs = [
        mtime(&init),
        mtime(&init.with_file_name("aftermath-engine")),
        mtime(&init.with_file_name("aftermath-strata")),
        mtime(&init.with_file_name("aftermath-early")),
        mtime(&init.with_file_name("strat")),
        mtime(&init.with_file_name("amctl")),
        newest_mtime(&ctx.config().join("overlay")),
        newest_mtime(&ctx.kernel_modules().join("usr/lib/modules")),
        mtime(&ctx.rootfs().join("var/lib/pacman/local")),
        newest_mtime(&grub::tree(ctx)),
        mtime(&ctx.kernel_image()),
    ];
    if is_stale(&ctx.initramfs(), &inputs) {
        pack_initramfs(ctx, &init)
    } else {
        println!("initramfs is up to date: {}", ctx.initramfs().display());
        Ok(())
    }
}

fn pack_initramfs(ctx: &Ctx, init: &Path) -> Result<()> {
    if !rootfs_exists(ctx) && !ctx.dry_run {
        bail!("no rootfs yet, run `cargo am rootfs` first (uses sudo pacstrap)");
    }
    let rootfs = ctx.rootfs();
    grub::ensure_tree(ctx)?;

    // 1. Stage everything Aftermath adds in a user-owned dir...
    let stage = ctx.overlay_stage();
    ctx.remove(&stage)?;
    println!("==> stage overlay in {}", stage.display());
    if !ctx.dry_run {
        copy_tree(&ctx.config().join("overlay"), &stage)?;
        let bin = stage.join("usr/bin");
        fs::create_dir_all(&bin)?;
        let dst = bin.join("aftermath-init");
        fs::copy(init, &dst).with_context(|| format!("copy {}", init.display()))?;
        fs::set_permissions(&dst, fs::Permissions::from_mode(0o755))?;
        for name in ["init", "poweroff", "reboot", "halt"] {
            symlink("aftermath-init", bin.join(name))?;
        }
        // The engine (supervised by PID 1) lives in /usr/lib/aftermath.
        let lib = stage.join("usr/lib/aftermath");
        fs::create_dir_all(lib.join("rescue"))?;
        let engine = init.with_file_name("aftermath-engine");
        fs::copy(&engine, lib.join("aftermath-engine"))
            .with_context(|| format!("copy {}", engine.display()))?;
        fs::set_permissions(
            lib.join("aftermath-engine"),
            fs::Permissions::from_mode(0o755),
        )?;
        // Strata: the setup service and the `strat` multicall launcher (all
        // /aftermath/cross launchers are symlinks to it).
        for name in ["aftermath-strata", "strat", "aftermath-early"] {
            let src = init.with_file_name(name);
            fs::copy(&src, lib.join(name)).with_context(|| format!("copy {}", src.display()))?;
            fs::set_permissions(lib.join(name), fs::Permissions::from_mode(0o755))?;
        }
        symlink("../lib/aftermath/strat", bin.join("strat"))?;
        // amctl, the front door (static, so it also works in BusyBox rescue).
        let amctl = init.with_file_name("amctl");
        fs::copy(&amctl, bin.join("amctl")).with_context(|| format!("copy {}", amctl.display()))?;
        fs::set_permissions(bin.join("amctl"), fs::Permissions::from_mode(0o755))?;
        stage_keymaps(ctx, &stage)?;
        for d in ["aftermath/store", "aftermath/cross"] {
            fs::create_dir_all(stage.join(d))?;
        }
        let info = stage.join("etc/aftermath");
        fs::create_dir_all(&info)?;
        fs::write(info.join("version"), format!("{VERSION}\n"))?;
        // Identifies this image build; aftermath-early warns when saved changes
        // (host overlay disk) were made on an older image.
        fs::write(
            info.join("build-id"),
            format!(
                "{VERSION}-{}\n",
                chrono::Local::now().format("%Y%m%d-%H%M%S")
            ),
        )?;
        // /boot with GRUB config + theme and the kernel, for future installs.
        let boot = stage.join("boot");
        copy_tree(&grub::tree(ctx).join("boot/grub"), &boot.join("grub"))?;
        if ctx.kernel_image().exists() {
            fs::copy(ctx.kernel_image(), boot.join("vmlinuz-aftermath"))?;
        }
        grub::write_grubenv(&boot.join("grub/grubenv"), &[])?;
    }

    // 2. ...then copy it into the root-owned rootfs as root.
    let mut cp = sudo(["cp", "-a", "--no-preserve=ownership"]);
    cp.arg(stage.join(".")).arg(&rootfs);
    ctx.run(&mut cp)?;

    // strat must be setuid root so normal users can chroot into strata (it
    // drops back to the caller's uid before running anything).
    let mut c = sudo(["chmod", "4755"]);
    c.arg(rootfs.join("usr/lib/aftermath/strat"));
    ctx.run(&mut c)?;

    // Rescue layer: a private copy of the static BusyBox from the rootfs,
    // so the rescue CLI survives a broken /usr/bin.
    if rootfs.join("usr/bin/busybox").exists() || ctx.dry_run {
        let mut c = sudo(["install", "-D", "-m", "0755"]);
        c.arg(rootfs.join("usr/bin/busybox"))
            .arg(rootfs.join("usr/lib/aftermath/rescue/busybox"));
        ctx.run(&mut c)?;
    } else {
        println!("note: busybox not in the rootfs yet; run `cargo am rootfs --update`");
    }

    // 3. Kernel modules from `cargo am kernel`, if built.
    let mods = ctx.kernel_modules();
    if mods.join("usr/lib/modules").is_dir() {
        for e in fs::read_dir(mods.join("usr/lib/modules"))?.flatten() {
            let old = rootfs.join("usr/lib/modules").join(e.file_name());
            if old.exists() {
                ctx.run(sudo(["rm", "-rf"]).arg(&old))?;
            }
        }
        let mut cp = sudo(["cp", "-a", "--no-preserve=ownership"]);
        cp.arg(mods.join(".")).arg(&rootfs);
        ctx.run(&mut cp)?;
    } else {
        println!("(no kernel modules staged yet; run `cargo am kernel`)");
    }

    // 4. Pack the rootfs as a zstd-compressed newc cpio archive.
    let mut tar = sudo([
        "bsdtar", "--format", "newc", "--zstd", "-c", "-f", "-", "-C",
    ]);
    tar.arg(&rootfs).arg(".");
    ctx.run_to_file(&mut tar, &ctx.initramfs())?;
    if !ctx.dry_run {
        let size = fs::metadata(ctx.initramfs())?.len();
        println!(
            "initramfs: {} ({} MiB)",
            ctx.initramfs().display(),
            size >> 20
        );
    }
    Ok(())
}

/// Recursively copy a user-owned tree (files, dirs, symlinks).
/// Console keymaps that are always loadable, even without kbd in the image
/// (plan B for `loadkeys`): the configured one plus a few common ones,
/// precompiled with the build host's `loadkeys -b` into
/// /usr/share/aftermath/keymaps/<name>.bmap for the engine's built-in
/// KDSKBENT loader (crates/aftermath-ctl/src/locale.rs).
fn stage_keymaps(ctx: &Ctx, stage: &Path) -> Result<()> {
    let cfg = ctx.config().join("overlay/etc/aftermath/aftermath.toml");
    let configured = fs::read_to_string(&cfg)
        .ok()
        .and_then(|t| aftermath_ctl::locale::parse(&t).ok())
        .unwrap_or_default()
        .keymap;
    let mut names = vec![configured, "de-latin1".into(), "us".into()];
    names.dedup();
    let dir = stage.join("usr/share/aftermath/keymaps");
    fs::create_dir_all(&dir)?;
    for name in names {
        let out = Command::new("loadkeys")
            .args(["-b", "-q", &name])
            .stdin(std::process::Stdio::null())
            .output();
        match out {
            Ok(o) if o.status.success() && o.stdout.starts_with(b"bkeymap") => {
                fs::write(dir.join(format!("{name}.bmap")), &o.stdout)?;
            }
            Ok(o) => println!(
                "note: could not precompile keymap {name}: {} (the image still uses kbd's loadkeys)",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => println!(
                "note: no `loadkeys` on the build host ({e}); install `kbd` to precompile keymaps"
            ),
        }
    }
    Ok(())
}

pub fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let e = e?;
        let (s, d) = (e.path(), dst.join(e.file_name()));
        let ft = e.file_type()?;
        if ft.is_symlink() {
            symlink(fs::read_link(&s)?, &d)?;
        } else if ft.is_dir() {
            copy_tree(&s, &d)?;
        } else {
            fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ iso ----

pub fn ensure_kernel(ctx: &Ctx) -> Result<()> {
    let fragment = ctx.config().join("kernel/aftermath.config");
    if crate::ctx::is_stale(&ctx.kernel_image(), &[crate::ctx::mtime(&fragment)]) {
        println!("kernel missing or older than config/kernel/aftermath.config: building it (this takes a while)...");
        kernel::build(ctx, &kernel::KernelArgs::default())?;
    }
    Ok(())
}

pub fn ensure_rootfs(ctx: &Ctx) -> Result<()> {
    if !rootfs_exists(ctx) {
        println!("rootfs missing, creating it with sudo pacstrap...");
        rootfs(ctx, &RootfsArgs::default())?;
    }
    Ok(())
}

pub fn iso(ctx: &Ctx) -> Result<()> {
    ensure_kernel(ctx)?;
    ensure_rootfs(ctx)?;
    ensure_initramfs(ctx)?;
    build_iso(ctx)
}

/// Rebuild the ISO if the kernel or initramfs are newer than it.
pub fn ensure_iso(ctx: &Ctx) -> Result<()> {
    iso_inputs_ready(ctx)?;
    if is_stale(
        &ctx.iso(),
        &[mtime(&ctx.kernel_image()), mtime(&ctx.initramfs())],
    ) {
        build_iso(ctx)
    } else {
        Ok(())
    }
}

fn iso_inputs_ready(ctx: &Ctx) -> Result<()> {
    ensure_kernel(ctx)?;
    ensure_rootfs(ctx)?;
    ensure_initramfs(ctx)
}

fn build_iso(ctx: &Ctx) -> Result<()> {
    let iso_root = ctx.iso_root();
    ctx.remove(&iso_root)?;
    let boot = iso_root.join("boot");
    ctx.mkdir(&boot.join("grub"))?;
    for (src, name) in [
        (ctx.kernel_image(), "vmlinuz-aftermath"),
        (ctx.initramfs(), "initramfs-aftermath.img"),
    ] {
        println!(
            "==> link {} -> {}",
            src.display(),
            boot.join(name).display()
        );
        if !ctx.dry_run {
            // Hard link if possible (the initramfs is big), copy otherwise.
            if fs::hard_link(&src, boot.join(name)).is_err() {
                fs::copy(&src, boot.join(name))?;
            }
        }
    }
    // Aftermath GRUB menu + theme + fonts (grub-mkrescue adds the modules).
    grub::ensure_tree(ctx)?;
    if !ctx.dry_run {
        copy_tree(&grub::tree(ctx).join("boot/grub"), &boot.join("grub"))?;
    }

    // grub-mkrescue builds a hybrid ISO that boots on BIOS and UEFI.
    let mut c = Command::new("grub-mkrescue");
    c.arg("-o")
        .arg(ctx.iso())
        .arg(&iso_root)
        .args(["--", "-volid", "AFTERMATH"]);
    ctx.run(&mut c)?;
    println!("\nISO: {}", ctx.iso().display());
    Ok(())
}

// ---------------------------------------------------------------- clean ----

#[derive(Args)]
pub struct CleanArgs {
    /// Also delete the kernel build (out/kernel-build, out/kernel).
    #[arg(long)]
    pub kernel: bool,
    /// Also delete VM state (UEFI vars, disk image) in out/vm.
    #[arg(long)]
    pub vm: bool,
    /// Also delete the root-owned rootfs (runs `sudo rm -rf out/rootfs`).
    #[arg(long)]
    pub rootfs: bool,
    /// Also delete out/strata and the strata disk out/strata.img (sudo).
    #[arg(long)]
    pub strata: bool,
    /// Everything above: delete all of ./out.
    #[arg(long)]
    pub all: bool,
}

pub fn clean(ctx: &Ctx, a: &CleanArgs) -> Result<()> {
    ctx.remove(&ctx.overlay_stage())?;
    ctx.remove(&ctx.iso_root())?;
    ctx.remove(&ctx.initramfs())?;
    ctx.remove(&ctx.initramfs().with_extension("partial"))?;
    ctx.remove(&ctx.iso())?;
    if a.kernel || a.all {
        ctx.remove(&ctx.kernel_build())?;
        ctx.remove(&ctx.out.join("kernel"))?;
    }
    if a.vm || a.all {
        ctx.remove(&ctx.vm_dir())?;
    }
    if a.rootfs || a.all {
        sudo_rm_rootfs(ctx)?;
    }
    if a.strata || a.all {
        crate::stratum::clean(ctx)?;
    }
    if a.all && ctx.out.exists() && !ctx.dry_run && fs::read_dir(&ctx.out)?.next().is_none() {
        fs::remove_dir(&ctx.out)?;
    }
    println!("clean done (Rust build output: use `cargo clean`)");
    Ok(())
}
