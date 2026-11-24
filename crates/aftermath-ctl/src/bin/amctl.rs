//! amctl - Aftermath control: the single front door to the running system
//! (the systemctl of Aftermath).
//!
//! Talks to aftermath-engine over /run/aftermath/control.sock. Anyone may
//! ask (status, list, logs); changing things needs root. If the engine or
//! its socket is not there (BusyBox rescue, engine crashed), amctl says so
//! and does what it can by itself: status from /proc and /run, logs, boot
//! next, overlay reset, keymap, and poweroff/reboot via PID 1 or the reboot
//! syscall (plan B).

use std::fs::{self, File};
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use aftermath_ctl::proto::{self, Request};
use aftermath_ctl::services::load_dir;
use aftermath_ctl::{facts, grubenv, locale, overlay, SERVICES_DIR, SOCKET, STRAT_BIN};
use clap::{CommandFactory, Parser, Subcommand};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(
    name = "amctl",
    bin_name = "amctl",
    version,
    about = "Aftermath control: status, strata, services, logs, boot and power",
    after_help = "Examples:\n  amctl status\n  amctl strata list\n  amctl strata enter ubuntu\n  \
                  amctl services restart heartbeat\n  amctl logs -f --service heartbeat\n  \
                  amctl boot next host\n  amctl overlay reset"
)]
struct Cli {
    /// Machine-readable JSON output (for scripts)
    #[arg(long, global = true)]
    json: bool,
    /// No colours
    #[arg(long, global = true)]
    no_color: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Overview: mode, root/overlay, network, strata, services, uptime
    Status,
    /// Strata (Arch host, Ubuntu, ...)
    #[command(subcommand)]
    Strata(StrataCmd),
    /// Services from /etc/aftermath/services/*.toml
    #[command(subcommand, visible_alias = "svc")]
    Services(SvcCmd),
    /// Session logs of this boot (consoles, kernel, services)
    Logs(LogsArgs),
    /// Power off cleanly
    Poweroff,
    /// Reboot cleanly
    Reboot,
    /// GRUB: pick the next boot once
    #[command(subcommand)]
    Boot(BootCmd),
    /// The persistent host root (overlay disk)
    #[command(subcommand)]
    Overlay(OverlayCmd),
    /// Keyboard layout: show, or `amctl keymap set <name>`
    Keymap {
        #[command(subcommand)]
        cmd: Option<KeymapCmd>,
    },
    /// Subsystems the engine offers
    Subsystems,
    /// Print shell completions (bash, zsh, fish, ...)
    Completions { shell: clap_complete::Shell },
}

