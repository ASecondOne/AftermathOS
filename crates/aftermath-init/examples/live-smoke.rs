//! Private, static guest probe for persistence, clean boot and GRUB recovery.
//! The host installs it only on its disposable boot disk, never in normal images.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use nix::mount::{mount, MsFlags};
use nix::sys::signal::{kill, Signal};
use nix::unistd::{sync, Pid};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const MEDIA: &str = "/aftermath/image/media";
const SAVED: &str = "saved across the SquashFS reboot\n";
const WHITEOUT: &str = "/usr/share/aftermath/wayland-sessions/plasma.desktop";

fn main() {
    if let Err(error) = run() {
        eprintln!("AM_LIVE_FAILED: {error}");
        std::process::exit(1);
    }
}

/// Each phase ends in a real guest reboot, except the final poweroff.
fn run() -> Result<()> {
    require(unsafe { libc::getuid() } == 0, "probe needs guest root")?;
    let phase = std::env::args().nth(1).ok_or("missing test phase")?;
    let cmdline = fs::read_to_string("/proc/cmdline")?;
    require(
        cmdline.contains("aftermath.test=live-smoke")
            || cmdline.contains("aftermath.test=no-success"),
        "private test boot required",
    )?;
    match phase.as_str() {
        "save" => save()?,
        "verify" => {
            verify_saved()?;
            select("aftermath-clean", "live-smoke")?;
        }
        "clean" => clean()?,
        "restore" => {
            verify_saved()?;
            select("aftermath-console", "no-success")?;
        }
        "unconfirmed" => unconfirmed()?,
        "counted" => counted(&cmdline)?,
        "rescue" => rescue(&cmdline)?,
        "missing" => missing(&cmdline)?,
        _ => return Err("unknown test phase".into()),
    }
    println!("AM_LIVE_{}_OK", phase.to_uppercase());
    io::stdout().flush()?;
    sync();
    kill(
        Pid::from_raw(1),
        if phase == "missing" {
            Signal::SIGUSR2
        } else {
            Signal::SIGINT
        },
    )?;
    // Never return to the console before PID 1 has stopped this probe.
    loop {
        std::thread::park();
    }
}

/// Assert disk-backed lower storage and a successful engine acknowledgement.
fn live_root() -> Result<()> {
    let mounts = fs::read_to_string("/proc/self/mountinfo")?;
    require(
        mounts.contains(" - squashfs "),
        "SquashFS lower mount is missing",
    )?;
    require(
        mounts.lines().any(|line| {
            line.split_whitespace().nth(4) == Some("/") && line.contains(" - overlay ")
        }),
        "root is not overlayfs",
    )?;
    let env = fs::read_to_string(Path::new(MEDIA).join("boot/grub/grubenv"))?;
    require(
        env.lines().any(|line| line == "boot_success=1"),
        "engine failed to acknowledge GRUB boot",
    )?;
    require(
        env.lines().any(|line| line == "boot_fails=0"),
        "successful boot did not reset failure count",
    )
}

/// Changes in /root and /etc, plus a lower-file whiteout, must survive reboot.
fn save() -> Result<()> {
    live_root()?;
    require(
        Path::new("/aftermath/hostdisk/upper").is_dir(),
        "persistent upper missing",
    )?;
    fs::write("/root/live-smoke-saved", SAVED)?;
    fs::write("/etc/live-smoke-saved", SAVED)?;
    fs::remove_file(WHITEOUT)?;
    select("aftermath-console", "live-smoke")
}

fn verify_saved() -> Result<()> {
    live_root()?;
    require(
        fs::read_to_string("/root/live-smoke-saved")? == SAVED,
        "/root change lost",
    )?;
    require(
        fs::read_to_string("/etc/live-smoke-saved")? == SAVED,
        "/etc change lost",
    )?;
    require(
        !Path::new(WHITEOUT).exists(),
        "deleted image file returned after reboot",
    )
}

/// Ignoring saved changes must expose the original image without erasing them.
fn clean() -> Result<()> {
    live_root()?;
    require(
        !Path::new("/root/live-smoke-saved").exists(),
        "clean boot applied saved changes",
    )?;
    require(
        !Path::new("/etc/live-smoke-saved").exists(),
        "clean boot applied saved /etc",
    )?;
    require(
        Path::new(WHITEOUT).is_file(),
        "clean image inherited a whiteout",
    )?;
    require(
        !fs::read_to_string("/proc/self/mountinfo")?
            .lines()
            .any(|line| line.split_whitespace().nth(4) == Some("/aftermath/hostdisk")),
        "clean boot mounted saved upper",
    )?;
    select("aftermath-console", "live-smoke")
}

