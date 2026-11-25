//! GRUB: theme, fonts, grub.cfg, and a BIOS+UEFI bootable disk image built
//! without root (mtools writes the FAT partition, we write the MBR).
//!
//! Output tree: out/grub/boot/grub/{grub.cfg, fonts/, themes/aftermath/}
//! This tree is used by the disk image, the ISO, and copied into the rootfs.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::ctx::{is_stale, mtime, newest_mtime, Ctx, VERSION};
use crate::image::CMDLINE_BASE;

const GRUB_LIB: &str = "/usr/lib/grub";
const FONT_DIRS: &[&str] = &["/usr/share/fonts/TTF", "/usr/share/fonts/dejavu"];
/// (output name, TTF file, size). Names inside the .pf2 come from the TTF,
/// e.g. "DejaVu Sans Bold 48", and are what theme.txt refers to.
const FONTS: &[(&str, &str, u32)] = &[
    ("dejavu_sans_16", "DejaVuSans.ttf", 16),
    ("dejavu_sans_20", "DejaVuSans.ttf", 20),
    ("dejavu_sans_bold_48", "DejaVuSans-Bold.ttf", 48),
    ("dejavu_sans_mono_14", "DejaVuSansMono.ttf", 14),
];
/// Modules baked into the UEFI image (the rest load from /boot/grub/x86_64-efi).
const EFI_MODULES: &[&str] = &[
    "part_msdos",
    "part_gpt",
    "fat",
    "iso9660",
    "normal",
    "linux",
    "all_video",
    "efi_gop",
    "gfxterm",
    "gfxmenu",
    "png",
    "font",
    "serial",
    "terminal",
    "search",
    "search_fs_file",
    "configfile",
    "echo",
    "test",
    "reboot",
    "halt",
];
/// Modules in the BIOS core.img; must stay small (it lives in the MBR gap).
const BIOS_MODULES: &[&str] = &[
    "biosdisk",
    "part_msdos",
    "fat",
    "normal",
    "search",
    "search_fs_file",
    "configfile",
];

pub fn tree(ctx: &Ctx) -> PathBuf {
    ctx.out.join("grub")
}

pub fn disk_image(ctx: &Ctx) -> PathBuf {
    ctx.out.join(format!("aftermath-{VERSION}-x86_64.img"))
}

/// Build out/grub (cfg + theme + fonts) if anything changed.
pub fn ensure_tree(ctx: &Ctx) -> Result<()> {
    let stamp = tree(ctx).join("boot/grub/grub.cfg");
    let inputs = [
        newest_mtime(&ctx.config().join("grub")),
        mtime(&std::env::current_exe()?),
    ];
    if is_stale(&stamp, &inputs) {
        build_tree(ctx)?;
    }
    Ok(())
}

pub fn build_tree(ctx: &Ctx) -> Result<()> {
    let grub = tree(ctx).join("boot/grub");
    ctx.remove(&tree(ctx))?;
    println!("==> generate GRUB theme + config in {}", grub.display());
    if ctx.dry_run {
        return Ok(());
    }
    // theme.txt (+ anything else in config/grub) ...
    crate::image::copy_tree(&ctx.config().join("grub"), &grub)?;
    // ... generated images ...
    let theme = grub.join("themes/aftermath");
    fs::create_dir_all(&theme)?;
    write_background(&theme.join("background.png"))?;
    write_select_pixmaps(&theme)?;
    // ... fonts ...
    let fonts = grub.join("fonts");
    fs::create_dir_all(&fonts)?;
    build_fonts(ctx, &fonts)?;
    // ... and the menu.
    fs::write(grub.join("grub.cfg"), grub_cfg())?;
    Ok(())
}

