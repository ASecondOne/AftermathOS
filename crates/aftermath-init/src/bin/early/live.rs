//! Mount the disk-backed live image and a persistent or temporary upper layer.
//!
//! All boot-storage drivers are built into the kernel. Discovery needs neither
//! udev nor the root image's programs. The bootstrap's private BusyBox remains
//! available when discovery, validation or mounting fails.

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use nix::mount::{mount, MsFlags};
use nix::unistd::{chdir, chroot};

use super::{
    find_disk, log, note_image_change, remove_any, warn, ApiMounts, Undo, IMAGE_OWNED, LABEL, WORK,
};

pub const MARKER: &str = "/etc/aftermath/live-root";
const PAYLOAD: &str = "boot/rootfs-aftermath.squashfs";
const IMAGE_IN_ROOT: &str = "aftermath/image";

/// Mounted resources stay alive until they have been moved into the new root.
struct Image {
    lower: PathBuf,
    media: Option<PathBuf>,
    mounts: Undo,
}

/// Discover the matching image, choose an upper layer and hand over to real init.
pub(super) fn boot() -> Result<String, String> {
    let api = ApiMounts::mount();
    let cmdline = fs::read_to_string("/proc/cmdline").map_err(error)?;
    if rescue_requested(&cmdline) {
        return Err("BusyBox rescue requested; skipping the desktop root image".into());
    }
    let mut image = discover()?;
    let (root, backing, descriptor, mut overlay_mounts) = match persistent(&image, &cmdline) {
        Ok(overlay) => overlay,
        Err(why) => {
            warn(&format!("{why}; using a temporary writable layer"));
            temporary(&image)?
        }
    };
    validate_root(&root)?;
    keep_mount(
        &root,
        &backing,
        if descriptor.starts_with("overlay:") {
            "aftermath/hostdisk"
        } else {
            "aftermath/image/writable"
        },
    )?;
    keep_mount(&root, &image.lower, &format!("{IMAGE_IN_ROOT}/lower"))?;
    if let Some(media) = &image.media {
        keep_mount(&root, media, &format!("{IMAGE_IN_ROOT}/media"))?;
    }
    // Init mounts fresh API filesystems after the switch. Storage mounts remain
    // reachable outside /run, which init replaces with a new tmpfs.
    drop(api);
    chdir(&root).map_err(error)?;
    mount(Some("."), "/", None::<&str>, MsFlags::MS_MOVE, None::<&str>).map_err(error)?;
    chroot(".").map_err(error)?;
    chdir("/").map_err(error)?;
    image.mounts.0.clear();
    overlay_mounts.0.clear();
    log(&format!("disk-backed SquashFS root ready ({descriptor})"));
    Ok(descriptor)
}

/// Explicit rescue never depends on a readable desktop image.
fn rescue_requested(cmdline: &str) -> bool {
    cmdline
        .split_whitespace()
        .any(|arg| matches!(arg, "aftermath.mode=busybox" | "aftermath.rescue"))
}

/// Try raw SquashFS disks (direct boot), FAT boot partitions and live ISOs.
fn discover() -> Result<Image, String> {
    fs::create_dir_all(WORK).map_err(error)?;
    for _ in 0..50 {
        let mut devices: Vec<_> = fs::read_dir("/sys/class/block")
            .map_err(error)?
            .flatten()
            .map(|entry| Path::new("/dev").join(entry.file_name()))
            .collect();
        devices.sort();
        for device in devices {
            if let Ok(image) = try_device(&device) {
                return Ok(image);
            }
        }
        sleep(Duration::from_millis(100));
    }
    Err(format!(
        "no matching SquashFS root found ({PAYLOAD}); entering bootstrap rescue"
    ))
}

