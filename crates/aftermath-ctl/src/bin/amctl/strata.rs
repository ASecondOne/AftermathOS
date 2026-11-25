//! `amctl strata ...`: list, inspect, start/stop and enter strata.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use aftermath_ctl::{facts, STRATA_SERVICE, STRAT_BIN};
use serde_json::{json, Value};

use crate::cli::StrataCmd;
use crate::output::{is_true, items, need_engine, text, Ctx, Style};

pub fn run(cx: &Ctx, cmd: StrataCmd) -> Result<(), String> {
    let (verb, name) = match cmd {
        StrataCmd::Enter { name, cmd } => return enter(&name, &cmd),
        StrataCmd::List => ("list", None),
        StrataCmd::Status { name } => ("status", Some(name)),
        StrataCmd::Start { name } => ("start", Some(name)),
        StrataCmd::Stop { name } => ("stop", Some(name)),
        StrataCmd::Restart { name } => ("restart", Some(name)),
        StrataCmd::Enable { name } => ("enable", Some(name)),
        StrataCmd::Disable { name } => ("disable", Some(name)),
    };

    let args: Vec<&str> = name.as_deref().into_iter().collect();
    if verb == "enable" {
        // Preflight through the engine before downloading anything. The long
        // transfer runs as amctl's child, never in the engine's event loop.
        let ready = cx
            .ask("strata", "prepare-enable", &args)?
            .ok_or_else(|| need_engine("enable"))?;
        if !is_true(&ready, "installed") && is_true(&ready, "installable") {
            crate::output::need_root("enable")?;
            eprintln!(
                "amctl: downloading and installing {}; this may take a few minutes",
                args[0]
            );
            let status = Command::new(STRATA_SERVICE)
                .args(["install", args[0]])
                .stdin(Stdio::null())
                // Keep --json stdout machine readable, with progress on stderr.
                .stdout(Stdio::from(std::io::stderr()))
                .status()
                .map_err(|e| format!("{STRATA_SERVICE}: {e}"))?;
            if !status.success() {
                return Err(format!(
                    "installation of {} failed ({status}); retry `amctl enable {}`",
                    args[0], args[0]
                ));
            }
        }
    }
    let reply = match cx.ask("strata", verb, &args)? {
        Some(v) => v,
        None => offline(verb, name.as_deref())?,
    };

    cx.emit(&reply, |v| {
        if verb == "list" {
            print_list(&cx.style, v);
        } else {
            print_one(&cx.style, v);
        }
    });
    Ok(())
}

/// Plan B: reading works without the engine, changing does not.
fn offline(verb: &str, name: Option<&str>) -> Result<Value, String> {
    let cfg = facts::load_cfg()?;

    match (verb, name) {
        ("list", _) => {
            let all: Result<Vec<Value>, String> = cfg
                .stratum
                .iter()
                .map(|s| facts::stratum_json(&cfg, &s.name))
                .collect();
            Ok(json!({ "strata": all? }))
        }
        ("status", Some(name)) => facts::stratum_json(&cfg, name),
        _ => Err(need_engine(&format!("strata {verb}"))),
    }
}

fn print_list(st: &Style, v: &Value) {
    st.header("strata");

    let heading = format!(
        "{:<10} {:<8} {:<10} {:<10} {:<10} {:<8} {}",
        "NAME", "KIND", "INSTALLED", "INTEGRATED", "STATE", "AT BOOT", "SYSTEM"
    );
    println!("  {}", st.dim(&heading));

    for s in items(v, "strata") {
        let host = is_true(s, "host");
        let at_boot = if host {
            "always"
        } else if is_true(s, "enabled") {
            "on"
        } else {
            "off"
        };
        let system = if host {
            "the booted Arch root"
        } else {
            text(s, "os")
        };

        println!(
            "  {} {:<8} {:<10} {:<10} {} {:<8} {}",
            st.bold(&format!("{:<10}", text(s, "name"))), // pad before colouring
            text(s, "kind"),
            if is_true(s, "installed") { "yes" } else { "no" },
            if is_true(s, "integrated") {
                "yes"
            } else {
                "no"
            },
            st.state(&format!("{:<10}", text(s, "state"))),
            at_boot,
            system
        );
    }
}

fn print_one(st: &Style, s: &Value) {
    st.header(&format!("stratum {}", text(s, "name")));

    st.row("state", &st.state(text(s, "state")));
    st.row("kind", text(s, "kind"));
    st.row(
        "installed",
        if is_true(s, "installed") { "yes" } else { "no" },
    );
    st.row(
        "integrated",
        if is_true(s, "integrated") {
            "yes"
        } else {
            "no"
        },
    );
    st.row("path", text(s, "path"));
    st.row("at boot", if is_true(s, "enabled") { "on" } else { "off" });

    if !text(s, "os").is_empty() {
        st.row("system", text(s, "os"));
    }

    if !is_true(s, "host") {
        let hint = format!("enter it: amctl strata enter {}", text(s, "name"));
        println!("  {}", st.dim(&hint));
    }
}

/// A shell (or one command) inside a stratum. Uses `strat`, which chroots
/// and drops back to the calling user. Only returns on error.
fn enter(name: &str, cmd: &[String]) -> Result<(), String> {
    let cfg = facts::load_cfg()?;
    let stratum = cfg
        .get(name)
        .ok_or_else(|| format!("no stratum `{name}` (see `amctl strata list`)"))?;

    let cmd: Vec<String> = if cmd.is_empty() {
        vec!["bash".into(), "-i".into()]
    } else {
        cmd.to_vec()
    };

    let err = if stratum.kind == "host" {
        Command::new(&cmd[0]).args(&cmd[1..]).exec()
    } else {
        Command::new(STRAT_BIN)
            .arg(name)
            .args(&cmd)
            // Ubuntu's bash prompt then reads `(ubuntu)root@aftermath`.
            .env("debian_chroot", name)
            .exec()
    };
    Err(format!("cannot start {}: {err}", cmd[0]))
}