fn build_fonts(ctx: &Ctx, out: &Path) -> Result<()> {
    let dir = FONT_DIRS
        .iter()
        .map(Path::new)
        .find(|d| d.join("DejaVuSans.ttf").exists());
    let Some(dir) = dir else {
        println!("warning: DejaVu fonts not found (install ttf-dejavu); using GRUB's unicode.pf2");
        fs::copy("/usr/share/grub/unicode.pf2", out.join("unicode.pf2"))?;
        return Ok(());
    };
    for (name, ttf, size) in FONTS {
        let mut c = Command::new("grub-mkfont");
        c.args(["-s", &size.to_string()])
            // Latin + punctuation + arrows/boxes: keeps the .pf2 files small.
            .args([
                "-r",
                "0x20-0x7E,0xA0-0x17F,0x2010-0x206F,0x2190-0x21FF,0x2500-0x25FF",
            ])
            .arg("-o")
            .arg(out.join(format!("{name}.pf2")))
            .arg(dir.join(ttf));
        ctx.run(&mut c)?;
    }
    Ok(())
}

// ------------------------------------------------------------- images ----
// Aftermath colours: red, purple, black.

fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) -> Result<()> {
    let f = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Best);
    enc.write_header()?.write_image_data(rgba)?;
    Ok(())
}

/// Black background, deep purple haze, a red ember glow behind the title,
/// a red-to-purple accent line and drifting red/purple embers.
/// Pure Rust, deterministic.
fn write_background(path: &Path) -> Result<()> {
    let (w, h) = (1280u32, 800u32);
    let mut px = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            // Base: black with a purple haze rising from the bottom.
            let haze = (fy - 0.35).max(0.0) / 0.65;
            let mut r = 8.0 + 22.0 * haze;
            let mut g = 2.0 + 4.0 * haze;
            let mut b = 10.0 + 42.0 * haze;
            // Red glow centred behind the title.
            let dx = (fx - 0.5) / 0.42;
            let dy = (fy - 0.18) / 0.30;
            let glow = (1.0 - (dx * dx + dy * dy)).max(0.0).powf(2.2);
            r += 150.0 * glow;
            g += 10.0 * glow;
            b += 40.0 * glow;
            // Accent line under the subtitle: red in the middle -> purple.
            let line_y = (h as f32 * 0.30) as u32;
            if y == line_y || y == line_y + 1 {
                let side = (dx.abs() / 0.9).min(1.0);
                let fade = 1.0 - side;
                r += (230.0 * (1.0 - side) + 140.0 * side) * fade;
                g += 20.0 * fade;
                b += (60.0 * (1.0 - side) + 230.0 * side) * fade;
            }
            let i = ((y * w + x) * 4) as usize;
            px[i] = r.min(255.0) as u8;
            px[i + 1] = g.min(255.0) as u8;
            px[i + 2] = b.min(255.0) as u8;
            px[i + 3] = 255;
        }
    }
    // Embers (xorshift PRNG so the image is reproducible).
    let mut s: u32 = 0x4146_5445;
    let mut rnd = || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s
    };
    for _ in 0..500 {
        let (x, y) = (rnd() % w, rnd() % h);
        let a = 50 + (rnd() % 150) as u16;
        let tint: [u16; 3] = if rnd() % 2 == 0 {
            [255, 40, 70]
        } else {
            [170, 70, 255]
        };
        let big = rnd() % 6 == 0;
        for (ox, oy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            if !big && (ox, oy) != (0, 0) {
                continue;
            }
            let (px_x, px_y) = (x + ox, y + oy);
            if px_x >= w || px_y >= h {
                continue;
            }
            let i = ((px_y * w + px_x) * 4) as usize;
            for (c, t) in tint.iter().enumerate() {
                let v = px[i + c] as u16;
                px[i + c] = (v + (t.saturating_sub(v) * a) / 255).min(255) as u8;
            }
        }
    }
    write_png(path, w, h, &px)
}