#[derive(Subcommand)]
enum StrataCmd {
    /// All strata and their state
    List,
    /// One stratum in detail
    Status {
        name: String,
    },
    Start {
        name: String,
    },
    Stop {
        name: String,
    },
    Restart {
        name: String,
    },
    /// Start now and at every boot
    Enable {
        name: String,
    },
    /// Stop now and keep it off at boot
    Disable {
        name: String,
    },
    /// A shell (or a command) inside a stratum
    Enter {
        name: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
}

#[derive(Subcommand)]
enum SvcCmd {
    List,
    Status {
        name: String,
    },
    Start {
        name: String,
    },
    Stop {
        name: String,
    },
    Restart {
        name: String,
    },
    /// Start now and at every boot
    Enable {
        name: String,
    },
    /// Stop now and keep it off at boot
    Disable {
        name: String,
    },
    /// Re-read /etc/aftermath/services
    Reload,
}

#[derive(Subcommand)]
enum BootCmd {
    /// normal | host | busybox | clean | serial | debug | none (cancel)
    Next { target: String },
    /// What GRUB will do next
    Status,
}

#[derive(Subcommand)]
enum OverlayCmd {
    Status,
    /// Throw away all saved changes on the next boot
    Reset {
        /// Withdraw a pending reset
        #[arg(long)]
        cancel: bool,
    },
}

#[derive(Subcommand)]
enum KeymapCmd {
    /// Load a console keymap now and save it in /etc/aftermath/aftermath.toml
    Set { name: String },
}

#[derive(clap::Args)]
struct LogsArgs {
    /// Keep printing new lines
    #[arg(short, long)]
    follow: bool,
    /// Number of lines to show
    #[arg(short = 'n', long, default_value_t = 40)]
    lines: usize,
    /// A service's log
    #[arg(long, conflicts_with_all = ["kernel", "console"])]
    service: Option<String>,
    /// The whole kernel log (default: only Aftermath's own messages)
    #[arg(long, conflicts_with = "console")]
    kernel: bool,
    /// A console's log, e.g. tty1 or ttyS0
    #[arg(long)]
    console: Option<String>,
    /// List the log files of this boot
    #[arg(long)]
    list: bool,
}

// -------------------------------------------------------------- style ----

struct Style {
    on: bool,
}

impl Style {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    }
    fn red(&self, s: &str) -> String {
        self.paint("1;31", s)
    }
    fn purple(&self, s: &str) -> String {
        self.paint("1;35", s)
    }
    fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    fn header(&self, title: &str) {
        println!("{} {}", self.red("AFTERMATH"), self.purple(title));
    }
    fn row(&self, key: &str, val: &str) {
        println!("  {}  {val}", self.purple(&format!("{key:<10}")));
    }
    /// running/up -> purple, failed -> red, everything else dim.
    fn state(&self, s: &str) -> String {
        if s == "running" || s == "up" {
            self.purple(s)
        } else if s.starts_with("failed") || s == "down" || s == "restarting" {
            self.red(s)
        } else {
            self.dim(s)
        }
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

// --------------------------------------------------------------- main ----

struct Ctx {
    json: bool,
    st: Style,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let color = !cli.no_color
        && !cli.json
        && std::env::var_os("NO_COLOR").is_none()
        && std::io::stdout().is_terminal();
    let cx = Ctx {
        json: cli.json,
        st: Style { on: color },
    };
    match run(&cx, cli.cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if cx.json {
                println!("{}", json!({"ok": false, "error": e}));
            } else {
                eprintln!("{} {e}", cx.st.red("amctl:"));
            }
            ExitCode::FAILURE
        }
    }
}

/// Ask the engine. Ok(None) = engine not reachable (caller falls back).
fn ask(cx: &Ctx, subsystem: &str, verb: &str, args: &[&str]) -> Result<Option<Value>, String> {
    let req = Request {
        subsystem: subsystem.into(),
        verb: verb.into(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
    };
    match proto::call(SOCKET, &req) {
        Ok(r) if r.ok => Ok(Some(r.data)),
        Ok(r) => Err(r.error.unwrap_or_else(|| "the engine refused".into())),
        Err(e) => {
            if !cx.json {
                eprintln!(
                    "{} the engine is not reachable ({SOCKET}: {e}); working offline",
                    cx.st.red("amctl:")
                );
            }
            Ok(None)
        }
    }
}

fn need_engine(what: &str) -> String {
    format!(
        "`amctl {what}` needs the engine, which is not running \
         (BusyBox rescue or the engine crashed; see `amctl logs`)"
    )
}

fn need_root(what: &str) -> Result<(), String> {
    if is_root() {
        Ok(())
    } else {
        Err(format!("`amctl {what}` changes the system and needs root"))
    }
}

fn emit(cx: &Ctx, v: &Value, human: impl FnOnce(&Value)) {
    if cx.json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    } else {
        human(v);
    }
}

fn run(cx: &Ctx, cmd: Cmd) -> Result<(), String> {
    match cmd {
        Cmd::Status => {
            let v = match ask(cx, "system", "status", &[])? {
                Some(v) => v,
                None => {
                    let mut v = facts::collect();
                    v["engine"] = json!({"running": false});
                    v
                }
            };
            emit(cx, &v, |v| print_status(&cx.st, v));
        }
        Cmd::Strata(c) => strata(cx, c)?,
        Cmd::Services(c) => services(cx, c)?,
        Cmd::Logs(a) => logs(cx, a)?,
        Cmd::Poweroff => power(cx, "poweroff")?,
        Cmd::Reboot => power(cx, "reboot")?,
        Cmd::Boot(BootCmd::Next { target }) => {
            let v = match ask(cx, "boot", "next", &[&target])? {
                Some(v) => v,
                None => {
                    need_root("boot next")?;
                    let dev = grubenv::set_next(&target)?;
                    json!({"next": target, "device": dev})
                }
            };
            emit(cx, &v, |v| {
                let t = s(v, "next");
                if t == "none" {
                    println!("next boot: {}", cx.st.purple("the normal menu default"));
                } else {
                    println!(
                        "next boot (once): {}  {}",
                        cx.st.purple(t),
                        cx.st.dim(&format!("grubenv on {}", s(v, "device")))
                    );
                }
            });
        }
        Cmd::Boot(BootCmd::Status) => {
            let v = ask(cx, "boot", "status", &[])?.ok_or_else(|| need_engine("boot status"))?;
            emit(cx, &v, |v| {
                cx.st.header("boot");
                if v.get("device").is_some_and(Value::is_null) {
                    cx.st.row("grubenv", &cx.st.dim(s(v, "note")));
                    return;
                }
                cx.st.row("grubenv", s(v, "device"));
                let next = s(v, "next");
                cx.st.row(
                    "next",
                    &if next.is_empty() {
                        "menu default".into()
                    } else {
                        cx.st.purple(&format!("{next} (once)"))
                    },
                );
                cx.st.row("boot_fails", s(v, "boot_fails"));
            });
        }
        Cmd::Overlay(OverlayCmd::Status) => {
            let v = ask(cx, "overlay", "status", &[])?.unwrap_or_else(overlay::status);
            emit(cx, &v, |v| {
                cx.st.header("overlay");
                cx.st.row("root", &root_line(&cx.st, v));
            });
        }
        Cmd::Overlay(OverlayCmd::Reset { cancel }) => {
            let args: &[&str] = if cancel { &["--cancel"] } else { &[] };
            let v = match ask(cx, "overlay", "reset", args)? {
                Some(v) => v,
                None => {
                    need_root("overlay reset")?;
                    let disk = overlay::request_reset(cancel)?;
                    json!({"reset_pending": !cancel, "disk": disk})
                }
            };
            emit(cx, &v, |v| {
                if v["reset_pending"].as_bool() == Some(true) {
                    println!(
                        "{} all saved system changes are wiped at the next boot ({}).\n\
                         Cancel with `amctl overlay reset --cancel`.",
                        cx.st.red("overlay reset requested:"),
                        s(v, "disk")
                    );
                } else {
                    println!("overlay reset {}", cx.st.purple("cancelled"));
                }
            });
        }
        Cmd::Keymap { cmd: None } => {
            let v = match ask(cx, "locale", "keymap", &[])? {
                Some(v) => v,
                None => {
                    let (l, _) = locale::load(Path::new("/"));
                    json!({"configured": l, "loaded": fs::read_to_string("/run/aftermath/keymap").ok().map(|s| s.trim().to_owned()), "probe_key_y": locale::probe_y_key().map(String::from)})
                }
            };
            emit(cx, &v, |v| {
                cx.st.header("keyboard");
                let c = &v["configured"];
                cx.st.row("console", s(c, "keymap"));
                cx.st.row(
                    "loaded",
                    &match v["loaded"].as_str() {
                        Some(k) => cx.st.purple(k),
                        None => cx.st.red("no (kernel default US layout)"),
                    },
                );
                cx.st.row(
                    "X11/Wayland",
                    &format!("{} {}", s(c, "xkb_layout"), s(c, "xkb_variant")),
                );
                if let Some(p) = v["probe_key_y"].as_str() {
                    cx.st
                        .row("check", &format!("the key right of T types `{p}`"));
                }
                println!(
                    "  {}",
                    cx.st
                        .dim("configured in /etc/aftermath/aftermath.toml [locale]")
                );
            });
        }
        Cmd::Keymap {
            cmd: Some(KeymapCmd::Set { name }),
        } => {
            let v = match ask(cx, "locale", "set", &[&name])? {
                Some(v) => v,
                None => {
                    need_root("keymap set")?;
                    let root = Path::new("/");
                    let how = locale::load_keymap(root, &name)?;
                    let text = fs::read_to_string(locale::CONFIG).unwrap_or_default();
                    fs::write(locale::CONFIG, locale::set_keymap_in(&text, &name))
                        .map_err(|e| e.to_string())?;
                    locale::write_derived(&locale::load(root).0, root);
                    json!({"keymap": name, "how": how})
                }
            };
            emit(cx, &v, |v| {
                println!(
                    "keymap {} loaded ({}) and saved",
                    cx.st.purple(s(v, "keymap")),
                    s(v, "how")
                )
            });
        }
        Cmd::Subsystems => {
            let v =
                ask(cx, "system", "subsystems", &[])?.ok_or_else(|| need_engine("subsystems"))?;
            emit(cx, &v, |v| {
                cx.st.header("subsystems");
                for sub in v.as_array().into_iter().flatten() {
                    cx.st.row(s(sub, "name"), s(sub, "about"));
                }
            });
        }
        Cmd::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "amctl", &mut std::io::stdout());
        }
    }
    Ok(())
}

