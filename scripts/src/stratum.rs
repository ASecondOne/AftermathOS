//! `cargo am stratum ...`: build, list and remove strata.
//!
//! Strata are built into out/strata/<name> (root-owned, like out/rootfs) and
//! packed into ONE ext4 disk image, out/strata.img (label AFTERMATH_STRATA).
//! `cargo am run` attaches it as a second, writable virtio disk; at boot
//! aftermath-strata finds it by label and mounts it on /aftermath/store.
//!
//! Why a separate disk and not the initramfs: the whole initramfs is
//! unpacked into RAM, so a ~0.5-1 GB Ubuntu would cost that much RAM on every
//! boot (plus the compressed copy while unpacking), and anything apt
//! installs would be lost on reboot. A squashfs would fix the RAM cost but is
//! read-only, so apt would need an overlay on top and the upper layer would
//! again need a disk. An ext4 disk is simple, writable and persistent, and if
//! it is missing or broken the system just boots in Arch host mode (plan B).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};

use crate::ctx::{sudo, which, Ctx};

pub const DISK_LABEL: &str = "AFTERMATH_STRATA";
const UBUNTU_KEYRING: &str = "/usr/share/keyrings/ubuntu-archive-keyring.gpg";
/// Stage 1, `debootstrap --include`: only packages debootstrap's simple
/// resolver handles. It cannot resolve virtual packages or `a | b`
/// alternatives, and software-properties-common pulls in several
/// (python3-cffi-backend-api-max, default-dbus-system-bus, default-logind).
const UBUNTU_BOOTSTRAP: &[&str] = &["ubuntu-keyring", "apt-utils", "ca-certificates", "gnupg"];
/// Stage 2, `apt-get install` inside the chroot (real resolver): the extras.
const UBUNTU_EXTRAS: &[&str] = &[
    "software-properties-common", // add-apt-repository / PPAs
    "less",
    "nano",
    "man-db",
];
/// Written last; a stratum dir without it is half-built.
const COMPLETE_MARKER: &str = "etc/aftermath-stratum";

#[derive(Subcommand)]
pub enum StratumCmd {
    /// Build the Ubuntu stratum with debootstrap (sudo) into
    /// out/strata/ubuntu, then rebuild out/strata.img.
    Ubuntu(UbuntuArgs),
    /// (Re)build the strata disk image out/strata.img from out/strata (sudo).
    Image(ImageArgs),
    /// List the built strata and what strata.toml says about them.
    List,
    /// Delete out/strata/<name> (sudo) and rebuild the strata disk.
    Remove { name: String },
}

#[derive(Args)]
pub struct UbuntuArgs {
    /// Ubuntu release codename (noble = 24.04 LTS).
    #[arg(long, default_value = "noble")]
    pub suite: String,
    /// Ubuntu archive mirror.
    #[arg(long, default_value = "http://archive.ubuntu.com/ubuntu")]
    pub mirror: String,
    /// Delete an existing out/strata/ubuntu and bootstrap it again.
    #[arg(long)]
    pub force: bool,
    /// Only stage 1 (debootstrap minbase + apt); skip installing the extras.
    #[arg(long)]
    pub minimal: bool,
    #[command(flatten)]
    pub image: ImageArgs,
}

#[derive(Args)]
pub struct ImageArgs {
    /// Size of the (sparse) strata disk image.
    #[arg(long, default_value = "8G")]
    pub size: String,
}

pub fn run(ctx: &Ctx, cmd: &StratumCmd) -> Result<()> {
    match cmd {
        StratumCmd::Ubuntu(a) => {
            ubuntu(ctx, a)?;
            image(ctx, &a.image)
        }
        StratumCmd::Image(a) => image(ctx, a),
        StratumCmd::List => list(ctx),
        StratumCmd::Remove { name } => remove(ctx, name),
    }
}