/// 9-slice pixmaps for the highlighted menu entry (red bar on the left,
/// translucent purple-red body) and the GRUB console box.
fn write_select_pixmaps(theme: &Path) -> Result<()> {
    let solid = |w: u32, h: u32, c: [u8; 4]| -> Vec<u8> { (0..w * h).flat_map(|_| c).collect() };
    write_png(
        &theme.join("select_c.png"),
        8,
        8,
        &solid(8, 8, [120, 20, 70, 150]),
    )?;
    write_png(
        &theme.join("select_w.png"),
        4,
        8,
        &solid(4, 8, [235, 30, 60, 255]),
    )?;
    write_png(
        &theme.join("select_e.png"),
        4,
        8,
        &solid(4, 8, [120, 20, 70, 150]),
    )?;
    write_png(
        &theme.join("terminal_box_c.png"),
        8,
        8,
        &solid(8, 8, [6, 2, 8, 240]),
    )?;
    Ok(())
}

// ------------------------------------------------------------ grubenv ----

/// Write a fresh 1024-byte GRUB environment block (what `grub-editenv
/// create` makes) with the boot counter reset, plus `extra` variables.
pub fn write_grubenv(path: &Path, extra: &[(String, String)]) -> Result<()> {
    let mut s = String::from("# GRUB Environment Block\nboot_success=1\nboot_fails=0\n");
    for (k, v) in extra {
        s.push_str(&format!("{k}={v}\n"));
    }
    if s.len() > 1024 {
        bail!("grubenv too large");
    }
    let mut block = s.into_bytes();
    block.resize(1024, b'#');
    fs::write(path, block).with_context(|| format!("write {}", path.display()))
}

// ------------------------------------------------------------ grub.cfg ----