/// One deliberately unacknowledged launch arms the existing GRUB counter.
fn unconfirmed() -> Result<()> {
    let env = fs::read_to_string(Path::new(MEDIA).join("boot/grub/grubenv"))?;
    require(
        env.lines().any(|line| line == "boot_success=0"),
        "unconfirmed boot was acknowledged",
    )?;
    require(
        env.lines().any(|line| line == "boot_fails=0"),
        "failure counted before return to GRUB",
    )?;
    select("aftermath-console", "no-success")
}

/// Exercise a real failure increment, then the established four-failure rescue.
fn counted(cmdline: &str) -> Result<()> {
    require(
        cmdline
            .split_whitespace()
            .any(|arg| arg == "aftermath.bootfails=1"),
        "GRUB did not count the failed launch exactly once",
    )?;
    update_env(
        MEDIA,
        &[
            ("boot_fails", "3"),
            ("boot_success", "0"),
            ("aftermath_next", ""),
            ("aftermath_test", "live-smoke"),
        ],
    )?;
    writable(MEDIA, || {
        fs::rename(
            Path::new(MEDIA).join("boot/rootfs-aftermath.squashfs"),
            Path::new(MEDIA).join("boot/rootfs-unavailable.squashfs"),
        )?;
        Ok(())
    })
}

/// BusyBox starts without SquashFS, and saved data is available read-only.
fn rescue(cmdline: &str) -> Result<()> {
    require(
        cmdline.contains("aftermath.bootfails=4") && cmdline.contains("aftermath.mode=busybox"),
        "GRUB did not select the rescue fallback",
    )?;
    require(
        Path::new("/etc/aftermath/live-root").exists(),
        "rescue depends on the desktop root",
    )?;
    require(
        fs::read_to_string("/mnt/vdb/upper/root/live-smoke-saved")? == SAVED,
        "rescue cannot read saved data",
    )?;
    update_env(
        "/mnt/vda1",
        &[
            ("boot_fails", "0"),
            ("boot_success", "1"),
            ("aftermath_next", "aftermath-console"),
        ],
    )
}

/// A normal entry with a missing image also reaches independent bootstrap rescue.
fn missing(cmdline: &str) -> Result<()> {
    require(
        !cmdline.contains("aftermath.mode=busybox"),
        "missing-root test selected rescue explicitly",
    )?;
    require(
        Path::new("/etc/aftermath/live-root").exists(),
        "missing root did not preserve bootstrap",
    )?;
    require(
        !Path::new("/mnt/vda1/boot/rootfs-aftermath.squashfs").exists(),
        "missing-root test still has its image",
    )
}

fn select(entry: &str, test: &str) -> Result<()> {
    update_env(
        MEDIA,
        &[("aftermath_next", entry), ("aftermath_test", test)],
    )
}

/// Rewrite grubenv in place, keeping its block allocation and 1024-byte size.
fn update_env(media: &str, updates: &[(&str, &str)]) -> Result<()> {
    writable(media, || {
        let path = PathBuf::from(media).join("boot/grub/grubenv");
        let previous = fs::read_to_string(&path)?;
        let mut fields: Vec<_> = previous
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        for (key, value) in updates {
            if let Some(field) = fields.iter_mut().find(|(name, _)| name == key) {
                field.1 = (*value).into();
            } else {
                fields.push(((*key).into(), (*value).into()));
            }
        }
        let mut data = String::from("# GRUB Environment Block\n");
        for (key, value) in fields {
            data.push_str(&format!("{key}={value}\n"));
        }
        require(data.len() <= 1024, "grubenv too large")?;
        let mut data = data.into_bytes();
        data.resize(1024, b'#');
        let mut file = OpenOptions::new().write(true).open(path)?;
        file.write_all(&data)?;
        file.sync_all()?;
        Ok(())
    })
}

/// Temporarily allow GRUB metadata updates, then restore read-only boot media.
fn writable(media: &str, action: impl FnOnce() -> Result<()>) -> Result<()> {
    mount(
        None::<&str>,
        media,
        None::<&str>,
        MsFlags::MS_REMOUNT,
        None::<&str>,
    )?;
    let result = action();
    let restored = mount(
        None::<&str>,
        media,
        None::<&str>,
        MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    );
    result?;
    restored?;
    Ok(())
}

fn require(condition: bool, message: &str) -> Result<()> {
    if !condition {
        return Err(io::Error::other(message).into());
    }
    Ok(())
}