pub fn ubuntu(ctx: &Ctx, a: &UbuntuArgs) -> Result<()> {
    if which("debootstrap").is_none() && !ctx.dry_run {
        bail!("debootstrap not found: sudo pacman -S --needed debootstrap ubuntu-keyring");
    }
    if !Path::new(UBUNTU_KEYRING).exists() && !ctx.dry_run {
        bail!("{UBUNTU_KEYRING} missing: sudo pacman -S --needed ubuntu-keyring");
    }
    let target = ctx.strata_dir().join("ubuntu");
    ctx.mkdir(&ctx.strata_dir())?;

    // A run killed with Ctrl-C (Drop never ran) may have left /proc etc. mounted.
    unmount_below(ctx, &target)?;

    // ---- what is there already? -------------------------------------------
    let state = StratumState::of(&target);
    match state {
        _ if a.force && state != StratumState::Missing => {
            println!("--force: deleting {} and starting over", target.display());
            sudo_rm(ctx, &target)?;
        }
        StratumState::Broken => {
            println!(
                "{} is a half-finished debootstrap (no working apt / debootstrap/ dir left over)\n\
                 -> deleting it and starting over",
                target.display()
            );
            sudo_rm(ctx, &target)?;
        }
        StratumState::Bootstrapped => {
            println!("debootstrap already finished earlier: resuming at stage 2 (apt-get)")
        }
        StratumState::Complete => println!(
            "out/strata/ubuntu is complete; re-running stage 2 (idempotent). \
             Use --force to rebuild it from scratch."
        ),
        StratumState::Missing => {}
    }

    // ---- stage 1: debootstrap a lean minbase ----------------------------------
    if !target.join("usr/bin/apt-get").exists() || ctx.dry_run {
        let mut c = sudo([
            "debootstrap",
            "--variant=minbase",
            "--components=main,universe",
        ]);
        c.arg(format!("--include={}", UBUNTU_BOOTSTRAP.join(",")))
            .arg(format!("--keyring={UBUNTU_KEYRING}"))
            .arg(&a.suite)
            .arg(&target)
            .arg(&a.mirror);
        ctx.run(&mut c).with_context(|| {
            format!(
                "debootstrap failed. Details: {}/debootstrap/debootstrap.log (readable without sudo).\n\
                 Rerunning `cargo am stratum ubuntu` deletes the half-finished tree automatically.",
                target.display()
            )
        })?;
    }

    // ---- Aftermath's files for the stratum (staged user-owned, copied as root)
    let stage = ctx.out.join("strata-stage/ubuntu");
    ctx.remove(&stage)?;
    if !ctx.dry_run {
        let etc = stage.join("etc");
        fs::create_dir_all(etc.join("apt/sources.list.d"))?;
        fs::write(
            etc.join("apt/sources.list.d/ubuntu.sources"),
            ubuntu_sources(&a.suite, &a.mirror),
        )?;
        fs::write(
            etc.join("apt/sources.list"),
            "# Ubuntu sources are in /etc/apt/sources.list.d/ubuntu.sources (deb822).\n",
        )?;
        // Debian's standard "never start services" hook (exit 101 = action
        // forbidden): strata have no init of their own, neither while building
        // nor later inside Aftermath. It has to be an executable; this two-line
        // file is the documented format (man invoke-rc.d).
        let sbin = stage.join("usr/sbin");
        fs::create_dir_all(&sbin)?;
        fs::write(sbin.join("policy-rc.d"), "#!/bin/sh\nexit 101\n")?;
        fs::set_permissions(sbin.join("policy-rc.d"), fs::Permissions::from_mode(0o755))?;
    }
    let mut cp = sudo(["cp", "-a", "--no-preserve=ownership"]);
    cp.arg(stage.join(".")).arg(&target);
    ctx.run(&mut cp)?;

    // ---- stage 2: the extras via apt (a real dependency resolver) -------------
    if a.minimal {
        println!("--minimal: skipping stage 2 ({})", UBUNTU_EXTRAS.join(" "));
    } else {
        // DNS for the build only; at boot resolv.conf points at the host's.
        let mut c = sudo(["cp", "-L", "--remove-destination", "/etc/resolv.conf"]);
        c.arg(target.join("etc/resolv.conf"));
        ctx.run(&mut c)?;

        let mounts = ChrootMounts::new(ctx, &target)?;
        chroot(ctx, &target, &["apt-get", "update"])
            .context("stage 2: `apt-get update` failed inside the stratum (network? mirror?)")?;
        let mut install = vec!["apt-get", "install", "-y", "--no-install-recommends"];
        install.extend(UBUNTU_EXTRAS);
        chroot(ctx, &target, &install).with_context(|| {
            format!(
                "stage 2: `apt-get install {}` failed inside the stratum (see the apt output above);\n\
                 fix the cause and rerun `cargo am stratum ubuntu` (it resumes here)",
                UBUNTU_EXTRAS.join(" ")
            )
        })?;
        chroot(ctx, &target, &["apt-get", "clean"])?;
        drop(mounts);
    }

    // ---- done: mark complete (anything without the marker is half-built) ------
    let marker = stage.join("aftermath-stratum");
    if !ctx.dry_run {
        fs::write(&marker, "ubuntu\n")?;
    }
    let mut c = sudo(["install", "-D", "-m", "0644"]);
    c.arg(&marker).arg(target.join(COMPLETE_MARKER));
    ctx.run(&mut c)?;
    ctx.remove(&ctx.out.join("strata-stage"))?;
    println!("ubuntu stratum ready in {}", target.display());
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StratumState {
    Missing,
    /// debootstrap died midway: unusable, delete and redo.
    Broken,
    /// debootstrap finished (it deletes its debootstrap/ dir on success), stage 2 not.
    Bootstrapped,
    Complete,
}

impl StratumState {
    fn of(t: &Path) -> Self {
        if !t.exists() {
            Self::Missing
        } else if t.join(COMPLETE_MARKER).exists() {
            Self::Complete
        } else if t.join("usr/bin/apt-get").exists() && !t.join("debootstrap").exists() {
            Self::Bootstrapped
        } else {
            Self::Broken
        }
    }
}

/// Run a command inside the stratum with a clean, non-interactive environment.
fn chroot(ctx: &Ctx, root: &Path, cmd: &[&str]) -> Result<()> {
    let mut c = sudo([
        "env",
        "-i",
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        "HOME=/root",
        "TERM=dumb",
        "LC_ALL=C.UTF-8",
        "DEBIAN_FRONTEND=noninteractive",
        "chroot",
    ]);
    c.arg(root).args(cmd);
    ctx.run(&mut c)
}

/// /proc, /sys, /dev, /dev/pts mounted into a chroot for as long as this
/// lives; unmounted in reverse order on drop (also on errors / early `?`).
/// Mounts left behind by a killed run are cleaned up by `unmount_below`.
struct ChrootMounts<'a> {
    ctx: &'a Ctx,
    mounted: Vec<PathBuf>,
}

