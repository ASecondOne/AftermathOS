//! aftermath-early: mount the live root before normal PID 1 supervision.
//!
//! aftermath-init execs this first (same PID). The small bootstrap dispatches
//! to `live`, which mounts a matching disk-backed SquashFS. Older full-root
//! initramfs images retain their original path for compatibility. Both use:
//!
//!   lower = the clean image (SquashFS, or the legacy RAM image)
//!   upper = <disk>/upper   (everything changed in the VM: pacman -Syu, /root...)
//!
//! The overlay becomes /, then init is exec'd with AFTERMATH_EARLY_DONE=1.
//! An unusable persistent upper selects a temporary layer over SquashFS.
//! A missing/mismatched image enters the bootstrap's independent BusyBox.
//! `aftermath.overlay=off` skips saved changes; BusyBox mode skips SquashFS.
//!
//! Paths the image owns (Aftermath's own binaries, kernel modules, /boot)
//! are removed from the upper layer at every boot, so a rebuilt image always
//! brings its own init/engine/strata service.
//!
//! `amctl overlay reset` leaves a `reset-requested` file on the disk; the
//! next boot wipes upper/ and work/ here before building the overlay.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::unistd::{chdir, chroot};

#[path = "early/live.rs"]
mod live;

const LABEL: &str = "AFTERMATH_HOST";
const WORK: &str = "/.aftermath-early";
/// Where the overlay disk stays mounted in the new root.
const DISK_IN_ROOT: &str = "aftermath/hostdisk";
const IMAGE_OWNED: &[&str] = &[
    "usr/lib/aftermath",
    "usr/bin/aftermath-init",
    "usr/lib/modules",
    "etc/aftermath/version",
    "etc/aftermath/build-id",
    "boot",
];

fn log_at(level: u8, msg: &str) {
    let line = format!("<{level}>aftermath-early: {msg}\n");
    match OpenOptions::new().write(true).open("/dev/kmsg") {
        Ok(mut k) => {
            let _ = k.write_all(line.as_bytes());
        }
        Err(_) => eprint!("{}", &line[3..]),
    }
}
fn log(msg: &str) {
    log_at(6, msg)
}
fn warn(msg: &str) {
    log_at(3, msg)
}

fn main() {
    panic::set_hook(Box::new(|info| warn(&format!("PANIC: {info}"))));
    let root = match panic::catch_unwind(AssertUnwindSafe(run)) {
        Ok(Ok(root)) => root,
        Ok(Err(Skip::Quiet(why))) => {
            with_dev(|| log(&format!("RAM root ({why})")));
            "ram".into()
        }
        Ok(Err(Skip::Failed(why))) => {
            with_dev(|| {
                warn(&format!(
                    "persistent root FAILED: {why} -> plain RAM root (changes are not saved)"
                ))
            });
            "ram".into()
        }
        Err(_) => "ram".into(),
    };
    let mut init = Command::new("/usr/bin/init");
    // The marker exists only in the small bootstrap image. If we are still
    // there, the requested rescue boot or failed root discovery needs BusyBox.
    if Path::new(live::MARKER).exists() {
        init.env("AFTERMATH_EARLY_RESCUE", "1");
    }
    let err = init
        .env("AFTERMATH_EARLY_DONE", "1")
        .env("AFTERMATH_ROOT", root)
        .exec();
    // Only if exec failed: nothing sane is left to do; let PID 1 handle it by
    // returning (aftermath-init continues when its exec of us fails, but we
    // ARE that exec now) - so try the init binary directly as a last resort.
    warn(&format!("exec /usr/bin/init: {err}"));
    let err = Command::new("/usr/bin/aftermath-init")
        .env("AFTERMATH_EARLY_DONE", "1")
        .exec();
    warn(&format!("exec /usr/bin/aftermath-init: {err}"));
    loop {
        sleep(Duration::from_secs(3600));
    }
}

enum Skip {
    /// Normal: no disk, disabled, rescue mode.
    Quiet(String),
    /// Something is wrong with the disk or the saved changes.
    Failed(String),
}

