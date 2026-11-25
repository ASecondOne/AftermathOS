//! `cargo am kernel`: out-of-tree build of ./linux into out/kernel-build.
//!
//! Config = x86_64_defconfig + kvm_guest.config + config/kernel/aftermath.config
//! The kernel source tree itself is never modified (we build with O=).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::ctx::Ctx;

#[derive(Args, Default)]
pub struct KernelArgs {
    /// Parallel make jobs (default: number of CPUs).
    #[arg(long, short)]
    pub jobs: Option<usize>,
    /// Regenerate .config from the defconfig + Aftermath fragment.
    #[arg(long)]
    pub reconfigure: bool,
    /// Open `make menuconfig` before building.
    #[arg(long)]
    pub menuconfig: bool,
}

pub fn build(ctx: &Ctx, a: &KernelArgs) -> Result<()> {
    let src = ctx.linux_src();
    if !src.join("Makefile").exists() {
        bail!(
            "no kernel source at {} (expected ./linux; it is not part of this repo, \
             see README \"Kernel source\")",
            src.display()
        );
    }
    if src.join(".config").exists() {
        bail!(
            "{} exists: the kernel source tree must be clean for out-of-tree builds.\n\
             Run `make -C linux mrproper` (it deletes the in-tree config).",
            src.join(".config").display()
        );
    }
    let build = ctx.kernel_build();
    ctx.mkdir(&build)?;
    let jobs = a.jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    });

    let make = |targets: &[&str]| {
        let mut c = Command::new("make");
        c.arg("-C").arg(&src).arg(format!("O={}", build.display()));
        c.args(targets);
        c
    };

    let config = build.join(".config");
    let fragment = ctx.config().join("kernel/aftermath.config");
    // A changed fragment means a fresh config (defconfig + fragments again).
    let fragment_changed = crate::ctx::is_stale(&config, &[crate::ctx::mtime(&fragment)]);
    if fragment_changed && config.exists() && !a.reconfigure {
        println!(
            "{} changed: regenerating the kernel config",
            fragment.display()
        );
    }
    if a.reconfigure || fragment_changed {
        ctx.run(&mut make(&["x86_64_defconfig"]))?;
        ctx.run(&mut make(&["kvm_guest.config"]))?;
        append_fragment(ctx, &config, &fragment)?;
        ctx.run(&mut make(&["olddefconfig"]))?;
    }
    if a.menuconfig {
        ctx.run(&mut make(&["menuconfig"]))?;
    }

    ctx.run(&mut make(&[&format!("-j{jobs}"), "bzImage", "modules"]))?;

    // Collect results into out/kernel/.
    let image = ctx.kernel_image();
    ctx.mkdir(image.parent().unwrap())?;
    let bz = build.join("arch/x86/boot/bzImage");
    println!("==> cp {} {}", bz.display(), image.display());
    if !ctx.dry_run {
        let temporary = image.with_extension("partial");
        std::fs::copy(&bz, &temporary).with_context(|| format!("copy {}", bz.display()))?;
        std::fs::rename(temporary, &image)?;
    }

    let mods = ctx.kernel_modules();
    ctx.remove(&mods)?;
    let mut mi = make(&["modules_install"]);
    // Arch keeps modules in /usr/lib/modules (/lib is a symlink to usr/lib).
    mi.arg(format!("INSTALL_MOD_PATH={}", mods.join("usr").display()))
        .arg("INSTALL_MOD_STRIP=1");
    ctx.run(&mut mi)?;
    // `build` symlink points back into out/kernel-build; useless in the image.
    if !ctx.dry_run {
        if let Ok(rd) = std::fs::read_dir(mods.join("usr/lib/modules")) {
            for e in rd.flatten() {
                let _ = std::fs::remove_file(e.path().join("build"));
            }
        }
    }

    println!("\nkernel: {}\nmodules: {}", image.display(), mods.display());
    Ok(())
}

fn append_fragment(ctx: &Ctx, config: &Path, fragment: &Path) -> Result<()> {
    println!("==> append {} >> {}", fragment.display(), config.display());
    if ctx.dry_run {
        return Ok(());
    }
    let text = std::fs::read_to_string(fragment)
        .with_context(|| format!("read {}", fragment.display()))?;
    let mut f = OpenOptions::new().append(true).open(config)?;
    writeln!(f, "\n# ---- {} ----", fragment.display())?;
    f.write_all(text.as_bytes())?;
    Ok(())
}