// ------------------------------------------------------------- status ----

fn root_line(st: &Style, o: &Value) -> String {
    let mut line = if o["persistent"].as_bool() == Some(true) {
        let used = match (o["used_bytes"].as_u64(), o["size_bytes"].as_u64()) {
            (Some(u), Some(t)) if t > 0 => format!(", {}% of {} MiB used", u * 100 / t, t >> 20),
            _ => String::new(),
        };
        format!(
            "{} (overlay disk {}{used})",
            st.purple("persistent"),
            o["device"].as_str().unwrap_or("?")
        )
    } else {
        st.red("RAM only (changes are lost at poweroff)")
    };
    if o["reset_pending"].as_bool() == Some(true) {
        line.push_str(&format!("  {}", st.red("[reset at next boot]")));
    }
    line
}

fn print_status(st: &Style, v: &Value) {
    st.header("status");
    let mode = s(v, "mode");
    st.row(
        "mode",
        &match mode {
            "full" => st.purple("full (Arch host + strata)"),
            "host" => st.red("host (strata off: plan B)"),
            "busybox" => st.red("busybox rescue"),
            other => other.to_owned(),
        },
    );
    let e = &v["engine"];
    st.row(
        "engine",
        &if e["running"].as_bool() == Some(true) {
            format!("{} (pid {})", st.purple("running"), e["pid"])
        } else {
            st.red("NOT reachable (offline view)")
        },
    );
    st.row("root", &root_line(st, &v["overlay"]));
    let n = &v["network"];
    let ifs: Vec<String> = n["interfaces"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| format!("{} {}", s(i, "name"), s(i, "ipv4")))
        .collect();
    let dns: Vec<&str> = n["dns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    st.row(
        "network",
        &format!(
            "{}  {}{}{}",
            if n["online"].as_bool() == Some(true) {
                st.purple("online")
            } else {
                st.red("offline")
            },
            ifs.join(", "),
            n["gateway"]
                .as_str()
                .map(|g| format!("  gw {g}"))
                .unwrap_or_default(),
            if dns.is_empty() {
                String::new()
            } else {
                format!("  dns {}", dns.join(" "))
            }
        ),
    );
    let strata: Vec<String> = v["strata"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|x| format!("{} {}", s(x, "name"), st.state(s(x, "state"))))
        .collect();
    st.row(
        "strata",
        &if strata.is_empty() {
            st.dim("host only")
        } else {
            format!("arch (host) {}, {}", st.purple("up"), strata.join(", "))
        },
    );
    let sv = &v["services"];
    if sv.is_object() {
        let failed = sv["failed"].as_u64().unwrap_or(0);
        st.row(
            "services",
            &format!(
                "{} running, {} failed, {} total",
                sv["running"],
                if failed > 0 {
                    st.red(&failed.to_string())
                } else {
                    "0".into()
                },
                sv["total"]
            ),
        );
    } else {
        st.row("services", &st.dim("unknown (needs the engine)"));
    }
    st.row(
        "keymap",
        v["keymap"].as_str().unwrap_or("kernel default (US)"),
    );
    st.row(
        "uptime",
        &format!(
            "{}  {}",
            s(v, "uptime"),
            st.dim(&format!(
                "kernel {}  boot {}",
                s(v, "kernel"),
                s(v, "boot_id")
            ))
        ),
    );
    if let Some(l) = v["log_dir"].as_str() {
        st.row("logs", l);
    }
}