pub fn grub_cfg() -> String {
    let normal = "console=ttyS0,115200 console=tty0 loglevel=4";
    let serial = "console=tty0 console=ttyS0,115200";
    let debug = "console=tty0 console=ttyS0,115200 earlyprintk=serial,ttyS0,115200 \
                 ignore_loglevel loglevel=8";
    format!(
        r#"# Aftermath OS - generated by `cargo am grub` (scripts/src/grub.rs).
# Edit the generator, not this file.

insmod part_msdos
insmod part_gpt
insmod fat
insmod iso9660
insmod all_video
insmod gfxterm
insmod png
insmod serial
insmod loadenv

# Works for the ISO, the disk image and (later) installed systems.
search --no-floppy --set=root --file /boot/vmlinuz-aftermath

serial --unit=0 --speed=115200
terminal_input console serial

# ---- Boot counting (plan B) -------------------------------------------
# Only launching Linux sets boot_success=0. aftermath-engine resets
# boot_success=1 and boot_fails=0 once the system is up. On return to GRUB,
# consume an unconfirmed attempt once and pick a safer default:
#   0-1 failed boots -> Aftermath OS
#   2-3 failed boots -> Arch host only (no strata)
#   4+  failed boots -> BusyBox rescue CLI
# Read-only media (ISO) have no grubenv, so this is skipped there.
set boot_fails=0
set aftermath_test=""
set aftermath_next=""
if [ -f /boot/grub/grubenv ]; then
    load_env -f /boot/grub/grubenv boot_success boot_fails aftermath_test aftermath_next
    if [ -z "${{boot_fails}}" ]; then set boot_fails=0; fi
    if [ "${{boot_success}}" = "0" ]; then
        if [ "${{boot_fails}}" = "0" ]; then set boot_fails=1
        elif [ "${{boot_fails}}" = "1" ]; then set boot_fails=2
        elif [ "${{boot_fails}}" = "2" ]; then set boot_fails=3
        else set boot_fails=4
        fi
        # Acknowledge this attempt now. Visiting/rebooting the menu again
        # must not count the same failure twice or clear genuine failures.
        set boot_success=1
        save_env -f /boot/grub/grubenv boot_success boot_fails
    fi
fi

# Called only after both kernel and initramfs loaded successfully. Reboot,
# poweroff, firmware setup and simply waiting in the menu never arm it.
function aftermath_boot_attempt {{
    if [ -f /boot/grub/grubenv ]; then
        set boot_success=0
        set aftermath_next=""
        save_env -f /boot/grub/grubenv boot_success aftermath_next
    fi
}}

set default=aftermath
if [ "${{boot_fails}}" = "4" ]; then
    set default=aftermath-busybox
elif [ "${{boot_fails}}" = "2" ]; then
    set default=aftermath-host
elif [ "${{boot_fails}}" = "3" ]; then
    set default=aftermath-host
fi
# One-shot choice from `amctl boot next host|busybox|clean|...`: used for
# the next Linux launch only. Menu-only reboots keep the choice pending.
if [ -n "${{aftermath_next}}" ]; then
    set default="${{aftermath_next}}"
fi
set timeout=5
set timeout_style=menu

set aftermath_args="aftermath.bootfails=${{boot_fails}}"
if [ -n "${{aftermath_test}}" ]; then
    set aftermath_args="${{aftermath_args}} aftermath.test=${{aftermath_test}}"
fi
export aftermath_args

if loadfont /boot/grub/fonts/dejavu_sans_16.pf2 ; then
    loadfont /boot/grub/fonts/dejavu_sans_20.pf2
    loadfont /boot/grub/fonts/dejavu_sans_bold_48.pf2
    loadfont /boot/grub/fonts/dejavu_sans_mono_14.pf2
    set gfxmode=1280x800,1024x768,auto
    terminal_output gfxterm serial
    set theme=/boot/grub/themes/aftermath/theme.txt
    export theme
else
    terminal_output console serial
fi

menuentry "Aftermath OS" --id aftermath --class aftermath {{
    echo "Loading Aftermath {VERSION} ..."
    if linux /boot/vmlinuz-aftermath {CMDLINE_BASE} {normal} ${{aftermath_args}}; then
        if initrd /boot/initramfs-aftermath.img; then
            aftermath_boot_attempt
            boot
        fi
    fi
}}

menuentry "Aftermath OS (serial console)" --id aftermath-serial --class aftermath {{
    if linux /boot/vmlinuz-aftermath {CMDLINE_BASE} {serial} ${{aftermath_args}}; then
        if initrd /boot/initramfs-aftermath.img; then
            aftermath_boot_attempt
            boot
        fi
    fi
}}

menuentry "Aftermath OS (Arch host only, no strata)" --id aftermath-host --class recovery {{
    if linux /boot/vmlinuz-aftermath {CMDLINE_BASE} aftermath.mode=host {normal} ${{aftermath_args}}; then
        if initrd /boot/initramfs-aftermath.img; then
            aftermath_boot_attempt
            boot
        fi
    fi
}}

menuentry "Aftermath OS (clean image, ignore saved changes)" --id aftermath-clean --class recovery {{
    if linux /boot/vmlinuz-aftermath {CMDLINE_BASE} aftermath.overlay=off {normal} ${{aftermath_args}}; then
        if initrd /boot/initramfs-aftermath.img; then
            aftermath_boot_attempt
            boot
        fi
    fi
}}

menuentry "Aftermath OS (rescue: BusyBox CLI)" --id aftermath-busybox --class recovery {{
    if linux /boot/vmlinuz-aftermath {CMDLINE_BASE} aftermath.mode=busybox {normal} ${{aftermath_args}}; then
        if initrd /boot/initramfs-aftermath.img; then
            aftermath_boot_attempt
            boot
        fi
    fi
}}

menuentry "Aftermath OS (debug: verbose kernel log)" --id aftermath-debug --class aftermath {{
    if linux /boot/vmlinuz-aftermath {CMDLINE_BASE} {debug} ${{aftermath_args}}; then
        if initrd /boot/initramfs-aftermath.img; then
            aftermath_boot_attempt
            boot
        fi
    fi
}}

if [ "$grub_platform" = "efi" ]; then
    menuentry "UEFI Firmware Settings" --id fwsetup --class efi {{
        fwsetup
    }}
fi

menuentry "Reboot" --id reboot --class restart {{
    reboot
}}

menuentry "Power off" --id poweroff --class shutdown {{
    halt
}}
"#
    )
}