impl<'a> ChrootMounts<'a> {
    fn new(ctx: &'a Ctx, root: &Path) -> Result<Self> {
        let mut m = Self {
            ctx,
            mounted: Vec::new(),
        };
        let plan: [(&[&str], &str); 4] = [
            (&["-t", "proc", "proc"], "proc"),
            (&["-t", "sysfs", "sysfs"], "sys"),
            (&["--bind", "/dev"], "dev"),
            (&["--bind", "/dev/pts"], "dev/pts"),
        ];
        for (args, dir) in plan {
            let d = root.join(dir);
            ctx.run(sudo(["mkdir", "-p"]).arg(&d))?;
            ctx.run(sudo(["mount"]).args(args).arg(&d))?;
            m.mounted.push(d);
        }
        Ok(m)
    }
}

impl Drop for ChrootMounts<'_> {
    fn drop(&mut self) {
        for d in self.mounted.iter().rev() {
            if self.ctx.run(sudo(["umount"]).arg(d)).is_err() {
                let _ = self.ctx.run(sudo(["umount", "--lazy"]).arg(d));
            }
        }
    }
}

/// Mount points at or below `dir` (from /proc/self/mountinfo), deepest first.
fn mounts_below(dir: &Path) -> Vec<PathBuf> {
    let Ok(base) = fs::canonicalize(dir) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(|m| PathBuf::from(m.replace("\\040", " ")))
        .filter(|m| m.starts_with(&base))
        .collect();
    v.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    v.dedup();
    v
}

/// Unmount whatever an interrupted run (Ctrl-C skips Drop) left mounted.
fn unmount_below(ctx: &Ctx, dir: &Path) -> Result<()> {
    for m in mounts_below(dir) {
        println!(
            "note: {} is still mounted from an interrupted run; unmounting",
            m.display()
        );
        if ctx.run(sudo(["umount"]).arg(&m)).is_err() {
            ctx.run(sudo(["umount", "--lazy"]).arg(&m))?;
        }
    }
    Ok(())
}