// ------------------------------------------------------------- strata ----

fn print_strata(st: &Style, v: &Value) {
    st.header("strata");
    println!(
        "  {}",
        st.dim(&format!(
            "{:<10} {:<8} {:<10} {:<8} {}",
            "NAME", "KIND", "STATE", "AT BOOT", "SYSTEM"
        ))
    );
    for x in v["strata"].as_array().into_iter().flatten() {
        let state = s(x, "state");
        let host = x["host"].as_bool() == Some(true);
        println!(
            "  {:<10} {:<8} {} {:<8} {}",
            st.bold(s(x, "name")),
            s(x, "kind"),
            st.state(&format!("{state:<10}")),
            if host {
                "always"
            } else if x["enabled"].as_bool() == Some(true) {
                "on"
            } else {
                "off"
            },
            if host {
                "the booted Arch root"
            } else {
                s(x, "os")
            }
        );
    }
}

fn print_stratum(st: &Style, x: &Value) {
    st.header(&format!("stratum {}", s(x, "name")));
    st.row("state", &st.state(s(x, "state")));
    st.row("kind", s(x, "kind"));
    st.row("path", s(x, "path"));
    st.row(
        "at boot",
        if x["enabled"].as_bool() == Some(true) {
            "on"
        } else {
            "off"
        },
    );
    if !s(x, "os").is_empty() {
        st.row("system", s(x, "os"));
    }
    if x["host"].as_bool() != Some(true) {
        println!(
            "  {}",
            st.dim(&format!("enter it: amctl strata enter {}", s(x, "name")))
        );
    }
}