// ---------------------------------------------------------- disk image ----

/// Rebuild the GRUB disk image if kernel, initramfs or GRUB files changed.
pub fn ensure_disk(ctx: &Ctx) -> Result<()> {
    ensure_tree(ctx)?;
    let inputs = [
        mtime(&ctx.kernel_image()),
        mtime(&ctx.initramfs()),
        newest_mtime(&tree(ctx)),
    ];
    if is_stale(&disk_image(ctx), &inputs) {
        build_disk(ctx, &[])?;
    }
    Ok(())
}

/// MBR disk: [MBR + GRUB core.img in the 1 MiB gap][FAT32 partition, type EF].
/// The FAT partition holds EFI/BOOT/BOOTX64.EFI, /boot/grub, kernel, initramfs.
/// `grubenv`: extra variables for the boot partition's grubenv (e.g. test hooks).
pub fn build_disk(ctx: &Ctx, grubenv: &[(String, String)]) -> Result<()> {
    for p in [ctx.kernel_image(), ctx.initramfs()] {
        if !p.exists() && !ctx.dry_run {
            bail!(
                "{} missing (run `cargo am kernel` / `cargo am initramfs`)",
                p.display()
            );
        }
    }
    let work = ctx.out.join("disk-work");
    ctx.remove(&work)?;
    ctx.mkdir(&work)?;
    let esp = work.join("esp.img");
    let efi = work.join("BOOTX64.EFI");
    let core = work.join("core.img");

    // 1. GRUB images.
    let mut c = Command::new("grub-mkimage");
    c.args(["-O", "x86_64-efi", "-p", "/boot/grub", "-o"])
        .arg(&efi)
        .args(EFI_MODULES);
    ctx.run(&mut c)?;
    let mut c = Command::new("grub-mkimage");
    c.args(["-O", "i386-pc", "-p", "(hd0,msdos1)/boot/grub", "-o"])
        .arg(&core)
        .args(BIOS_MODULES);
    ctx.run(&mut c)?;

    // 2. FAT32 partition image, filled with mtools (no mount, no root).
    let payload = [ctx.kernel_image(), ctx.initramfs()]
        .iter()
        .map(|p| fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum::<u64>();
    let esp_mib = ((payload >> 20) + 64).max(64);
    println!("==> create {} ({esp_mib} MiB)", esp.display());
    if !ctx.dry_run {
        File::create(&esp)?.set_len(esp_mib << 20)?;
    }
    let mtool = |tool: &str| {
        let mut c = Command::new(tool);
        c.arg("-i").arg(&esp);
        c
    };
    ctx.run(mtool("mformat").args(["-F", "-H", "2048", "-v", "AFTERMATH", "::"]))?;
    ctx.run(mtool("mmd").args(["::/EFI", "::/EFI/BOOT", "::/boot"]))?;
    ctx.run(mtool("mcopy").arg(&efi).arg("::/EFI/BOOT/BOOTX64.EFI"))?;
    ctx.run(
        mtool("mcopy")
            .arg("-s")
            .arg(tree(ctx).join("boot/grub"))
            .arg("::/boot/"),
    )?;
    // Writable GRUB environment (boot counter): 1024 bytes, rewritten in place.
    let env = work.join("grubenv");
    if !ctx.dry_run {
        write_grubenv(&env, grubenv)?;
    }
    ctx.run(mtool("mcopy").arg(&env).arg("::/boot/grub/grubenv"))?;
    for platform in ["i386-pc", "x86_64-efi"] {
        // Module directory so `insmod` works at runtime.
        ctx.run(
            mtool("mcopy")
                .arg("-s")
                .arg(Path::new(GRUB_LIB).join(platform))
                .arg("::/boot/grub/"),
        )?;
    }
    ctx.run(
        mtool("mcopy")
            .arg(ctx.kernel_image())
            .arg("::/boot/vmlinuz-aftermath"),
    )?;
    ctx.run(
        mtool("mcopy")
            .arg(ctx.initramfs())
            .arg("::/boot/initramfs-aftermath.img"),
    )?;

    // 3. Assemble the disk: MBR (boot.img + partition table), core.img, ESP.
    let disk = disk_image(ctx);
    println!(
        "==> assemble {} (MBR + GRUB core.img + ESP)",
        disk.display()
    );
    if !ctx.dry_run {
        assemble_mbr_disk(&disk, &core, &esp, esp_mib << 20)?;
        fs::remove_dir_all(&work)?;
        println!("disk image: {}", disk.display());
    }
    Ok(())
}

fn assemble_mbr_disk(disk: &Path, core: &Path, esp: &Path, esp_len: u64) -> Result<()> {
    const START_LBA: u64 = 2048;
    let boot_img = fs::read(Path::new(GRUB_LIB).join("i386-pc/boot.img"))
        .context("read grub i386-pc/boot.img")?;
    let core_img = fs::read(core)?;
    if boot_img.len() != 512 {
        bail!("unexpected boot.img size {}", boot_img.len());
    }
    if core_img.len() as u64 > (START_LBA - 1) * 512 {
        bail!(
            "core.img ({} bytes) does not fit in the MBR gap",
            core_img.len()
        );
    }

    let tmp = disk.with_extension("partial");
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp)?;
    f.set_len(START_LBA * 512 + esp_len)?;

    // MBR: GRUB boot code (first 440 bytes; it loads core.img from LBA 1,
    // the default kernel_sector baked into boot.img), then one partition.
    let mut mbr = [0u8; 512];
    mbr[..440].copy_from_slice(&boot_img[..440]);
    let sectors = (esp_len / 512) as u32;
    let entry = &mut mbr[446..462];
    entry[0] = 0x80; // bootable
    entry[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]); // CHS: use LBA
    entry[4] = 0xEF; // EFI System Partition (FAT)
    entry[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    entry[8..12].copy_from_slice(&(START_LBA as u32).to_le_bytes());
    entry[12..16].copy_from_slice(&sectors.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    f.write_all(&mbr)?;
    f.write_all(&core_img)?; // LBA 1..

    // ESP contents at LBA 2048.
    f.seek(SeekFrom::Start(START_LBA * 512))?;
    let mut src = File::open(esp)?;
    let mut buf = vec![0u8; 4 << 20];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n])?;
    }
    f.sync_all()?;
    fs::rename(&tmp, disk)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Two halves, both pure Rust:
    //! 1. a model of the boot counter, i.e. the rules grub.cfg implements,
    //!    walked through the scenarios that matter (menu-only reboots,
    //!    real failures, `amctl boot next`);
    //! 2. checks that the generated grub.cfg is built the way the model
    //!    assumes (where `boot_success=0` is armed, where it is counted).
    //!
    //! The real GRUB behaviour was checked headless in QEMU; see README
    //! "Bootloader".

    use super::*;

    /// The grubenv variables GRUB and the engine share.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Env {
        boot_success: bool,
        boot_fails: u8,
        next: Option<&'static str>,
    }

    impl Env {
        /// A freshly built disk (`write_grubenv`).
        fn fresh() -> Self {
            Env {
                boot_success: true,
                boot_fails: 0,
                next: None,
            }
        }

        /// grub.cfg runs (menu shown): count an unconfirmed attempt once,
        /// then return the entry the menu would boot.
        fn menu(&mut self) -> &'static str {
            if !self.boot_success {
                self.boot_fails = (self.boot_fails + 1).min(4);
                self.boot_success = true; // counted: don't count it again
            }

            if let Some(entry) = self.next {
                return entry;
            }
            match self.boot_fails {
                0 | 1 => "aftermath",
                2 | 3 => "aftermath-host",
                _ => "aftermath-busybox",
            }
        }

        /// A menu entry loaded kernel + initramfs: `aftermath_boot_attempt`.
        fn launch(&mut self) {
            self.boot_success = false;
            self.next = None;
        }

        /// aftermath-engine: `grubenv::mark_boot_success`.
        fn engine_up(&mut self) {
            self.boot_success = true;
            self.boot_fails = 0;
        }
    }

    #[test]
    fn menu_only_reboots_are_not_failures() {
        let mut env = Env::fresh();

        for _ in 0..5 {
            assert_eq!(env.menu(), "aftermath");
            // `reboot` / "Reboot" / closing the VM in the menu: no launch.
        }
        assert_eq!(env, Env::fresh());
    }

    #[test]
    fn real_failures_still_fall_back_and_menu_visits_do_not_recount() {
        let mut env = Env::fresh();
        let mut booted = Vec::new();

        for _ in 0..5 {
            let entry = env.menu();
            env.menu(); // a menu-only reboot in between changes nothing
            booted.push(entry);
            env.launch(); // ... and the system never comes up
        }

        assert_eq!(
            booted,
            [
                "aftermath",
                "aftermath",
                "aftermath-host",
                "aftermath-host",
                "aftermath-busybox"
            ]
        );

        // One good boot (even in plan B) resets the counter.
        env.engine_up();
        assert_eq!(env.menu(), "aftermath");
    }

    #[test]
    fn boot_next_survives_menu_reboots_and_is_used_once() {
        let mut env = Env::fresh();
        env.next = Some("aftermath-host"); // amctl boot next host

        assert_eq!(env.menu(), "aftermath-host");
        assert_eq!(env.menu(), "aftermath-host", "kept over a menu reboot");

        env.launch();
        env.engine_up();
        assert_eq!(env.menu(), "aftermath", "used up by the launch");
    }

    /// The text of `function aftermath_boot_attempt { ... }`.
    fn attempt_function(cfg: &str) -> &str {
        let start = cfg.find("function aftermath_boot_attempt").unwrap();
        let end = start + cfg[start..].find("\n}\n").unwrap();
        &cfg[start..end]
    }

    #[test]
    fn only_a_launch_arms_the_counter() {
        let cfg = grub_cfg();
        let attempt = attempt_function(&cfg);

        // `boot_success=0` and clearing boot-next happen only in the function.
        assert_eq!(cfg.matches("set boot_success=0").count(), 1);
        assert!(attempt.contains("set boot_success=0"));
        assert_eq!(cfg.matches("set aftermath_next=\"\"").count(), 2);
        assert!(attempt.contains("set aftermath_next=\"\""));
        assert!(attempt.contains("save_env"));

        // Counting marks the attempt as counted (no double counting).
        let counting = &cfg[cfg.find("# ---- Boot counting").unwrap()..];
        let counting = &counting[..counting.find("function aftermath_boot_attempt").unwrap()];
        assert!(counting.contains("set boot_success=1"));
        assert!(counting.contains("save_env -f /boot/grub/grubenv boot_success boot_fails"));
    }

    #[test]
    fn every_linux_entry_arms_after_loading_and_before_booting() {
        let cfg = grub_cfg();
        let entries: Vec<&str> = cfg
            .split("menuentry ")
            .skip(1)
            .filter(|e| e.contains("linux /boot/vmlinuz"))
            .collect();
        assert_eq!(entries.len(), 6);

        for entry in entries {
            let linux = entry.find("if linux ").expect("linux is checked");
            let initrd = entry.find("if initrd ").expect("initrd is checked");
            let arm = entry
                .find("aftermath_boot_attempt")
                .expect("arms the counter");
            let boot = entry.find("boot\n").expect("boots explicitly");
            assert!(linux < initrd && initrd < arm && arm < boot, "{entry}");
        }

        // Reboot / Power off entries never arm it.
        for id in ["--id reboot", "--id poweroff"] {
            let at = cfg.find(id).unwrap();
            let entry = &cfg[at..at + cfg[at..].find('}').unwrap()];
            assert!(!entry.contains("aftermath_boot_attempt"));
        }
    }
}