/// Each failed candidate is unmounted before examining the next device.
fn try_device(device: &Path) -> Result<Image, String> {
    let lower = Path::new(WORK).join("lower");
    let media = Path::new(WORK).join("media");
    for path in [&lower, &media] {
        fs::create_dir_all(path).map_err(error)?;
    }
    let mut mounts = Undo(Vec::new());
    if mount_readonly(device, &lower, "squashfs").is_ok() {
        mounts.0.push(lower.clone());
        validate_image(&lower)?;
        return Ok(Image {
            lower,
            media: None,
            mounts,
        });
    }
    for kind in ["vfat", "iso9660"] {
        if mount_readonly(device, &media, kind).is_err() {
            continue;
        }
        mounts.0.push(media.clone());
        let payload = media.join(PAYLOAD);
        if payload.is_file() {
            let (loop_device, _association) = loop_device(&payload)?;
            mount_readonly(&loop_device, &lower, "squashfs")?;
            mounts.0.push(lower.clone());
            validate_image(&lower)?;
            return Ok(Image {
                lower,
                media: Some(media),
                mounts,
            });
        }
        return Err("boot medium has no root image".into());
    }
    Err("unrecognized boot medium".into())
}

/// Reject a stale or unrelated root image rather than mixing image versions.
fn validate_image(lower: &Path) -> Result<(), String> {
    validate_root(lower)?;
    let expected = fs::read_to_string("/etc/aftermath/build-id").map_err(error)?;
    let actual = fs::read_to_string(lower.join("etc/aftermath/build-id")).map_err(error)?;
    if expected.trim().is_empty() || actual != expected {
        return Err("SquashFS build ID differs from this bootstrap".into());
    }
    Ok(())
}

/// Saved changes must retain the real init, engine and normal shell.
fn validate_root(root: &Path) -> Result<(), String> {
    for file in [
        "usr/bin/aftermath-init",
        "usr/lib/aftermath/aftermath-engine",
        "usr/bin/bash",
    ] {
        if !root.join(file).is_file() {
            return Err(format!("root image or saved changes hide /{file}"));
        }
    }
    Ok(())
}

type Overlay = (PathBuf, PathBuf, String, Undo);

/// Reuse the existing AFTERMATH_HOST format without changing saved user files.
fn persistent(image: &Image, cmdline: &str) -> Result<Overlay, String> {
    if cmdline
        .split_whitespace()
        .any(|arg| arg == "aftermath.overlay=off")
    {
        return Err("saved changes disabled by the kernel command line".into());
    }
    let device = (0..30)
        .find_map(|_| {
            let found = find_disk(LABEL);
            if found.is_none() {
                sleep(Duration::from_millis(100));
            }
            found
        })
        .ok_or("no AFTERMATH_HOST disk")?;
    let disk = Path::new(WORK).join("disk");
    fs::create_dir_all(&disk).map_err(error)?;
    mount(
        Some(device.as_path()),
        &disk,
        Some("ext4"),
        MsFlags::MS_NOATIME,
        None::<&str>,
    )
    .map_err(error)?;
    let mut mounts = Undo(vec![disk.clone()]);
    if disk.join("reset-requested").exists() {
        remove_any(&disk.join("upper")).map_err(error)?;
        remove_any(&disk.join("work")).map_err(error)?;
        fs::remove_file(disk.join("reset-requested")).map_err(error)?;
    }
    for owned in IMAGE_OWNED {
        remove_any(&disk.join("upper").join(owned)).map_err(error)?;
    }
    note_image_change(&disk);
    let root = mount_overlay(&image.lower, &disk)?;
    mounts.0.push(root.clone());
    validate_root(&root)?;
    Ok((root, disk, format!("overlay:{}", device.display()), mounts))
}

/// A clean boot still needs writable /etc and /var, without copying the image.
fn temporary(image: &Image) -> Result<Overlay, String> {
    let ram = Path::new(WORK).join("writable");
    fs::create_dir_all(&ram).map_err(error)?;
    mount(
        Some("tmpfs"),
        &ram,
        Some("tmpfs"),
        MsFlags::MS_NOSUID,
        Some("mode=0755"),
    )
    .map_err(error)?;
    let mut mounts = Undo(vec![ram.clone()]);
    let root = mount_overlay(&image.lower, &ram)?;
    mounts.0.push(root.clone());
    Ok((root, ram, "squashfs".into(), mounts))
}