fn strata(cx: &Ctx, c: StrataCmd) -> Result<(), String> {
    let (verb, name) = match &c {
        StrataCmd::List => ("list", None),
        StrataCmd::Status { name } => ("status", Some(name)),
        StrataCmd::Start { name } => ("start", Some(name)),
        StrataCmd::Stop { name } => ("stop", Some(name)),
        StrataCmd::Restart { name } => ("restart", Some(name)),
        StrataCmd::Enable { name } => ("enable", Some(name)),
        StrataCmd::Disable { name } => ("disable", Some(name)),
        StrataCmd::Enter { name, cmd } => return enter(name, cmd),
    };
    let args: Vec<&str> = name.map(|n| vec![n.as_str()]).unwrap_or_default();
    let v = match ask(cx, "strata", verb, &args)? {
        Some(v) => v,
        None => match verb {
            "list" => {
                let cfg = facts::load_cfg()?;
                let list: Result<Vec<Value>, String> = cfg
                    .stratum
                    .iter()
                    .map(|x| facts::stratum_json(&cfg, &x.name))
                    .collect();
                json!({"strata": list?})
            }
            "status" => facts::stratum_json(&facts::load_cfg()?, name.expect("name"))?,
            other => return Err(need_engine(&format!("strata {other}"))),
        },
    };
    emit(cx, &v, |v| {
        if verb == "list" {
            print_strata(&cx.st, v)
        } else {
            print_stratum(&cx.st, v)
        }
    });
    Ok(())
}

/// A shell (or command) inside a stratum, through `strat` (which chroots
/// and drops back to the calling user).
fn enter(name: &str, cmd: &[String]) -> Result<(), String> {
    let cfg = facts::load_cfg()?;
    let x = cfg
        .get(name)
        .ok_or_else(|| format!("no stratum `{name}` (see `amctl strata list`)"))?;
    let cmd: Vec<String> = if cmd.is_empty() {
        vec!["bash".into(), "-i".into()]
    } else {
        cmd.to_vec()
    };
    let err = if x.kind == "host" {
        Command::new(&cmd[0]).args(&cmd[1..]).exec()
    } else {
        // Ubuntu's bash prompt shows `(ubuntu)root@aftermath` with this.
        Command::new(STRAT_BIN)
            .arg(name)
            .args(&cmd)
            .env("debian_chroot", name)
            .exec()
    };
    Err(format!("cannot start {}: {err}", cmd[0]))
}

// ----------------------------------------------------------- services ----