pub fn ubuntu_sources(suite: &str, mirror: &str) -> String {
    let security = if mirror.contains("archive.ubuntu.com") {
        "http://security.ubuntu.com/ubuntu"
    } else {
        mirror
    };
    format!(
        "Types: deb\nURIs: {mirror}\nSuites: {suite} {suite}-updates {suite}-backports\n\
         Components: main restricted universe multiverse\nSigned-By: {UBUNTU_KEYRING}\n\n\
         Types: deb\nURIs: {security}\nSuites: {suite}-security\n\
         Components: main restricted universe multiverse\nSigned-By: {UBUNTU_KEYRING}\n"
    )
}

fn built_strata(ctx: &Ctx) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(ctx.strata_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("usr").is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

pub fn image(ctx: &Ctx, a: &ImageArgs) -> Result<()> {
    let strata = built_strata(ctx);
    let img = ctx.strata_image();
    if strata.is_empty() && !ctx.dry_run {
        bail!("no strata in out/strata yet (try `cargo am stratum ubuntu`)");
    }
    if which("mkfs.ext4").is_none() && !ctx.dry_run {
        bail!("mkfs.ext4 not found: sudo pacman -S --needed e2fsprogs");
    }
    for s in strata
        .iter()
        .filter(|s| !ctx.strata_dir().join(s).join(COMPLETE_MARKER).exists())
    {
        println!("warning: out/strata/{s} is incomplete (no {COMPLETE_MARKER}); rerun `cargo am stratum {s}`");
    }
    let bytes = parse_size(&a.size)?;
    println!("==> create sparse {} ({})", img.display(), a.size);
    if !ctx.dry_run {
        let tmp = img.with_extension("img.partial");
        let f = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.set_len(bytes)?;
        drop(f);
        // -d copies out/strata into the new filesystem (needs root: the
        // strata are root-owned). The image file itself stays yours.
        let mut c = sudo([
            "mkfs.ext4",
            "-F",
            "-q",
            "-L",
            DISK_LABEL,
            "-E",
            "root_owner=0:0",
            "-d",
        ]);
        c.arg(ctx.strata_dir()).arg(&tmp);
        if let Err(e) = ctx.run(&mut c) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        fs::rename(&tmp, &img)?;
    } else {
        let mut c = sudo([
            "mkfs.ext4",
            "-F",
            "-q",
            "-L",
            DISK_LABEL,
            "-E",
            "root_owner=0:0",
            "-d",
        ]);
        c.arg(ctx.strata_dir()).arg(&img);
        ctx.run(&mut c)?;
    }
    println!("strata disk: {} ({})", img.display(), strata.join(", "));
    Ok(())
}

pub fn parse_size(s: &str) -> Result<u64> {
    let (num, mul) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M' | 'm') => (&s[..s.len() - 1], 1 << 20),
        Some('G' | 'g') => (&s[..s.len() - 1], 1 << 30),
        Some('T' | 't') => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    let n: u64 = num
        .parse()
        .with_context(|| format!("bad size `{s}` (e.g. 8G)"))?;
    Ok(n * mul)
}

fn list(ctx: &Ctx) -> Result<()> {
    let cfg_path = ctx.config().join("overlay/etc/aftermath/strata.toml");
    let cfg = aftermath_strata::config::Config::load(&cfg_path).map_err(anyhow::Error::msg)?;
    let built = built_strata(ctx);
    println!(
        "{:<10} {:<8} {:<9} {:<10} RELEASE",
        "STRATUM", "KIND", "ENABLED", "BUILT"
    );
    let mut names: Vec<String> = cfg.stratum.iter().map(|s| s.name.clone()).collect();
    names.extend(built.iter().filter(|b| cfg.get(b).is_none()).cloned());
    for name in names {
        let s = cfg.get(&name);
        let kind = s.map(|s| s.kind.as_str()).unwrap_or("?");
        let enabled = match s {
            Some(s) if s.enabled => "yes",
            Some(_) => "no",
            None => "not in toml",
        };
        let (state, release) = if s.is_some_and(|s| s.kind == "host") {
            (
                if ctx.rootfs().join("usr").is_dir() {
                    "out/rootfs"
                } else {
                    "missing"
                },
                os_release(&ctx.rootfs()),
            )
        } else if built.contains(&name) {
            let dir = ctx.strata_dir().join(&name);
            let state = if dir.join(COMPLETE_MARKER).exists() {
                "yes"
            } else {
                "INCOMPLETE"
            };
            (state, os_release(&dir))
        } else {
            ("no", String::new())
        };
        println!("{name:<10} {kind:<8} {enabled:<9} {state:<10} {release}");
    }
    let img = ctx.strata_image();
    match fs::metadata(&img) {
        Ok(m) => println!(
            "\nstrata disk: {} ({} MiB used of the sparse image)",
            img.display(),
            used_bytes(&m) >> 20
        ),
        Err(_) => println!("\nno strata disk yet: `cargo am stratum ubuntu` (or `stratum image`)"),
    }
    Ok(())
}

fn used_bytes(m: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.blocks() * 512
}

fn os_release(root: &Path) -> String {
    ["usr/lib/os-release", "etc/os-release"]
        .iter()
        .find_map(|p| fs::read_to_string(root.join(p)).ok())
        .and_then(|t| {
            t.lines().find_map(|l| {
                l.strip_prefix("PRETTY_NAME=")
                    .map(|v| v.trim_matches('"').to_owned())
            })
        })
        .unwrap_or_default()
}

fn remove(ctx: &Ctx, name: &str) -> Result<()> {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        bail!("bad stratum name `{name}`");
    }
    let dir = ctx.strata_dir().join(name);
    if !dir.exists() && !ctx.dry_run {
        bail!(
            "{} does not exist (see `cargo am stratum list`)",
            dir.display()
        );
    }
    sudo_rm(ctx, &dir)?;
    if built_strata(ctx).is_empty() {
        ctx.remove(&ctx.strata_image())?;
        println!("no strata left: Aftermath will boot in Arch host mode");
    } else {
        image(ctx, &ImageArgs { size: "8G".into() })?;
    }
    println!(
        "note: also set `enabled = false` for `{name}` in config/overlay/etc/aftermath/strata.toml"
    );
    Ok(())
}