fn run() -> Result<String, Skip> {
    if Path::new(live::MARKER).exists() {
        return live::boot().map_err(Skip::Failed);
    }
    legacy_root().map(|dev| format!("overlay:{dev}"))
}

/// Support older all-in-RAM images and the existing session test fixture.
fn legacy_root() -> Result<String, Skip> {
    let api = ApiMounts::mount();
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    for arg in cmdline.split_whitespace() {
        match arg {
            "aftermath.overlay=off" => return Err(Skip::Quiet("aftermath.overlay=off".into())),
            "aftermath.mode=busybox" | "aftermath.rescue" => {
                return Err(Skip::Quiet("rescue mode uses the clean image".into()))
            }
            _ => {}
        }
    }

    // Disks may still be probing right after the kernel starts.
    let mut dev = None;
    for _ in 0..30 {
        dev = find_disk(LABEL);
        if dev.is_some() {
            break;
        }
        sleep(Duration::from_millis(100));
    }
    let dev = dev.ok_or_else(|| Skip::Quiet(format!("no disk labelled {LABEL}")))?;
    let dev_s = dev.display().to_string();
    let fail = |what: &str, e: &dyn std::fmt::Display| Skip::Failed(format!("{what}: {e}"));

    let work = Path::new(WORK);
    let (disk, lower, newroot) = (work.join("disk"), work.join("lower"), work.join("root"));
    for d in [&disk, &lower, &newroot] {
        fs::create_dir_all(d).map_err(|e| fail("mkdir", &e))?;
    }
    mount(
        Some(dev.as_path()),
        &disk,
        Some("ext4"),
        MsFlags::MS_NOATIME,
        None::<&str>,
    )
    .map_err(|e| fail(&format!("mount {dev_s}"), &e))?;
    let mut undo = Undo(vec![disk.clone()]);

    let (upper, wdir) = (disk.join("upper"), disk.join("work"));
    // `amctl overlay reset` asked for a clean slate: wipe the saved changes
    // now, before anything uses them.
    let marker = disk.join("reset-requested");
    if marker.exists() {
        let a = remove_any(&upper);
        let b = remove_any(&wdir);
        let _ = fs::remove_file(&marker);
        match (a, b) {
            (Ok(()), Ok(())) => log("saved changes wiped (amctl overlay reset)"),
            (a, b) => warn(&format!("overlay reset incomplete: {a:?} {b:?}")),
        }
    }
    for d in [&upper, &wdir] {
        fs::create_dir_all(d).map_err(|e| fail("mkdir on the overlay disk", &e))?;
    }
    for p in IMAGE_OWNED {
        let _ = remove_any(&upper.join(p));
    }
    note_image_change(&disk);

    // The pure image: a non-recursive bind of the RAM root (no /proc, /dev...).
    mount(
        Some("/"),
        &lower,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .map_err(|e| fail("bind the image", &e))?;
    undo.0.push(lower.clone());
    let opts = format!(
        "lowerdir={},upperdir={},workdir={}",
        lower.display(),
        upper.display(),
        wdir.display()
    );
    mount(
        Some("aftermath-root"),
        &newroot,
        Some("overlay"),
        MsFlags::empty(),
        Some(opts.as_str()),
    )
    .map_err(|e| fail("mount overlay", &e))?;
    undo.0.push(newroot.clone());

    // Saved changes must not break the boot chain.
    for must in [
        "usr/bin/init",
        "usr/lib/aftermath/aftermath-engine",
        "usr/bin/bash",
    ] {
        if !newroot.join(must).exists() {
            return Err(Skip::Failed(format!(
                "the saved changes hide /{must} (boot \"clean image\" or run `cargo am reset-vm`)"
            )));
        }
    }

    // Keep the disk reachable in the new root, then switch.
    let disk_target = newroot.join(DISK_IN_ROOT);
    fs::create_dir_all(&disk_target).map_err(|e| fail("mkdir hostdisk", &e))?;
    mount(
        Some(&disk),
        &disk_target,
        None::<&str>,
        MsFlags::MS_MOVE,
        None::<&str>,
    )
    .map_err(|e| fail("move the disk mount", &e))?;
    undo.0.clear(); // from here on there is no going back
    log(&format!("persistent host root: image + overlay on {dev_s}"));
    drop(api); // init mounts /proc /sys /dev itself, in the new root
    chdir(&newroot).map_err(|e| fail("chdir", &e))?;
    mount(Some("."), "/", None::<&str>, MsFlags::MS_MOVE, None::<&str>)
        .map_err(|e| fail("move root", &e))?;
    chroot(".").map_err(|e| fail("chroot", &e))?;
    chdir("/").map_err(|e| fail("chdir /", &e))?;
    Ok(dev_s)
}

/// Remember which image the saved changes were made on; warn when it changed.
fn note_image_change(disk: &Path) {
    let image = fs::read_to_string("/etc/aftermath/build-id").unwrap_or_default();
    let stamp = disk.join("image-build-id");
    let before = fs::read_to_string(&stamp).unwrap_or_default();
    if !before.is_empty() && before != image {
        warn(&format!(
            "the image was rebuilt ({} -> {}); saved changes are kept on top. If packages misbehave: \
             boot \"clean image\" or `cargo am reset-vm`",
            before.trim(),
            image.trim()
        ));
    }
    let _ = fs::write(stamp, image);
}

fn remove_any(p: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
        Err(_) => Ok(()),
    }
}