fn print_services(st: &Style, v: &Value) {
    st.header("services");
    let list = v["services"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!(
            "  {}",
            st.dim(&format!("no services (put *.toml files in {SERVICES_DIR})"))
        );
    } else {
        println!(
            "  {}",
            st.dim(&format!(
                "{:<16} {:<11} {:<8} {:<7} {}",
                "NAME", "STATE", "PID", "BOOT", "DESCRIPTION"
            ))
        );
    }
    for x in &list {
        println!(
            "  {:<16} {} {:<8} {:<7} {}",
            st.bold(s(x, "name")),
            st.state(&format!("{:<11}", s(x, "state"))),
            x["pid"]
                .as_i64()
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into()),
            match x["enabled"].as_bool() {
                Some(true) => "on",
                Some(false) => "off",
                None => "?",
            },
            s(x, "description")
        );
    }
    for b in v["broken"].as_array().into_iter().flatten() {
        println!("  {} {}: {}", st.red("broken"), s(b, "file"), s(b, "error"));
    }
}

fn print_service(st: &Style, x: &Value) {
    st.header(&format!("service {}", s(x, "name")));
    if !s(x, "description").is_empty() {
        st.row("about", s(x, "description"));
    }
    let mut state = st.state(s(x, "state"));
    if let Some(p) = x["pid"].as_i64() {
        state.push_str(&format!(" (pid {p})"));
    }
    if !s(x, "for").is_empty() {
        state.push_str(&format!(" for {}", s(x, "for")));
    }
    st.row("state", &state);
    if !s(x, "why").is_empty() {
        st.row("why", &st.red(s(x, "why")));
    }
    if let Some(e) = x["exec"].as_array() {
        let e: Vec<&str> = e.iter().filter_map(Value::as_str).collect();
        st.row("exec", &e.join(" "));
    }
    if !s(x, "stratum").is_empty() {
        st.row("stratum", s(x, "stratum"));
    }
    st.row(
        "restart",
        &format!("{} ({} restarts so far)", s(x, "restart"), x["restarts"]),
    );
    st.row(
        "at boot",
        match x["enabled"].as_bool() {
            Some(true) => "on",
            Some(false) => "off",
            None => "?",
        },
    );
    if !s(x, "last_exit").is_empty() {
        st.row("last exit", s(x, "last_exit"));
    }
    if !s(x, "note").is_empty() {
        st.row("note", s(x, "note"));
    }
    if !s(x, "log").is_empty() {
        st.row("log", s(x, "log"));
    }
}

fn services(cx: &Ctx, c: SvcCmd) -> Result<(), String> {
    let (verb, name) = match &c {
        SvcCmd::List => ("list", None),
        SvcCmd::Reload => ("reload", None),
        SvcCmd::Status { name } => ("status", Some(name)),
        SvcCmd::Start { name } => ("start", Some(name)),
        SvcCmd::Stop { name } => ("stop", Some(name)),
        SvcCmd::Restart { name } => ("restart", Some(name)),
        SvcCmd::Enable { name } => ("enable", Some(name)),
        SvcCmd::Disable { name } => ("disable", Some(name)),
    };
    let args: Vec<&str> = name.map(|n| vec![n.as_str()]).unwrap_or_default();
    let v = match ask(cx, "services", verb, &args)? {
        Some(v) => v,
        None if verb == "list" => {
            let (defs, broken) = load_dir(Path::new(SERVICES_DIR));
            let list: Vec<Value> = defs
                .iter()
                .map(|d| json!({"name": d.name, "state": "unknown", "description": d.description, "enabled": d.enabled}))
                .collect();
            let broken: Vec<Value> = broken
                .iter()
                .map(|(f, e)| json!({"file": f, "error": e}))
                .collect();
            json!({"services": list, "broken": broken})
        }
        None => return Err(need_engine(&format!("services {verb}"))),
    };
    emit(cx, &v, |v| match verb {
        "list" => print_services(&cx.st, v),
        "reload" => println!(
            "loaded {} service file(s), {} broken",
            v["loaded"], v["broken"]
        ),
        _ => print_service(&cx.st, v),
    });
    Ok(())
}

// --------------------------------------------------------------- power ----

