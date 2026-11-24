//! `strat`: run a command in a stratum, Bedrock style.
//!
//! * `strat ubuntu apt install foo`   run `apt` inside the ubuntu stratum
//! * `strat arch pacman -Syu`         run in the host (no chroot)
//! * `strat --list` / `strat --which apt`
//! * as a cross launcher (symlink named e.g. `apt-get` in /aftermath/cross/*),
//!   look the name up in /aftermath/cross/index and run it in its stratum.
//!
//! The current directory (if it exists in the stratum, which is true for
//! everything under the shared /home, /root, /tmp, /run), the environment and
//! the uid are kept; only PATH is replaced by the stratum's default PATH.
//!
//! chroot(2) needs root. strat is installed setuid-root (like Bedrock's) so
//! normal users can use cross commands: it chroots, then drops back to the
//! caller's real uid/gid *before* exec'ing anything.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use aftermath_strata::config::Config;
use aftermath_strata::cross::{decide, lookup_index, Action};
use aftermath_strata::{at, CONFIG, CROSS_DIR, STRATUM_PATH};

const USAGE: &str = "\
usage: strat <stratum> <command> [args...]   run a command in a stratum
       strat --list                          list strata
       strat --which <command>               which stratum runs <command>
Cross launchers in /aftermath/cross/{pin,bin} call strat automatically.";

fn die(code: i32, msg: &str) -> ! {
    eprintln!("strat: {msg}");
    std::process::exit(code)
}

fn config() -> Config {
    Config::load(&at(CONFIG)).unwrap_or_else(|e| die(1, &e))
}

fn index() -> String {
    std::fs::read_to_string(at(&format!("{CROSS_DIR}/index"))).unwrap_or_default()
}

/// The caller's PATH without the cross launcher dirs (so the host's own
/// command is found and launchers never call themselves).
fn host_path() -> String {
    let p =
        std::env::var("PATH").unwrap_or_else(|_| "/usr/local/sbin:/usr/local/bin:/usr/bin".into());
    p.split(':')
        .filter(|d| !d.is_empty() && !d.starts_with(CROSS_DIR) && !d.contains("/aftermath/cross/"))
        .collect::<Vec<_>>()
        .join(":")
}

fn find_in(path: &str, root: &Path, cmd: &str) -> Option<PathBuf> {
    if cmd.contains('/') {
        return Some(PathBuf::from(cmd));
    }
    path.split(':').find_map(|d| {
        let p = root.join(d.trim_start_matches('/')).join(cmd);
        p.exists().then(|| Path::new(d).join(cmd))
    })
}

fn exec(prog: &Path, argv0: &str, args: &[String]) -> ! {
    let c = |s: &[u8]| CString::new(s).unwrap_or_else(|_| die(2, "NUL in argument"));
    let prog_c = c(prog.as_os_str().as_bytes());
    let mut argv = vec![c(argv0.as_bytes())];
    argv.extend(args.iter().map(|a| c(a.as_bytes())));
    let err = nix::unistd::execv(&prog_c, &argv).unwrap_err();
    die(126, &format!("{}: {err}", prog.display()))
}

fn run(stratum: &str, cmd: &str, args: &[String]) -> ! {
    let cfg = config();
    let s = cfg.get(stratum).unwrap_or_else(|| {
        die(
            1,
            &format!("unknown stratum `{stratum}` (see strat --list)"),
        )
    });
    if !s.enabled {
        die(1, &format!("stratum `{stratum}` is disabled in {CONFIG}"));
    }

    if s.kind == "host" {
        let path = host_path();
        let prog = find_in(&path, Path::new("/"), cmd).unwrap_or_else(|| {
            die(
                127,
                &format!("{cmd}: not found in the {stratum} (host) stratum"),
            )
        });
        std::env::set_var("PATH", &path);
        exec(&prog, cmd, args);
    }

    let root = at(&s.path);
    if !root.join("usr").is_dir() {
        die(
            1,
            &format!(
                "stratum `{stratum}` is not set up ({} is empty; did aftermath-strata run?)",
                root.display()
            ),
        );
    }
    let prog = find_in(STRATUM_PATH, &root, cmd)
        .unwrap_or_else(|| die(127, &format!("{cmd}: not found in stratum `{stratum}`")));
    // Under AFTERMATH_PREFIX (tests) the host paths carry the prefix.
    let cwd =
        std::env::current_dir()
            .ok()
            .map(|d| match d.strip_prefix(aftermath_strata::prefix()) {
                Ok(rel) => Path::new("/").join(rel),
                Err(_) => d,
            });

    nix::unistd::chroot(&root).unwrap_or_else(|e| {
        die(
            1,
            &format!(
                "chroot {}: {e} (strat must be root or setuid root)",
                root.display()
            ),
        )
    });
    // Drop first, so the caller can only end up in directories it may enter.
    drop_privileges();
    let dir = cwd
        .filter(|d| d.is_dir())
        .unwrap_or_else(|| PathBuf::from("/"));
    if std::env::set_current_dir(&dir).is_err() {
        let _ = std::env::set_current_dir("/");
    }
    std::env::set_var("PATH", STRATUM_PATH);
    std::env::set_var("AFTERMATH_STRATUM", stratum);
    exec(&prog, cmd, args)
}

