//! End-to-end graphical test using a disposable RAM guest and QMP input.
use crate::ctx::Ctx;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct Guest(Child);
impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn qmp(stream: &mut UnixStream, reader: &mut BufReader<UnixStream>, command: Value) -> Result<()> {
    writeln!(stream, "{command}")?;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            bail!("QMP disconnected");
        }
        let value: Value = serde_json::from_str(&line)?;
        if let Some(error) = value.get("error") {
            bail!("QMP: {error}");
        }
        if value.get("return").is_some() {
            return Ok(());
        }
    }
}

fn screenshot(path: &std::path::Path) -> Result<bool> {
    let mut input = BufReader::new(File::open(path)?);
    let mut magic = String::new();
    input.read_line(&mut magic)?;
    let mut size = String::new();
    input.read_line(&mut size)?;
    let mut maximum = String::new();
    input.read_line(&mut maximum)?;
    if magic.trim() != "P6" || maximum.trim() != "255" {
        bail!("unexpected QMP screenshot format");
    }
    let dimensions: Vec<u32> = size
        .split_whitespace()
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    if dimensions.len() != 2 {
        bail!("invalid screenshot size");
    }
    let mut pixels = Vec::new();
    input.read_to_end(&mut pixels)?;
    let mut encoder = png::Encoder::new(
        File::create(path.with_extension("png"))?,
        dimensions[0],
        dimensions[1],
    );
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&pixels)?;
    // A black compositor with only a cursor must not pass as a working desktop.
    Ok(pixels
        .as_chunks::<3>()
        .0
        .iter()
        .filter(|p| p.iter().any(|c| *c > 16))
        .count()
        * 20
        > pixels.len() / 3)
}