fn power(cx: &Ctx, verb: &str) -> Result<(), String> {
    if ask(cx, "system", verb, &[])?.is_some() {
        if !cx.json {
            println!("{}", cx.st.purple(&format!("{verb}...")));
        }
        return Ok(());
    }
    need_root(verb)?;
    use nix::sys::signal::{kill, Signal};
    let sig = if verb == "poweroff" {
        Signal::SIGUSR2
    } else {
        Signal::SIGINT
    };
    // PID 1 (aftermath-init) shuts down cleanly on these signals.
    if kill(nix::unistd::Pid::from_raw(1), sig).is_ok() {
        println!("{verb}: asked PID 1");
        return Ok(());
    }
    // Last resort: sync and do it ourselves.
    nix::unistd::sync();
    let mode = if verb == "poweroff" {
        nix::sys::reboot::RebootMode::RB_POWER_OFF
    } else {
        nix::sys::reboot::RebootMode::RB_AUTOBOOT
    };
    let err = nix::sys::reboot::reboot(mode).unwrap_err();
    Err(format!("reboot syscall: {err}"))
}

// ---------------------------------------------------------------- logs ----

fn log_dir() -> Result<PathBuf, String> {
    if let Some(d) = facts::log_dir() {
        return Ok(PathBuf::from(d));
    }
    for base in ["/aftermath/store/logs/latest", "/run/aftermath/logs/latest"] {
        if Path::new(base).is_dir() {
            return fs::canonicalize(base).map_err(|e| e.to_string());
        }
    }
    Err("no session logs found for this boot".into())
}

fn logs(cx: &Ctx, a: LogsArgs) -> Result<(), String> {
    let dir = log_dir()?;
    if a.list {
        cx.st.header(&format!("logs in {}", dir.display()));
        let mut files = Vec::new();
        for e in fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
            let p = e.path();
            if p.is_dir() {
                for f in fs::read_dir(&p).into_iter().flatten().flatten() {
                    files.push(f.path());
                }
            } else {
                files.push(p);
            }
        }
        files.sort();
        for f in files {
            let size = fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
            let rel = f.strip_prefix(&dir).unwrap_or(&f).display().to_string();
            println!("  {:<28} {}", rel, cx.st.dim(&format!("{size} bytes")));
        }
        return Ok(());
    }
    let (file, filter): (PathBuf, Option<&str>) = if let Some(s) = &a.service {
        if !aftermath_ctl::services::valid_name(s) {
            return Err(format!("invalid service name `{s}`"));
        }
        (dir.join("services").join(format!("{s}.log")), None)
    } else if let Some(c) = &a.console {
        if !c.chars().all(|ch| ch.is_ascii_alphanumeric()) {
            return Err(format!("invalid console `{c}`"));
        }
        (dir.join(format!("{c}.log")), None)
    } else if a.kernel {
        (dir.join("kmsg.log"), None)
    } else {
        (dir.join("kmsg.log"), Some("aftermath"))
    };
    let mut f = File::open(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut text = String::new();
    f.read_to_string(&mut text).map_err(|e| e.to_string())?;
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| filter.is_none_or(|w| l.contains(w)))
        .collect();
    let start = lines.len().saturating_sub(a.lines);
    let out = std::io::stdout();
    let mut out = out.lock();
    for l in &lines[start..] {
        let _ = writeln!(out, "{l}");
    }
    let _ = out.flush();
    if !a.follow {
        return Ok(());
    }
    let mut pos = f.stream_position().map_err(|e| e.to_string())?;
    let mut partial = String::new();
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let len = fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        if len < pos {
            pos = 0; // truncated / rotated
        }
        if len == pos {
            continue;
        }
        let _ = f.seek(SeekFrom::Start(pos));
        let mut buf = Vec::new();
        let _ = (&mut f).take(len - pos).read_to_end(&mut buf);
        pos = len;
        partial.push_str(&String::from_utf8_lossy(&buf));
        while let Some(i) = partial.find('\n') {
            let line: String = partial.drain(..=i).collect();
            if filter.is_none_or(|w| line.contains(w)) {
                print!("{line}");
            }
        }
        let _ = std::io::stdout().flush();
    }
}