/// setuid-root invocation by a normal user: go back to the real ids.
fn drop_privileges() {
    use nix::unistd::{getegid, geteuid, getgid, getuid, setresgid, setresuid};
    let (uid, gid) = (getuid(), getgid());
    if geteuid() != uid || getegid() != gid {
        setresgid(gid, gid, gid).unwrap_or_else(|e| die(1, &format!("setresgid: {e}")));
        setresuid(uid, uid, uid).unwrap_or_else(|e| die(1, &format!("setresuid: {e}")));
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    match decide(&argv) {
        Action::Usage => {
            println!("{USAGE}");
        }
        Action::List => {
            let cfg = config();
            let status = std::fs::read_to_string(at("/run/aftermath/strata")).unwrap_or_default();
            for s in &cfg.stratum {
                let state = if !s.enabled {
                    "disabled".to_owned()
                } else if s.kind == "host" {
                    "host".to_owned()
                } else {
                    status
                        .lines()
                        .find_map(|l| l.strip_prefix(&format!("{} ", s.name)).map(str::to_owned))
                        .unwrap_or_else(|| "not started".into())
                };
                println!("{:<10} {:<8} {:<28} {state}", s.name, s.kind, s.path);
            }
        }
        Action::Which(cmd) => match lookup_index(&index(), &cmd) {
            Some(s) => println!("{s}"),
            None => {
                let cfg = config();
                if find_in(&host_path(), Path::new("/"), &cmd).is_some() {
                    println!("{}", cfg.host_name());
                } else {
                    die(1, &format!("{cmd}: not provided by any stratum"));
                }
            }
        },
        Action::Run { stratum, cmd, args } => run_maybe_refresh(&stratum, &cmd, &args),
        Action::Launcher { cmd, args } => {
            let stratum = lookup_index(&index(), &cmd).unwrap_or_else(|| {
                die(
                    127,
                    &format!("{cmd}: no stratum provides it (stale launcher?)"),
                )
            });
            run_maybe_refresh(&stratum, &cmd, &args)
        }
    }
}

/// Package managers that add or remove commands in a (non-host) stratum.
fn is_package_manager(cmd: &str) -> bool {
    matches!(
        cmd,
        "apt" | "apt-get" | "aptitude" | "dpkg" | "pacman" | "dnf" | "apk"
    )
}

/// For package managers run as root: run them as a child, then regenerate
/// the cross launchers so `apt install htop && htop` works right away.
/// Everything else is exec'd directly.
fn run_maybe_refresh(stratum: &str, cmd: &str, args: &[String]) -> ! {
    let host = config().host_name() == stratum;
    let root = nix::unistd::getuid().is_root();
    if host
        || !root
        || !is_package_manager(cmd)
        || std::env::var_os("AFTERMATH_NO_REFRESH").is_some()
    {
        run(stratum, cmd, args);
    }
    let me = std::env::current_exe().unwrap_or_else(|_| PathBuf::from(aftermath_strata::STRAT_BIN));
    let status = std::process::Command::new(me)
        .arg0("strat")
        .arg(stratum)
        .arg(cmd)
        .args(args)
        .env("AFTERMATH_NO_REFRESH", "1")
        .status()
        .unwrap_or_else(|e| die(126, &format!("{cmd}: {e}")));
    let service = at("/usr/lib/aftermath/aftermath-strata");
    if service.exists() {
        let mut c = std::process::Command::new(service);
        c.arg("refresh").stdout(std::process::Stdio::null());
        if let Ok(bin) = std::env::var("AFTERMATH_STRAT_BIN") {
            c.args(["--strat-bin", &bin]);
        }
        let _ = c.status();
    }
    std::process::exit(status.code().unwrap_or(1))
}