/// `sudo rm -rf --one-file-system` on a path that must be inside out/strata.
pub fn sudo_rm(ctx: &Ctx, dir: &Path) -> Result<()> {
    let base: PathBuf = ctx.strata_dir();
    if !dir.starts_with(&base) || dir == base.as_path() {
        bail!(
            "refusing to delete {} (not inside {})",
            dir.display(),
            base.display()
        );
    }
    unmount_below(ctx, dir)?;
    let mut c: Command = sudo(["rm", "-rf", "--one-file-system"]);
    c.arg(dir);
    ctx.run(&mut c)
}

/// Remove out/strata completely (`cargo am clean --strata`).
pub fn clean(ctx: &Ctx) -> Result<()> {
    for s in built_strata(ctx) {
        sudo_rm(ctx, &ctx.strata_dir().join(s))?;
    }
    if ctx.strata_dir().exists() && !ctx.dry_run {
        let left = fs::read_dir(ctx.strata_dir())?.flatten().map(|e| e.path());
        for p in left {
            sudo_rm(ctx, &p)?;
        }
        fs::remove_dir(ctx.strata_dir())?;
    }
    ctx.remove(&ctx.strata_image())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stratum_states() {
        let t = std::env::temp_dir().join(format!("am-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&t);
        assert_eq!(StratumState::of(&t), StratumState::Missing);
        fs::create_dir_all(t.join("debootstrap")).unwrap();
        fs::create_dir_all(t.join("usr/bin")).unwrap();
        fs::write(t.join("usr/bin/apt-get"), "").unwrap();
        assert_eq!(
            StratumState::of(&t),
            StratumState::Broken,
            "debootstrap/ left = died midway"
        );
        fs::remove_dir(t.join("debootstrap")).unwrap();
        assert_eq!(StratumState::of(&t), StratumState::Bootstrapped);
        fs::create_dir_all(t.join("etc")).unwrap();
        fs::write(t.join(COMPLETE_MARKER), "ubuntu\n").unwrap();
        assert_eq!(StratumState::of(&t), StratumState::Complete);
        assert!(mounts_below(&t).is_empty());
        assert!(
            !mounts_below(Path::new("/")).is_empty(),
            "/ itself is a mount"
        );
        fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("8G").unwrap(), 8 << 30);
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert!(parse_size("lots").is_err());
    }

    #[test]
    fn sources_deb822() {
        let s = ubuntu_sources("noble", "http://archive.ubuntu.com/ubuntu");
        assert!(s.contains("Suites: noble noble-updates noble-backports\n"));
        assert!(s.contains("URIs: http://security.ubuntu.com/ubuntu\nSuites: noble-security\n"));
    }
}
