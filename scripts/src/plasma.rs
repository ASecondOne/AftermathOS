//! Build the local Plasma checkouts and standalone login1 provider. Everything
//! installs into DESTDIR; the build host's desktop is never overwritten.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::ctx::{is_stale, mtime, which, Ctx};

const MODULES: &[&str] = &[
    "libplasma",
    "kwin",
    "breeze",
    "plasma-workspace",
    "plasma-desktop",
];
const ELOGIND_TAG: &str = "v257.16";
const ELOGIND_COMMIT: &str = "1b62c6de143abafee70ab66b0470b4b6cbe3f798";

pub fn enabled(ctx: &Ctx) -> Result<bool> {
    let config = fs::read_to_string(ctx.config().join("overlay/etc/aftermath/aftermath.toml"))?;
    Ok(aftermath_ctl::graphical::parse(&config)
        .map_err(anyhow::Error::msg)?
        .enabled)
}

pub fn stage(ctx: &Ctx) -> PathBuf {
    ctx.out.join("plasma/stage")
}

fn source_mtime(path: &Path) -> Option<std::time::SystemTime> {
    let mut newest = mtime(path);
    if path.is_dir() && !path.is_symlink() {
        for entry in fs::read_dir(path).into_iter().flatten().flatten() {
            if entry.file_name() != ".git" {
                newest = newest.max(source_mtime(&entry.path()));
            }
        }
    }
    newest
}

fn tool_environment(ctx: &Ctx) -> Result<Vec<(String, std::ffi::OsString)>> {
    let tools = ctx.out.join("plasma/tools");
    let mut paths = vec![tools.join("usr/bin")];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let python = [tools.join("usr/lib"), PathBuf::from("/usr/lib")]
        .into_iter()
        .flat_map(|dir| fs::read_dir(dir).into_iter().flatten().flatten())
        .filter(|e| e.file_name().to_string_lossy().starts_with("python"))
        .map(|e| e.path().join("site-packages"))
        .collect::<Vec<_>>();
    let mut perl = vec![
        tools.join("usr/share/perl5/vendor_perl"),
        tools.join("usr/share/perl5/core_perl"),
    ];
    for version in fs::read_dir(tools.join("usr/lib/perl5"))
        .into_iter()
        .flatten()
        .flatten()
    {
        for sub in ["vendor_perl", "core_perl", "site_perl"] {
            perl.push(version.path().join(sub));
        }
    }
    Ok(vec![
        ("PATH".into(), std::env::join_paths(paths)?),
        ("PYTHONPATH".into(), std::env::join_paths(python)?),
        ("PERL5LIB".into(), std::env::join_paths(perl)?),
    ])
}

fn logged(ctx: &Ctx, command: &mut Command, name: &str) -> Result<()> {
    command.envs(tool_environment(ctx)?);
    let path = ctx.out.join("plasma/logs").join(format!("{name}.log"));
    ctx.mkdir(path.parent().context("no log directory")?)?;
    println!("Plasma: {name} (log: {})", path.display());
    if !ctx.dry_run {
        let log = File::create(&path)?;
        command
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
    }
    if ctx.dry_run {
        return ctx.run(command);
    }
    let status = command.status().with_context(|| format!("start {name}"))?;
    if !status.success() {
        bail!("{name} failed ({status}); see {}", path.display());
    }
    Ok(())
}

pub fn ensure(ctx: &Ctx) -> Result<PathBuf> {
    let stamp = ctx.out.join("plasma/built");
    let mut inputs = vec![
        mtime(&ctx.root.join("scripts/src/plasma.rs")),
        mtime(Path::new("/var/lib/pacman/local")),
    ];
    for module in MODULES {
        let source = ctx.root.join("plasma").join(module);
        inputs.push(source_mtime(&source));
        inputs.push(mtime(&source.join(".git/logs/HEAD")));
    }
    if !is_stale(&stamp, &inputs)
        && stage(ctx).join("usr/bin/startplasma-wayland").is_file()
        && stage(ctx).join("usr/lib/elogind/elogind").is_file()
    {
        return Ok(stage(ctx));
    }
    build(ctx)
}