/// Both upper-layer policies use identical overlay mount options.
fn mount_overlay(lower: &Path, backing: &Path) -> Result<PathBuf, String> {
    let root = Path::new(WORK).join("root");
    for path in [root.clone(), backing.join("upper"), backing.join("work")] {
        fs::create_dir_all(path).map_err(error)?;
    }
    let options = format!(
        "lowerdir={},upperdir={},workdir={}",
        lower.display(),
        backing.join("upper").display(),
        backing.join("work").display()
    );
    mount(
        Some("aftermath-root"),
        &root,
        Some("overlay"),
        MsFlags::empty(),
        Some(options.as_str()),
    )
    .map_err(error)?;
    Ok(root)
}

/// Move storage below the new root before abandoning bootstrap paths.
fn keep_mount(root: &Path, source: &Path, relative: &str) -> Result<(), String> {
    let target = root.join(relative);
    fs::create_dir_all(&target).map_err(error)?;
    mount(
        Some(source),
        &target,
        None::<&str>,
        MsFlags::MS_MOVE,
        None::<&str>,
    )
    .map_err(error)
}

fn mount_readonly(device: &Path, target: &Path, kind: &str) -> Result<(), String> {
    mount(
        Some(device),
        target,
        Some(kind),
        MsFlags::MS_RDONLY | MsFlags::MS_NOATIME,
        None::<&str>,
    )
    .map_err(error)
}

/// Associate a read-only image with an automatically released kernel loop device.
fn loop_device(image: &Path) -> Result<(PathBuf, File), String> {
    const LOOP_CTL_GET_FREE: libc::c_ulong = 0x4c82;
    const LOOP_SET_FD: libc::c_ulong = 0x4c00;
    const LOOP_SET_STATUS64: libc::c_ulong = 0x4c04;
    const LOOP_CLR_FD: libc::c_ulong = 0x4c01;
    let control = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/loop-control")
        .map_err(error)?;
    // SAFETY: this ioctl takes no pointer argument and returns a device number.
    let number = unsafe { libc::ioctl(control.as_raw_fd(), LOOP_CTL_GET_FREE) };
    if number < 0 {
        return Err(error(std::io::Error::last_os_error()));
    }
    let path = PathBuf::from(format!("/dev/loop{number}"));
    let device = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(error)?;
    let file = File::open(image).map_err(error)?;
    // SAFETY: both descriptors remain open for the duration of the ioctl.
    if unsafe { libc::ioctl(device.as_raw_fd(), LOOP_SET_FD, file.as_raw_fd()) } < 0 {
        return Err(error(std::io::Error::last_os_error()));
    }
    // SAFETY: this Linux UAPI structure contains only integer fields/arrays.
    let mut info: LoopInfo = unsafe { std::mem::zeroed() };
    info.flags = 1 | 4;
    // SAFETY: LoopInfo matches Linux's loop_info64 and remains readable here.
    if unsafe { libc::ioctl(device.as_raw_fd(), LOOP_SET_STATUS64, &info) } < 0 {
        let err = error(std::io::Error::last_os_error());
        unsafe {
            libc::ioctl(device.as_raw_fd(), LOOP_CLR_FD);
        }
        return Err(err);
    }
    // The caller keeps this descriptor open until the filesystem is mounted.
    Ok((path, device))
}

/// Linux UAPI loop_info64; no encryption or offsets are used.
#[repr(C)]
struct LoopInfo {
    device: u64,
    inode: u64,
    rdevice: u64,
    offset: u64,
    sizelimit: u64,
    number: u32,
    encryption: u32,
    key_size: u32,
    flags: u32,
    file_name: [u8; 64],
    crypt_name: [u8; 64],
    key: [u8; 32],
    init: [u64; 2],
}

fn error(err: impl std::fmt::Display) -> String {
    err.to_string()
}
