//! Reproducible Rust-only host/guest integration test. Private live-image guest,
//! no saved VM disks; the test account only exists inside that guest.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::ctx::Ctx;

struct Guest(Child);
impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn entry(out: &mut Vec<u8>, name: &str, data: &[u8], mode: u32) -> Result<()> {
    let length = u32::try_from(data.len())?;
    let namesize = u32::try_from(name.len() + 1)?;
    out.extend_from_slice(b"070701");
    for n in [0, mode, 0, 0, 1, 0, length, 0, 0, 0, 0, namesize, 0] {
        out.extend_from_slice(format!("{n:08x}").as_bytes());
    }
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
    out.extend_from_slice(data);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
    Ok(())
}

pub fn run(ctx: &Ctx) -> Result<()> {
    if ctx.dry_run {
        println!("Build Rust engine/PAM module and guest test into a private SquashFS root; boot QEMU without saved disks and verify authenticated sessions.");
        return Ok(());
    }
    let init = crate::image::build_init(ctx)?;
    if !ctx
        .out
        .join("vm/desktop-smoke/rootfs/usr/bin/bash")
        .exists()
        || !ctx.kernel_image().exists()
    {
        bail!("build the private root fixture with `cargo am smoke-desktop --grub` first");
    }
    let mut build = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    build
        .current_dir(&ctx.root)
        .args([
            "build",
            "--release",
            "-p",
            "aftermath-pam",
            "--example",
            "session-smoke",
            "--target",
            "x86_64-unknown-linux-gnu",
        ])
        .env("RUSTFLAGS", "");
    ctx.run(&mut build)?;
    let work = ctx.out.join("vm/session-smoke");
    fs::create_dir_all(&work)?;
    let mut archive = Vec::new();
    for (source, target, mode) in [
        (
            init.with_file_name("aftermath-engine"),
            "usr/lib/aftermath/aftermath-engine",
            0o100755,
        ),
        (init.with_file_name("amctl"), "usr/bin/amctl", 0o100755),
        (
            init.with_file_name("aftermath-strata"),
            "usr/lib/aftermath/aftermath-strata",
            0o100755,
        ),
        (
            init.with_file_name("libpam_aftermath.so"),
            "usr/lib/security/pam_aftermath.so",
            0o100755,
        ),
        (
            init.parent()
                .context("no binary directory")?
                .join("examples/session-smoke"),
            "usr/lib/aftermath/session-smoke",
            0o100755,
        ),
        (
            ctx.config().join("overlay/etc/pam.d/login"),
            "etc/pam.d/login",
            0o100644,
        ),
        (
            ctx.config().join("overlay/etc/aftermath/aftermath.toml"),
            "etc/aftermath/aftermath.toml",
            0o100644,
        ),
    ] {
        entry(&mut archive, target, &fs::read(source)?, mode)?;
    }
    entry(&mut archive, "TRAILER!!!", &[], 0)?;
    let patch = work.join("patch.cpio.zst");
    let mut zstd = Command::new("zstd")
        .args(["-q", "-c"])
        .stdin(Stdio::piped())
        .stdout(Stdio::from(File::create(&patch)?))
        .spawn()?;
    zstd.stdin
        .take()
        .context("no compression stdin")?
        .write_all(&archive)?;
    if !zstd.wait()?.success() {
        bail!("could not compress test initramfs overlay");
    }
    let private = prepare_live_fixture(ctx, &work, &patch, &init)?;
    let mut qemu = Command::new("qemu-system-x86_64");
    qemu.args(["-machine", "q35", "-m", "4G", "-smp", "2", "-display", "none", "-vga", "virtio", "-serial", "stdio", "-monitor", "none", "-no-reboot"])
        .arg("-kernel").arg(ctx.kernel_image()).arg("-initrd").arg(private.initramfs())
        .arg("-drive").arg(format!("file={},format=raw,if=virtio,readonly=on", private.root_image().display()))
        .args(["-append", "rdinit=/usr/bin/init panic=5 console=tty0 console=ttyS0,115200 loglevel=4 aftermath.netwait=5 aftermath.test=session-smoke",
            "-device", "virtio-rng-pci", "-nic", "user,model=virtio-net-pci"]);
    if OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok()
    {
        qemu.args(["-enable-kvm", "-cpu", "host"]);
    } else {
        qemu.args(["-cpu", "max"]);
    }
    let mut guest = Guest(
        qemu.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    let mut output = guest.0.stdout.take().context("no guest serial output")?;
    let log_path = work.join("serial.log");
    let mut log = File::create(&log_path)?;
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = output.read(&mut buf) {
            if n == 0 {
                break;
            }
            if log.write_all(&buf[..n]).is_err() {
                break;
            }
            let _ = log.flush();
            if tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut received = Vec::new();
    let mut launched = false;
    let mut success = false;
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(bytes) => received.extend_from_slice(&bytes),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        let text = String::from_utf8_lossy(&received);
        if !launched && text.contains("[root@") && text.contains("]# ") {
            guest
                .0
                .stdin
                .as_mut()
                .context("no guest stdin")?
                .write_all(b"/usr/lib/aftermath/session-smoke\n")?;
            launched = true;
            println!("Session smoke: running Rust guest checks");
        }
        if text.contains("AM_SESSION_SMOKE_OK") {
            success = true;
        }
        if guest.0.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            bail!("session smoke timed out; see {}", log_path.display());
        }
    }
    let _ = reader.join();
    if !success {
        bail!(
            "guest exited without passing session checks; see {}",
            log_path.display()
        );
    }
    println!("Session smoke passed: account creation, interactive/own password changes, password locking, stratum identity sync, two authenticated logins, permissions and final-logout cleanup.");
    println!("Serial log: {}", log_path.display());
    Ok(())
}

/// Put overrides in the actual root image; bootstrap overlays do not replace it.
fn prepare_live_fixture(
    ctx: &Ctx,
    work: &std::path::Path,
    patch: &std::path::Path,
    init: &std::path::Path,
) -> Result<Ctx> {
    let private = Ctx {
        root: ctx.root.clone(),
        out: work.join("build"),
        dry_run: false,
    };
    ctx.remove(&private.out)?;
    let root = private.rootfs();
    crate::image::copy_tree(&ctx.out.join("vm/desktop-smoke/rootfs"), &root)?;
    ctx.run(
        Command::new("bsdtar")
            .args(["-xpf"])
            .arg(patch)
            .args([
                "--no-same-owner",
                "--no-xattrs",
                "--no-acls",
                "--no-fflags",
                "-C",
            ])
            .arg(&root),
    )?;
    fs::copy(init, root.join("usr/bin/aftermath-init"))?;
    fs::copy(
        init.with_file_name("aftermath-early"),
        root.join("usr/lib/aftermath/aftermath-early"),
    )?;
    // This suite tests Aftermath-owned runtimes. The graphical suite covers
    // elogind-owned sessions separately. Do not let D-Bus activation silently
    // switch this fixture to a different runtime owner through optional PAM.
    let login = root.join("etc/pam.d/login");
    let policy = fs::read_to_string(&login)?;
    let policy = policy
        .lines()
        .filter(|line| !line.contains("pam_elogind.so"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(login, format!("{policy}\n"))?;
    crate::live_image::build(&private, &root, init, false)?;
    Ok(private)
}
