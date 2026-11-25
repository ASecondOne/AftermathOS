//! Partition only the disposable disk with the private test serial.
use super::{require, Result};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Identify the private disk independently of virtio enumeration order.
fn device() -> Result<PathBuf> {
    for entry in fs::read_dir("/sys/class/block")?.flatten() {
        if fs::read_to_string(entry.path().join("serial"))
            .is_ok_and(|serial| serial.trim() == "AFTERMATH_TEST_DISK")
        {
            let sectors: u64 = fs::read_to_string(entry.path().join("size"))?
                .trim()
                .parse()?;
            require(
                sectors * 512 == 32 * 1024 * 1024 * 1024,
                "test disk is not 32 GiB",
            )?;
            let path = Path::new("/dev").join(entry.file_name());
            require(
                fs::metadata(&path)?.file_type().is_block_device(),
                "test target is not a block device",
            )?;
            return Ok(path);
        }
    }
    Err("private partition-test disk is missing".into())
}

/// A real guest GPT, partition and filesystem are created with native tools.
pub(super) fn create() -> Result<()> {
    let disk = device()?;
    let mut initial = [0; 512];
    File::open(&disk)?.read_exact(&mut initial)?;
    require(
        initial == [0; 512],
        "refusing to overwrite a nonblank test disk",
    )?;
    let mut partitioner = Command::new("/usr/bin/sfdisk")
        .args(["--label", "gpt"])
        .arg(&disk)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()?;
    partitioner
        .stdin
        .take()
        .ok_or("no partitioner input")?
        .write_all(b",128M\n")?;
    require(
        partitioner.wait()?.success(),
        "guest partition table creation failed",
    )?;
    let partition = PathBuf::from(format!("{}1", disk.display()));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !partition.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    require(
        Command::new("/usr/bin/mkfs.ext4")
            .args(["-F", "-q", "-L", "AM_TEST_DATA"])
            .arg(&partition)
            .status()?
            .success(),
        "guest filesystem creation failed",
    )?;
    with_filesystem(&partition, || {
        fs::write("/mnt/partition-test/saved", b"guest partition data\n")?;
        Ok(())
    })?;
    println!("AM_GUEST_GPT_CREATED");
    Ok(())
}

/// GPT and file contents must remain intact after a real guest reboot.
pub(super) fn verify() -> Result<()> {
    let disk = device()?;
    let mut header = [0; 8];
    let mut file = File::open(&disk)?;
    file.seek(SeekFrom::Start(512))?;
    file.read_exact(&mut header)?;
    require(
        &header == b"EFI PART",
        "guest GPT header did not survive reboot",
    )?;
    with_filesystem(&PathBuf::from(format!("{}1", disk.display())), || {
        require(
            fs::read("/mnt/partition-test/saved")? == b"guest partition data\n",
            "guest partition contents lost",
        )
    })
}

/// Keep the test filesystem unmounted before asking PID 1 to reboot.
fn with_filesystem(partition: &Path, action: impl FnOnce() -> Result<()>) -> Result<()> {
    fs::create_dir_all("/mnt/partition-test")?;
    nix::mount::mount(
        Some(partition),
        "/mnt/partition-test",
        Some("ext4"),
        nix::mount::MsFlags::empty(),
        None::<&str>,
    )?;
    let result = action();
    let unmount = nix::mount::umount("/mnt/partition-test");
    result?;
    unmount?;
    Ok(())
}