/// Unmounts (deepest first) on drop, unless cleared.
struct Undo(Vec<PathBuf>);
impl Drop for Undo {
    fn drop(&mut self) {
        for p in self.0.iter().rev() {
            let _ = umount2(p, MntFlags::MNT_DETACH);
        }
    }
}

/// /proc /sys /dev for the duration of the early stage only.
struct ApiMounts(Vec<&'static str>);
impl ApiMounts {
    fn mount() -> Self {
        let mut done = Vec::new();
        for (src, target, fstype) in [
            ("proc", "/proc", "proc"),
            ("sysfs", "/sys", "sysfs"),
            ("devtmpfs", "/dev", "devtmpfs"),
        ] {
            let _ = fs::create_dir_all(target);
            if mount(
                Some(src),
                target,
                Some(fstype),
                MsFlags::MS_NOSUID,
                None::<&str>,
            )
            .is_ok()
            {
                done.push(target);
            }
        }
        Self(done)
    }
}
impl Drop for ApiMounts {
    fn drop(&mut self) {
        for t in self.0.iter().rev() {
            let _ = umount2(*t, MntFlags::MNT_DETACH);
        }
    }
}

fn find_disk(label: &str) -> Option<PathBuf> {
    let mut names: Vec<String> = fs::read_dir("/sys/class/block")
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|n| PathBuf::from("/dev").join(n))
        .find(|dev| {
            let mut buf = [0u8; 1024 + 136];
            let ok = File::open(dev)
                .and_then(|mut f| f.read_exact(&mut buf))
                .is_ok();
            ok && ext4_label(&buf).as_deref() == Some(label)
        })
}

/// ext2/3/4 superblock at 1024: magic 0xEF53 at +56, volume name at +120.
fn ext4_label(buf: &[u8]) -> Option<String> {
    let sb = buf.get(1024..1024 + 136)?;
    if sb[56..58] != [0x53, 0xEF] {
        return None;
    }
    let raw = &sb[120..136];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    Some(String::from_utf8_lossy(&raw[..end]).into_owned())
}

/// /dev (for /dev/kmsg) only while `f` runs: init mounts /dev itself later.
fn with_dev(f: impl FnOnce()) {
    let mounted = mount(
        Some("devtmpfs"),
        "/dev",
        Some("devtmpfs"),
        MsFlags::MS_NOSUID,
        None::<&str>,
    )
    .is_ok();
    f();
    if mounted {
        let _ = umount2("/dev", MntFlags::MNT_DETACH);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label() {
        let mut buf = vec![0u8; 2048];
        buf[1024 + 56] = 0x53;
        buf[1024 + 57] = 0xEF;
        buf[1024 + 120..1024 + 134].copy_from_slice(b"AFTERMATH_HOST");
        assert_eq!(ext4_label(&buf).as_deref(), Some(LABEL));
    }
}