pub fn run(ctx: &Ctx, use_grub: bool, bios: bool) -> Result<()> {
    if ctx.dry_run {
        println!("Build Plasma and Rust probes, assemble a private initramfs, boot QEMU without saved disks, verify greeter/login/Wayland/logout.");
        return Ok(());
    }
    let init = crate::image::build_init(ctx)?;
    let stage = crate::plasma::ensure(ctx)?;
    let runtime = crate::plasma::runtime(ctx)?;
    let mut build = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    build
        .current_dir(&ctx.root)
        .args([
            "build",
            "--release",
            "-p",
            "aftermath-pam",
            "--example",
            "desktop-smoke",
            "--target",
            "x86_64-unknown-linux-gnu",
        ])
        .env("RUSTFLAGS", "");
    ctx.run(&mut build)?;
    let work = ctx.out.join("vm/desktop-smoke");
    fs::create_dir_all(&work)?;
    let patch = work.join("overlay");
    if patch.exists() {
        fs::remove_dir_all(&patch)?;
    }
    fs::create_dir_all(patch.join("usr/lib/aftermath"))?;
    fs::create_dir_all(patch.join("usr/lib/security"))?;
    fs::create_dir_all(patch.join("usr/bin"))?;
    for (source, target) in [
        (init.clone(), "usr/bin/aftermath-init"),
        (
            init.with_file_name("aftermath-engine"),
            "usr/lib/aftermath/aftermath-engine",
        ),
        (
            init.with_file_name("aftermath-strata"),
            "usr/lib/aftermath/aftermath-strata",
        ),
        (
            init.with_file_name("aftermath-early"),
            "usr/lib/aftermath/aftermath-early",
        ),
        (init.with_file_name("strat"), "usr/lib/aftermath/strat"),
        (
            init.with_file_name("aftermath-desktop-session"),
            "usr/lib/aftermath/aftermath-desktop-session",
        ),
        (
            init.with_file_name("aftermath-desktop-defaults"),
            "usr/lib/aftermath/aftermath-desktop-defaults",
        ),
        (
            init.with_file_name("aftermath-greeter"),
            "usr/lib/aftermath/aftermath-greeter",
        ),
        (init.with_file_name("amctl"), "usr/bin/amctl"),
        (
            init.with_file_name("libpam_aftermath.so"),
            "usr/lib/security/pam_aftermath.so",
        ),
        (
            init.parent()
                .context("no binary directory")?
                .join("examples/desktop-smoke"),
            "usr/lib/aftermath/desktop-smoke",
        ),
    ] {
        fs::copy(source, patch.join(target))?;
        fs::set_permissions(patch.join(target), fs::Permissions::from_mode(0o755))?;
    }
    for name in ["init", "poweroff", "reboot", "halt"] {
        symlink("aftermath-init", patch.join("usr/bin").join(name))?;
    }
    fs::set_permissions(
        patch.join("usr/lib/aftermath/strat"),
        fs::Permissions::from_mode(0o4755),
    )?;
    symlink("../lib/aftermath/strat", patch.join("usr/bin/strat"))?;
    fs::create_dir_all(patch.join("usr/lib/aftermath/rescue"))?;
    fs::copy(
        runtime.join("usr/bin/busybox"),
        patch.join("usr/lib/aftermath/rescue/busybox"),
    )?;
    fs::create_dir_all(patch.join("etc/aftermath"))?;
    fs::write(patch.join("etc/aftermath/version"), crate::ctx::VERSION)?;
    fs::write(
        patch.join("etc/aftermath/build-id"),
        "private-desktop-smoke\n",
    )?;
    let merged = work.join("rootfs");
    if merged.is_dir() {
        crate::plasma::readable_tree(&merged)?;
    }
    ctx.remove(&merged)?;
    fs::create_dir_all(&merged)?;
    // Match a fresh production root: the full signed package closure, local
    // source installs, Aftermath configuration/binaries and current modules.
    for source in [
        runtime,
        stage,
        ctx.config().join("overlay"),
        ctx.kernel_modules(),
        patch,
    ] {
        crate::image::copy_tree(&source, &merged)?;
    }
    let private = assemble_live_boot(ctx, &work, &merged, &init)?;
    let boot = if use_grub {
        crate::grub::build_tree(&private)?;
        crate::grub::build_disk(
            &private,
            &[("aftermath_test".into(), "desktop-smoke".into())],
        )?;
        Some(crate::grub::disk_image(&private))
    } else {
        None
    };
    let socket = work.join("qmp.sock");
    let _ = fs::remove_file(&socket);
    let mut command = Command::new("qemu-system-x86_64");
    crate::vm_disk::audio(&mut command, "none");
    command
        .args([
            "-machine",
            "q35",
            "-m",
            "4G",
            "-smp",
            "4",
            "-display",
            "none",
            "-vga",
            "virtio",
            "-serial",
            "stdio",
            "-monitor",
            "none",
            "-no-reboot",
        ])
        .arg("-qmp")
        .arg(format!("unix:{},server=on,wait=off", socket.display()))
        .args([
            "-device",
            "virtio-rng-pci",
            "-device",
            "qemu-xhci",
            "-device",
            "usb-tablet",
            "-nic",
            "user,model=virtio-net-pci",
        ]);
    if let Some(boot) = boot {
        if !bios {
            let code = crate::deps::OVMF_CODE
                .iter()
                .find(|p| std::path::Path::new(p).is_file())
                .context("OVMF firmware is missing (install edk2-ovmf)")?;
            let template = crate::deps::OVMF_VARS
                .iter()
                .find(|p| std::path::Path::new(p).is_file())
                .context("OVMF variables template is missing")?;
            let vars = work.join("OVMF_VARS.fd");
            fs::copy(template, &vars)?;
            command
                .arg("-drive")
                .arg(format!(
                    "if=pflash,format=raw,unit=0,readonly=on,file={code}"
                ))
                .arg("-drive")
                .arg(format!(
                    "if=pflash,format=raw,unit=1,file={}",
                    vars.display()
                ));
        }
        command
            .arg("-drive")
            .arg(format!(
                "file={},format=raw,if=none,id=bootdisk,snapshot=on",
                boot.display()
            ))
            .args(["-device", "virtio-blk-pci,drive=bootdisk,bootindex=0"]);
    } else {
        command.arg("-kernel").arg(ctx.kernel_image()).arg("-initrd").arg(private.initramfs())
            .args(["-append", "rdinit=/usr/bin/init panic=5 console=tty0 console=ttyS0,115200 loglevel=4 aftermath.netwait=2 aftermath.test=desktop-smoke"]);
        command.arg("-drive").arg(format!(
            "file={},format=raw,if=virtio,readonly=on",
            private.root_image().display()
        ));
    }
    if OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok()
    {
        command.args(["-enable-kvm", "-cpu", "host"]);
    } else {
        command.args(["-cpu", "max"]);
    }
    let mut guest = Guest(
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    let mut serial = guest.0.stdout.take().context("no serial stream")?;
    let log_path = work.join("serial.log");
    let mut log = File::create(&log_path)?;
    let (tx, rx) = mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        while let Ok(n) = serial.read(&mut buffer) {
            if n == 0 {
                break;
            }
            let _ = log.write_all(&buffer[..n]);
            let _ = log.flush();
            if tx.send(buffer[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let start = Instant::now();
    let mut stream = loop {
        match UnixStream::connect(&socket) {
            Ok(s) => break s,
            Err(_) if start.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(50))
            }
            Err(e) => return Err(e.into()),
        }
    };
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut greeting = String::new();
    reader.read_line(&mut greeting)?;
    qmp(
        &mut stream,
        &mut reader,
        json!({"execute":"qmp_capabilities"}),
    )?;
    let mut received = Vec::new();
    let mut probe = false;
    let mut login = false;
    let mut logout = false;
    let mut passed = false;
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(data) => received.extend_from_slice(&data),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
        let text = String::from_utf8_lossy(&received);
        if !probe && text.contains("[root@") && text.contains("]# ") {
            guest
                .0
                .stdin
                .as_mut()
                .context("no guest input")?
                .write_all(b"/usr/lib/aftermath/desktop-smoke\n")?;
            probe = true;
            println!("Graphical smoke: checking credentials and waiting for SDDM");
        }
        if !login && text.contains("AM_GREETER_READY") {
            std::thread::sleep(Duration::from_secs(3));
            qmp(
                &mut stream,
                &mut reader,
                json!({"execute":"screendump", "arguments":{"filename": work.join("login.ppm")}}),
            )?;
            screenshot(&work.join("login.ppm"))?;
            for key in ["b", "o", "b", "ret"] {
                qmp(
                    &mut stream,
                    &mut reader,
                    json!({"execute":"send-key", "arguments":{"keys":[{"type":"qcode", "data": key}], "hold-time":50}}),
                )?;
                std::thread::sleep(Duration::from_millis(150));
            }
            login = true;
            println!("Graphical smoke: logging Bob in through the greeter");
        }
        if !logout && text.contains("AM_PLASMA_READY") {
            std::thread::sleep(Duration::from_secs(4));
            let deadline = Instant::now() + Duration::from_secs(30);
            let visible = loop {
                qmp(
                    &mut stream,
                    &mut reader,
                    json!({"execute":"screendump", "arguments":{"filename": work.join("plasma.ppm")}}),
                )?;
                if screenshot(&work.join("plasma.ppm"))? {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(500));
            };
            println!("Graphical smoke: Plasma Wayland and its D-Bus services are running as Bob; checking logout");
            guest
                .0
                .stdin
                .as_mut()
                .context("no guest input")?
                .write_all(if visible {
                    b"logout\n"
                } else {
                    b"diagnostics\n"
                })?;
            logout = true;
        }
        if text.contains("AM_DESKTOP_SMOKE_OK") {
            passed = true;
        }
        if guest.0.try_wait()?.is_some() {
            break;
        }
        if start.elapsed() > Duration::from_secs(300) {
            bail!("graphical smoke timed out; see {}", log_path.display());
        }
    }
    let _ = reader_thread.join();
    if !passed {
        bail!("graphical smoke failed; see {}", log_path.display());
    }
    println!("Graphical smoke passed: temporary Bob/root credentials, SDDM, real Plasma Wayland login, unprivileged desktop, seat/session registration, user D-Bus and logout cleanup.");
    println!("Serial log: {}", log_path.display());
    Ok(())
}

/// Exercise the production bootstrap/SquashFS builder without host root access.
fn assemble_live_boot(
    ctx: &Ctx,
    work: &std::path::Path,
    root: &std::path::Path,
    init: &std::path::Path,
) -> Result<Ctx> {
    let private = Ctx {
        root: ctx.root.clone(),
        out: work.join("boot"),
        dry_run: false,
    };
    ctx.remove(&private.out)?;
    fs::create_dir_all(
        private
            .kernel_image()
            .parent()
            .context("no kernel directory")?,
    )?;
    fs::hard_link(ctx.kernel_image(), private.kernel_image())?;
    crate::live_image::build(&private, root, init, false)?;
    Ok(private)
}