pub fn build(ctx: &Ctx) -> Result<PathBuf> {
    ctx.mkdir(&ctx.out.join("plasma"))?;
    let _lock = if ctx.dry_run {
        None
    } else {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(ctx.out.join("plasma/build.lock"))?;
        file.try_lock()
            .context("another Plasma build is already using out/plasma")?;
        Some(file)
    };
    for module in MODULES {
        if !ctx
            .root
            .join("plasma")
            .join(module)
            .join("CMakeLists.txt")
            .is_file()
        {
            bail!("missing local Plasma source: plasma/{module} (clone KDE/{module} from GitHub)");
        }
    }
    let tools = ctx.out.join("plasma/tools");
    let missing: Vec<_> = ["meson", "gperf", "intltool-merge"]
        .iter()
        .filter(|name| which(name).is_none() && !tools.join("usr/bin").join(name).is_file())
        .map(|name| {
            if *name == "intltool-merge" {
                "intltool"
            } else {
                *name
            }
        })
        .map(str::to_owned)
        .collect();
    if !missing.is_empty() {
        packages(ctx, &missing, &tools)?;
    }
    let jinja = [tools.join("usr/lib"), PathBuf::from("/usr/lib")]
        .into_iter()
        .any(|dir| {
            fs::read_dir(dir).into_iter().flatten().flatten().any(|e| {
                e.file_name().to_string_lossy().starts_with("python")
                    && e.path().join("site-packages/jinja2/__init__.py").is_file()
            })
        });
    if !jinja {
        packages(ctx, &["python-jinja".into()], &tools)?;
    }
    let dest = stage(ctx);
    // Install the complete selected source set afresh, retaining incremental
    // object builds but never mixing leftover files from another source branch.
    ctx.remove(&dest)?;
    ctx.mkdir(&dest)?;
    let jobs = std::thread::available_parallelism().map_or(4, |n| n.get().min(16));
    build_elogind(ctx, &dest, jobs)?;
    for module in MODULES {
        let dir = ctx.out.join("plasma/build").join(module);
        let mut configure = Command::new("cmake");
        configure
            .arg("-S")
            .arg(ctx.root.join("plasma").join(module))
            .arg("-B")
            .arg(&dir)
            .args([
                "-G",
                "Ninja",
                "-U",
                "*_DIR",
                "-DCMAKE_INSTALL_PREFIX=/usr",
                "-DCMAKE_BUILD_TYPE=Release",
                "-DBUILD_TESTING=OFF",
                "-Wno-dev",
            ])
            .arg(format!(
                "-DCMAKE_PREFIX_PATH={}",
                dest.join("usr").display()
            ));
        if *module == "breeze" {
            configure.args(["-DBUILD_QT5=OFF", "-DBUILD_QT6=ON"]);
        }
        logged(ctx, &mut configure, &format!("{module}-configure"))?;
        logged(
            ctx,
            Command::new("cmake")
                .arg("--build")
                .arg(&dir)
                .arg("--parallel")
                .arg(jobs.to_string()),
            &format!("{module}-build"),
        )?;
        logged(
            ctx,
            Command::new("cmake")
                .arg("--install")
                .arg(&dir)
                .env("DESTDIR", &dest),
            &format!("{module}-install"),
        )?;
    }
    if !ctx.dry_run {
        fs::write(
            ctx.out.join("plasma/built"),
            "local Plasma sources + elogind v257.16\n",
        )?;
    }
    Ok(dest)
}

pub fn runtime(ctx: &Ctx) -> Result<PathBuf> {
    let dest = ctx.out.join("plasma/runtime");
    let list = ctx.config().join("packages.x86_64");
    let stamp = ctx.out.join("plasma/runtime-built");
    if dest.join("usr/bin/sddm").is_file()
        && !is_stale(
            &stamp,
            &[
                mtime(&list),
                mtime(Path::new("/var/lib/pacman/sync/extra.db")),
                mtime(Path::new("/var/lib/pacman/sync/core.db")),
            ],
        )
    {
        readable_tree(&dest)?;
        return Ok(dest);
    }
    let names: Vec<String> = fs::read_to_string(list)?
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    packages(ctx, &names, &dest)?;
    if !ctx.dry_run {
        fs::write(stamp, "verified Arch runtime closure\n")?;
    }
    Ok(dest)
}

