//! `cargo am check-deps`: which host tools are there, which are missing.

use std::path::Path;

use anyhow::Result;

use crate::ctx::which;

enum Probe {
    Bin(&'static str),
    /// Any of these files exists.
    File(&'static [&'static str]),
}

struct Dep {
    probe: Probe,
    package: &'static str,
    used_for: &'static str,
    required: bool,
}

const fn bin(b: &'static str, package: &'static str, used_for: &'static str) -> Dep {
    Dep {
        probe: Probe::Bin(b),
        package,
        used_for,
        required: true,
    }
}
const fn file(f: &'static [&'static str], package: &'static str, used_for: &'static str) -> Dep {
    Dep {
        probe: Probe::File(f),
        package,
        used_for,
        required: true,
    }
}
const fn optional(mut d: Dep) -> Dep {
    d.required = false;
    d
}

pub const OVMF_CODE: &[&str] = &[
    "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
    "/usr/share/edk2-ovmf/x64/OVMF_CODE.4m.fd",
    "/usr/share/OVMF/x64/OVMF_CODE.4m.fd",
];
pub const OVMF_VARS: &[&str] = &[
    "/usr/share/edk2/x64/OVMF_VARS.4m.fd",
    "/usr/share/edk2-ovmf/x64/OVMF_VARS.4m.fd",
    "/usr/share/OVMF/x64/OVMF_VARS.4m.fd",
];

const DEPS: &[Dep] = &[
    // general
    bin("sudo", "sudo", "root steps (rootfs, initramfs)"),
    file(
        &["/usr/lib/libpam.so", "/usr/lib/x86_64-linux-gnu/libpam.so"],
        "pam",
        "Rust PAM session module (shared library)",
    ),
    // kernel
    bin("make", "make", "kernel"),
    bin("gcc", "gcc", "kernel"),
    bin("flex", "flex", "kernel"),
    bin("bison", "bison", "kernel"),
    bin("bc", "bc", "kernel"),
    bin("perl", "perl", "kernel"),
    file(&["/usr/include/libelf.h"], "libelf", "kernel (objtool)"),
    file(
        &["/usr/include/openssl/ssl.h"],
        "openssl",
        "kernel (module signing)",
    ),
    optional(bin("pahole", "pahole", "kernel (BTF, optional)")),
    // rootfs
    bin("pacstrap", "arch-install-scripts", "rootfs"),
    bin("pacman", "pacman", "rootfs"),
    file(
        &["/usr/share/pacman/keyrings/archlinux.gpg"],
        "archlinux-keyring",
        "rootfs",
    ),
    // initramfs
    bin("bsdtar", "libarchive", "initramfs (cpio packing)"),
    bin("zstd", "zstd", "initramfs"),
    // iso
    bin("grub-mkrescue", "grub", "iso"),
    file(
        &["/usr/lib/grub/i386-pc/modinfo.sh"],
        "grub",
        "iso (BIOS boot)",
    ),
    file(
        &["/usr/lib/grub/x86_64-efi/modinfo.sh"],
        "grub",
        "iso (UEFI boot)",
    ),
    bin("xorriso", "libisoburn", "iso"),
    bin("mformat", "mtools", "iso (UEFI boot image)"),
    // grub disk image + theme
    bin("grub-mkimage", "grub", "GRUB disk image"),
    bin("grub-mkfont", "grub", "GRUB theme fonts"),
    file(
        &["/usr/lib/grub/i386-pc/boot.img"],
        "grub",
        "GRUB disk image (BIOS)",
    ),
    bin("mcopy", "mtools", "GRUB disk image"),
    optional(file(
        &[
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        ],
        "ttf-dejavu",
        "GRUB theme fonts (fallback: plain GRUB font)",
    )),
    // strata
    optional(bin(
        "debootstrap",
        "debootstrap",
        "stratum ubuntu (Ubuntu stratum)",
    )),
    optional(file(
        &["/usr/share/keyrings/ubuntu-archive-keyring.gpg"],
        "ubuntu-keyring",
        "stratum ubuntu (archive signatures)",
    )),
    optional(bin("mkfs.ext4", "e2fsprogs", "stratum image (strata disk)")),
    optional(bin(
        "loadkeys",
        "kbd",
        "initramfs: precompiled console keymaps (plan B loader)",
    )),
    // qemu
    bin("qemu-system-x86_64", "qemu-desktop", "run"),
    bin("qemu-img", "qemu-img", "run --disk"),
    file(OVMF_CODE, "edk2-ovmf", "default UEFI boot"),
    // future
    optional(bin(
        "mksquashfs",
        "squashfs-tools",
        "future: squashfs live image",
    )),
];

pub fn check_deps() -> Result<()> {
    let mut missing_req: Vec<&str> = Vec::new();
    let mut missing_opt: Vec<&str> = Vec::new();

    println!(
        "{:<40} {:<22} {:<8} USED FOR",
        "TOOL / FILE", "PACKAGE", "STATUS"
    );
    for d in DEPS {
        let (label, found) = match d.probe {
            Probe::Bin(b) => (b.to_string(), which(b).is_some()),
            Probe::File(fs) => (fs[0].to_string(), fs.iter().any(|f| Path::new(f).exists())),
        };
        let status = match (found, d.required) {
            (true, _) => "\x1b[32mok\x1b[0m     ",
            (false, true) => "\x1b[31mMISSING\x1b[0m",
            (false, false) => "\x1b[33moptional\x1b[0m",
        };
        println!("{label:<40} {:<22} {status}  {}", d.package, d.used_for);
        if !found {
            let list = if d.required {
                &mut missing_req
            } else {
                &mut missing_opt
            };
            if !list.contains(&d.package) {
                list.push(d.package);
            }
        }
    }

    // KVM
    let kvm = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok();
    println!(
        "{:<40} {:<22} {}  {}",
        "/dev/kvm (read/write)",
        "-",
        if kvm {
            "\x1b[32mok\x1b[0m     "
        } else {
            "\x1b[33mno\x1b[0m     "
        },
        if kvm {
            "fast VMs"
        } else {
            "QEMU will fall back to slow TCG (add yourself to group kvm)"
        }
    );

    println!();
    if missing_req.is_empty() {
        println!("All required tools are installed.");
    } else {
        println!("Missing required packages. Install them with:\n");
        println!("    sudo pacman -S --needed {}", missing_req.join(" "));
    }
    if !missing_opt.is_empty() {
        println!(
            "\nOptional extras:\n\n    sudo pacman -S --needed {}",
            missing_opt.join(" ")
        );
    }
    Ok(())
}