// Some Arch files are shipped execute-only until an install hook fixes their
// ownership/mode. The unprivileged test packer must read them. Only owner access
// is added; archive ownership is root and the guest applies the helper's policy.
pub(crate) fn readable_tree(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        if metadata.permissions().mode() & 0o700 != 0o700 {
            fs::set_permissions(
                path,
                fs::Permissions::from_mode(metadata.permissions().mode() | 0o700),
            )?;
        }
        for entry in fs::read_dir(path)? {
            readable_tree(&entry?.path())?;
        }
    } else if metadata.is_file() && metadata.permissions().mode() & 0o400 == 0 {
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(metadata.permissions().mode() | 0o400),
        )?;
    }
    Ok(())
}

/// Download Arch's signed packages to a user-owned tree. Used for missing build
/// tools, and for the private graphical integration guest (no host installation).
pub fn packages(ctx: &Ctx, names: &[String], dest: &Path) -> Result<()> {
    ctx.mkdir(dest)?;
    if ctx.dry_run {
        println!("Download and verify Arch packages: {}", names.join(" "));
        return Ok(());
    }
    // Resolve against an empty local database: the host may have every KDE
    // dependency already installed, while the guest still needs all of them.
    let database = ctx.out.join("plasma/resolve-db");
    let lookup = ctx.out.join("plasma/url-lookup");
    fs::create_dir_all(database.join("local"))?;
    fs::create_dir_all(&lookup)?;
    if !database.join("sync").exists() {
        std::os::unix::fs::symlink("/var/lib/pacman/sync", database.join("sync"))?;
    }
    let output = Command::new("pacman")
        .args(["-Sp", "--dbpath"])
        .arg(&database)
        .arg("--cachedir")
        .arg(&lookup)
        .args(["--print-format", "%n %l"])
        .args(names)
        .output()?;
    if !output.status.success() {
        bail!(
            "resolve packages: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let cache = ctx.out.join("plasma/packages");
    fs::create_dir_all(&cache)?;
    let keyring = ctx.out.join("plasma/archlinux.gpg");
    if is_stale(
        &keyring,
        &[mtime(Path::new("/usr/share/pacman/keyrings/archlinux.gpg"))],
    ) {
        let homedir = ctx.out.join("plasma/gpg");
        fs::create_dir_all(&homedir)?;
        fs::set_permissions(&homedir, fs::Permissions::from_mode(0o700))?;
        ctx.run(
            Command::new("gpg")
                .arg("--homedir")
                .arg(homedir)
                .args(["--batch", "--yes", "--dearmor", "--output"])
                .arg(&keyring)
                .arg("/usr/share/pacman/keyrings/archlinux.gpg"),
        )?;
    }
    for line in String::from_utf8(output.stdout)?.lines() {
        let (name, url) = line
            .split_once(' ')
            .context("unexpected pacman package URL")?;
        if !url.starts_with("https://") {
            bail!("package {name} needs an HTTPS mirror in pacman.conf");
        }
        let filename = url.rsplit('/').next().context("package has no filename")?;
        let file = cache.join(filename);
        let sig = cache.join(format!("{filename}.sig"));
        for (url, path) in [(url.to_owned(), &file), (format!("{url}.sig"), &sig)] {
            if !path.exists() {
                let cached = Path::new("/var/cache/pacman/pkg")
                    .join(path.file_name().context("no package filename")?);
                if cached.is_file() {
                    fs::copy(cached, path)?;
                    continue;
                }
                let partial = path.with_extension("partial");
                ctx.run(
                    Command::new("curl")
                        .args([
                            "--fail",
                            "--location",
                            "--retry",
                            "2",
                            "--silent",
                            "--show-error",
                            "--output",
                        ])
                        .arg(&partial)
                        .arg(url),
                )?;
                fs::rename(partial, path)?;
            }
        }
        let verified = Command::new("gpgv")
            .arg("--keyring")
            .arg(&keyring)
            .arg(&sig)
            .arg(&file)
            .output()?;
        if !verified.status.success() {
            bail!(
                "Arch signature verification failed for {name}: {}",
                String::from_utf8_lossy(&verified.stderr)
            );
        }
        let extracted = Command::new("bsdtar")
            .args(["-xpf"])
            .arg(&file)
            .arg("-C")
            .arg(dest)
            .args([
                "--no-same-owner",
                "--no-xattrs",
                "--no-acls",
                "--no-fflags",
                "--exclude=.PKGINFO",
                "--exclude=.BUILDINFO",
                "--exclude=.MTREE",
                "--exclude=.INSTALL",
            ])
            .output()?;
        if !extracted.status.success() {
            bail!(
                "extract {name}: {}",
                String::from_utf8_lossy(&extracted.stderr)
            );
        }
        println!("Plasma dependency: {name}");
    }
    readable_tree(dest)
}

fn build_elogind(ctx: &Ctx, dest: &Path, jobs: usize) -> Result<()> {
    let source = ctx.out.join("plasma/elogind");
    if !source.join("meson.build").exists() {
        ctx.run(
            Command::new("git")
                .args([
                    "clone",
                    "--depth",
                    "1",
                    "--branch",
                    ELOGIND_TAG,
                    "--recurse-submodules",
                    "--shallow-submodules",
                    "https://github.com/elogind/elogind.git",
                ])
                .arg(&source),
        )?;
    }
    if !ctx.dry_run {
        let revision = Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["rev-parse", "HEAD"])
            .output()?;
        if String::from_utf8_lossy(&revision.stdout).trim() != ELOGIND_COMMIT {
            bail!("unexpected elogind source revision; expected {ELOGIND_COMMIT}");
        }
    }
    let tools = ctx.out.join("plasma/tools");
    let missing: Vec<_> = ["meson", "gperf"]
        .iter()
        .filter(|name| which(name).is_none() && !tools.join("usr/bin").join(name).is_file())
        .map(|s| (*s).to_owned())
        .collect();
    if !missing.is_empty() {
        packages(ctx, &missing, &tools)?;
    }
    let mut paths = vec![tools.join("usr/bin")];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths)?;
    let python = fs::read_dir(tools.join("usr/lib"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("python"))
        .map(|e| e.path().join("site-packages"))
        .collect::<Vec<_>>();
    let python = std::env::join_paths(python)?;
    let dir = ctx.out.join("plasma/build/elogind");
    let mut configure = Command::new("meson");
    configure.arg("setup");
    if dir.join("build.ninja").exists() {
        configure.arg("--reconfigure");
    }
    configure
        .arg(&dir)
        .arg(&source)
        .args([
            "--prefix=/usr",
            "--libdir=lib",
            "--libexecdir=lib/elogind",
            "--sysconfdir=/etc",
            "--buildtype=release",
            "-Dpam=enabled",
            "-Dpamlibdir=/usr/lib/security",
            "-Dman=disabled",
            "-Dhtml=disabled",
            "-Dtests=false",
            "-Dinstall-tests=false",
            "-Dcgroup-controller=elogind",
            "-Ddefault-hierarchy=unified",
            "-Dhalt-path=/usr/bin/halt",
            "-Dpoweroff-path=/usr/bin/poweroff",
            "-Dreboot-path=/usr/bin/reboot",
        ])
        .env("PATH", &path)
        .env("PYTHONPATH", &python);
    logged(ctx, &mut configure, "elogind-configure")?;
    logged(
        ctx,
        Command::new("ninja")
            .arg("-C")
            .arg(&dir)
            .arg(format!("-j{jobs}"))
            .env("PATH", &path)
            .env("PYTHONPATH", &python),
        "elogind-build",
    )?;
    logged(
        ctx,
        Command::new("meson")
            .arg("install")
            .arg("-C")
            .arg(&dir)
            .arg("--no-rebuild")
            .env("DESTDIR", dest)
            .env("PATH", path)
            .env("PYTHONPATH", python),
        "elogind-install",
    )?;
    Ok(())
}
